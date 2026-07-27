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
        let mut notices: HashMap<i32, (u64, String)> = HashMap::new();
        run_pr_sweep(&app, &mut notices).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(sweep_secs)).await;
            run_pr_sweep(&app, &mut notices).await;
        }
    });
}

/// One sweep pass over every still-`open` tracked row.
async fn run_pr_sweep(app: &AppHandle, notices: &mut HashMap<i32, (u64, String)>) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    let Some(bus) = app.try_state::<crate::bus::BusRegistry>() else {
        return;
    };
    let bus = bus.inner().clone();

    let open = match repo::list_open_pull_requests(&db).await {
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
    notices: &mut HashMap<i32, (u64, String)>,
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
    notices: &mut HashMap<i32, (u64, String)>,
    pr: &pull_request::Model,
    kind: Option<HostKind>,
    result: Result<PrSnapshot, super::HostError>,
) {
    let desired_text = match &result {
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
            }
        }
        Err(e) => {
            if let Err(store_err) = repo::mark_pull_request_probe_error(db, pr.id, &e.message()).await {
                eprintln!("[weft][host] pr #{}: could not save probe error: {store_err}", pr.id);
            }
            Some(match kind {
                Some(k) => judge::probe_error_text(k, pr.number, e),
                None => format!("🔌 无法查询 PR/MR #{} 的状态:{}。", pr.number, e.message()),
            })
        }
    };

    reconcile_notice(bus, app, notices, pr, desired_text);
}

/// Apply whatever [`judge::plan_notice_action`] says about the Needs-you
/// notice for this PR/MR: post, replace, retract, or leave alone. `from` is
/// the direction id (as a string) that owns this PR — same convention every
/// other per-task notice in this codebase already uses (see
/// `lead_chat::engine`'s stall/freeze notices), so it lands attributed to the
/// right task in the Needs-you list with zero frontend changes.
fn reconcile_notice(
    bus: &crate::bus::BusRegistry,
    app: &AppHandle,
    notices: &mut HashMap<i32, (u64, String)>,
    pr: &pull_request::Model,
    desired_text: Option<String>,
) {
    let existing = notices.get(&pr.id).map(|(_, text)| text.as_str());
    let action = judge::plan_notice_action(existing, desired_text.as_deref());
    match action {
        judge::NoticeAction::NoOp => {}
        judge::NoticeAction::Post => {
            if let Some(text) = desired_text {
                let id = bus.notify_human(pr.thread_id, &pr.direction_id.to_string(), &text);
                notices.insert(pr.id, (id, text));
                let _ = app.emit("needs-you://changed", pr.thread_id);
            }
        }
        judge::NoticeAction::Replace => {
            if let Some((old_id, _)) = notices.remove(&pr.id) {
                bus.cancel_open_asks_by_id(pr.thread_id, old_id);
            }
            if let Some(text) = desired_text {
                let id = bus.notify_human(pr.thread_id, &pr.direction_id.to_string(), &text);
                notices.insert(pr.id, (id, text));
            }
            let _ = app.emit("needs-you://changed", pr.thread_id);
        }
        judge::NoticeAction::Retract => {
            if let Some((old_id, _)) = notices.remove(&pr.id) {
                if bus.cancel_open_asks_by_id(pr.thread_id, old_id) {
                    let _ = app.emit("needs-you://changed", pr.thread_id);
                }
            }
        }
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
    use crate::host::{CiStatus, ConflictStatus, PrLifecycle, ReviewStatus};

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
}
