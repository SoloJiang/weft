//! Background PR/MR monitor (issue #110 T1): the load-bearing piece that
//! makes tracked PR/MR state a durable, PROCESS-level fact instead of
//! something that only lives in an agent's turn or a chat session's memory —
//! a session can hit a 500 or a quota wall and evaporate; this sweep does
//! not. It performs an immediate pass followed by a timer loop.
//!
//! HARD BOUNDARY, enforced by construction (not just this comment): this
//! module calls `PrHost::fetch_status` (a READ) and then only ever writes
//! into weft's OWN `pull_request` table (`store::repo::apply_pull_request_
//! snapshot` / `mark_pull_request_probe_error`).
//! It NEVER shells out to `gh`/`glab` for a MUTATING subcommand (`pr merge`,
//! `pr comment`, `pr review`, …), never touches git, and never resolves a
//! review thread on the host. Every host-facing call in this file funnels
//! through `PrHost`, whose trait signature (`fetch_status(&self, target) ->
//! Result<PrSnapshot, HostError>`) has no side-effecting counterpart to call
//! by mistake — that stays exclusively the agent's job, using the agent's own
//! tools.

use tauri::{AppHandle, Emitter, Manager};

use super::judge;
use super::{HostKind, MergeReadiness, PrSnapshot, PrTarget};
use crate::store::entities::pull_request;
use crate::store::{repo, Db};

/// Default PR polling cadence. `0` disables the loop entirely.
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
/// The row also becomes a canonical `pr_tracking_retry` attention item at
/// this boundary, so the user can explicitly restart tracking.
pub(crate) const MAX_CONSECUTIVE_PROBE_FAILURES: i32 = 10;

/// Start the runtime PR/MR sweep. Call once at app setup.
pub fn spawn_pr_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let sweep_secs = crate::commands::env_secs("WEFT_PR_SWEEP_SECS", PR_SWEEP_DEFAULT_SECS);
        if sweep_secs == 0 {
            return; // disabled
        }
        run_pr_sweep(&app).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(sweep_secs)).await;
            run_pr_sweep(&app).await;
        }
    });
}

/// One sweep pass over every still-`open` tracked row.
async fn run_pr_sweep(app: &AppHandle) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    let open = match repo::list_open_pull_requests(&db, MAX_CONSECUTIVE_PROBE_FAILURES).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weft][host] pr sweep: could not list tracked PR/MRs: {e}");
            return;
        }
    };
    for pr in open {
        check_one(app, &db, pr).await;
    }
}

/// Probe one PR/MR (off the async runtime — `PrHost::fetch_status` shells out
/// synchronously, same `spawn_blocking` discipline `check::run_checks`'
/// caller already uses) and apply the result.
async fn check_one(
    app: &AppHandle,
    db: &Db,
    pr: pull_request::Model,
) {
    let Some(kind) = HostKind::parse(&pr.host_kind) else {
        apply_probe_result(
            app,
            db,
            &pr,
            Err(super::HostError::Other {
                message: format!("unrecognized host_kind '{}' on this row", pr.host_kind),
            }),
        )
        .await;
        return;
    };
    let target = PrTarget {
        host_base: pr.host_base.clone(),
        owner: pr.host_owner.clone(),
        repo: pr.host_repo.clone(),
        number: pr.number,
    };
    let result = tokio::task::spawn_blocking(move || {
        super::resolve_host(kind).and_then(|h| h.fetch_status(&target))
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(super::HostError::Other { message: format!("internal: sweep task join error: {join_err}") })
    });
    apply_probe_result(app, db, &pr, result).await;
}

/// Write the fetch outcome back to the DB. Routine readiness and retrying
/// probe failures are telemetry only; once the durable failure count reaches
/// give-up, the canonical AttentionSnapshot projects a real Retry action.
async fn apply_probe_result(
    app: &AppHandle,
    db: &Db,
    pr: &pull_request::Model,
    result: Result<PrSnapshot, super::HostError>,
) {
    match &result {
        Ok(snapshot) => {
            // Ordering is resolved per sweep, not cached on the row: the
            // upstream's lifecycle changes on ITS own schedule, and a stale
            // copy here would either hold a mergeable PR back or release one
            // early. `direction_id == 0` (a legacy row) resolves to Unknown,
            // which is the honest answer for a PR whose task we cannot find.
            let upstream = repo::upstream_merge_state(db, pr.direction_id).await;
            let readiness = judge::merge_readiness(
                &snapshot.ci,
                &snapshot.review,
                &snapshot.threads,
                &snapshot.conflict,
                &upstream,
            );
            let axis_error = snapshot.unreadable_axis_error();
            let changed = snapshot_changed(pr, snapshot, &readiness, axis_error.is_some());
            let fail_count = match repo::apply_pull_request_snapshot(
                db,
                pr.id,
                snapshot,
                &readiness,
                match axis_error {
                    Some(reason) => repo::StreakUpdate::Extend(reason),
                    None => repo::StreakUpdate::Clear,
                },
            )
            .await
            {
                Ok(count) => Some(count),
                Err(e) => {
                    eprintln!("[weft][host] pr #{}: could not save snapshot: {e}", pr.id);
                    None
                }
            };
            // Only a PERSISTED change is announced. Restructuring this write
            // to also return the failure count turned the original `if let
            // Err … else if changed` into an unconditional emit, which would
            // announce state the DB never accepted — the same "claiming
            // something we could not confirm" this whole feature exists to
            // stop. Found in self-review, not by a reviewer.
            if fail_count.is_some() && changed {
                emit_pr_changed(app, pr);
            }
        }
        Err(e) => {
            match repo::mark_pull_request_probe_error(db, pr.id, &e.message()).await {
                Ok(count) => count,
                Err(store_err) => {
                    eprintln!("[weft][host] pr #{}: could not save probe error: {store_err}", pr.id);
                    None
                }
            };
            emit_pr_changed(app, pr);
            if repo::get_pull_request(db, pr.id)
                .await
                .ok()
                .flatten()
                .is_some_and(|row| row.probe_fail_count >= MAX_CONSECUTIVE_PROBE_FAILURES)
            {
                let _ = app.emit("needs-you://changed", pr.thread_id);
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
fn snapshot_changed(
    old: &pull_request::Model,
    snapshot: &PrSnapshot,
    readiness: &MergeReadiness,
    new_probe_failed: bool,
) -> bool {
    let new_ci = serde_json::to_string(&snapshot.ci).unwrap_or_default();
    let new_review = serde_json::to_string(&snapshot.review).unwrap_or_default();
    let new_threads = serde_json::to_string(&snapshot.threads).unwrap_or_default();
    let new_conflict = serde_json::to_string(&snapshot.conflict).unwrap_or_default();
    let new_readiness = serde_json::to_string(readiness).unwrap_or_default();
    let old_probe_failed = !old.last_error.trim().is_empty() || old.probe_fail_count > 0;
    old.head_sha != snapshot.head_sha
        || old.lifecycle != snapshot.lifecycle.as_str()
        || old.ci_status != new_ci
        || old.review_status != new_review
        || old.thread_status != new_threads
        || old.conflict_status != new_conflict
        || old.merge_readiness != new_readiness
        // Probe health is a state transition, not a comparison of the error
        // text or retry count, so repeated failures do not emit every sweep.
        || old_probe_failed != new_probe_failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{CiStatus, ConflictStatus, PrLifecycle, ReviewStatus, ThreadStatus};

    /// Only a failed thread READ counts as partial. `ConflictStatus::Unknown`
    /// is what GitHub reports for seconds after every push — treating it as a
    /// probe failure would march healthy PRs to the give-up threshold, and
    /// `Unchecked` means a backend does not implement the axis at all, which
    /// retrying cannot fix.
    #[test]
    fn only_an_unreadable_thread_axis_makes_a_read_partial() {
        let mut snap = base_snapshot();
        assert_eq!(snap.unreadable_axis_error(), None);

        snap.conflict = ConflictStatus::Unknown { reason: "not computed yet".to_string() };
        assert_eq!(
            snap.unreadable_axis_error(),
            None,
            "a transient mergeability window is not a probe failure"
        );

        snap.threads = crate::host::ThreadStatus::Unchecked;
        assert_eq!(snap.unreadable_axis_error(), None, "an unimplemented axis is not a failure");

        snap.threads = crate::host::ThreadStatus::Unresolved { count: 2 };
        assert_eq!(snap.unreadable_axis_error(), None, "a successful count is not a failure");

        snap.threads = crate::host::ThreadStatus::Unknown { reason: "boom".to_string() };
        assert_eq!(snap.unreadable_axis_error(), Some("boom"));
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
            thread_status: serde_json::to_string(&ThreadStatus::AllResolved).unwrap(),
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
            threads: ThreadStatus::AllResolved,
            conflict: ConflictStatus::Clean,
        }
    }

    #[test]
    fn identical_snapshot_is_not_a_change() {
        let old = base_row();
        let snap = base_snapshot();
        let readiness = MergeReadiness::Ready;
        assert!(!snapshot_changed(&old, &snap, &readiness, false));
    }

    #[test]
    fn a_successful_probe_recovery_is_a_change_even_when_snapshot_is_identical() {
        let mut old = base_row();
        old.last_error = "previous probe failure".to_string();
        old.probe_fail_count = 1;
        let snap = base_snapshot();

        assert_eq!(snap.unreadable_axis_error(), None);
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready, false));
    }

    #[test]
    fn an_unchanged_probe_failure_is_not_a_change() {
        let mut old = base_row();
        let mut snap = base_snapshot();
        snap.threads = ThreadStatus::Unknown { reason: "latest probe failure".to_string() };
        old.thread_status = serde_json::to_string(&snap.threads).unwrap();
        old.last_error = "previous probe failure".to_string();
        old.probe_fail_count = 1;
        let new_probe_failed = snap.unreadable_axis_error().is_some();

        assert!(new_probe_failed);
        assert!(!snapshot_changed(&old, &snap, &MergeReadiness::Ready, new_probe_failed));
    }

    #[test]
    fn a_new_head_sha_is_a_change() {
        let old = base_row();
        let mut snap = base_snapshot();
        snap.head_sha = "bbb".to_string();
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready, false));
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
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready, false));
    }

    #[test]
    fn a_lifecycle_flip_is_a_change_even_if_axes_are_identical() {
        let old = base_row();
        let mut snap = base_snapshot();
        snap.lifecycle = PrLifecycle::Merged;
        assert!(snapshot_changed(&old, &snap, &MergeReadiness::Ready, false));
    }

    #[test]
    fn a_merge_readiness_flip_alone_is_a_change() {
        // Same raw axes stored, but readiness recomputed differently (e.g. the
        // judgement function itself changed) must still be caught — this is
        // what keeps `snapshot_changed` honest about DERIVED state, not just
        // the raw axis columns.
        let old = base_row();
        let snap = base_snapshot();
        let readiness = MergeReadiness::Blocked { reasons: vec!["something".to_string()] };
        assert!(snapshot_changed(&old, &snap, &readiness, false));
    }

}
