//! Auto-merge executor (issue #110 T3): the ONLY mutating write path for a
//! tracked PR/MR anywhere in this crate. Deliberately separate from
//! `host::monitor` (read-only sweep) and `host::github`'s `PrHost` impl
//! (read-only `fetch_status`) — see `host`'s own module doc for why that
//! separation is load-bearing, not stylistic: `host::monitor` never calls
//! anything in this file, and nothing in this file is reachable except from
//! this file's own spawned loop. Opt-in, default OFF (`K_AUTO_MERGE_ENABLED`)
//! — merging a PR is an irreversible action with no human confirming the
//! specific merge, so the default posture is "tell me it's ready", not
//! "merge it for me" (see `auto_merge_enabled`'s doc).
//!
//! Flow, once per candidate row per sweep tick (`maybe_merge_one`):
//!   1. Cheap PRE-FILTER (`gate::decide_auto_merge`) against the STORED,
//!      possibly-stale DB row. Only decides whether the row is worth a live
//!      re-check at all.
//!   2. A defensive existence check: the row's owning `thread` must still be
//!      there (an issue/repo/workspace delete cascades `pull_request` rows —
//!      see `repo::delete_thread_cascade` et al — but this catches any row
//!      that predates that cascade in an already-affected install, or a
//!      narrow delete-vs-sweep race, without needing a data migration).
//!   3. A per-row, per-commit BACKOFF: after `MAX_MERGE_ATTEMPTS_PER_HEAD`
//!      consecutive FAILED merge attempts against the same `head_sha`, stop
//!      retrying until a new commit lands.
//!   4. A FRESH `PrHost::fetch_status` read + `judge::merge_readiness`
//!      recompute — persisted immediately via the EXISTING `repo::apply_
//!      pull_request_snapshot`.
//!   5. The FINAL authorization: `gate::decide_auto_merge` AGAIN, fed ONLY
//!      the fresh data from step 4 (`age`/`probe_fail_count` forced to `0`;
//!      `enabled` itself re-read fresh too, never reused from step 1 or from
//!      an earlier row in the same sweep pass). Only a `Merge` verdict here
//!      proceeds — step 1's verdict never directly authorizes anything.
//!   6. [`run_gh_merge`] — `gh pr merge --squash --match-head-commit <sha>`,
//!      off the async runtime (`spawn_blocking`). The ONE new `Command::new`
//!      in this entire feature, using the head_sha from step 4's fresh read.
//!      `--match-head-commit` makes GitHub itself refuse the merge if the
//!      head has moved AGAIN since step 4, closing the last sliver of gap
//!      between "we just confirmed this" and "the API call executes".
//!   7. Regardless of outcome, ONE more fresh `PrHost::fetch_status` read is
//!      taken and persisted, and a durable, structured, i18n-rendered
//!      timeline marker is inserted either way (see
//!      [`insert_automerge_marker`]'s doc).
//!
//! FOLLOW-UP (issue #110 T3 seam + retry hardening): steps 1-5 above live in
//! [`evaluate_row`], which returns a plain [`RowVerdict`] instead of
//! performing the mutating call itself — `maybe_merge_one` only proceeds to
//! steps 6-7 on a `RowVerdict::Merge`. That split exists so the exact
//! property steps 1-5 encode (the FINAL authorization must use FRESH data,
//! never step 1's stale verdict) is directly testable: a test asserts on
//! `evaluate_row`'s return value, so it is structurally impossible for any
//! test of it — including this follow-up's own mutation self-check, see
//! `tests::` below — to reach [`run_gh_merge`], this file's only
//! `Command::new("gh")` site (reaching it for real would mean actually
//! shelling out to the operator's own authenticated `gh`). Steps 4 and 7's
//! fresh reads go through an injected [`HostResolver`] — a plain,
//! non-capturing `fn` pointer, defaulting to `super::resolve_host` at the
//! real call site (`run_automerge_sweep`) — instead of calling that free
//! function directly, closing the gap an independent review found: neither
//! call site had any way to substitute a fake `PrHost`, so the "fresh, not
//! stale" property above had ZERO regression coverage (reverting step 5 to
//! reuse step 1's `pre_decision` left `cargo test --lib host::` 101/101
//! green). Step 3's backoff moved from a plain local `HashMap` into
//! [`MergeBackoffState`], Tauri-managed state — not for testability, but so
//! `commands::retry_pr_tracking_core` (the Needs-you "Retry" button's
//! backend) can reach in and clear a row's exhausted entry; see that
//! struct's own doc for why the backoff itself stays in-memory-only.
//!
//! Review round 1 on this PR found the original version skipped steps 2/3/5
//! entirely and only ever re-read fresh state AFTER attempting the merge
//! (step 7 existed, steps 4-5 did not) — meaning the mutating call in step 6
//! could fire off a DB snapshot up to `MAX_READY_AGE_SECS` stale, including
//! at process boot before `host::monitor`'s own first sweep had a chance to
//! refresh anything. Steps 4-5 close that: the merge is now NEVER attempted
//! without a fresh, live confirmation taken immediately beforehand.
//!
//! Double-merge safety across a crash: if the process dies between step 6
//! succeeding and step 7's write landing, the STORED row still reads
//! open+Ready on restart — but step 4's fresh read on the RETRY observes the
//! PR as already `Merged` on GitHub, and step 5's gate then refuses with
//! `NotOpen` BEFORE step 6 is ever reached again. This holds regardless of
//! what `gh pr merge` itself does when asked to merge an already-merged PR
//! (review round 1 Codex P2 found a prior version of this doc incorrectly
//! assumed `gh` errors in that case — reading `cli/cli`'s own merge command
//! shows it can exit 0 as a silent no-op instead — but that behavior is
//! irrelevant here specifically BECAUSE step 4-5's own fresh check already
//! prevents the retry from ever calling `run_gh_merge` a second time; this
//! file does not rely on `gh`'s own idempotency for that guarantee).

use std::collections::HashMap;
use std::process::Command;

use tauri::{AppHandle, Emitter, Manager};

use super::gate::{self, AutoMergeDecision};
use super::judge;
use super::{HostError, HostKind, PrLifecycle, PrTarget};
use crate::store::entities::pull_request;
use crate::store::{repo, Db};

/// The `app_setting` key for the opt-in switch. Unset (fresh install, or an
/// upgraded DB that predates this feature) reads as OFF — see
/// `auto_merge_enabled`'s doc for the fail-closed treatment of a DB read
/// error too.
pub const K_AUTO_MERGE_ENABLED: &str = "pr_auto_merge_enabled";

/// Default sweep cadence for THIS feature's own loop — deliberately NOT
/// shared with `host::monitor::PR_SWEEP_DEFAULT_SECS`, even though both
/// currently default to the same number, because the two loops must stay
/// independently schedulable (see this module's doc on why they are
/// separate loops at all). Override with `WEFT_PR_AUTOMERGE_SWEEP_SECS`;
/// `0` disables the loop entirely.
const PR_AUTOMERGE_SWEEP_DEFAULT_SECS: u64 = 60;

/// How stale a row's last SUCCESSFUL probe may be before the PRE-FILTER
/// refuses to even attempt a fresh check on its stored `Ready` verdict, even
/// with zero recorded probe failures (`probe_fail_count == 0`) — the OTHER
/// way a row's `Ready` column can outlive its truth: not a failing probe
/// (see `gate::AutoMergeSkipReason::ProbeFailing`), but a STALLED one (the
/// sweep loop itself wedged, or the whole process was suspended for hours
/// and just resumed). Ten sweep intervals at `host::monitor`'s own default
/// cadence — generous enough to ride out ordinary scheduling jitter, far
/// short of "hours-old". The FINAL authorization (step 5 in this module's
/// doc) never uses this — it always passes `age_secs: 0` against data that
/// was just read live.
const MAX_READY_AGE_SECS: i64 = 600;

/// After this many CONSECUTIVE FAILED `gh pr merge` attempts against the
/// SAME `head_sha`, stop retrying that exact request until a new commit
/// lands. Without this, a persistently-failing merge (missing merge
/// permission, a repo that rejects squash merges, ...) would retry — and
/// post a fresh failure marker — every sweep tick forever (review round 1
/// P2). Small and in-memory (see [`MergeBackoffState`]): a process restart or
/// a fresh push both legitimately deserve a clean slate — and so, now, does
/// a human clicking the Needs-you "Retry" button (`commands::
/// retry_pr_tracking_core`).
const MAX_MERGE_ATTEMPTS_PER_HEAD: u32 = 3;

/// Cloneable handle to the per-(row, head_sha) merge-FAILURE backoff table
/// (step 3, this module's doc) — Tauri-managed state (`.manage(...)` in
/// `lib.rs`, fetched via `AppHandle::try_state`), not a plain local `HashMap`
/// owned by `spawn_pr_automerge_watch`'s loop the way an earlier version of
/// this file had it. The ONLY reason it moved: `commands::
/// retry_pr_tracking_core` (the Needs-you "Retry" button's backend) needs a
/// door to reach in and forget a row's exhausted streak, and a plain
/// task-local `HashMap` has none. Mirrors `bus::BusRegistry`'s exact shape
/// (`Arc<Mutex<_>>` behind a `#[derive(Clone)]` newtype) for the same reason:
/// cheap to clone, safe to hand a copy into every sweep call, no lifetime
/// coupling to any one `AppHandle` borrow.
///
/// STAYS in-memory-only (never persisted to the `pull_request` table, unlike
/// `probe_fail_count`) — a process restart or a fresh push both legitimately
/// deserve a clean slate (this module's doc, step 3), and [`clear`]'s Retry
/// reach-in deliberately does not change that. [`clear`] also deliberately
/// has no rate limit of its OWN beyond the ordinary per-attempt gate: a human
/// clicking Retry only ever grants `MAX_MERGE_ATTEMPTS_PER_HEAD` MORE
/// attempts, and every one of those still has to clear the FULL gate (fresh
/// CI-green/review-approved/conflict-free — `gate::decide_auto_merge`, step 5)
/// before `run_gh_merge` is attempted again — Retry cannot make this feature
/// merge anything it would otherwise refuse; it can only let a target that
/// already looks ready keep trying. Same precedent `register_pull_request`
/// already set for `probe_fail_count`'s own unlimited reset.
///
/// [`clear`]: MergeBackoffState::clear
#[derive(Default, Clone)]
pub struct MergeBackoffState {
    inner: std::sync::Arc<std::sync::Mutex<HashMap<i32, (String, u32)>>>,
}

impl MergeBackoffState {
    /// Step 3's check: has this row exhausted its attempts against EXACTLY
    /// this `head_sha`? A different (e.g. newer) `head_sha` always reads as
    /// fresh, even with a nonzero streak recorded against some OTHER sha.
    /// `pub(crate)`, not private: `commands::tests` (a different module)
    /// asserts on it directly rather than through the indirect, mutating
    /// `clear`'s return value — see `retry_pr_tracking_also_clears_an_
    /// exhausted_merge_attempt_backoff`.
    pub(crate) fn is_exhausted(&self, pr_id: i32, head_sha: &str) -> bool {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&pr_id)
            .is_some_and(|(last_sha, count)| last_sha == head_sha && *count >= MAX_MERGE_ATTEMPTS_PER_HEAD)
    }

    /// Step 6 success bookkeeping: a merge finally landed, forget any prior
    /// failure streak for this row entirely.
    fn record_success(&self, pr_id: i32) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(&pr_id);
    }

    /// Step 6 failure bookkeeping: bump (or start) the streak for this exact
    /// `head_sha`, resetting it first if the head moved since the last
    /// recorded failure. Returns whether THIS attempt just crossed the
    /// exhaustion threshold (for the marker's `attempts_exhausted` field).
    /// `pub(crate)` for the same cross-module test reason as `is_exhausted`
    /// (`commands::tests` seeds an exhausted entry directly, without needing
    /// to route through a real, failing `run_gh_merge` call).
    pub(crate) fn record_failure(&self, pr_id: i32, head_sha: &str) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(pr_id).or_insert_with(|| (head_sha.to_string(), 0));
        if entry.0 != head_sha {
            *entry = (head_sha.to_string(), 0);
        }
        entry.1 += 1;
        entry.1 >= MAX_MERGE_ATTEMPTS_PER_HEAD
    }

    /// The Retry button's reach-in (`commands::retry_pr_tracking_core`):
    /// forget this row's backoff entirely, regardless of its current streak
    /// or which `head_sha` it was recorded against. Returns whether an entry
    /// actually existed, so a caller can distinguish "there was something
    /// stuck here" from "this row had no merge-attempt history at all".
    pub fn clear(&self, pr_id: i32) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(&pr_id).is_some()
    }
}

/// Start the runtime PR/MR auto-merge sweep. Call once at app setup,
/// alongside `host::monitor::spawn_pr_watch` (see `lib.rs`) — NOT instead of
/// it, and not chained off it; see this module's doc for why the two stay
/// separate loops.
pub fn spawn_pr_automerge_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let sweep_secs = crate::commands::env_secs(
            "WEFT_PR_AUTOMERGE_SWEEP_SECS",
            PR_AUTOMERGE_SWEEP_DEFAULT_SECS,
        );
        if sweep_secs == 0 {
            return; // disabled
        }
        run_automerge_sweep(&app).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(sweep_secs)).await;
            run_automerge_sweep(&app).await;
        }
    });
}

/// One sweep pass. Short-circuits before listing any rows when the feature
/// is off (the default for almost every install) rather than doing real
/// per-row work just to gate on `enabled` — same shape as `host::monitor::
/// run_pr_sweep`'s own `try_state` guards. This EARLY check is an efficiency
/// short-circuit only: `evaluate_row` re-reads `enabled` fresh, per row,
/// immediately before its own final authorization (step 5 in this module's
/// doc) — so a mid-pass toggle-off still takes effect within the same pass,
/// it just does not save the listing query itself.
async fn run_automerge_sweep(app: &AppHandle) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);

    if !auto_merge_enabled(&db).await {
        return;
    }

    let Some(backoff) = app.try_state::<MergeBackoffState>() else {
        return;
    };
    let backoff = backoff.inner().clone();

    // No probe-failure ceiling here (`i32::MAX`, effectively "don't exclude
    // anything at the query level") — unlike `host::monitor`'s own sweep,
    // which stops RETRYING a persistently-failing row past its give-up
    // threshold. This feature must still SEE every open row so its own gate
    // (`gate::AutoMergeSkipReason::ProbeFailing`) is what skips a failing
    // one, visibly and testably, rather than an invisible SQL-level
    // exclusion silently doing the same job.
    let open = match repo::list_open_pull_requests(&db, i32::MAX).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weft][automerge] sweep: could not list tracked PR/MRs: {e}");
            return;
        }
    };
    for pr in open {
        maybe_merge_one(app, &db, pr, &backoff, super::resolve_host).await;
    }
}

/// The `resolve_host`-shaped seam steps 4 and 7 (this module's doc) call
/// through, instead of hard-coding `super::resolve_host` at each site. A
/// plain, non-capturing `fn` pointer — `resolve_host` itself already has
/// exactly this signature (zero captures), so passing it needs no wrapping;
/// a test double does too (see `tests::` below), and being `Copy + Send +
/// 'static`, either moves into `spawn_blocking`'s closure exactly as freely
/// as the hard-coded call it replaces. `host::monitor` (and `host::judge`)
/// deliberately do NOT gain this seam: only `automerge.rs`'s own two
/// fresh-read call sites needed to become testable, and this crate's
/// read-only boundary (`host/mod.rs`'s module doc) means `monitor.rs` stays
/// untouched, byte-for-byte, by this file's own follow-up work.
type HostResolver = fn(HostKind) -> Result<Box<dyn super::PrHost>, HostError>;

/// [`evaluate_row`]'s return: whether `maybe_merge_one` should actually
/// attempt [`run_gh_merge`], and if so, against exactly which `head_sha` —
/// always the one step 4 just fetched, NEVER the row's original,
/// possibly-stale one.
#[derive(Debug, PartialEq, Eq)]
enum RowVerdict {
    Skip,
    Merge { head_sha: String },
}

/// Steps 1 through 5 of this module's flow (see module doc): the ENTIRE
/// decision of whether this row is safe to merge RIGHT NOW, stopping at (and
/// never past) the FINAL authorization. Split out from the mutating tail
/// (steps 6-7, still in `maybe_merge_one`) specifically so this — the exact
/// property review round 1 added (`gate::decide_auto_merge` called a SECOND
/// time, fed only data from a fresh read, step 1's verdict never reused) —
/// is directly testable in isolation: a test asserts on the returned
/// [`RowVerdict`], never on whether a real merge happened, so it is
/// structurally impossible for any test of this function — including this
/// follow-up's own mutation self-check, `tests::` below — to reach
/// [`run_gh_merge`].
async fn evaluate_row(
    db: &Db,
    pr: &pull_request::Model,
    host_kind: HostKind,
    backoff: &MergeBackoffState,
    resolver: HostResolver,
) -> RowVerdict {
    // Step 1: cheap DB-level pre-filter using STORED (possibly stale) state.
    let now = repo::now_unix();
    let stored_lifecycle = gate::parse_lifecycle(&pr.lifecycle);
    let stored_ci = gate::parse_ci(&pr.ci_status);
    let stored_threads = gate::parse_threads(&pr.thread_status);
    let stored_readiness = gate::parse_readiness(&pr.merge_readiness);
    let age = gate::age_secs(&pr.last_checked_at, &now);
    let pre_decision = gate::decide_auto_merge(
        true, // `run_automerge_sweep` already confirmed `enabled` before listing
        // rows; passed through explicitly (not hard-coded inside the gate)
        // so `gate::decide_auto_merge` stays a complete, independently
        // testable decision — see that function's own doc. The FINAL
        // authorization below re-reads this fresh regardless.
        host_kind,
        stored_lifecycle,
        &stored_ci,
        &stored_threads,
        &stored_readiness,
        pr.probe_fail_count,
        age,
        MAX_READY_AGE_SECS,
    );
    if pre_decision != AutoMergeDecision::Merge {
        return RowVerdict::Skip;
    }

    // Step 2: defensive existence check — see this module's doc. A deleted
    // issue/repo/workspace cascades `pull_request` rows away (`repo::
    // delete_thread_cascade` et al), so this should be unreachable in normal
    // operation; kept as free insurance against a row that predates that
    // fix, or a narrow delete-vs-sweep timing race.
    match repo::get_thread(db, pr.thread_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return RowVerdict::Skip, // orphaned — nowhere to post a marker
        Err(e) => {
            eprintln!(
                "[weft][automerge] pr #{}: could not confirm the owning thread still exists: {e}",
                pr.id
            );
            return RowVerdict::Skip;
        }
    }

    // Step 3: per-(row, head_sha) backoff after repeated failed attempts.
    if backoff.is_exhausted(pr.id, &pr.head_sha) {
        return RowVerdict::Skip;
    }

    // Step 4: fresh, live read via the INJECTED resolver — the ONLY thing
    // this feature ever trusts to actually authorize a merge attempt (step
    // 5). Mirrors `host::monitor`'s own `check_one` exactly (same
    // `spawn_blocking` discipline).
    let target = PrTarget {
        host_base: pr.host_base.clone(),
        owner: pr.host_owner.clone(),
        repo: pr.host_repo.clone(),
        number: pr.number,
    };
    let fresh = tokio::task::spawn_blocking(move || {
        resolver(host_kind).and_then(|h| h.fetch_status(&target))
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(HostError::Other {
            message: format!("internal: automerge pre-merge fresh read join error: {join_err}"),
        })
    });

    let snapshot = match fresh {
        Ok(s) => s,
        Err(e) => {
            let _ = repo::mark_pull_request_probe_error(db, pr.id, &e.message()).await;
            return RowVerdict::Skip; // couldn't confirm live state — never merge on a guess
        }
    };
    // Re-resolved here rather than trusting the swept row: this is the
    // pre-merge confirmation, and an upstream can have moved since the sweep.
    let upstream = repo::upstream_merge_state(db, pr.direction_id).await;
    let fresh_readiness = judge::merge_readiness(
        &snapshot.ci,
        &snapshot.review,
        &snapshot.threads,
        &snapshot.conflict,
        &upstream,
    );
    if let Err(e) = repo::apply_pull_request_snapshot(db, pr.id, &snapshot, &fresh_readiness).await {
        eprintln!(
            "[weft][automerge] pr #{}: could not save pre-merge confirmation snapshot: {e}",
            pr.id
        );
    }

    // Step 5: FINAL authorization against the FRESH state only. `enabled` is
    // re-read here too (never reused from `run_automerge_sweep`'s own
    // earlier, pass-level check) — closes the "user disables the switch
    // mid-pass, later rows in the same pass still merge" gap (review round 1
    // P1 / Codex P1).
    let enabled_now = auto_merge_enabled(db).await;
    let final_decision = gate::decide_auto_merge(
        enabled_now,
        host_kind,
        Some(snapshot.lifecycle),
        &snapshot.ci,
        &snapshot.threads,
        &fresh_readiness,
        0,
        0,
        MAX_READY_AGE_SECS,
    );
    if final_decision != AutoMergeDecision::Merge {
        return RowVerdict::Skip; // downgraded since the pre-filter — silent, like any other skip
    }

    // Test-only seam: let a test pause HERE — between the upstream read this authorization
    // was based on (above) and the re-check immediately below — to drive a genuinely
    // concurrent write from a separate task. See `tests::between_upstream_authorization_probe`.
    #[cfg(test)]
    tests::between_upstream_authorization_probe(pr.direction_id).await;

    // Step 5.5: re-check the upstream axis ONE more time, as close to the actual merge call as
    // this function can get — Codex review, PR #159 automerge.rs:395. Unlike CI/review/
    // conflict, GitHub has no concept of this product's own cross-repo dependency ordering to
    // enforce server-side: `--match-head-commit` (step 6, in `maybe_merge_one`) only guards the
    // PR's OWN head commit moving, never this LOCAL fact. Without this, a re-proposal that
    // adds or replaces this consumer's dependency in the window between the read above and
    // `run_gh_merge` actually executing would merge a consumer whose upstream just changed,
    // with no backstop at all — CI/review/conflict staleness in that same window is at least
    // partly covered by GitHub's OWN branch-protection enforcement, but this axis is purely a
    // Weft-side invariant that only Weft can protect. This is INTENTIONALLY separate from (and
    // does not touch) `record_upstream_edges`'s two-pass write-ordering mechanism (round 5+6):
    // that solves DB write atomicity for a multi-edge write; this is a read-authorize-execute
    // TOCTOU window on this file's OWN side, and re-reading the SAME already-atomic source of
    // truth closer to the action is sufficient — no new lock or generation counter needed, and
    // none of that machinery would help here anyway (it does not extend the window this file's
    // own control flow takes to reach `run_gh_merge`).
    //
    // A sixth review round (Codex review, PR #159 automerge.rs:457) pressed on the word
    // "narrows" above: this re-check does NOT close the window, it only moves it as late as
    // this file's own control flow allows. The REMAINING window runs from the instant this
    // read returns to the instant GitHub actually processes the `gh pr merge` call dispatched
    // in `maybe_merge_one`'s step 6 — dominated by process-spawn + network round-trip time to
    // GitHub's API (realistically ~100ms to a few seconds, NOT a negligible number of CPU
    // cycles; the handful of synchronous Rust statements between this check and the
    // `spawn_blocking` call in step 6 contribute microseconds, not the risk).
    //
    // Full closure was considered and rejected as disproportionate to the residual risk:
    //   - A lock held from this re-check through `run_gh_merge`'s completion would need a NEW
    //     coordination primitive between this module and every writer of
    //     `depends_on_direction_id` (`planner::set_upstream_edge_if_changed`) — a module that
    //     today has zero automerge awareness — plus its own stuck-lock recovery story for a
    //     crashed/hung `gh` process, to gate a race this narrow.
    //   - A generation counter checked just before dispatch does not actually help: unlike
    //     `--match-head-commit` (enforced ATOMICALLY by GitHub itself, server-side, as part of
    //     executing the merge), there is no GitHub-side hook for this Weft-only axis, so
    //     "check-then-dispatch" has the identical TOCTOU shape no matter how late the check
    //     runs, unless it is ITSELF paired with the same lock above — it does not remove the
    //     cost, it only relocates it.
    //   - A synthetic GitHub check run plus a required branch-protection rule could in
    //     principle give this axis the same atomic, server-side enforcement
    //     `--match-head-commit` gets for head-sha staleness — but that depends on external repo
    //     configuration this codebase does not manage today, and would be a new integration
    //     surface (Checks API, a setup burden on every target repo) rather than a
    //     review-response-sized fix.
    //
    // The residual window is accepted, not ignored: reaching it requires a human or lead to
    // CONFIRM a re-proposal that changes THIS SPECIFIC direction's upstream within a
    // sub-few-second window that carries no visible "a merge is dispatching right now" cue —
    // and even if hit, `maybe_merge_one`'s own step 7 re-reads and re-persists this exact row's
    // true state, INCLUDING this axis, immediately after the merge attempt completes, so the
    // consequence is bounded to one merge whose upstream had just changed (a recoverable,
    // visible-on-CI outcome, the same class of risk as any other check-then-merge gap) and
    // self-corrects in the DB rather than silently drifting.
    match repo::upstream_merge_state(db, pr.direction_id).await {
        super::UpstreamStatus::None | super::UpstreamStatus::Merged => {}
        other => {
            eprintln!(
                "[weft][automerge] pr #{}: upstream changed to {other:?} after authorization — \
                 aborting this merge attempt",
                pr.id
            );
            return RowVerdict::Skip;
        }
    }

    RowVerdict::Merge { head_sha: snapshot.head_sha }
}

/// Gate one row twice (pre-filter, then final authorization against fresh
/// state — both in [`evaluate_row`]), and if it clears both, execute +
/// confirm + record the merge attempt. No-op (silently) for every `Skip`
/// verdict — a blocked or indeterminate row already has `host::monitor`'s own
/// Needs-you notice telling the human why, when that is warranted; this
/// feature only speaks up when it actually ACTS (see
/// `insert_automerge_marker`'s doc). See this module's own doc for the full
/// numbered flow, and [`evaluate_row`]'s doc for why steps 1-5 live there.
async fn maybe_merge_one(
    app: &AppHandle,
    db: &Db,
    pr: pull_request::Model,
    backoff: &MergeBackoffState,
    resolver: HostResolver,
) {
    let Some(host_kind) = HostKind::parse(&pr.host_kind) else {
        return; // unrecognized host_kind on the row — nothing sane to do
    };

    let head_sha = match evaluate_row(db, &pr, host_kind, backoff, resolver).await {
        RowVerdict::Skip => return,
        RowVerdict::Merge { head_sha } => head_sha,
    };

    // Step 6: the ONE mutating call, off the async runtime, using the FRESH
    // head_sha (never the stale row's) and the row's recorded host_base (GHE
    // support — review round 1 Codex P1: a prior version always targeted
    // `gh`'s own default host regardless of what was recorded at
    // registration).
    let host_base = pr.host_base.clone();
    let owner = pr.host_owner.clone();
    let repo_name = pr.host_repo.clone();
    let number = pr.number;
    let head_sha_for_merge = head_sha.clone();
    let merge_result = tokio::task::spawn_blocking(move || {
        run_gh_merge(&host_base, &owner, &repo_name, number, &head_sha_for_merge)
    })
    .await
    .unwrap_or_else(|join_err| Err(format!("internal: merge task join error: {join_err}")));

    // Track consecutive failures per (row, head_sha) for step 3's backoff.
    let attempts_exhausted = match &merge_result {
        Ok(()) => {
            backoff.record_success(pr.id);
            false
        }
        Err(_) => backoff.record_failure(pr.id, &head_sha),
    };

    // Step 7: regardless of outcome, one more fresh read (same injected
    // resolver as step 4) + persist + marker.
    let target2 = PrTarget {
        host_base: pr.host_base.clone(),
        owner: pr.host_owner.clone(),
        repo: pr.host_repo.clone(),
        number: pr.number,
    };
    let confirmed = tokio::task::spawn_blocking(move || {
        resolver(host_kind).and_then(|h| h.fetch_status(&target2))
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(HostError::Other {
            message: format!("internal: automerge confirmation join error: {join_err}"),
        })
    });

    let (state, state_error) = match &confirmed {
        Ok(s) => {
            let upstream = repo::upstream_merge_state(db, pr.direction_id).await;
            let r = judge::merge_readiness(&s.ci, &s.review, &s.threads, &s.conflict, &upstream);
            if let Err(e) = repo::apply_pull_request_snapshot(db, pr.id, s, &r).await {
                eprintln!(
                    "[weft][automerge] pr #{}: could not save confirmation snapshot: {e}",
                    pr.id
                );
            }
            (lifecycle_state_tag(s.lifecycle), None)
        }
        Err(err) => {
            if let Err(store_err) = repo::mark_pull_request_probe_error(db, pr.id, &err.message()).await
            {
                eprintln!(
                    "[weft][automerge] pr #{}: could not save confirmation probe error: {store_err}",
                    pr.id
                );
            }
            ("unknown", Some(err.message()))
        }
    };

    insert_automerge_marker(
        app,
        db,
        pr.thread_id,
        &MergeAttemptOutcome {
            merged: merge_result.is_ok(),
            abbrev: host_kind.native_abbrev(),
            number: pr.number,
            base_ref: pr.base_ref.clone(),
            reason: merge_result.err(),
            state,
            state_error,
            attempts_exhausted,
            attempts_max: MAX_MERGE_ATTEMPTS_PER_HEAD,
        },
    )
    .await;
}

/// Exhaustive, discriminated tag for a FRESH lifecycle read, for the
/// structured `"state"` field of the auto-merge timeline marker (see
/// [`insert_automerge_marker`]'s doc on why this stays structured facts, not
/// pre-composed prose). `Merged` is the EXPECTED state for a success outcome
/// (confirming the merge this file just made actually landed); it is also
/// reachable on a FAILURE outcome — e.g. someone else merges the same PR in
/// the narrow window between step 4's read and step 6's attempt — in which
/// case the marker still honestly shows "failed, but here's what's true
/// right now" rather than conflating the two.
fn lifecycle_state_tag(lifecycle: PrLifecycle) -> &'static str {
    match lifecycle {
        PrLifecycle::Open => "open",
        PrLifecycle::Merged => "merged",
        PrLifecycle::Closed => "closed",
    }
}

/// The ONE mutating call in this entire feature — `gh pr merge`. Nothing in
/// `host::monitor` / `host::github` / `host::judge` / `host::gate` ever calls
/// this, and this file only calls it (via `spawn_blocking` — review round 1
/// Codex P2: a synchronous `Command::output()` call directly on the async
/// runtime would occupy a Tokio worker for the whole `gh` round trip) from
/// `maybe_merge_one`, gated by TWO `gate::decide_auto_merge` calls (both in
/// `evaluate_row`), the second against data read immediately before this
/// call.
///
/// `--match-head-commit` makes GitHub itself refuse the merge if `head_sha`
/// has moved since the judgement this attempt is based on — server-side
/// enforcement that cannot be raced between "we decided" and "the API call
/// executes", stronger than any client-side re-check this process could do
/// on its own. `--squash` matches this repo's own established merge
/// convention (CLAUDE.md: "user habit squash merge"; also the only strategy
/// this repo's own PR workflow uses).
///
/// `host_base` (the hostname recorded at registration — see `host/mod.rs`'s
/// module doc on why GitHub Enterprise needs this recorded per row) is
/// threaded into `--repo` as `HOST/OWNER/REPO` when non-empty, via the SAME
/// `super::qualified_repo_slug` `github::GitHubHost::fetch_status` uses, so this call and every
/// status read of the same row can never independently drift on which host they each target
/// (review round 1 Codex P1; Codex review, PR #159 repo.rs:3873 — the status-fetch path used
/// to skip this entirely).
fn run_gh_merge(host_base: &str, owner: &str, repo: &str, number: i32, head_sha: &str) -> Result<(), String> {
    let repo_arg = super::qualified_repo_slug(host_base, owner, repo)?;
    // A `Ready` verdict can only ever be produced from a SUCCESSFUL snapshot
    // (which always sets a real `head_sha`), so this should be unreachable
    // through the gate above — kept anyway as cheap, independent insurance
    // against ever asking `gh` to match an empty/absent commit.
    if head_sha.is_empty() {
        return Err("refusing to merge: no confirmed head_sha on record".to_string());
    }
    let out = Command::new("gh")
        .args(build_merge_args(&repo_arg, number, head_sha))
        // Checks run user tooling that a GUI launch's minimal PATH can't
        // resolve (Homebrew/local installs of `gh`) — same reasoning as
        // `github::GitHubHost::fetch_status` / `check::run_check`.
        .env("PATH", crate::detect::tool_path())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("gh is not installed".to_string())
        }
        Err(e) => return Err(e.to_string()),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(stderr.trim().to_string());
    }
    Ok(())
}

/// The exact `gh pr merge` argument vector, pulled out as its own pure
/// function so the presence of `--match-head-commit` (this feature's
/// server-side head-consistency enforcement — the mechanism `gate`'s own doc
/// points to instead of a second client-side sha comparison) is
/// independently, directly unit-tested below without ever spawning a
/// process. `run_gh_merge` is the only caller; nothing here talks to `gh`.
/// `repo_arg` arrives ALREADY host-qualified (`super::qualified_repo_slug`,
/// called by `run_gh_merge` — see that function's doc for why the folding
/// itself lives there, shared with `github::GitHubHost::fetch_status`,
/// rather than duplicated in this formatter too).
fn build_merge_args(repo_arg: &str, number: i32, head_sha: &str) -> Vec<String> {
    vec![
        "pr".to_string(),
        "merge".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repo_arg.to_string(),
        "--squash".to_string(),
        "--match-head-commit".to_string(),
        head_sha.to_string(),
    ]
}

/// Every fact needed to render one auto-merge attempt's timeline marker —
/// see [`insert_automerge_marker`]'s doc for why this stays PLAIN FACTS
/// rather than a pre-composed sentence.
struct MergeAttemptOutcome {
    merged: bool,
    abbrev: &'static str,
    number: i32,
    base_ref: String,
    /// Raw `gh` failure text (or an internal error) — only `Some` when
    /// `merged` is `false`. Never localized: it is the host's/OS's own
    /// diagnostic passthrough, the same treatment `HostError::message`'s
    /// callers already give probe-failure text elsewhere in this module tree
    /// (e.g. `judge::probe_error_text`).
    reason: Option<String>,
    /// `"open" | "merged" | "closed" | "unknown"` — see `lifecycle_state_tag`.
    state: &'static str,
    /// Raw diagnostic for `state == "unknown"` (the post-attempt confirmation
    /// read itself failed) — same never-localized treatment as `reason`.
    state_error: Option<String>,
    attempts_exhausted: bool,
    attempts_max: u32,
}

/// A durable, visible timeline record of an auto-merge attempt — the same
/// "system-owned, always part of the record" treatment `lead_chat::commands`
/// already gives an engine switch / a failed quota fail-over. `kind:
/// "pr_auto_merge"`, with STRUCTURED content (never a pre-composed sentence)
/// — review round 1 Codex P1: an earlier version composed the marker text as
/// fixed Chinese in this file, which left English-UI users an untranslated
/// message; every user-facing string here now lives in `src/i18n/en.ts` +
/// `zh.ts` (`ChatTimeline.tsx`'s `AutoMergeMarker` composes the sentence via
/// `t()` from these plain facts) like the rest of this codebase's UI copy.
async fn insert_automerge_marker(app: &AppHandle, db: &Db, thread_id: i32, outcome: &MergeAttemptOutcome) {
    let turn_id = repo::next_turn_id(db, thread_id).await.unwrap_or(1);
    let content = serde_json::json!({
        "merged": outcome.merged,
        "abbrev": outcome.abbrev,
        "number": outcome.number,
        "base_ref": outcome.base_ref,
        "reason": outcome.reason,
        "state": outcome.state,
        "state_error": outcome.state_error,
        "attempts_exhausted": outcome.attempts_exhausted,
        "attempts_max": outcome.attempts_max,
    })
    .to_string();
    match repo::insert_lead_message(
        db,
        thread_id,
        None,
        turn_id,
        "system",
        "pr_auto_merge",
        &content,
        "complete",
    )
    .await
    {
        Ok(m) => {
            let _ = app.emit(
                crate::lead_chat::engine::EVENT,
                crate::lead_chat::engine::Push::Message { thread_id, message: m },
            );
        }
        Err(e) => eprintln!(
            "[weft][automerge] marker insert failed (thread_id={thread_id}): {e}"
        ),
    }
}

/// Whether the opt-in auto-merge switch is on. Mirrors `engine_routing::
/// quota_failover_for_db`'s exact fail-closed shape (issue #97/#143's
/// established pattern for an opt-in automation switch): a DB read error
/// (corrupt settings row, a locked/unreachable DB) is NOT treated as "off,
/// proceed with default behavior" implicitly — it is logged and returns
/// `false` explicitly, because "off" and "we could not tell" must never
/// collapse into the same code path for an IRREVERSIBLE action. See
/// `try_auto_merge_enabled` for the underlying `Result`-returning read.
pub async fn auto_merge_enabled(db: &Db) -> bool {
    match try_auto_merge_enabled(db).await {
        Ok(v) => v,
        Err(err) => {
            eprintln!("[weft][automerge] policy read failed; auto-merge disabled: {err}");
            false
        }
    }
}

/// The underlying settings read, for a caller that needs to distinguish "off"
/// from "could not tell" (`get_pr_auto_merge_enabled`'s own test coverage;
/// see `commands::get_quota_failover_enabled`'s identical shape).
pub async fn try_auto_merge_enabled(db: &Db) -> anyhow::Result<bool> {
    Ok(is_enabled(repo::get_setting(db, K_AUTO_MERGE_ENABLED).await?.as_deref()))
}

fn is_enabled(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1") | Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{
        CiStatus, ConflictStatus, MergeReadiness, PrHost, PrSnapshot, ReviewStatus, ThreadStatus,
    };
    use sea_orm::ConnectionTrait;

    // --- run_gh_merge: guards that must fire before any process spawns ----

    #[test]
    fn run_gh_merge_refuses_an_embedded_slash_before_shelling_out() {
        // Mirrors `github::tests::fetch_status_refuses_an_embedded_slash_
        // before_shelling_out` exactly, including WHY the assertion checks
        // the guard's own message text rather than just success/failure: a
        // real `gh pr merge` failure (auth, not found, ...) could otherwise
        // make a deleted guard look identical to a passing test.
        match run_gh_merge("", "evil.example.org/ownerx", "repox", 5, "abc123") {
            Err(message) => assert!(message.contains("host override"), "got: {message}"),
            Ok(()) => panic!("expected the embedded-slash guard to fire"),
        }
        match run_gh_merge("", "owner", "a/b", 5, "abc123") {
            Err(message) => assert!(message.contains("host override"), "got: {message}"),
            Ok(()) => panic!("expected the embedded-slash guard to fire"),
        }
        match run_gh_merge("evil.example.org/extra", "owner", "repo", 5, "abc123") {
            Err(message) => assert!(message.contains("host override"), "got: {message}"),
            Ok(()) => panic!("expected the embedded-slash guard to fire for a smuggled host_base"),
        }
    }

    #[test]
    fn run_gh_merge_refuses_an_empty_head_sha_before_shelling_out() {
        match run_gh_merge("", "owner", "repo", 5, "") {
            Err(message) => assert!(message.contains("head_sha"), "got: {message}"),
            Ok(()) => panic!("expected the empty-head_sha guard to fire"),
        }
    }

    // --- build_merge_args: the head-consistency enforcement must actually
    // reach the `gh` invocation, not just exist in a doc comment. Host-
    // targeting (`[HOST/]OWNER/REPO` folding) is `super::qualified_repo_slug`'s
    // job now — see `host::tests` for that coverage, shared with
    // `github::GitHubHost::fetch_status` --------------------------------------

    #[test]
    fn build_merge_args_always_squashes_and_pins_match_head_commit_to_the_exact_sha() {
        let args = build_merge_args("acme/widgets", 42, "deadbeef");
        assert_eq!(
            args,
            vec!["pr", "merge", "42", "--repo", "acme/widgets", "--squash", "--match-head-commit", "deadbeef"]
        );
        assert!(args.contains(&"--squash".to_string()), "must always squash-merge, matching this repo's convention");
        let idx = args
            .iter()
            .position(|a| a == "--match-head-commit")
            .expect("--match-head-commit must be present — this is GitHub's own server-side enforcement that the merged commit is the one that was judged Ready, and this crate does not re-check it any other way");
        assert_eq!(
            args[idx + 1],
            "deadbeef",
            "the value right after the flag must be the exact judged head_sha, not some other field"
        );
    }

    #[test]
    fn build_merge_args_passes_through_an_already_host_qualified_repo_arg_verbatim() {
        // Review round 1 Codex P1 / Codex review PR #159 repo.rs:3873: a GitHub Enterprise
        // row's recorded host must actually reach the invocation — folding now happens in
        // `super::qualified_repo_slug` (`run_gh_merge`'s job to call), so this only needs to
        // prove the ALREADY-qualified string reaches --repo verbatim, untouched.
        let args = build_merge_args("github.acme-corp.com/acme/widgets", 42, "deadbeef");
        let idx = args.iter().position(|a| a == "--repo").unwrap();
        assert_eq!(args[idx + 1], "github.acme-corp.com/acme/widgets");
    }

    // --- lifecycle_state_tag: exhaustive, always distinguishable ----------

    #[test]
    fn lifecycle_state_tag_is_distinct_for_every_variant() {
        let open = lifecycle_state_tag(PrLifecycle::Open);
        let merged = lifecycle_state_tag(PrLifecycle::Merged);
        let closed = lifecycle_state_tag(PrLifecycle::Closed);
        assert_ne!(open, merged);
        assert_ne!(open, closed);
        assert_ne!(merged, closed);
        assert_eq!(open, "open");
        assert_eq!(merged, "merged");
        assert_eq!(closed, "closed");
    }

    // --- is_enabled --------------------------------------------------------

    #[test]
    fn is_enabled_recognizes_only_the_explicit_on_values() {
        assert!(is_enabled(Some("1")));
        assert!(is_enabled(Some("true")));
        assert!(!is_enabled(Some("0")));
        assert!(!is_enabled(Some("false")));
        assert!(
            !is_enabled(Some("yes")),
            "unlike the automatic-routing flag (which also accepts on/yes), this stays as narrow as quota_failover's own 1/true-only precedent — appropriate for an irreversible action"
        );
        assert!(!is_enabled(None));
        assert!(!is_enabled(Some("")));
    }

    // --- DB-backed fail-closed behavior (mirrors engine_routing::tests::
    // routing_policy_read_errors_fail_closed) ------------------------------

    #[tokio::test]
    async fn unset_setting_reads_as_disabled() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        assert!(!auto_merge_enabled(&db).await);
        assert!(!try_auto_merge_enabled(&db).await.unwrap());
    }

    #[tokio::test]
    async fn explicit_setting_values_round_trip() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, K_AUTO_MERGE_ENABLED, "1").await.unwrap();
        assert!(auto_merge_enabled(&db).await);
        repo::set_setting(&db, K_AUTO_MERGE_ENABLED, "0").await.unwrap();
        assert!(!auto_merge_enabled(&db).await);
    }

    #[tokio::test]
    async fn a_db_read_failure_fails_closed_never_silently_enabled() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        db.0.execute_unprepared("DROP TABLE app_setting").await.unwrap();

        let checked = try_auto_merge_enabled(&db).await;
        assert!(checked.is_err(), "the underlying error must still be observable to a caller that wants it");
        assert!(!auto_merge_enabled(&db).await, "the fail-closed wrapper must return false, never true, on a read error");
    }

    // --- MergeBackoffState: exhaustion / success / Retry-clear semantics --

    #[test]
    fn merge_backoff_exhausts_after_max_attempts_and_a_new_head_sha_gets_a_clean_slate() {
        let backoff = MergeBackoffState::default();
        assert!(!backoff.is_exhausted(1, "sha1"));
        for _ in 0..MAX_MERGE_ATTEMPTS_PER_HEAD {
            backoff.record_failure(1, "sha1");
        }
        assert!(backoff.is_exhausted(1, "sha1"));
        assert!(
            !backoff.is_exhausted(1, "sha2"),
            "a different head_sha must not inherit the exhausted streak — a new commit is always a clean slate"
        );
    }

    #[test]
    fn merge_backoff_a_success_forgets_the_streak() {
        let backoff = MergeBackoffState::default();
        backoff.record_failure(1, "sha1");
        backoff.record_failure(1, "sha1");
        backoff.record_success(1);
        assert!(!backoff.is_exhausted(1, "sha1"));
    }

    #[test]
    fn merge_backoff_record_failure_reports_exactly_when_this_attempt_crosses_the_threshold() {
        let backoff = MergeBackoffState::default();
        assert!(!backoff.record_failure(1, "sha1")); // 1
        assert!(!backoff.record_failure(1, "sha1")); // 2
        assert!(backoff.record_failure(1, "sha1"), "the 3rd (== MAX) failure must report exhausted"); // 3
    }

    #[test]
    fn merge_backoff_clear_removes_an_exhausted_entry_and_un_sticks_the_row() {
        // Task B's own regression guard, at the unit level: this is exactly
        // what `commands::retry_pr_tracking_core` now calls on Retry — see
        // `commands::tests::retry_pr_tracking_also_clears_an_exhausted_
        // merge_attempt_backoff` for the integration-level version.
        let backoff = MergeBackoffState::default();
        assert!(!backoff.clear(1), "nothing to clear yet");
        for _ in 0..MAX_MERGE_ATTEMPTS_PER_HEAD {
            backoff.record_failure(1, "sha1");
        }
        assert!(backoff.is_exhausted(1, "sha1"));
        assert!(backoff.clear(1), "an entry existed and was removed");
        assert!(!backoff.is_exhausted(1, "sha1"), "Retry must actually un-stick the row, not just report success");
    }

    // --- evaluate_row: the fresh-vs-stale safety property ------------------
    //
    // `evaluate_row` (steps 1-5) is the ONLY thing standing between "the
    // stored row looked ready a while ago" and "actually attempt a merge".
    // Every test below seeds the STORED row as fully ready (so `pre_decision`
    // — step 1 — would say `Merge` all on its own) and injects a FRESH read
    // that is NOT ready: exactly the scenario a caller that reused step 1's
    // stale verdict for step 5 would get wrong. Because `evaluate_row` only
    // ever returns a `RowVerdict` (it never itself calls `run_gh_merge`),
    // NONE of these tests can shell out to `gh` — including under the
    // mutation self-check below.
    //
    // MUTATION SELF-CHECK (see PR body for the full transcript): with step
    // 5's body temporarily changed from
    //   `let final_decision = gate::decide_auto_merge(enabled_now, ...);`
    // to
    //   `let final_decision = pre_decision;`
    // (discarding the fresh snapshot and `enabled_now` entirely — the exact
    // regression an independent review demonstrated has ZERO coverage today),
    // `evaluate_row_refuses_when_the_fresh_read_shows_ci_failing_even_though_
    // the_stored_row_was_ready` and `evaluate_row_refuses_when_the_fresh_
    // read_shows_the_pr_already_merged` both went from green to a hard
    // failure (`RowVerdict::Merge { .. }` instead of the expected `Skip`).
    // Reverting the mutation restored both to green with no other changes.

    struct FakeHost(PrSnapshot);

    impl PrHost for FakeHost {
        fn kind(&self) -> HostKind {
            HostKind::GitHub
        }
        fn fetch_status(&self, _target: &PrTarget) -> Result<PrSnapshot, HostError> {
            Ok(self.0.clone())
        }
    }

    fn resolver_fresh_ci_failing(_: HostKind) -> Result<Box<dyn PrHost>, HostError> {
        Ok(Box::new(FakeHost(PrSnapshot {
            head_sha: "fresh_sha_ci_failing".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: PrLifecycle::Open,
            ci: CiStatus::Failing,
            review: ReviewStatus::Approved,
            threads: ThreadStatus::AllResolved,
            conflict: ConflictStatus::Clean,
        })))
    }

    /// A fresh read showing a NEW review round opened since the stored
    /// snapshot: still open, still green, still approved — and now with
    /// unresolved threads.
    fn resolver_fresh_threads_unresolved(_: HostKind) -> Result<Box<dyn PrHost>, HostError> {
        Ok(Box::new(FakeHost(PrSnapshot {
            head_sha: "fresh_sha_threads_unresolved".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: PrLifecycle::Open,
            ci: CiStatus::Passing,
            review: ReviewStatus::Approved,
            threads: ThreadStatus::Unresolved { count: 2 },
            conflict: ConflictStatus::Clean,
        })))
    }

    fn resolver_fresh_already_merged(_: HostKind) -> Result<Box<dyn PrHost>, HostError> {
        Ok(Box::new(FakeHost(PrSnapshot {
            head_sha: "fresh_sha_already_merged".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: PrLifecycle::Merged,
            ci: CiStatus::Passing,
            review: ReviewStatus::Approved,
            threads: ThreadStatus::AllResolved,
            conflict: ConflictStatus::Clean,
        })))
    }

    fn resolver_fresh_fully_ready(_: HostKind) -> Result<Box<dyn PrHost>, HostError> {
        Ok(Box::new(FakeHost(PrSnapshot {
            head_sha: "fresh_sha_fully_ready".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: PrLifecycle::Open,
            ci: CiStatus::Passing,
            review: ReviewStatus::Approved,
            threads: ThreadStatus::AllResolved,
            conflict: ConflictStatus::Clean,
        })))
    }

    /// Test-only seam (mirrors `planner::tests::between_upstream_passes_probe`): lets a test
    /// PAUSE `evaluate_row` exactly between its FIRST upstream read (step 4) and its re-check
    /// (step 5.5) — the same window a concurrent re-proposal could land in (Codex review, PR
    /// #159 automerge.rs:395) — write a NEW dependency onto `direction_id` from a SEPARATE
    /// task, then release it. `arm_between_upstream_authorization_probe(direction_id)` returns
    /// `(reached_rx, resume_tx)`: `reached_rx` resolves once the probe is hit, and sending on
    /// `resume_tx` lets `evaluate_row` continue into its re-check. A no-op (never blocks) for
    /// any direction_id that wasn't armed.
    #[allow(clippy::type_complexity)]
    fn between_upstream_authorization_map() -> &'static std::sync::Mutex<
        std::collections::HashMap<i32, (tokio::sync::oneshot::Sender<()>, tokio::sync::oneshot::Receiver<()>)>,
    > {
        static M: std::sync::OnceLock<
            std::sync::Mutex<
                std::collections::HashMap<i32, (tokio::sync::oneshot::Sender<()>, tokio::sync::oneshot::Receiver<()>)>,
            >,
        > = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    fn arm_between_upstream_authorization_probe(
        direction_id: i32,
    ) -> (tokio::sync::oneshot::Receiver<()>, tokio::sync::oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        between_upstream_authorization_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(direction_id, (reached_tx, resume_rx));
        (reached_rx, resume_tx)
    }

    pub(super) async fn between_upstream_authorization_probe(direction_id: i32) {
        let armed = between_upstream_authorization_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&direction_id);
        if let Some((reached_tx, resume_rx)) = armed {
            let _ = reached_tx.send(());
            let _ = resume_rx.await;
        }
    }

    /// A tracked row whose STORED state is fully ready (`pre_decision` —
    /// step 1 — reads `Merge`) and the opt-in switch is on — the exact
    /// precondition every test above needs, isolated here once. Deliberately
    /// fake, obviously-nonexistent owner/repo (this suite never reaches
    /// `run_gh_merge`, but names it defensively anyway — the same posture
    /// this file's OWN `run_gh_merge` guard tests already use, e.g.
    /// `evil.example.org`).
    async fn seam_fixture(db: &Db) -> pull_request::Model {
        let ws = repo::create_workspace(db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(db, ws.id, "widgets", "/tmp/widgets", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(db, ws.id, "issue", "feature", "codex").await.unwrap();
        let direction = repo::create_direction(
            db, thread.id, "ship it", "codex", repo_ref.id, "why", "impl-only", "",
        )
        .await
        .unwrap();
        let pr = repo::register_pull_request(
            db,
            thread.id,
            direction.id,
            repo_ref.id,
            "github",
            "github.com",
            "weft-automerge-seam-test-fixture",
            "does-not-exist",
            42,
            "https://github.com/weft-automerge-seam-test-fixture/does-not-exist/pull/42",
            "seam fixture",
        )
        .await
        .unwrap();
        let stored = PrSnapshot {
            head_sha: "stored_sha_before_fresh_read".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: PrLifecycle::Open,
            ci: CiStatus::Passing,
            review: ReviewStatus::Approved,
            threads: ThreadStatus::AllResolved,
            conflict: ConflictStatus::Clean,
        };
        repo::apply_pull_request_snapshot(db, pr.id, &stored, &MergeReadiness::Ready).await.unwrap();
        repo::set_setting(db, K_AUTO_MERGE_ENABLED, "1").await.unwrap();
        repo::get_pull_request(db, pr.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn evaluate_row_refuses_when_the_fresh_read_shows_ci_failing_even_though_the_stored_row_was_ready() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let pr = seam_fixture(&db).await;
        let backoff = MergeBackoffState::default();

        let verdict = evaluate_row(&db, &pr, HostKind::GitHub, &backoff, resolver_fresh_ci_failing).await;

        assert_eq!(
            verdict,
            RowVerdict::Skip,
            "the FRESH read (CI failing) must win over the STORED row's own Ready verdict"
        );
        // Property 2: the persisted row must now reflect the FRESH snapshot
        // (proving step 4 actually used it) — NOT the stored one this
        // fixture originally seeded it with.
        let reloaded = repo::get_pull_request(&db, pr.id).await.unwrap().unwrap();
        assert_eq!(reloaded.head_sha, "fresh_sha_ci_failing");
        assert_eq!(gate::parse_ci(&reloaded.ci_status), CiStatus::Failing);
    }

    /// The scenario this whole axis exists for, end-to-end through the real
    /// evaluation path rather than the pure gate alone: a reviewer opens a
    /// new round between two sweeps. The stored row still reads Ready from
    /// before, CI is still green, GitHub still says APPROVED — the ONLY thing
    /// that changed is that threads are open. Without the axis this merged.
    #[tokio::test]
    async fn evaluate_row_refuses_when_the_fresh_read_shows_unresolved_review_threads() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let pr = seam_fixture(&db).await;
        let backoff = MergeBackoffState::default();

        let verdict =
            evaluate_row(&db, &pr, HostKind::GitHub, &backoff, resolver_fresh_threads_unresolved).await;

        assert_eq!(
            verdict,
            RowVerdict::Skip,
            "open review threads must stop an otherwise-ready merge"
        );
        // And the fresh reading is persisted, proving the refusal came from
        // the live read rather than from the row never being re-evaluated.
        let reloaded = repo::get_pull_request(&db, pr.id).await.unwrap().unwrap();
        assert_eq!(reloaded.head_sha, "fresh_sha_threads_unresolved");
        assert_eq!(
            gate::parse_threads(&reloaded.thread_status),
            ThreadStatus::Unresolved { count: 2 }
        );
    }

    #[tokio::test]
    async fn evaluate_row_refuses_when_the_fresh_read_shows_the_pr_already_merged() {
        // The module doc's own "double-merge safety across a crash" scenario:
        // someone else merged it since the stored snapshot was taken.
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let pr = seam_fixture(&db).await;
        let backoff = MergeBackoffState::default();

        let verdict = evaluate_row(&db, &pr, HostKind::GitHub, &backoff, resolver_fresh_already_merged).await;

        assert_eq!(verdict, RowVerdict::Skip);
    }

    #[tokio::test]
    async fn evaluate_row_authorizes_with_the_fresh_head_sha_when_everything_checks_out() {
        // Positive-path complement: when the fresh read agrees the row is
        // ready, the returned `head_sha` must be the FRESH one — never the
        // stale row's originally-registered sha — so `maybe_merge_one`'s
        // step 6 can never accidentally `--match-head-commit` against stale
        // data either.
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let pr = seam_fixture(&db).await;
        let backoff = MergeBackoffState::default();

        let verdict = evaluate_row(&db, &pr, HostKind::GitHub, &backoff, resolver_fresh_fully_ready).await;

        assert_eq!(verdict, RowVerdict::Merge { head_sha: "fresh_sha_fully_ready".to_string() });
    }

    /// THE FIX (Codex review, PR #159 automerge.rs:395): between `evaluate_row`'s upstream
    /// read (step 4) and its FINAL authorization, a re-proposal could add or replace this
    /// consumer's dependency — `--match-head-commit` (step 6, in `maybe_merge_one`) only
    /// guards the PR's OWN head commit moving, never this LOCAL ordering fact, so an already-
    /// authorized sweep would otherwise merge a consumer whose upstream just changed, with no
    /// backstop at all. Pauses `evaluate_row` exactly in that window (`between_upstream_
    /// authorization_probe`, mirroring `planner::tests::between_upstream_passes_probe`'s exact
    /// pattern) and drives a genuinely concurrent write — `repo::set_direction_upstream`, the
    /// same primitive `record_upstream_edges` itself calls — from a separately spawned task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn evaluate_row_aborts_when_a_dependency_is_added_after_authorization_but_before_the_recheck() {
        let tag = format!("weft-automerge-upstream-race-{}", std::process::id());
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&weft_home).unwrap();
        // File-backed (not :memory:) DB, same reason as `planner::tests::thread_gate_
        // serializes_concurrent_confirms`: the cloned pool handle in the spawned task must see
        // the SAME store as the test's own concurrent write.
        let db_file = weft_home.join("automerge-race.sqlite");
        let db = Db::connect(&format!("sqlite://{}?mode=rwc", db_file.to_str().unwrap()))
            .await
            .unwrap();
        let pr = seam_fixture(&db).await; // dependency-free, everything else Ready
        let consumer_direction_id = pr.direction_id;
        // A real, undecided producer to attach as the race's dependency.
        let thread = repo::get_thread(&db, pr.thread_id).await.unwrap().unwrap();
        let repo_ref = repo::get_repo(&db, pr.repo_id).await.unwrap().unwrap();
        let producer = repo::create_direction(
            &db, thread.id, "producer", "codex", repo_ref.id, "why", "impl-only", "",
        )
        .await
        .unwrap();
        let backoff = MergeBackoffState::default();

        let (reached_rx, resume_tx) = tests::arm_between_upstream_authorization_probe(consumer_direction_id);
        let db2 = db.clone();
        let pr2 = pr.clone();
        let backoff2 = backoff.clone();
        let evaluate_handle = tokio::spawn(async move {
            evaluate_row(&db2, &pr2, HostKind::GitHub, &backoff2, resolver_fresh_fully_ready).await
        });

        // Wait for evaluate_row to reach the gap between its upstream read and its re-check,
        // then — WHILE it is still paused there — add a real dependency from a separate task,
        // the same write `record_upstream_edges` itself performs.
        reached_rx.await.expect("evaluate_row reached the between-authorization probe");
        repo::set_direction_upstream(&db, consumer_direction_id, producer.id).await.unwrap();
        let _ = resume_tx.send(());

        let verdict = evaluate_handle.await.unwrap();
        assert_eq!(
            verdict,
            RowVerdict::Skip,
            "a dependency added between authorization and the re-check must abort this merge \
             attempt, not silently proceed on the stale (dependency-free) read"
        );

        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn evaluate_row_respects_the_backoff_even_when_the_stored_row_still_looks_ready() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let pr = seam_fixture(&db).await;
        let backoff = MergeBackoffState::default();
        for _ in 0..MAX_MERGE_ATTEMPTS_PER_HEAD {
            backoff.record_failure(pr.id, &pr.head_sha);
        }

        let verdict = evaluate_row(&db, &pr, HostKind::GitHub, &backoff, resolver_fresh_fully_ready).await;

        assert_eq!(
            verdict,
            RowVerdict::Skip,
            "step 3's backoff must stop the row before step 4 is ever reached, exhausted or not"
        );
    }
}
