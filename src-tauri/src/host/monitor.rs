//! Background PR/MR monitor (issue #110 T1): the load-bearing piece that
//! makes tracked PR/MR state a durable, PROCESS-level fact instead of
//! something that only lives in an agent's turn or a chat session's memory —
//! a session can hit a 500 or a quota wall and evaporate; this sweep does
//! not. Mirrors `lead_chat::revive::spawn_stall_watch`'s shape deliberately:
//! an immediate first pass (so "at boot" and "at runtime" share the exact
//! same in-memory notice-tracking state, never two independently-seeded
//! copies — see that function's module doc for the double-bookkeeping bug a
//! split boot/runtime pass caused there) followed by a timer loop.
//!
//! HARD BOUNDARY, enforced by construction (not just this comment): this
//! module calls `PrHost::fetch_status` (a READ) and then only ever writes
//! into weft's OWN `pull_request` table (`store::repo::apply_pull_request_
//! snapshot` / `mark_pull_request_probe_error`) or posts/retracts a weft
//! Needs-you notice (`BusRegistry::notify_human` / `cancel_open_asks_by_id`).
//! It NEVER shells out to `gh`/`glab` for a MUTATING subcommand (`pr merge`,
//! `pr comment`, `pr review`, …), never touches git, and never resolves a
//! review thread on the host. Every host-facing call in this file funnels
//! through `PrHost`, whose trait signature (`fetch_status(&self, target) ->
//! Result<PrSnapshot, HostError>`) has no side-effecting counterpart to call
//! by mistake — that stays exclusively the agent's job, using the agent's own
//! tools.

use std::collections::HashMap;

use tauri::{AppHandle, Emitter, Manager};

use super::judge;
use super::{HostKind, MergeReadiness, PrSnapshot, PrTarget};
use crate::store::entities::pull_request;
use crate::store::{repo, Db};

/// Default sweep cadence, mirroring `revive::spawn_stall_watch`'s
/// `STALL_REDRIVE_SWEEP_DEFAULT_SECS`. `0` disables the loop entirely.
/// Override with `WEFT_PR_SWEEP_SECS`.
const PR_SWEEP_DEFAULT_SECS: u64 = 60;

/// After this many CONSECUTIVE failed probes, a row stops being swept (see
/// `repo::list_open_pull_requests`) — it is NOT retried forever. At the
/// default 60s cadence this is ~10 minutes of persistent failure: long
/// enough to ride out a transient blip (a network hiccup, a momentary `gh`
/// rate limit), short enough not to hammer the host indefinitely for
/// something that has clearly stopped being transient (a deleted PR, `gh`
/// auth revoked and never restored). The row's last-posted Needs-you notice
/// is left in place when this fires — not retracted (that would falsely
/// claim "resolved") — an honest "here's the last thing we knew, we've
/// stopped checking" rather than a silent infinite retry loop.
const MAX_CONSECUTIVE_PROBE_FAILURES: i32 = 10;

/// Which bus method should post a notice this module computed — mirrors the
/// two NOTICE variants of `bus::AskKind` (this module never posts a
/// `Question`, so it gets its own narrower, exhaustively-matched local type
/// instead of reusing the 3-way one and having to handle an impossible arm).
/// [`error_notice_text`] is the ONE place that decides this for a failed
/// probe; keeping the choice as data (not a direct `notify_human*` call
/// buried in that function) lets [`apply_notice_action`] be the single place
/// that actually calls the bus, for both the `Ok` (readiness) and `Err`
/// (probe error) paths alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeKind {
    /// A background sweep will retract this notice on its own once the
    /// condition it describes changes.
    SelfClearing,
    /// Nothing will retract this notice automatically — see
    /// `judge::give_up_text`.
    ActionRequired,
}

/// Start the runtime PR/MR sweep. Call once at app setup, alongside
/// `revive::spawn_stall_watch` (see `lib.rs`).
pub fn spawn_pr_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let sweep_secs = crate::commands::env_secs("WEFT_PR_SWEEP_SECS", PR_SWEEP_DEFAULT_SECS);
        if sweep_secs == 0 {
            return; // disabled
        }
        // Owned by this task for the process's whole life — the same
        // single-owner discipline as `spawn_stall_watch`'s `stopped_notices`:
        // one notice latch, seeded by an IMMEDIATE first pass, so a notice
        // posted before the first timer tick is tracked from the start
        // instead of being re-posted (or left un-retractable) a tick later.
        // Keyed by pr_id -> (thread_id it was posted under, ask_id, text) —
        // the thread_id MUST be stored alongside the ask, not read live off
        // the row at retraction time: `register_pull_request`'s own doc says
        // thread/direction can legitimately change across a re-registration,
        // and `cancel_open_asks_by_id` is scoped to a specific thread's bus,
        // so retracting under the row's CURRENT (possibly reassigned)
        // thread_id would silently miss the ask actually posted under the
        // OLD one, leaving it stuck forever (no manual dismiss exists for a
        // non-answerable notice — see `NeedsRows.tsx`'s `AskRow`). Mirrors
        // `revive::spawn_stall_watch`'s `stopped_notices: HashMap<i32, (i32,
        // u64)>` (dir_id -> (thread_id, ask_id)) exactly, for exactly the
        // same reason.
        let mut notices: HashMap<i32, (i32, u64, String)> = HashMap::new();
        run_pr_sweep(&app, &mut notices).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(sweep_secs)).await;
            run_pr_sweep(&app, &mut notices).await;
        }
    });
}

/// One sweep pass over every still-`open` tracked row.
async fn run_pr_sweep(app: &AppHandle, notices: &mut HashMap<i32, (i32, u64, String)>) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    let Some(bus) = app.try_state::<crate::bus::BusRegistry>() else {
        return;
    };
    let bus = bus.inner().clone();

    let open = match repo::list_open_pull_requests(&db, MAX_CONSECUTIVE_PROBE_FAILURES).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weft][host] pr sweep: could not list tracked PR/MRs: {e}");
            return;
        }
    };
    for pr in open {
        check_one(app, &db, &bus, notices, pr).await;
    }
}

/// Probe one PR/MR (off the async runtime — `PrHost::fetch_status` shells out
/// synchronously, same `spawn_blocking` discipline `check::run_checks`'
/// caller already uses) and apply the result.
async fn check_one(
    app: &AppHandle,
    db: &Db,
    bus: &crate::bus::BusRegistry,
    notices: &mut HashMap<i32, (i32, u64, String)>,
    pr: pull_request::Model,
) {
    let Some(kind) = HostKind::parse(&pr.host_kind) else {
        apply_probe_result(
            app,
            db,
            bus,
            notices,
            &pr,
            None,
            Err(super::HostError::Other {
                message: format!("unrecognized host_kind '{}' on this row", pr.host_kind),
            }),
        )
        .await;
        return;
    };
    let target = PrTarget { owner: pr.host_owner.clone(), repo: pr.host_repo.clone(), number: pr.number };
    let result = tokio::task::spawn_blocking(move || {
        super::resolve_host(kind).and_then(|h| h.fetch_status(&target))
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(super::HostError::Other { message: format!("internal: sweep task join error: {join_err}") })
    });
    apply_probe_result(app, db, bus, notices, &pr, Some(kind), result).await;
}

/// Write the fetch outcome back to the DB, then reconcile the Needs-you
/// notice. `kind` is `None` only for the "unrecognized host_kind" data-shape
/// case above (never a real host), just enough to still build SOME diagnostic
/// text without a native-terminology abbreviation to hang it on.
async fn apply_probe_result(
    app: &AppHandle,
    db: &Db,
    bus: &crate::bus::BusRegistry,
    notices: &mut HashMap<i32, (i32, u64, String)>,
    pr: &pull_request::Model,
    kind: Option<HostKind>,
    result: Result<PrSnapshot, super::HostError>,
) {
    let desired: Option<(NoticeKind, String)> = match &result {
        Ok(snapshot) => {
            let readiness = judge::merge_readiness(&snapshot.ci, &snapshot.review, &snapshot.conflict);
            let changed = snapshot_changed(pr, snapshot, &readiness);
            if let Err(e) = repo::apply_pull_request_snapshot(db, pr.id, snapshot, &readiness).await {
                eprintln!("[weft][host] pr #{}: could not save snapshot: {e}", pr.id);
            } else if changed {
                emit_pr_changed(app, pr);
            }
            if snapshot.lifecycle != super::PrLifecycle::Open {
                None // merged/closed — the readiness question is moot now
            } else {
                kind.and_then(|k| judge::notice_text(k, pr.number, &readiness))
                    .map(|text| (NoticeKind::SelfClearing, text))
            }
        }
        Err(e) => {
            let fail_count = match repo::mark_pull_request_probe_error(db, pr.id, &e.message()).await {
                Ok(count) => count,
                Err(store_err) => {
                    eprintln!("[weft][host] pr #{}: could not save probe error: {store_err}", pr.id);
                    None
                }
            };
            Some(error_notice_text(kind, pr.number, e, fail_count))
        }
    };

    reconcile_notice(bus, app, notices, pr, desired);
}

/// Which notice text (and [`NoticeKind`]) a FAILED probe should produce,
/// given the NEW consecutive-failure count this attempt just wrote (`None` if
/// the row vanished mid-write — no row to report a streak for). Pure and
/// unit-tested in isolation from `mark_pull_request_probe_error`'s DB write:
/// the boundary condition (`fail_count >= MAX_CONSECUTIVE_PROBE_FAILURES`) is
/// precisely the ONE sweep `list_open_pull_requests` is about to start
/// excluding this row from — so it is the ONLY sweep allowed to say "gave
/// up" (`judge::give_up_text`, `NoticeKind::ActionRequired` — the row will
/// NOT be revisited again on its own) instead of "still checking, will
/// retry" (`judge::probe_error_text`, `NoticeKind::SelfClearing`). Getting
/// this boundary wrong either direction is a real regression:
/// off-by-one-early falsely claims tracking stopped while it's still
/// retrying; off-by-one-late repeats the ordinary text (and the WRONG,
/// self-clearing kind) on the exact sweep where tracking silently stops for
/// good (P1-A: the bug this function exists to prevent from recurring).
fn error_notice_text(
    kind: Option<HostKind>,
    pr_number: i32,
    error: &super::HostError,
    fail_count: Option<i32>,
) -> (NoticeKind, String) {
    let gave_up_now = fail_count.is_some_and(|c| c >= MAX_CONSECUTIVE_PROBE_FAILURES);
    match kind {
        Some(k) if gave_up_now => (NoticeKind::ActionRequired, judge::give_up_text(k, pr_number, error)),
        Some(k) => (NoticeKind::SelfClearing, judge::probe_error_text(k, pr_number, error)),
        None => (
            NoticeKind::SelfClearing,
            format!("🔌 无法查询 PR/MR #{pr_number} 的状态:{}。", error.message()),
        ),
    }
}

/// Apply whatever [`judge::plan_notice_action`] says about the Needs-you
/// notice for this PR/MR against the real bus + notice tracker, THEN emit
/// `needs-you://changed` for every thread that actually needs a refresh.
/// Split into a pure core ([`apply_notice_action`], which owns the actual
/// decision + `BusRegistry`/map mutation and is unit-tested directly against
/// a real `BusRegistry::new()` — no `AppHandle`/Tauri runtime needed) plus
/// this thin `AppHandle`-emitting shell, so the P1 regression test below
/// (thread reassignment must still retract cleanly) doesn't need a Tauri
/// runtime to run.
fn reconcile_notice(
    bus: &crate::bus::BusRegistry,
    app: &AppHandle,
    notices: &mut HashMap<i32, (i32, u64, String)>,
    pr: &pull_request::Model,
    desired: Option<(NoticeKind, String)>,
) {
    for thread_id in apply_notice_action(bus, notices, pr, desired) {
        let _ = app.emit("needs-you://changed", thread_id);
    }
}

/// The pure decision + mutation core of [`reconcile_notice`]: post, replace,
/// retract, or leave alone. `from` is the direction id (as a string) that
/// owns this PR — same convention every other per-task notice in this
/// codebase already uses (see `lead_chat::engine`'s stall/freeze notices), so
/// it lands attributed to the right task in the Needs-you list with zero
/// frontend changes.
///
/// Returns every thread id that needs a `needs-you://changed` refresh (0, 1,
/// or — on a `Replace` that crosses a thread reassignment — 2 DISTINCT
/// threads: the old one loses a card, the new one gains one).
///
/// The critical property, and the P1 fix this function embodies: retraction
/// ALWAYS targets the thread_id STORED alongside the tracked ask (`old_
/// thread_id`), never `pr.thread_id` (the row's CURRENT, possibly-reassigned
/// value) — `cancel_open_asks_by_id` is scoped to one thread's bus, so
/// canceling under the wrong thread silently no-ops (returns `false`) and
/// strands the notice under its original thread forever, un-retractable and
/// with no manual dismiss (see `NeedsRows.tsx`'s `AskRow`, which has an
/// answer box only for a Question, never a close button for a NOTICE).
///
/// Also the ONE place that calls into the bus to actually POST a notice — so
/// it is the single spot that picks `notify_human` vs `notify_human_action_
/// required` from the incoming [`NoticeKind`] (via `post_notice`), for both
/// the `Post` and `Replace` arms alike. `desired`'s text (`.1`) is still what
/// `plan_notice_action` compares against the previously-posted text to decide
/// NoOp/Post/Replace/Retract — the KIND never affects that decision, only
/// which method posts the result.
fn apply_notice_action(
    bus: &crate::bus::BusRegistry,
    notices: &mut HashMap<i32, (i32, u64, String)>,
    pr: &pull_request::Model,
    desired: Option<(NoticeKind, String)>,
) -> Vec<i32> {
    let existing = notices.get(&pr.id).map(|(_, _, text)| text.as_str());
    let desired_text = desired.as_ref().map(|(_, text)| text.as_str());
    let action = judge::plan_notice_action(existing, desired_text);
    let mut changed_threads = Vec::new();
    match action {
        judge::NoticeAction::NoOp => {}
        judge::NoticeAction::Post => {
            if let Some((kind, text)) = desired {
                let id = post_notice(bus, kind, pr.thread_id, &pr.direction_id.to_string(), &text);
                notices.insert(pr.id, (pr.thread_id, id, text));
                changed_threads.push(pr.thread_id);
            }
        }
        judge::NoticeAction::Replace => {
            if let Some((old_thread_id, old_id, _)) = notices.remove(&pr.id) {
                bus.cancel_open_asks_by_id(old_thread_id, old_id);
                changed_threads.push(old_thread_id);
            }
            if let Some((kind, text)) = desired {
                let id = post_notice(bus, kind, pr.thread_id, &pr.direction_id.to_string(), &text);
                notices.insert(pr.id, (pr.thread_id, id, text));
                changed_threads.push(pr.thread_id);
            }
        }
        judge::NoticeAction::Retract => {
            if let Some((old_thread_id, old_id, _)) = notices.remove(&pr.id) {
                if bus.cancel_open_asks_by_id(old_thread_id, old_id) {
                    changed_threads.push(old_thread_id);
                }
            }
        }
    }
    changed_threads.sort_unstable();
    changed_threads.dedup();
    changed_threads
}

/// Post one notice via the bus method matching its [`NoticeKind`] — the only
/// place this module chooses between `notify_human` and `notify_human_
/// action_required`, so `Post` and `Replace` in [`apply_notice_action`] can't
/// drift apart on that choice.
fn post_notice(bus: &crate::bus::BusRegistry, kind: NoticeKind, thread: i32, from: &str, text: &str) -> u64 {
    match kind {
        NoticeKind::SelfClearing => bus.notify_human(thread, from, text),
        NoticeKind::ActionRequired => bus.notify_human_action_required(thread, from, text),
    }
}

fn emit_pr_changed(app: &AppHandle, pr: &pull_request::Model) {
    let _ = app.emit(
        "pr://changed",
        serde_json::json!({
            "pr_id": pr.id,
            "thread_id": pr.thread_id,
            "direction_id": pr.direction_id,
        }),
    );
}

/// Whether a freshly fetched snapshot differs from the last stored row in any
/// way worth telling the frontend about — decides `pr://changed` emission
/// only; the DB write itself always happens regardless (cheap, and keeps
/// `last_checked_at` honest even on a sweep where nothing else moved). This is
/// the OTHER function this PR's mutation self-check targets alongside
/// `judge::merge_readiness`: flip any comparison below and a test must fail.
fn snapshot_changed(old: &pull_request::Model, snapshot: &PrSnapshot, readiness: &MergeReadiness) -> bool {
    let new_ci = serde_json::to_string(&snapshot.ci).unwrap_or_default();
    let new_review = serde_json::to_string(&snapshot.review).unwrap_or_default();
    let new_conflict = serde_json::to_string(&snapshot.conflict).unwrap_or_default();
    let new_readiness = serde_json::to_string(readiness).unwrap_or_default();
    old.head_sha != snapshot.head_sha
        || old.lifecycle != snapshot.lifecycle.as_str()
        || old.ci_status != new_ci
        || old.review_status != new_review
        || old.conflict_status != new_conflict
        || old.merge_readiness != new_readiness
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{CiStatus, ConflictStatus, HostError, PrLifecycle, ReviewStatus};

    // --- P1-A: the give-up boundary must never strand a row silently -----

    #[test]
    fn under_the_threshold_still_reports_an_ordinary_probe_error() {
        let err = HostError::NotFound;
        let (kind, text) =
            error_notice_text(Some(HostKind::GitHub), 1, &err, Some(MAX_CONSECUTIVE_PROBE_FAILURES - 1));
        assert_eq!(text, judge::probe_error_text(HostKind::GitHub, 1, &err));
        assert_eq!(kind, NoticeKind::SelfClearing, "still retrying — must stay self-clearing");
    }

    #[test]
    fn exactly_at_the_threshold_reports_gave_up_not_an_ordinary_error() {
        // This is the EXACT sweep `list_open_pull_requests` starts excluding
        // the row from — the one and only chance to tell the human tracking
        // just stopped, instead of silently going quiet.
        let err = HostError::NotFound;
        let (kind, text) =
            error_notice_text(Some(HostKind::GitHub), 1, &err, Some(MAX_CONSECUTIVE_PROBE_FAILURES));
        assert_eq!(text, judge::give_up_text(HostKind::GitHub, 1, &err));
        assert_eq!(
            kind,
            NoticeKind::ActionRequired,
            "the give-up sweep must be tagged action-required, not self-clearing"
        );
    }

    #[test]
    fn past_the_threshold_also_reports_gave_up() {
        // Reachable if the threshold constant is ever lowered while a row
        // already has a higher stored count — must not fall through to the
        // ordinary-error text just because it's not an EXACT match.
        let err = HostError::NotFound;
        let (kind, text) =
            error_notice_text(Some(HostKind::GitHub), 1, &err, Some(MAX_CONSECUTIVE_PROBE_FAILURES + 5));
        assert_eq!(text, judge::give_up_text(HostKind::GitHub, 1, &err));
        assert_eq!(kind, NoticeKind::ActionRequired);
    }

    #[test]
    fn a_missing_row_never_claims_gave_up() {
        // `fail_count: None` means the DB write couldn't even find the row —
        // that's a DIFFERENT fact from "we tracked it and gave up", and must
        // not be misreported as the latter.
        let err = HostError::NotFound;
        let (kind, text) = error_notice_text(Some(HostKind::GitHub), 1, &err, None);
        assert_eq!(text, judge::probe_error_text(HostKind::GitHub, 1, &err));
        assert_eq!(kind, NoticeKind::SelfClearing);
    }

    fn base_row() -> pull_request::Model {
        pull_request::Model {
            id: 1,
            thread_id: 1,
            direction_id: 1,
            repo_id: 1,
            host_kind: "github".to_string(),
            host_base: "github.com".to_string(),
            host_owner: "acme".to_string(),
            host_repo: "widgets".to_string(),
            number: 1,
            url: String::new(),
            title: String::new(),
            head_sha: "aaa".to_string(),
            base_ref: "main".to_string(),
            lifecycle: "open".to_string(),
            ci_status: serde_json::to_string(&CiStatus::Passing).unwrap(),
            review_status: serde_json::to_string(&ReviewStatus::Approved).unwrap(),
            conflict_status: serde_json::to_string(&ConflictStatus::Clean).unwrap(),
            merge_readiness: serde_json::to_string(&MergeReadiness::Ready).unwrap(),
            last_checked_at: "100".to_string(),
            last_error: String::new(),
            probe_fail_count: 0,
            created_at: "1".to_string(),
        }
    }

    fn base_snapshot() -> PrSnapshot {
        PrSnapshot {
            head_sha: "aaa".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: PrLifecycle::Open,
            ci: CiStatus::Passing,
            review: ReviewStatus::Approved,
            conflict: ConflictStatus::Clean,
        }
    }

    #[test]
    fn identical_snapshot_is_not_a_change() {
        let old = base_row();
        let snap = base_snapshot();
        let readiness = MergeReadiness::Ready;
        assert!(!snapshot_changed(&old, &snap, &readiness));
    }

    #[test]
    fn a_new_head_sha_is_a_change() {
        let old = base_row();
        let mut snap = base_snapshot();
        snap.head_sha = "bbb".to_string();
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready));
    }

    #[test]
    fn a_ci_status_flip_is_a_change_even_when_it_does_not_move_the_readiness_bucket() {
        // Passing → NotConfigured is the tricky case: both map to the SAME
        // AxisVerdict::Clear with no reason text, so `MergeReadiness` stays
        // `Ready` either way and can't be the thing that notices this flip —
        // only a DEDICATED `ci_status` comparison catches it. (Failing would
        // also change `merge_readiness` itself, which is too easy: that
        // wouldn't prove this specific comparison is pulling its weight.)
        let old = base_row();
        let mut snap = base_snapshot();
        snap.ci = CiStatus::NotConfigured;
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready));
    }

    #[test]
    fn a_lifecycle_flip_is_a_change_even_if_axes_are_identical() {
        let old = base_row();
        let mut snap = base_snapshot();
        snap.lifecycle = PrLifecycle::Merged;
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready));
    }

    #[test]
    fn a_merge_readiness_flip_alone_is_a_change() {
        // Same raw axes stored, but readiness recomputed differently (e.g. the
        // judgement function itself changed) must still be caught — this is
        // what keeps `snapshot_changed` honest about DERIVED state, not just
        // the three raw axis columns.
        let old = base_row();
        let snap = base_snapshot();
        let readiness = MergeReadiness::Blocked { reasons: vec!["something".to_string()] };
        assert!(snapshot_changed(&old, &snap, &readiness));
    }

    // --- P1: the notice tracker must survive a thread reassignment --------

    #[test]
    fn replace_retracts_the_notice_under_its_original_thread_even_after_reassignment() {
        let bus = crate::bus::BusRegistry::new();
        let mut notices: HashMap<i32, (i32, u64, String)> = HashMap::new();

        // The PR starts tracked under thread 1; a Blocked notice is posted
        // there.
        let mut pr = base_row();
        pr.thread_id = 1;
        let touched = apply_notice_action(
            &bus,
            &mut notices,
            &pr,
            Some((NoticeKind::SelfClearing, "first blocked text".to_string())),
        );
        assert_eq!(touched, vec![1]);
        assert_eq!(bus.open_asks(1).len(), 1, "the notice must land in thread 1's queue");

        // `register_pull_request`'s own doc: "thread/direction/repo can
        // legitimately change across a re-registration". Simulate exactly
        // that — the row now belongs to thread 2 — AND the readiness text
        // changes too (still not-Ready, so this is a Replace, not a NoOp).
        pr.thread_id = 2;
        let touched = apply_notice_action(
            &bus,
            &mut notices,
            &pr,
            Some((NoticeKind::SelfClearing, "second blocked text".to_string())),
        );

        assert!(
            bus.open_asks(1).is_empty(),
            "the stale notice under thread 1 must be retracted, not stranded there forever"
        );
        assert_eq!(bus.open_asks(2).len(), 1, "the fresh notice must be posted under the NEW thread");
        assert!(touched.contains(&1), "thread 1's Needs-you list lost a card and needs a refresh");
        assert!(touched.contains(&2), "thread 2's Needs-you list gained a card and needs a refresh");
    }

    #[test]
    fn retract_after_reassignment_also_targets_the_original_thread() {
        let bus = crate::bus::BusRegistry::new();
        let mut notices: HashMap<i32, (i32, u64, String)> = HashMap::new();

        let mut pr = base_row();
        pr.thread_id = 1;
        apply_notice_action(&bus, &mut notices, &pr, Some((NoticeKind::SelfClearing, "blocked".to_string())));
        assert_eq!(bus.open_asks(1).len(), 1);

        // Reassigned to thread 2, AND the PR became Ready (desired =
        // None) in the same sweep — a pure Retract, no Replace involved.
        pr.thread_id = 2;
        let touched = apply_notice_action(&bus, &mut notices, &pr, None);

        assert!(bus.open_asks(1).is_empty(), "must retract from thread 1, where the ask actually lives");
        assert!(bus.open_asks(2).is_empty(), "nothing was ever posted under thread 2");
        assert_eq!(touched, vec![1]);
    }

    #[test]
    fn replace_under_the_same_thread_reports_it_only_once() {
        // Sanity check on the dedup: a Replace that does NOT cross a thread
        // reassignment must report that one thread exactly once, not twice.
        let bus = crate::bus::BusRegistry::new();
        let mut notices: HashMap<i32, (i32, u64, String)> = HashMap::new();
        let pr = base_row(); // thread_id stays 1 throughout

        apply_notice_action(&bus, &mut notices, &pr, Some((NoticeKind::SelfClearing, "first".to_string())));
        let touched = apply_notice_action(
            &bus,
            &mut notices,
            &pr,
            Some((NoticeKind::SelfClearing, "second".to_string())),
        );
        assert_eq!(touched, vec![1]);
        assert_eq!(bus.open_asks(1).len(), 1, "old notice retracted, new one posted — still exactly one open");
    }

    // --- the NoticeKind → bus method wiring itself -------------------------

    #[test]
    fn post_notice_picks_the_bus_method_matching_its_kind() {
        // Direct unit coverage of `post_notice` (and therefore the choice
        // `apply_notice_action`'s Post/Replace arms make): SelfClearing must
        // land as `AskKind::Notice`, ActionRequired as `AskKind::
        // NoticeActionRequired` — never crossed.
        let bus = crate::bus::BusRegistry::new();
        post_notice(&bus, NoticeKind::SelfClearing, 1, "10", "self-clearing text");
        post_notice(&bus, NoticeKind::ActionRequired, 1, "10", "action-required text");
        let open = bus.open_asks(1);
        assert_eq!(open.len(), 2);
        let self_clearing = open.iter().find(|a| a.text == "self-clearing text").unwrap();
        let action_required = open.iter().find(|a| a.text == "action-required text").unwrap();
        assert_eq!(self_clearing.kind, crate::bus::AskKind::Notice);
        assert_eq!(action_required.kind, crate::bus::AskKind::NoticeActionRequired);
    }

    #[test]
    fn apply_notice_action_posts_an_action_required_notice_with_the_matching_ask_kind() {
        // End-to-end through the real decision path (not just `post_notice`
        // directly): a give-up-shaped `desired` must actually reach the bus
        // as `AskKind::NoticeActionRequired`, so the frontend discriminator
        // this whole PR adds is fed correctly from the one sweep that needs it.
        let bus = crate::bus::BusRegistry::new();
        let mut notices: HashMap<i32, (i32, u64, String)> = HashMap::new();
        let pr = base_row();
        apply_notice_action(
            &bus,
            &mut notices,
            &pr,
            Some((NoticeKind::ActionRequired, "gave up".to_string())),
        );
        let open = bus.open_asks(pr.thread_id);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, crate::bus::AskKind::NoticeActionRequired);
    }
}
