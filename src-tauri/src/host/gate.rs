//! Pure gate for issue #110 T3 (auto-merge): whether a PR/MR's state clears
//! the bar for Weft to run `gh pr merge` on its own, unattended, with nobody
//! confirming the specific action. This is deliberately a STRICTER bar than
//! `judge::merge_readiness == Ready` alone — [`decide_auto_merge`]'s doc
//! walks every extra dimension and why each one earns its place. Zero I/O
//! anywhere in this file; every input is a value the impure caller (`host::
//! automerge`) already read from the DB or a fresh `PrHost::fetch_status`.
//! This file is the primary target of this PR's mutation self-check (see the
//! PR body): flip any branch below and a test must go red.
//!
//! `automerge::maybe_merge_one` calls [`decide_auto_merge`] TWICE per
//! candidate row, deliberately reusing the exact same pure function for both
//! calls rather than two different ones:
//!   1. A cheap PRE-FILTER against the STORED (possibly up to `automerge::
//!      MAX_READY_AGE_SECS` stale) DB row — decides only whether a row is
//!      worth a fresh, live re-check at all (an efficiency/rate-limit
//!      concern, never a final authority).
//!   2. The FINAL authorization against a FRESH `PrHost::fetch_status` read
//!      taken immediately before the mutating call — `age_secs`/
//!      `probe_fail_count` are passed as `0` here (the read just succeeded),
//!      and `enabled`/`lifecycle`/`ci`/`readiness` are all re-derived from
//!      that fresh read, never trusted from the pre-filter's stale snapshot.
//! Review round 1 on this PR found the ORIGINAL version only ever consulted
//! the stored row and re-read fresh state AFTER the merge attempt (for
//! bookkeeping only) — a startup race and a "review dismissed / CI re-run
//! and failed while Weft was closed, on the SAME head_sha" gap that
//! `--match-head-commit` alone cannot catch (it only pins the commit, not
//! the review/CI verdicts about it). Calling the SAME gate function twice,
//! with the second call fed only fresh data, closes both.
//!
//! Kept in its own file rather than folded into `judge.rs` — not because the
//! two are unrelated (both are pure PR-state judgements over the exact same
//! normalized types), but so this feature never has to touch `judge.rs`'s
//! lines while an unrelated text-only change to that file is in flight
//! elsewhere (see this PR's own notes on scope/territory).

use super::{CiStatus, HostKind, MergeReadiness, PrLifecycle};

/// The verdict. `Merge` is the ONLY variant that authorizes the one mutating
/// call in this feature (`automerge::run_gh_merge`) — every other outcome,
/// INCLUDING a `MergeReadiness` variant this function has never heard of,
/// means "leave this row alone this round". There is no catch-all arm on the
/// `MergeReadiness` match below: a new variant is a compile error here until
/// this file is updated for it, the same exhaustiveness discipline
/// `judge::merge_readiness` itself already uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoMergeDecision {
    Merge,
    Skip(AutoMergeSkipReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoMergeSkipReason {
    /// The opt-in setting is off — the default, and what an unreadable
    /// setting fails closed to (see `automerge::auto_merge_enabled`'s doc).
    Disabled,
    /// This row's host has no merge-capable backend yet (MVP: GitLab — see
    /// `host::resolve_host`). Never attempt a `gh` call for a host `gh`
    /// cannot possibly act on.
    UnsupportedHost,
    /// The stored/fresh `lifecycle` string was not one of the exact three
    /// values this crate itself ever writes. Unlike `github::lifecycle_of`
    /// (which defaults an unrecognized value to `Open` so the READ-ONLY
    /// monitor never silently stops watching a row on a fluke — the right
    /// call for a passive observer), the auto-merge gate's failure mode is
    /// inverted: an unrecognized value (DB corruption, a future lifecycle
    /// value this file has not been updated for) must NEVER be read as
    /// "open, safe to act on". Review round 1, Codex P2.
    UnknownLifecycle,
    /// Already merged/closed — nothing left to do. Reachable even though
    /// `automerge`'s own row query already filters to `open` rows, because
    /// this function must stay honest for ANY caller, not just today's one —
    /// the same "defense in depth, don't trust the caller already filtered"
    /// posture `github::GitHubHost::fetch_status`'s embedded-slash guard
    /// documents alongside `parse_pr_url`'s own rejection.
    NotOpen,
    /// At least one probe since the last SUCCESS has failed. A row can carry
    /// a `Ready` readiness column that predates those failures — a failed
    /// probe never touches the stored axes/readiness columns (see
    /// `repo::mark_pull_request_probe_error`'s doc) — so merging on that
    /// no-longer-confirmed verdict would be exactly the "hours-old snapshot"
    /// risk this gate exists to close.
    ProbeFailing,
    /// The last SUCCESSFUL probe is older than the freshness window. A
    /// different way a row's `Ready` column can outlive its truth than
    /// `ProbeFailing`: `probe_fail_count` can read 0 (no failures recorded)
    /// while `last_checked_at` is still stale, if the sweep loop itself
    /// stalled or the whole process was suspended for hours and just
    /// resumed. Neither signal alone is sufficient, both are checked.
    Stale,
    /// `ci` is not LITERALLY `CiStatus::Passing`. `judge::merge_readiness`'s
    /// own `ci_verdict` folds `NotConfigured` into the SAME "Clear" bucket as
    /// `Passing` — correct for a Needs-you notice (no CI configured is
    /// vacuously fine to leave a human alone about) but wrong for an
    /// unattended, irreversible merge: an empty `statusCheckRollup` also
    /// covers the window right after a new head is pushed, before its checks
    /// have even been reported yet (`github::ci_of`'s own doc). Review round
    /// 1 P1 / Codex P1: this axis is checked INDEPENDENTLY of `readiness`
    /// below, so a `Ready` verdict built on a `NotConfigured` CI axis is
    /// still refused here.
    CiNotPassing,
    /// `judge::merge_readiness` says `Blocked` — CI/review/conflict are all
    /// readable, at least one is a real blocker.
    MergeReadinessBlocked,
    /// `judge::merge_readiness` says `Indeterminate` — at least one axis was
    /// unreadable. This must NEVER be treated as mergeable; see
    /// `MergeReadiness::Indeterminate`'s own doc for why `Indeterminate` and
    /// `Ready` are never conflated anywhere else in this codebase either.
    MergeReadinessIndeterminate,
}

/// The gate itself. Every dimension is AND-ed — `Merge` requires ALL of: the
/// opt-in switch on, a host this feature can actually act on, a RECOGNIZED
/// open lifecycle, zero probe failures since the last success, a
/// recent-enough last successful probe, CI literally `Passing`, AND
/// `MergeReadiness::Ready`.
///
/// `age_secs`/`max_age_secs` are handed in as plain values rather than read
/// from a clock in here, so every boundary is a deterministic unit test with
/// no time mocking — see `automerge::MAX_READY_AGE_SECS` for the real
/// threshold production calls this with (and `0`, unconditionally, for the
/// SECOND call against a freshly-read snapshot — see this file's own doc).
///
/// Deliberately does NOT re-check `head_sha` against a second read — that
/// job belongs to `gh pr merge --match-head-commit`, enforced by GitHub
/// itself at the exact moment of the merge attempt. That is stronger than
/// any client-side comparison this function could do: it cannot be raced
/// between "we decided this was fine" and "the API call actually executes".
/// See `automerge::run_gh_merge`'s doc.
///
/// KNOWN, DELIBERATE GAP (review round 1 P1 / Codex P1, tracked rather than
/// silently accepted): `MergeReadiness::Ready` reflects GitHub's own
/// aggregate `reviewDecision` — it does NOT independently verify this
/// repo's OWN CLAUDE.md conventions layered on top of that (zero unresolved
/// review discussion threads; a specific bot's 👍/approving review). Those
/// need paginated GraphQL `reviewThreads` reads this MVP does not have (see
/// `ReviewStatus`'s own doc in `host/mod.rs`) plus a repo-specific,
/// configurable notion of "which bot's sign-off counts" that would vary
/// per-installation — building either correctly, under review pressure,
/// risked exactly the "fixed one gap, shipped a new bug in the process" this
/// repo's CLAUDE.md explicitly warns about (subtly wrong thread-pagination,
/// or a hardcoded bot-login check that silently makes auto-merge NEVER fire
/// for the vast majority of repos that don't run that specific bot). The
/// honest mitigation available today: GitHub's OWN branch protection can
/// enforce "Require conversation resolution before merging" and "Require
/// review from Code Owners" / a specific required reviewer SERVER-SIDE —
/// enable those on any branch this feature is allowed to touch, and `gh pr
/// merge` will refuse exactly like it already does for `--match-head-commit`.
/// The Settings toggle's own copy names this limitation explicitly.
pub fn decide_auto_merge(
    enabled: bool,
    host_kind: HostKind,
    lifecycle: Option<PrLifecycle>,
    ci: &CiStatus,
    readiness: &MergeReadiness,
    probe_fail_count: i32,
    age_secs: i64,
    max_age_secs: i64,
) -> AutoMergeDecision {
    if !enabled {
        return AutoMergeDecision::Skip(AutoMergeSkipReason::Disabled);
    }
    match host_kind {
        HostKind::GitHub => {}
        HostKind::GitLab => return AutoMergeDecision::Skip(AutoMergeSkipReason::UnsupportedHost),
    }
    match lifecycle {
        Some(PrLifecycle::Open) => {}
        Some(PrLifecycle::Closed) | Some(PrLifecycle::Merged) => {
            return AutoMergeDecision::Skip(AutoMergeSkipReason::NotOpen)
        }
        None => return AutoMergeDecision::Skip(AutoMergeSkipReason::UnknownLifecycle),
    }
    if probe_fail_count != 0 {
        return AutoMergeDecision::Skip(AutoMergeSkipReason::ProbeFailing);
    }
    // A negative age (clock skew, or a malformed timestamp the caller could
    // not parse — see `age_secs` below, which returns `i64::MAX` for that
    // case, not a negative) must never read as "even fresher than fresh".
    if !(0..=max_age_secs).contains(&age_secs) {
        return AutoMergeDecision::Skip(AutoMergeSkipReason::Stale);
    }
    if *ci != CiStatus::Passing {
        return AutoMergeDecision::Skip(AutoMergeSkipReason::CiNotPassing);
    }
    match readiness {
        MergeReadiness::Ready => AutoMergeDecision::Merge,
        MergeReadiness::Blocked { .. } => {
            AutoMergeDecision::Skip(AutoMergeSkipReason::MergeReadinessBlocked)
        }
        MergeReadiness::Indeterminate { .. } => {
            AutoMergeDecision::Skip(AutoMergeSkipReason::MergeReadinessIndeterminate)
        }
    }
}

/// Parse the `pull_request.lifecycle` column STRICTLY: only the exact three
/// values this crate itself ever writes are recognized. See
/// [`AutoMergeSkipReason::UnknownLifecycle`]'s doc for why this is the
/// opposite default from `github::lifecycle_of`.
pub fn parse_lifecycle(s: &str) -> Option<PrLifecycle> {
    match s {
        "open" => Some(PrLifecycle::Open),
        "closed" => Some(PrLifecycle::Closed),
        "merged" => Some(PrLifecycle::Merged),
        _ => None,
    }
}

/// Parse the `pull_request.merge_readiness` JSON column back into
/// [`MergeReadiness`]. Empty (never successfully probed yet) or malformed
/// (should not happen — this repo is the only writer — but a stored value
/// must never be trusted blindly) both fall back to `Indeterminate`, NEVER
/// to `Ready`: an unreadable verdict is exactly as unmergeable as one that
/// was honestly computed as `Indeterminate`.
pub fn parse_readiness(s: &str) -> MergeReadiness {
    serde_json::from_str(s).unwrap_or_else(|_| MergeReadiness::Indeterminate {
        reasons: vec!["尚未成功探测过合并就绪状态,或存储的状态无法解析".to_string()],
    })
}

/// Parse the `pull_request.ci_status` JSON column back into [`CiStatus`],
/// used by the auto-merge PRE-FILTER (`decide_auto_merge`'s `ci` dimension)
/// to catch a `NotConfigured`/stale-cached CI reading BEFORE even attempting
/// a fresh check — same never-trust-blindly posture as [`parse_readiness`].
/// Empty/malformed falls back to `Unknown`, never `Passing`.
pub fn parse_ci(s: &str) -> CiStatus {
    serde_json::from_str(s).unwrap_or_else(|_| CiStatus::Unknown {
        reason: "尚未成功探测过 CI 状态,或存储的状态无法解析".to_string(),
    })
}

/// `now - last_checked_at`, both unix-seconds strings (`pull_request`'s own
/// storage convention — see `repo::now_unix`). Unparseable or empty (a row
/// that has never completed a successful probe leaves `last_checked_at`
/// empty) reads as `i64::MAX` — maximally stale, never treated as fresh by
/// accident.
pub fn age_secs(last_checked_at: &str, now: &str) -> i64 {
    let parsed = |s: &str| s.trim().parse::<i64>().ok();
    match (parsed(last_checked_at), parsed(now)) {
        (Some(then), Some(now)) => now - then,
        _ => i64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_AGE: i64 = 600;

    fn blocked() -> MergeReadiness {
        MergeReadiness::Blocked { reasons: vec!["x".to_string()] }
    }

    fn indeterminate() -> MergeReadiness {
        MergeReadiness::Indeterminate { reasons: vec!["x".to_string()] }
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        enabled: bool,
        host_kind: HostKind,
        lifecycle: Option<PrLifecycle>,
        ci: &CiStatus,
        readiness: &MergeReadiness,
        probe_fail_count: i32,
        age_secs: i64,
    ) -> AutoMergeDecision {
        decide_auto_merge(enabled, host_kind, lifecycle, ci, readiness, probe_fail_count, age_secs, MAX_AGE)
    }

    fn ready_ci() -> CiStatus {
        CiStatus::Passing
    }

    // --- decide_auto_merge: one test isolates exactly one disqualifier ----

    #[test]
    fn all_conditions_met_authorizes_merge() {
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, 0),
            AutoMergeDecision::Merge
        );
    }

    #[test]
    fn disabled_switch_skips_even_when_everything_else_is_ready() {
        assert_eq!(
            decide(false, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::Disabled)
        );
    }

    #[test]
    fn gitlab_host_is_unsupported_even_when_otherwise_ready() {
        assert_eq!(
            decide(true, HostKind::GitLab, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::UnsupportedHost)
        );
    }

    #[test]
    fn unrecognized_lifecycle_skips_as_unknown_never_as_open() {
        assert_eq!(
            decide(true, HostKind::GitHub, None, &ready_ci(), &MergeReadiness::Ready, 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::UnknownLifecycle)
        );
    }

    #[test]
    fn closed_or_merged_lifecycle_skips_as_not_open() {
        for lifecycle in [PrLifecycle::Closed, PrLifecycle::Merged] {
            assert_eq!(
                decide(true, HostKind::GitHub, Some(lifecycle), &ready_ci(), &MergeReadiness::Ready, 0, 0),
                AutoMergeDecision::Skip(AutoMergeSkipReason::NotOpen),
                "lifecycle={lifecycle:?}"
            );
        }
    }

    #[test]
    fn any_probe_failure_since_last_success_skips_even_when_the_stored_readiness_still_reads_ready() {
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 1, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::ProbeFailing)
        );
    }

    #[test]
    fn age_exactly_at_the_boundary_is_still_fresh_enough() {
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, MAX_AGE),
            AutoMergeDecision::Merge
        );
    }

    #[test]
    fn age_one_past_the_boundary_is_stale() {
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, MAX_AGE + 1),
            AutoMergeDecision::Skip(AutoMergeSkipReason::Stale)
        );
    }

    #[test]
    fn negative_age_is_treated_as_stale_never_as_extra_fresh() {
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, -1),
            AutoMergeDecision::Skip(AutoMergeSkipReason::Stale)
        );
    }

    #[test]
    fn not_configured_ci_skips_even_though_merge_readiness_treats_it_as_clear() {
        // THE regression test for review round 1's P1 / Codex P1: an empty
        // statusCheckRollup makes `judge::merge_readiness` itself read
        // `Ready` (ci_verdict folds NotConfigured into Clear) — the
        // auto-merge gate must refuse anyway, independently of that verdict.
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &CiStatus::NotConfigured, &MergeReadiness::Ready, 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::CiNotPassing)
        );
    }

    #[test]
    fn pending_failing_or_unknown_ci_all_skip_as_not_passing() {
        for ci in [
            CiStatus::Pending,
            CiStatus::Failing,
            CiStatus::Unknown { reason: "x".to_string() },
        ] {
            assert_eq!(
                decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ci, &MergeReadiness::Ready, 0, 0),
                AutoMergeDecision::Skip(AutoMergeSkipReason::CiNotPassing),
                "ci={ci:?}"
            );
        }
    }

    #[test]
    fn blocked_readiness_skips_with_its_own_distinct_reason() {
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &blocked(), 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::MergeReadinessBlocked)
        );
    }

    #[test]
    fn indeterminate_readiness_never_authorizes_a_merge() {
        // The core honesty property this whole gate exists to enforce: an
        // axis we could not read is never treated as "fine, go ahead".
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &ready_ci(), &indeterminate(), 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::MergeReadinessIndeterminate)
        );
    }

    #[test]
    fn checks_are_ordered_so_two_simultaneous_disqualifiers_report_the_earlier_one() {
        // Disabled (checked first) must win over UnsupportedHost (checked
        // second) when both apply — not an arbitrary/unspecified pick.
        assert_eq!(
            decide(false, HostKind::GitLab, Some(PrLifecycle::Open), &ready_ci(), &MergeReadiness::Ready, 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::Disabled)
        );
    }

    #[test]
    fn ci_not_passing_is_checked_before_readiness_not_after() {
        // If CI-not-passing were checked AFTER matching `readiness` (or
        // folded into it), a `Blocked`/`Indeterminate` readiness could mask
        // the CI-specific reason. Pin the ordering: CiNotPassing wins even
        // when readiness ALSO would have blocked for an unrelated reason.
        assert_eq!(
            decide(true, HostKind::GitHub, Some(PrLifecycle::Open), &CiStatus::Failing, &blocked(), 0, 0),
            AutoMergeDecision::Skip(AutoMergeSkipReason::CiNotPassing)
        );
    }

    // --- parse_lifecycle ---------------------------------------------------

    #[test]
    fn parse_lifecycle_recognizes_exactly_the_three_stored_values() {
        assert_eq!(parse_lifecycle("open"), Some(PrLifecycle::Open));
        assert_eq!(parse_lifecycle("closed"), Some(PrLifecycle::Closed));
        assert_eq!(parse_lifecycle("merged"), Some(PrLifecycle::Merged));
    }

    #[test]
    fn parse_lifecycle_treats_anything_else_as_unknown_not_open() {
        assert_eq!(parse_lifecycle("anything_else"), None);
        assert_eq!(parse_lifecycle(""), None);
        assert_eq!(parse_lifecycle("OPEN"), None, "case-sensitive — must not guess at case variants");
    }

    // --- parse_readiness -----------------------------------------------

    #[test]
    fn parse_readiness_round_trips_a_stored_value() {
        let stored = serde_json::to_string(&MergeReadiness::Ready).unwrap();
        assert_eq!(parse_readiness(&stored), MergeReadiness::Ready);
        let stored_blocked = serde_json::to_string(&blocked()).unwrap();
        assert_eq!(parse_readiness(&stored_blocked), blocked());
    }

    #[test]
    fn parse_readiness_treats_empty_or_malformed_as_indeterminate_never_as_ready() {
        assert!(matches!(parse_readiness(""), MergeReadiness::Indeterminate { .. }));
        assert!(matches!(parse_readiness("not json"), MergeReadiness::Indeterminate { .. }));
        assert!(matches!(parse_readiness("{}"), MergeReadiness::Indeterminate { .. }));
    }

    // --- parse_ci ------------------------------------------------------

    #[test]
    fn parse_ci_round_trips_a_stored_value() {
        let stored = serde_json::to_string(&CiStatus::Passing).unwrap();
        assert_eq!(parse_ci(&stored), CiStatus::Passing);
        let stored_failing = serde_json::to_string(&CiStatus::Failing).unwrap();
        assert_eq!(parse_ci(&stored_failing), CiStatus::Failing);
    }

    #[test]
    fn parse_ci_treats_empty_or_malformed_as_unknown_never_as_passing() {
        assert!(matches!(parse_ci(""), CiStatus::Unknown { .. }));
        assert!(matches!(parse_ci("not json"), CiStatus::Unknown { .. }));
    }

    // --- age_secs ------------------------------------------------------

    #[test]
    fn age_secs_computes_the_plain_difference() {
        assert_eq!(age_secs("100", "160"), 60);
        assert_eq!(age_secs("160", "100"), -60, "must not clamp — decide_auto_merge is what rejects negatives");
    }

    #[test]
    fn age_secs_treats_unparseable_or_empty_as_maximally_stale() {
        assert_eq!(age_secs("", "160"), i64::MAX);
        assert_eq!(age_secs("not a number", "160"), i64::MAX);
        assert_eq!(age_secs("100", "not a number"), i64::MAX);
    }
}
