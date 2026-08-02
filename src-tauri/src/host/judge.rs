//! The `truly mergeable` judgement (issue #110 T2): this repo's CLAUDE.md
//! "GitHub Remote Review Workflow" bar — CI green × review clear/approved ×
//! every review thread resolved × no conflict — turned into a pure,
//! exhaustively-matched function instead of
//! prose a lead has to remember to follow. [`merge_readiness`] is THE
//! judgement; everything else here builds its human-facing readiness reasons.
//!
//! Further axes join those for cross-repo change sets: an upstream
//! task's PR must be merged before this one is mergeable at all. Each is an axis
//! rather than a separate gate on purpose — [`merge_readiness`] is the single
//! source of truth every consumer already reads (monitor state, auto-merge
//! gate, the UI), so ordering flows to all of them by construction instead of
//! being re-derived per caller. [`thread_verdict`] joined the same way, for
//! the same reason: it is listed in that CLAUDE.md bar right next to
//! approval, so it belongs beside it here rather than bolted onto the one
//! caller that happens to care most.
//!
//! This file is the primary target of this PR's mutation self-check (see the
//! PR body): flip any arm of [`ci_verdict`] / [`review_verdict`] /
//! [`thread_verdict`] / [`conflict_verdict`] / [`upstream_verdict`] / the
//! `has_unknown` / `has_blocking` branches in [`merge_readiness`], and a test
//! below must go red.

use super::{CiStatus, ConflictStatus, MergeReadiness, ReviewStatus, ThreadStatus, UpstreamStatus};

/// Each axis reduced to a 3-way verdict before combining — keeps
/// `merge_readiness` a single small exhaustive match instead of a spelled-out
/// cross-product of the raw axis enums, which grows multiplicatively with
/// every axis added (CI × review × threads × conflict × upstream today).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AxisVerdict {
    Unknown,
    Blocking,
    Clear,
}

fn ci_verdict(s: &CiStatus) -> AxisVerdict {
    match s {
        CiStatus::Unknown { .. } => AxisVerdict::Unknown,
        CiStatus::NotConfigured | CiStatus::Passing => AxisVerdict::Clear,
        CiStatus::Pending | CiStatus::Failing => AxisVerdict::Blocking,
    }
}

fn review_verdict(s: &ReviewStatus) -> AxisVerdict {
    match s {
        ReviewStatus::Unknown { .. } => AxisVerdict::Unknown,
        ReviewStatus::Approved => AxisVerdict::Clear,
        ReviewStatus::ChangesRequested | ReviewStatus::AwaitingApproval => AxisVerdict::Blocking,
    }
}

/// Thread resolution is its OWN axis, not a modifier on the review one:
/// GitHub reports `reviewDecision: APPROVED` and open `reviewThreads`
/// independently, so an approved PR with unresolved threads must come out
/// `Blocked` — which is exactly this repo's own CLAUDE.md bar ("unresolved
/// review threads are handled" is listed ALONGSIDE approval, not folded into
/// it), and the case the previous `AwaitingApproval { unresolved_discussions }`
/// shape could not represent at all.
///
/// `Unchecked` is `Clear` for the same reason `CiStatus::NotConfigured` is:
/// a backend that does not check threads must not make every PR read
/// `Indeterminate` in the Needs-you surface. That leniency is safe HERE and
/// deliberately not repeated in `gate`, which demands a positive
/// `AllResolved` before an unattended merge — the same split `ci_verdict`
/// already has with `gate`'s stricter `CiNotPassing`.
fn thread_verdict(s: &ThreadStatus) -> AxisVerdict {
    match s {
        ThreadStatus::Unknown { .. } => AxisVerdict::Unknown,
        ThreadStatus::Unchecked | ThreadStatus::AllResolved => AxisVerdict::Clear,
        ThreadStatus::Unresolved { .. } => AxisVerdict::Blocking,
    }
}

fn upstream_verdict(s: &UpstreamStatus) -> AxisVerdict {
    match s {
        UpstreamStatus::Unknown { .. } => AxisVerdict::Unknown,
        UpstreamStatus::None | UpstreamStatus::Merged => AxisVerdict::Clear,
        UpstreamStatus::Pending { .. } => AxisVerdict::Blocking,
    }
}

fn upstream_reason(s: &UpstreamStatus) -> Option<String> {
    match s {
        UpstreamStatus::Unknown { reason } => Some(format!("上游任务状态未知({reason})")),
        UpstreamStatus::Pending { what } => Some(format!("上游任务「{what}」还没合并")),
        UpstreamStatus::None | UpstreamStatus::Merged => None,
    }
}

fn conflict_verdict(s: &ConflictStatus) -> AxisVerdict {
    match s {
        ConflictStatus::Unknown { .. } => AxisVerdict::Unknown,
        ConflictStatus::Clean => AxisVerdict::Clear,
        ConflictStatus::Conflicting => AxisVerdict::Blocking,
    }
}

/// Chinese, human-facing diagnostics. Dev-English tokens such as CI/review/PR/MR
/// stay as-is. `None` means this axis is clear and contributes no reason.
fn ci_reason(s: &CiStatus) -> Option<String> {
    match s {
        CiStatus::Unknown { reason } => Some(format!("CI 状态未知({reason})")),
        CiStatus::Pending => Some("CI 还在跑".to_string()),
        CiStatus::Failing => Some("CI 未通过".to_string()),
        CiStatus::NotConfigured | CiStatus::Passing => None,
    }
}

fn review_reason(s: &ReviewStatus) -> Option<String> {
    match s {
        ReviewStatus::Unknown { reason } => Some(format!("review 状态未知({reason})")),
        ReviewStatus::ChangesRequested => Some("review 有待处理的修改意见".to_string()),
        ReviewStatus::AwaitingApproval => Some("还没有 review 批准".to_string()),
        ReviewStatus::Approved => None,
    }
}

fn thread_reason(s: &ThreadStatus) -> Option<String> {
    match s {
        ThreadStatus::Unknown { reason } => Some(format!("未解决的 review 线程数未知({reason})")),
        ThreadStatus::Unresolved { count } => Some(format!("还有 {count} 条 review 线程没有解决")),
        ThreadStatus::Unchecked | ThreadStatus::AllResolved => None,
    }
}

fn conflict_reason(s: &ConflictStatus) -> Option<String> {
    match s {
        ConflictStatus::Unknown { reason } => Some(format!("是否有冲突未知({reason})")),
        ConflictStatus::Conflicting => Some("与目标分支有冲突".to_string()),
        ConflictStatus::Clean => None,
    }
}

/// The single source of truth for "truly mergeable": CI × review × threads ×
/// conflict, each reduced to a verdict, combined by ONE rule — any axis
/// `Unknown` wins as `Indeterminate` (we must never claim `Ready` OR
/// `Blocked` when we can't actually tell); otherwise any `Blocking` axis
/// makes it `Blocked`; otherwise `Ready`. `reasons` always lists every
/// non-clear axis (not just the first found), in a fixed CI → review →
/// threads → conflict → upstream order so the same
/// input always renders identical text (no notice-flapping on wording order
/// alone — see `plan_notice_action`).
pub fn merge_readiness(
    ci: &CiStatus,
    review: &ReviewStatus,
    threads: &ThreadStatus,
    conflict: &ConflictStatus,
    upstream: &UpstreamStatus,
) -> MergeReadiness {
    let verdicts = [
        ci_verdict(ci),
        review_verdict(review),
        thread_verdict(threads),
        conflict_verdict(conflict),
        upstream_verdict(upstream),
    ];
    let has_unknown = verdicts.contains(&AxisVerdict::Unknown);
    let has_blocking = verdicts.contains(&AxisVerdict::Blocking);

    let reasons: Vec<String> = [
        ci_reason(ci),
        review_reason(review),
        thread_reason(threads),
        conflict_reason(conflict),
        upstream_reason(upstream),
    ]
    .into_iter()
    .flatten()
    .collect();

    if has_unknown {
        MergeReadiness::Indeterminate { reasons }
    } else if has_blocking {
        MergeReadiness::Blocked { reasons }
    } else {
        MergeReadiness::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> (CiStatus, ReviewStatus, ConflictStatus) {
        (CiStatus::Passing, ReviewStatus::Approved, ConflictStatus::Clean)
    }

    /// Every test that predates the review-thread axis isolates exactly ONE
    /// axis and says nothing about threads; this wrapper feeds them the clear
    /// value so each keeps isolating the axis it was written for. The threads
    /// axis gets its own dedicated tests, which call `merge_readiness`
    /// directly — so this wrapper's own hard-coded argument can never become
    /// the only thing covering it.
    fn readiness_of(
        ci: &CiStatus,
        review: &ReviewStatus,
        conflict: &ConflictStatus,
        upstream: &UpstreamStatus,
    ) -> MergeReadiness {
        merge_readiness(ci, review, &ThreadStatus::AllResolved, conflict, upstream)
    }

    /// The ordering axis, on a change set whose every other axis is perfect.
    /// This is the whole point: a consumer PR can be green, approved,
    /// thread-clear and conflict-free and STILL not be mergeable, because
    /// merging it would land
    /// a commit referencing a producer change that is not on any default
    /// branch yet.
    #[test]
    fn a_pending_upstream_blocks_an_otherwise_perfect_pr() {
        let (ci, review, conflict) = ready();
        let readiness = readiness_of(
            &ci,
            &review,
            &conflict,
            &UpstreamStatus::Pending { what: "chat-kit: expose send".into() },
        );
        match readiness {
            MergeReadiness::Blocked { reasons } => {
                assert_eq!(reasons.len(), 1, "only the ordering axis is unclear");
                assert!(
                    reasons[0].contains("chat-kit: expose send"),
                    "the notice must name WHICH task is holding it: {reasons:?}"
                );
            }
            other => panic!("a pending upstream must block, got {other:?}"),
        }
    }

    /// Once the producer lands, the consumer is free — no residue.
    #[test]
    fn a_merged_upstream_clears_the_axis() {
        let (ci, review, conflict) = ready();
        assert_eq!(
            readiness_of(&ci, &review, &conflict, &UpstreamStatus::Merged),
            MergeReadiness::Ready
        );
    }

    /// An unresolvable upstream must never read as "no upstream". `None` would
    /// release the merge; the honest answer to "can we tell?" is no, and this
    /// axis obeys the same rule as the other three.
    #[test]
    fn an_unknown_upstream_is_indeterminate_not_ready() {
        let (ci, review, conflict) = ready();
        let readiness = readiness_of(
            &ci,
            &review,
            &conflict,
            &UpstreamStatus::Unknown { reason: "上游任务 #7 不存在".into() },
        );
        match readiness {
            MergeReadiness::Indeterminate { reasons } => {
                assert!(reasons[0].contains("#7"), "reason keeps the diagnostic: {reasons:?}");
            }
            other => panic!("an unknown upstream must be indeterminate, got {other:?}"),
        }
    }

    /// Ordering does not outrank the other axes — it joins them. A PR blocked
    /// on BOTH a failing CI and a pending upstream reports both, in the fixed
    /// order the notice depends on.
    #[test]
    fn upstream_joins_the_other_axes_rather_than_overriding_them() {
        let readiness = readiness_of(
            &CiStatus::Failing,
            &ReviewStatus::Approved,
            &ConflictStatus::Clean,
            &UpstreamStatus::Pending { what: "producer".into() },
        );
        match readiness {
            MergeReadiness::Blocked { reasons } => {
                assert_eq!(reasons.len(), 2, "both axes are named: {reasons:?}");
                assert!(reasons[0].contains("CI"), "CI comes first: {reasons:?}");
                assert!(reasons[1].contains("producer"), "upstream comes last: {reasons:?}");
            }
            other => panic!("expected blocked, got {other:?}"),
        }
    }

    /// A task with no ordering edge behaves exactly as before this axis
    /// existed — the overwhelmingly common case must be untouched.
    #[test]
    fn no_upstream_is_indistinguishable_from_the_three_axis_judgement() {
        for (ci, review, conflict) in [
            (CiStatus::Passing, ReviewStatus::Approved, ConflictStatus::Clean),
            (CiStatus::Failing, ReviewStatus::Approved, ConflictStatus::Clean),
            (
                CiStatus::Unknown { reason: "x".into() },
                ReviewStatus::Approved,
                ConflictStatus::Clean,
            ),
        ] {
            let with_none = readiness_of(&ci, &review, &conflict, &UpstreamStatus::None);
            let with_merged = readiness_of(&ci, &review, &conflict, &UpstreamStatus::Merged);
            assert_eq!(
                with_none, with_merged,
                "None and Merged are both Clear, so they must agree"
            );
        }
    }

    #[test]
    fn all_three_axes_clear_is_ready() {
        let (ci, review, conflict) = ready();
        assert_eq!(readiness_of(&ci, &review, &conflict, &UpstreamStatus::None), MergeReadiness::Ready);
    }

    #[test]
    fn ci_not_configured_counts_as_clear_not_blocking() {
        let (_, review, conflict) = ready();
        assert_eq!(
            readiness_of(&CiStatus::NotConfigured, &review, &conflict, &UpstreamStatus::None),
            MergeReadiness::Ready
        );
    }

    #[test]
    fn failing_ci_alone_blocks() {
        let (_, review, conflict) = ready();
        match readiness_of(&CiStatus::Failing, &review, &conflict, &UpstreamStatus::None) {
            MergeReadiness::Blocked { reasons } => {
                assert_eq!(reasons, vec!["CI 未通过".to_string()]);
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn pending_ci_alone_blocks() {
        let (_, review, conflict) = ready();
        assert!(matches!(
            readiness_of(&CiStatus::Pending, &review, &conflict, &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn changes_requested_alone_blocks() {
        let (ci, _, conflict) = ready();
        assert!(matches!(
            readiness_of(&ci, &ReviewStatus::ChangesRequested, &conflict, &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn awaiting_approval_alone_blocks() {
        // Not yet reviewed is NOT the same as approved — issue #110's bar is
        // "review clear/approved", not "nobody objected yet".
        let (ci, _, conflict) = ready();
        assert!(matches!(
            readiness_of(
                &ci,
                &ReviewStatus::AwaitingApproval,
                &conflict
            , &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    // --- the review-thread axis ------------------------------------------

    /// THE case the axis exists for, and the one the previous
    /// `AwaitingApproval { unresolved_discussions }` shape could not even
    /// express: GitHub says APPROVED, every other axis is perfect, and there
    /// are still open review threads. Before this axis, that combination read
    /// `Ready` — and `gate` would have merged it unattended.
    #[test]
    fn unresolved_threads_block_an_approved_otherwise_perfect_pr() {
        let (ci, review, conflict) = ready();
        let readiness = merge_readiness(
            &ci,
            &review,
            &ThreadStatus::Unresolved { count: 3 },
            &conflict,
            &UpstreamStatus::None,
        );
        match readiness {
            MergeReadiness::Blocked { reasons } => {
                assert_eq!(reasons.len(), 1, "only the threads axis is unclear: {reasons:?}");
                assert!(
                    reasons[0].contains('3'),
                    "the notice must say HOW MANY are open: {reasons:?}"
                );
            }
            other => panic!("unresolved threads must block an approved PR, got {other:?}"),
        }
    }

    /// A count that could not be established is `Indeterminate`, NEVER
    /// `Ready` — the whole reason `ThreadStatus::Unknown` is a distinct
    /// variant from `AllResolved`. A read that silently degraded to zero is
    /// indistinguishable from a genuine all-clear, and errs in the one
    /// direction that ends a review early.
    #[test]
    fn an_unknown_thread_count_is_indeterminate_not_ready() {
        let (ci, review, conflict) = ready();
        let readiness = merge_readiness(
            &ci,
            &review,
            &ThreadStatus::Unknown { reason: "分页在中途断了".into() },
            &conflict,
            &UpstreamStatus::None,
        );
        match readiness {
            MergeReadiness::Indeterminate { reasons } => {
                assert!(reasons[0].contains("分页在中途断了"), "reason kept: {reasons:?}");
            }
            other => panic!("an unknown thread count must be indeterminate, got {other:?}"),
        }
    }

    /// `Unchecked` is vacuously clear HERE — a backend that doesn't read
    /// threads must not make every PR read `Indeterminate` in Needs-you. The
    /// auto-merge gate deliberately does NOT inherit this leniency; see
    /// `gate::AutoMergeSkipReason::ThreadsNotAllResolved` and its own test.
    #[test]
    fn unchecked_threads_are_clear_here_even_though_the_gate_refuses_them() {
        let (ci, review, conflict) = ready();
        for threads in [ThreadStatus::Unchecked, ThreadStatus::AllResolved] {
            assert_eq!(
                merge_readiness(&ci, &review, &threads, &conflict, &UpstreamStatus::None),
                MergeReadiness::Ready,
                "{threads:?} must be clear for the notice verdict"
            );
        }
    }

    /// Threads join the other axes rather than outranking them, and land in
    /// the documented CI → review → threads → conflict → upstream slot.
    #[test]
    fn thread_reasons_sit_between_review_and_conflict() {
        let readiness = merge_readiness(
            &CiStatus::Failing,
            &ReviewStatus::ChangesRequested,
            &ThreadStatus::Unresolved { count: 2 },
            &ConflictStatus::Conflicting,
            &UpstreamStatus::Pending { what: "producer".into() },
        );
        match readiness {
            MergeReadiness::Blocked { reasons } => {
                assert_eq!(reasons.len(), 5, "every axis is named: {reasons:?}");
                assert!(reasons[1].contains("review"), "review is 2nd: {reasons:?}");
                assert!(reasons[2].contains("线程"), "threads are 3rd: {reasons:?}");
                assert!(reasons[3].contains("冲突"), "conflict is 4th: {reasons:?}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_alone_blocks() {
        let (ci, review, _) = ready();
        assert!(matches!(
            readiness_of(&ci, &review, &ConflictStatus::Conflicting, &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn any_unknown_axis_is_indeterminate_even_if_others_are_blocking() {
        // Unknown must win over Blocked — we cannot claim "blocked" when part
        // of the picture is actually just unreadable.
        let readiness = readiness_of(
            &CiStatus::Unknown { reason: "gh not authenticated".to_string() },
            &ReviewStatus::ChangesRequested,
            &ConflictStatus::Conflicting,
            &UpstreamStatus::None,
        );
        match readiness {
            MergeReadiness::Indeterminate { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("gh not authenticated")));
                assert_eq!(reasons.len(), 3, "every non-clear axis is listed, not just the Unknown one");
            }
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }

    #[test]
    fn reasons_are_always_in_ci_review_conflict_order() {
        let readiness = readiness_of(
            &CiStatus::Failing,
            &ReviewStatus::ChangesRequested,
            &ConflictStatus::Conflicting,
            &UpstreamStatus::None,
        );
        match readiness {
            MergeReadiness::Blocked { reasons } => {
                assert_eq!(
                    reasons,
                    vec![
                        "CI 未通过".to_string(),
                        "review 有待处理的修改意见".to_string(),
                        "与目标分支有冲突".to_string(),
                    ]
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

}
