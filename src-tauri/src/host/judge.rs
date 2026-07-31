//! The `truly mergeable` judgement (issue #110 T2): this repo's CLAUDE.md
//! "GitHub Remote Review Workflow" bar — CI green × review clear/approved ×
//! no conflict — turned into a pure, exhaustively-matched function instead of
//! prose a lead has to remember to follow. [`merge_readiness`] is THE
//! judgement; everything else here builds the human-facing Needs-you notice
//! from its result.
//!
//! A FOURTH axis joins those three for cross-repo change sets: an upstream
//! task's PR must be merged before this one is mergeable at all. It is an axis
//! rather than a separate gate on purpose — [`merge_readiness`] is the single
//! source of truth every consumer already reads (monitor notice, auto-merge
//! gate, the UI), so ordering flows to all of them by construction instead of
//! being re-derived per caller.
//!
//! This file is the primary target of this PR's mutation self-check (see the
//! PR body): flip any arm of [`ci_verdict`] / [`review_verdict`] /
//! [`conflict_verdict`] / [`upstream_verdict`] / the `has_unknown` /
//! `has_blocking` branches in [`merge_readiness`], and a test below must go
//! red.

use super::{
    CiStatus, ConflictStatus, HostError, HostKind, MergeReadiness, ReviewStatus, UpstreamStatus,
};

/// Each axis reduced to a 3-way verdict before combining — keeps
/// `merge_readiness` a single small exhaustive match instead of a spelled-out
/// 4×4×3 cross-product of the three raw enums.
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
        // Not approved is blocking regardless of the (separate)
        // unresolved-discussions sub-signal — approval itself is the bar;
        // `unresolved_discussions` only adds detail to the REASON text below.
        ReviewStatus::ChangesRequested | ReviewStatus::AwaitingApproval { .. } => AxisVerdict::Blocking,
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

/// Chinese, human-facing (this codebase's established convention for
/// backend-composed Needs-you notice text — see `lead_chat::revive::
/// stopped_worker_notice_text` / `lead_chat::engine::stall_notice_text`;
/// dev-English tokens like CI/review/PR/MR stay as-is per that same
/// convention). `None` = this axis is clear, contributes no reason.
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
        // `unresolved_discussions` is a SEPARATE signal from approval (see
        // this type's doc on why GitLab needs them kept apart) — surfaced in
        // the reason text only when a backend actually checked it (`Some`);
        // `None` (this MVP's GitHub mapping never walks reviewThreads) says
        // nothing about discussions one way or the other.
        ReviewStatus::AwaitingApproval { unresolved_discussions: Some(true) } => {
            Some("还没有 review 批准,且有未解决的讨论".to_string())
        }
        ReviewStatus::AwaitingApproval { .. } => Some("还没有 review 批准".to_string()),
        ReviewStatus::Approved => None,
    }
}

fn conflict_reason(s: &ConflictStatus) -> Option<String> {
    match s {
        ConflictStatus::Unknown { reason } => Some(format!("是否有冲突未知({reason})")),
        ConflictStatus::Conflicting => Some("与目标分支有冲突".to_string()),
        ConflictStatus::Clean => None,
    }
}

/// The single source of truth for "truly mergeable": CI × review × conflict,
/// each reduced to a verdict, combined by ONE rule — any axis `Unknown` wins
/// as `Indeterminate` (we must never claim `Ready` OR `Blocked` when we can't
/// actually tell); otherwise any `Blocking` axis makes it `Blocked`;
/// otherwise `Ready`. `reasons` always lists every non-clear axis (not just
/// the first found), in a fixed CI → review → conflict → upstream order so
/// the same
/// input always renders identical text (no notice-flapping on wording order
/// alone — see `plan_notice_action`).
pub fn merge_readiness(
    ci: &CiStatus,
    review: &ReviewStatus,
    conflict: &ConflictStatus,
    upstream: &UpstreamStatus,
) -> MergeReadiness {
    let verdicts = [
        ci_verdict(ci),
        review_verdict(review),
        conflict_verdict(conflict),
        upstream_verdict(upstream),
    ];
    let has_unknown = verdicts.contains(&AxisVerdict::Unknown);
    let has_blocking = verdicts.contains(&AxisVerdict::Blocking);

    let reasons: Vec<String> = [
        ci_reason(ci),
        review_reason(review),
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

/// What to do about the Needs-you notice this sweep, given the text that's
/// CURRENTLY posted (if any) vs. the text that SHOULD be posted now (`None` =
/// fully clear). A four-way discriminated result instead of a chain of
/// booleans — CLAUDE.md's "discriminated state, exhaustive map" applied to
/// the monitor's own bookkeeping, not just UI code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    NoOp,
    Post,
    Replace,
    Retract,
}

pub fn plan_notice_action(existing: Option<&str>, desired: Option<&str>) -> NoticeAction {
    match (existing, desired) {
        (None, None) => NoticeAction::NoOp,
        (None, Some(_)) => NoticeAction::Post,
        (Some(_), None) => NoticeAction::Retract,
        (Some(e), Some(d)) if e == d => NoticeAction::NoOp,
        (Some(_), Some(_)) => NoticeAction::Replace,
    }
}

/// The Needs-you notice text for a readiness verdict, or `None` when nothing
/// needs the human's attention. Uses the host's OWN vocabulary
/// (`native_abbrev`) even though `MergeReadiness` itself stayed neutral —
/// issue #110's UI-terminology requirement, applied to the one surface this
/// MVP renders through (the existing self-clearing Needs-you notice; see
/// `host::monitor`).
pub fn notice_text(kind: HostKind, number: i32, readiness: &MergeReadiness) -> Option<String> {
    let abbrev = kind.native_abbrev();
    match readiness {
        MergeReadiness::Ready => None,
        MergeReadiness::Blocked { reasons } => Some(format!(
            "🔀 {abbrev} #{number} 还没到可合并的状态:{}。",
            reasons.join("；")
        )),
        MergeReadiness::Indeterminate { reasons } => Some(format!(
            "🔀 {abbrev} #{number} 暂时无法判定是否达标:{}。",
            reasons.join("；")
        )),
    }
}

/// The Needs-you notice text for a FAILED probe attempt (couldn't even reach
/// the host) — distinct wording from `notice_text`'s `Indeterminate` case
/// (which is about a specific axis being unknown while others were readable)
/// so a human reading it never confuses "we tried and one signal was murky"
/// with "we couldn't check anything at all this sweep".
pub fn probe_error_text(kind: HostKind, number: i32, error: &HostError) -> String {
    format!(
        "🔌 无法查询 {} #{number} 的状态:{}。",
        kind.native_abbrev(),
        error.message()
    )
}

/// The Needs-you notice text for the ONE sweep where a row crosses the
/// monitor's give-up threshold (`host::monitor::MAX_CONSECUTIVE_PROBE_
/// FAILURES`) — after this, `list_open_pull_requests` stops sweeping the
/// row, so this exact text is what stays posted indefinitely. Deliberately
/// distinct from [`probe_error_text`] (an ordinary, still-being-retried
/// failure): without this, a human has no way to tell "still checking, will
/// retry" from "gave up ~10 minutes ago and will never check again" apart —
/// both would otherwise read as byte-identical text. Names the ONLY way this
/// row's tracking resumes (a fresh `register_pr` call resets the streak —
/// see `repo::register_pull_request`'s update branch) and says plainly that
/// THIS notice will not clear itself. The generic "clears itself
/// automatically" Needs-you footer that would otherwise sit right below it is
/// no longer a frontend-side contradiction: this is the one notice `host::
/// monitor` posts via `BusRegistry::notify_human_action_required` rather than
/// `notify_human`, which the frontend renders with a DIFFERENT footer (see
/// `NeedsRows.tsx` / `bus::AskKind::NoticeActionRequired`) — but the text
/// still says so plainly on its own, since the notice must stand on its own
/// even if a future surface (IM, a notification) never renders that footer at
/// all. Both mentions of the host below use the SAME short form
/// (`native_abbrev`, "PR"/"MR") — this file's other notices never mix it with
/// `native_noun`'s long form ("Pull request"/"Merge request") mid-sentence,
/// and this one previously did (P3, PR #150 review).
pub fn give_up_text(kind: HostKind, number: i32, error: &HostError) -> String {
    let abbrev = kind.native_abbrev();
    format!(
        "🛑 已停止跟踪 {abbrev} #{number} 的状态:连续多次查询失败,最近一次原因:{}。这条提示不会自动消失——如果这个 {abbrev} 其实还活着,请让 agent 重新调用一次 register_pr 恢复跟踪。",
        error.message()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> (CiStatus, ReviewStatus, ConflictStatus) {
        (CiStatus::Passing, ReviewStatus::Approved, ConflictStatus::Clean)
    }

    /// The ordering axis, on a change set whose other three axes are perfect.
    /// This is the whole point: a consumer PR can be green, approved and
    /// conflict-free and STILL not be mergeable, because merging it would land
    /// a commit referencing a producer change that is not on any default
    /// branch yet.
    #[test]
    fn a_pending_upstream_blocks_an_otherwise_perfect_pr() {
        let (ci, review, conflict) = ready();
        let readiness = merge_readiness(
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
            merge_readiness(&ci, &review, &conflict, &UpstreamStatus::Merged),
            MergeReadiness::Ready
        );
    }

    /// An unresolvable upstream must never read as "no upstream". `None` would
    /// release the merge; the honest answer to "can we tell?" is no, and this
    /// axis obeys the same rule as the other three.
    #[test]
    fn an_unknown_upstream_is_indeterminate_not_ready() {
        let (ci, review, conflict) = ready();
        let readiness = merge_readiness(
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
        let readiness = merge_readiness(
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
            let with_none = merge_readiness(&ci, &review, &conflict, &UpstreamStatus::None);
            let with_merged = merge_readiness(&ci, &review, &conflict, &UpstreamStatus::Merged);
            assert_eq!(
                with_none, with_merged,
                "None and Merged are both Clear, so they must agree"
            );
        }
    }

    #[test]
    fn all_three_axes_clear_is_ready() {
        let (ci, review, conflict) = ready();
        assert_eq!(merge_readiness(&ci, &review, &conflict, &UpstreamStatus::None), MergeReadiness::Ready);
    }

    #[test]
    fn ci_not_configured_counts_as_clear_not_blocking() {
        let (_, review, conflict) = ready();
        assert_eq!(
            merge_readiness(&CiStatus::NotConfigured, &review, &conflict, &UpstreamStatus::None),
            MergeReadiness::Ready
        );
    }

    #[test]
    fn failing_ci_alone_blocks() {
        let (_, review, conflict) = ready();
        match merge_readiness(&CiStatus::Failing, &review, &conflict, &UpstreamStatus::None) {
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
            merge_readiness(&CiStatus::Pending, &review, &conflict, &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn changes_requested_alone_blocks() {
        let (ci, _, conflict) = ready();
        assert!(matches!(
            merge_readiness(&ci, &ReviewStatus::ChangesRequested, &conflict, &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn awaiting_approval_alone_blocks() {
        // Not yet reviewed is NOT the same as approved — issue #110's bar is
        // "review clear/approved", not "nobody objected yet".
        let (ci, _, conflict) = ready();
        assert!(matches!(
            merge_readiness(
                &ci,
                &ReviewStatus::AwaitingApproval { unresolved_discussions: None },
                &conflict
            , &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn awaiting_approval_blocks_regardless_of_the_unresolved_discussions_sub_signal() {
        // The sub-signal is extra REASON detail, not a second axis — both
        // `Some(true)`/`Some(false)`/`None` must still block on their own,
        // since approval itself (not discussion-resolution) is the bar.
        let (ci, _, conflict) = ready();
        for unresolved_discussions in [None, Some(true), Some(false)] {
            assert!(
                matches!(
                    merge_readiness(
                        &ci,
                        &ReviewStatus::AwaitingApproval { unresolved_discussions },
                        &conflict
                    , &UpstreamStatus::None),
                    MergeReadiness::Blocked { .. }
                ),
                "unresolved_discussions={unresolved_discussions:?} must still block"
            );
        }
    }

    #[test]
    fn unresolved_discussions_true_is_named_in_the_reason_when_a_backend_actually_checked() {
        let (ci, _, conflict) = ready();
        let readiness = merge_readiness(
            &ci,
            &ReviewStatus::AwaitingApproval { unresolved_discussions: Some(true) },
            &conflict,
            &UpstreamStatus::None,
        );
        match readiness {
            MergeReadiness::Blocked { reasons } => {
                assert!(reasons[0].contains("未解决的讨论"), "got: {reasons:?}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_alone_blocks() {
        let (ci, review, _) = ready();
        assert!(matches!(
            merge_readiness(&ci, &review, &ConflictStatus::Conflicting, &UpstreamStatus::None),
            MergeReadiness::Blocked { .. }
        ));
    }

    #[test]
    fn any_unknown_axis_is_indeterminate_even_if_others_are_blocking() {
        // Unknown must win over Blocked — we cannot claim "blocked" when part
        // of the picture is actually just unreadable.
        let readiness = merge_readiness(
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
        let readiness = merge_readiness(
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

    #[test]
    fn notice_text_is_none_when_ready() {
        assert_eq!(notice_text(HostKind::GitHub, 1, &MergeReadiness::Ready), None);
    }

    #[test]
    fn notice_text_uses_host_native_abbreviation() {
        let readiness = MergeReadiness::Blocked { reasons: vec!["CI 未通过".to_string()] };
        let gh = notice_text(HostKind::GitHub, 12, &readiness).unwrap();
        assert!(gh.contains("PR #12"), "got: {gh}");
        let gl = notice_text(HostKind::GitLab, 12, &readiness).unwrap();
        assert!(gl.contains("MR #12"), "got: {gl}");
    }

    #[test]
    fn notice_text_distinguishes_blocked_from_indeterminate() {
        let blocked = notice_text(
            HostKind::GitHub,
            1,
            &MergeReadiness::Blocked { reasons: vec!["x".to_string()] },
        )
        .unwrap();
        let indeterminate = notice_text(
            HostKind::GitHub,
            1,
            &MergeReadiness::Indeterminate { reasons: vec!["x".to_string()] },
        )
        .unwrap();
        assert_ne!(blocked, indeterminate, "the two must read differently — honesty requirement");
    }

    #[test]
    fn plan_notice_action_is_exhaustive_over_the_four_cases() {
        assert_eq!(plan_notice_action(None, None), NoticeAction::NoOp);
        assert_eq!(plan_notice_action(None, Some("x")), NoticeAction::Post);
        assert_eq!(plan_notice_action(Some("x"), None), NoticeAction::Retract);
        assert_eq!(plan_notice_action(Some("x"), Some("x")), NoticeAction::NoOp);
        assert_eq!(plan_notice_action(Some("x"), Some("y")), NoticeAction::Replace);
    }

    #[test]
    fn give_up_text_differs_from_an_ordinary_probe_error() {
        // The P1-A honesty requirement: byte-identical text between "still
        // retrying" and "gave up, will never retry again" would leave a
        // human with no way to tell the two apart.
        let error = HostError::NotAuthenticated { program: "gh".to_string() };
        let ordinary = probe_error_text(HostKind::GitHub, 9, &error);
        let gave_up = give_up_text(HostKind::GitHub, 9, &error);
        assert_ne!(ordinary, gave_up);
    }

    #[test]
    fn give_up_text_names_the_recovery_path() {
        let error = HostError::NotFound;
        let text = give_up_text(HostKind::GitHub, 3, &error);
        assert!(text.contains("register_pr"), "must say HOW to recover, got: {text}");
    }

    #[test]
    fn give_up_text_uses_host_native_terminology() {
        let error = HostError::NotFound;
        assert!(give_up_text(HostKind::GitHub, 1, &error).contains("PR #1"));
        assert!(give_up_text(HostKind::GitLab, 1, &error).contains("MR #1"));
    }

    #[test]
    fn give_up_text_never_mixes_the_abbreviation_with_the_long_form_noun() {
        // P3 (PR #150 review): the two host mentions in this ONE sentence must
        // both use `native_abbrev` ("PR"/"MR") — a prior version referred to
        // the SAME PR as "PR #1" and then, two clauses later, "这个 Pull
        // request", which reads as two different things to a Chinese-reading
        // human even though every other notice in this file stays on the
        // abbreviation throughout.
        let error = HostError::NotFound;
        for (kind, abbrev, noun) in [
            (HostKind::GitHub, "PR", "Pull request"),
            (HostKind::GitLab, "MR", "Merge request"),
        ] {
            let text = give_up_text(kind, 1, &error);
            assert!(!text.contains(noun), "must not mix in the long-form noun, got: {text}");
            assert_eq!(
                text.matches(abbrev).count(),
                2,
                "both host mentions must use the abbreviation consistently, got: {text}"
            );
        }
    }

    #[test]
    fn probe_error_text_differs_from_readiness_notice_text() {
        let probe = probe_error_text(
            HostKind::GitHub,
            5,
            &HostError::NotAuthenticated { program: "gh".to_string() },
        );
        let readiness = notice_text(
            HostKind::GitHub,
            5,
            &MergeReadiness::Indeterminate { reasons: vec!["CI 状态未知(gh not logged in)".to_string()] },
        )
        .unwrap();
        assert_ne!(probe, readiness, "a probe failure and an axis-unknown judgement must read differently");
    }
}
