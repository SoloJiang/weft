//! Delivery readiness is derived, never stored. This module is the one place
//! that turns a direction's policy, local execution evidence, verification
//! result, and tracked PR snapshot into an issue-level delivery verdict.
//!
//! ## State table
//!
//! Active lanes are candidates from the stored plan plus persisted directions
//! that are neither `inactive` nor `cancelled` and whose policy is not
//! [`PolicyDecision::Denied`]. A denied or inactive lane produces no lane
//! verdict and is not part of issue aggregation.
//!
//! ## Active lane sources
//!
//! Collection enumerates the union of persisted directions and the stored
//! [`crate::planner::Proposal`] lanes, then applies these source rules before
//! the state table below:
//!
//! | Stored plan / proposal lane | Collected lane |
//! | --- | --- |
//! | no plan, or `withdrawn` plan | every persisted direction is a legacy `AllowedByPolicy` lane; proposal lanes are ignored |
//! | parseable non-empty `proposed` or `confirmed` proposal, `decision == "denied"` | omitted |
//! | parseable proposal, pending (`decision == ""`) lane with `direction_id == 0` | virtual `NeedsGate` lane, so `NeedsYou[PolicyGatePending]` |
//! | parseable proposal, approved lane with `direction_id == 0` | virtual `AllowedByPolicy` lane with no execution evidence, so `Unknown[InProgress]` |
//! | parseable proposal, unsupported decision with `direction_id == 0` | virtual `NeedsGate` lane, so `NeedsYou[PolicyGatePending]` |
//! | parseable proposal, `decision == ""` or `"approved"` with `direction_id != 0` | the materialized direction with `AllowedByPolicy` |
//! | parseable proposal, unsupported decision with `direction_id != 0` | the materialized direction with `NeedsGate`; it remains visible rather than being denied |
//! | persisted direction not referenced by any proposal lane | legacy `AllowedByPolicy` lane |
//! | present plan with an invalid, empty, or unsupported proposal | every persisted direction is conservatively `NeedsGate` |
//!
//! A proposal decision is valid only when it is exactly `""`, `"approved"`,
//! or `"denied"`. For duplicate materialized references, `denied` excludes
//! the direction; otherwise `NeedsGate` wins over `AllowedByPolicy` so an
//! unsupported duplicate decision cannot be hidden by an earlier valid one.
//!
//! A missing materialized direction referenced by a valid proposal remains a
//! virtual fail-closed lane with unknown reconciliation. This protects against
//! a stale stored proposal without inventing a new persistence contract.
//! Conflicting duplicate proposal references are likewise fail-closed: a
//! `denied` reference excludes that persisted direction.
//!
//! Tracked PR rows with `direction_id == 0` are issue-level, unbound PR facts.
//! When their same deterministic PR reduction is non-clear, collection adds a
//! virtual `unbound PR` lane to issue aggregation. Its reason carries no
//! `direction_id`; an all-clear unbound PR adds no lane and cannot make an
//! issue ready on its own.
//!
//! | Lane facts, evaluated in this exact first-match order | Lane readiness | Reason |
//! | --- | --- | --- |
//! | inactive/cancelled, or policy denied | omitted | — |
//! | most recent worker turn ended `error` | Failed | WorkerFailed |
//! | answerable bus ask is open for the direction | NeedsYou | OpenNeed |
//! | policy needs a human gate | NeedsYou | PolicyGatePending |
//! | worktree/branch reconciliation drifted | Blocked | ExecutionDrifted |
//! | an inferred check failed for a claimed-complete lane | Blocked | ChecksFailing |
//! | upstream evidence is Unmet (including a pending or unregistered upstream PR) | Blocked | UpstreamUnmet |
//! | upstream evidence is Unknown | Unknown | RemoteUnknown |
//! | tracked PR lifecycle is `merged` | continue; terminal merge evidence clears every PR axis | — |
//! | tracked PR probe failed or lifecycle is unknown | Unknown | RemoteUnknown |
//! | tracked open PR has no valid successful timestamp, its snapshot is older than its TTL, or PR sweeping is disabled | Unknown | RemoteUnknown |
//! | tracked PR is closed without merge | Blocked | PrClosedUnmerged |
//! | tracked PR CI failed | Blocked | PrCiFailing |
//! | tracked PR CI is pending | Unknown | PrCiPending |
//! | tracked PR CI is unknown | Unknown | RemoteUnknown |
//! | tracked PR requests changes | Blocked | PrReviewChangesRequested |
//! | tracked PR review is unknown | Unknown | RemoteUnknown |
//! | tracked PR has unresolved review threads | Blocked | PrThreadsUnresolved |
//! | tracked PR review threads are unknown | Unknown | RemoteUnknown |
//! | tracked PR has conflicts | Blocked | PrConflict |
//! | tracked PR conflict state is unknown | Unknown | RemoteUnknown |
//! | tracked PR has only passing/non-configured CI, approved/awaiting review, clear/unchecked threads, and clean conflict state | continue | — |
//! | local reconciliation is unknown | Unknown | RemoteUnknown |
//! | checks were never produced for a claimed-complete lane | Unknown | ChecksUnknown |
//! | direction status is `review` or `done` | ReviewReady | — |
//! | direction status is `queued`, `planning`, or `working` | Unknown | InProgress |
//!
//! A CLI-reported `review`/`done` state is intentionally last: it cannot
//! override drift, remote uncertainty, or missing checks. A lane without a PR
//! skips the PR rows entirely; a PR is not a prerequisite for single-repo
//! review readiness.
//!
//! Check collection is intentionally gated by claimed completion: only
//! `review` and `done` lanes invoke inferred checks. The collector records
//! `NotApplicable` for `queued`, `planning`, and `working` lanes; that evidence
//! skips both check-failing and check-unknown verdict rows. A reconciliation
//! drift also records `NotApplicable` and never starts checks: its
//! `ExecutionDrifted` verdict has already won, and a mismatched checkout is not
//! a safe target for build/test work. This retains check.rs's
//! worker-done-means-checks-green contract without turning ordinary in-progress
//! work into automatic build/test execution.
//!
//! Readiness checks reuse `check::infer_checks`, but run each inferred rung in
//! a kill-on-drop Tokio child with a 120-second per-rung deadline. A timeout or
//! child-wait failure is `NotProduced`, so the result remains fail-closed while
//! the direction lock and global runner permit are released. Completed output
//! uses the same combined-output, 2000-byte tail convention as `CheckResult`.
//! The process-local single-flight cache is valid for at most 10 minutes and
//! only while its sorted worktree-path signature and every worktree HEAD SHA
//! match the newly collected values; any HEAD change immediately reruns checks.
//!
//! Upstream evidence is collected from the established
//! `repo::upstream_merge_state` contract:
//!
//! | UpstreamStatus | Upstream evidence | Verdict effect |
//! | --- | --- | --- |
//! | None or Merged | Satisfied | continue |
//! | Pending, including no registered upstream PR | Unmet | Blocked[UpstreamUnmet] |
//! | Unknown (dangling, unresolved, or unreadable edge) | Unknown | Unknown[RemoteUnknown] |
//!
//! A lane can have multiple tracked PR rows. Every row must be clear for the
//! PR gate to continue. Otherwise their row verdicts are reduced
//! deterministically: Blocked outranks Unknown, and a same-severity tie uses
//! the smaller persisted PR id.
//!
//! Open PR snapshots are live evidence, not durable completion facts. Their
//! freshness TTL is `3 * WEFT_PR_SWEEP_SECS`: the environment is read with the
//! same parser and default (`60` seconds) as `host::monitor`, so the default
//! TTL is `180` seconds. A `0` sweep disables host polling; open snapshots are
//! then always `Unknown[RemoteUnknown]`, because no process can keep them
//! fresh. Empty or invalid `last_checked_at` is likewise unknown. `merged` and
//! `closed` lifecycle facts are terminal and intentionally exempt from this
//! TTL gate.
//!
//! Reconciliation has the following collector truth table:
//!
//! | Direction status and registered worktree rows | Reconciliation |
//! | --- | --- |
//! | `queued` or `planning`, zero rows | Matched (execution has not begun) |
//! | `working`, `review`, or `done`, zero rows | Unknown |
//! | one or more rows, every readable branch equals direction.branch | Matched |
//! | one or more rows, any readable branch differs | Drifted |
//! | one or more rows, an unreadable path or branch and no drift | Unknown |
//!
//! Issue aggregation has no majority vote. Zero active lanes is
//! `Unknown[NoActiveLanes]`. Otherwise all active lanes must be `ReviewReady`.
//! If not, the greatest severity wins (`Failed > NeedsYou > Blocked > Unknown`)
//! and the aggregate reasons are the deduplicated reasons of lanes at that
//! winning severity, retaining each direction id.
//!
//! `EvidenceMissing` is reserved for the client-visible pre-collection state;
//! backend absence is reported by its specific fail-closed code
//! (`RemoteUnknown` or `ChecksUnknown`) rather than by a lossy generic flag.

use crate::bus::BusRegistry;
use crate::host::{
    CiStatus, ConflictStatus, PrLifecycle, ReviewStatus, ThreadStatus, UpstreamStatus,
};
use crate::store::{
    entities::{direction, lead_message, plan, pull_request},
    repo, Db,
};
use anyhow::{anyhow, Result};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

/// Whether a direction is admitted to delivery work by the settled policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    AllowedByPolicy,
    NeedsGate,
    Denied,
}

/// Whether the recorded worktree agrees with the direction's expected branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionReconciliation {
    Matched,
    Drifted,
    Unknown,
}

/// A lane's delivery readiness. It is deliberately separate from a direction's
/// agent-managed lifecycle status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneReadiness {
    ReviewReady,
    Blocked,
    NeedsYou,
    Unknown,
    Failed,
}

/// An issue's aggregate delivery readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueReadiness {
    ReviewReady,
    Blocked,
    NeedsYou,
    Unknown,
    Failed,
}

/// A stable, machine-readable explanation for every non-ready verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    NoActiveLanes,
    UpstreamUnmet,
    EvidenceMissing,
    RemoteUnknown,
    ExecutionDrifted,
    PolicyGatePending,
    OpenNeed,
    ChecksFailing,
    ChecksUnknown,
    WorkerFailed,
    InProgress,
    PrCiPending,
    PrCiFailing,
    PrReviewChangesRequested,
    PrThreadsUnresolved,
    PrConflict,
    PrClosedUnmerged,
}

/// A readiness reason plus the lane that contributed it. `None` denotes an
/// issue-wide fact such as `NoActiveLanes` or a virtual contributor not tied
/// to a persisted direction, such as an unbound PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessReason {
    pub code: ReasonCode,
    pub direction_id: Option<i32>,
}

/// The verification evidence available for one direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckEvidence {
    /// Checks are intentionally not run until a lane claims completion.
    NotApplicable,
    NotProduced,
    Passed,
    Failing,
}

/// The durable single-predecessor result normalized for lane readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamEvidence {
    Satisfied,
    Unmet,
    Unknown,
}

/// The collection-time policy for the freshness of an open PR snapshot.
/// Keeping it in lane facts preserves a pure readiness decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenPrSnapshotFreshness {
    Disabled,
    MaxAge { now_secs: i64, max_age_secs: i64 },
}

impl OpenPrSnapshotFreshness {
    fn allows(self, last_checked_at: Option<i64>) -> bool {
        match self {
            Self::Disabled => false,
            Self::MaxAge {
                now_secs,
                max_age_secs,
            } => {
                let Some(last_checked_at) = last_checked_at else {
                    return false;
                };
                if last_checked_at > now_secs {
                    return false;
                }
                now_secs.saturating_sub(last_checked_at) <= max_age_secs
            }
        }
    }
}

/// The PR facts consumed by the pure readiness function. `probe_failed` is
/// true whenever the latest host probe failed; stored axes from a previous
/// success are not treated as fresh evidence after that failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestFacts {
    /// Persisted primary key, used only to make multi-PR ties deterministic.
    pub id: i32,
    pub lifecycle: Option<PrLifecycle>,
    pub ci: CiStatus,
    pub review: ReviewStatus,
    pub threads: ThreadStatus,
    pub conflict: ConflictStatus,
    pub probe_failed: bool,
    /// Parsed unix-seconds probe timestamp. Empty or invalid stored text is
    /// deliberately `None` so an open row cannot become fresh by accident.
    pub last_checked_at: Option<i64>,
}

/// All non-I/O facts required to decide one direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneFacts {
    pub direction_id: i32,
    pub name: String,
    pub active: bool,
    pub policy: PolicyDecision,
    pub worker_failed: bool,
    pub has_open_ask: bool,
    pub reconciliation: ExecutionReconciliation,
    pub checks: CheckEvidence,
    pub upstream: UpstreamEvidence,
    pub open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
    pub pull_requests: Vec<PullRequestFacts>,
    pub direction_status: String,
}

/// The DTO for one included lane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneReadinessDto {
    pub direction_id: i32,
    pub name: String,
    pub readiness: LaneReadiness,
    pub reasons: Vec<ReadinessReason>,
}

/// Backend DTO consumed by both board surfaces and the global bus tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueReadinessDto {
    pub readiness: IssueReadiness,
    pub reasons: Vec<ReadinessReason>,
    pub active_lane_count: usize,
    pub lanes: Vec<LaneReadinessDto>,
}

fn lane_verdict(
    facts: &LaneFacts,
    readiness: LaneReadiness,
    reason: Option<ReasonCode>,
) -> LaneReadinessDto {
    let reasons = match reason {
        Some(code) => vec![ReadinessReason {
            code,
            direction_id: (facts.direction_id != 0).then_some(facts.direction_id),
        }],
        None => Vec::new(),
    };
    LaneReadinessDto {
        direction_id: facts.direction_id,
        name: facts.name.clone(),
        readiness,
        reasons,
    }
}

fn one_pull_request_verdict(
    pr: &PullRequestFacts,
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
) -> Option<(LaneReadiness, ReasonCode)> {
    // A merged lifecycle is terminal, stronger evidence than every live axis.
    // GitHub's real merged fixture has `mergeable: "UNKNOWN"` and an empty
    // review decision after GitHub stops computing them, so applying the open
    // PR axes here would turn a completed delivery back into RemoteUnknown.
    if pr.lifecycle == Some(PrLifecycle::Merged) {
        return None;
    }

    // A failed probe means any prior snapshot is stale. This includes the
    // first-probe case (error + all four stored axes empty) and later
    // failures, which must not silently reuse an old all-clear.
    if pr.probe_failed {
        return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
    }

    let Some(lifecycle) = pr.lifecycle else {
        return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
    };
    if lifecycle == PrLifecycle::Open && !open_pr_snapshot_freshness.allows(pr.last_checked_at) {
        return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
    }
    if lifecycle == PrLifecycle::Closed {
        return Some((LaneReadiness::Blocked, ReasonCode::PrClosedUnmerged));
    }

    match &pr.ci {
        CiStatus::Failing => return Some((LaneReadiness::Blocked, ReasonCode::PrCiFailing)),
        CiStatus::Pending => return Some((LaneReadiness::Unknown, ReasonCode::PrCiPending)),
        CiStatus::Unknown { .. } => {
            return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
        }
        CiStatus::NotConfigured | CiStatus::Passing => {}
    }

    match &pr.review {
        ReviewStatus::ChangesRequested => {
            return Some((LaneReadiness::Blocked, ReasonCode::PrReviewChangesRequested));
        }
        ReviewStatus::Unknown { .. } => {
            return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
        }
        ReviewStatus::Approved | ReviewStatus::AwaitingApproval => {}
    }

    match &pr.threads {
        ThreadStatus::Unresolved { .. } => {
            return Some((LaneReadiness::Blocked, ReasonCode::PrThreadsUnresolved));
        }
        ThreadStatus::Unknown { .. } => {
            return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
        }
        ThreadStatus::Unchecked | ThreadStatus::AllResolved => {}
    }

    match &pr.conflict {
        ConflictStatus::Conflicting => {
            return Some((LaneReadiness::Blocked, ReasonCode::PrConflict));
        }
        ConflictStatus::Unknown { .. } => {
            return Some((LaneReadiness::Unknown, ReasonCode::RemoteUnknown));
        }
        ConflictStatus::Clean => {}
    }

    None
}

fn pull_request_verdict(
    pull_requests: &[PullRequestFacts],
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
) -> Option<(LaneReadiness, ReasonCode)> {
    let mut selected: Option<(i32, LaneReadiness, ReasonCode)> = None;
    for pr in pull_requests {
        let Some((readiness, reason)) = one_pull_request_verdict(pr, open_pr_snapshot_freshness)
        else {
            continue;
        };
        let should_select = match selected {
            Some((selected_id, selected_readiness, _)) => {
                let candidate_priority = readiness_priority(readiness);
                let selected_priority = readiness_priority(selected_readiness);
                candidate_priority > selected_priority
                    || (candidate_priority == selected_priority && pr.id < selected_id)
            }
            None => true,
        };
        if should_select {
            selected = Some((pr.id, readiness, reason));
        }
    }
    selected.map(|(_, readiness, reason)| (readiness, reason))
}

/// Decide one active lane. A policy-denied or inactive lane returns `None` so
/// aggregation cannot accidentally make a cancelled/denied lane block or
/// satisfy an issue.
pub fn lane_readiness(facts: &LaneFacts) -> Option<LaneReadinessDto> {
    if !facts.active || facts.policy == PolicyDecision::Denied {
        return None;
    }
    if facts.worker_failed {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Failed,
            Some(ReasonCode::WorkerFailed),
        ));
    }
    if facts.has_open_ask {
        return Some(lane_verdict(
            facts,
            LaneReadiness::NeedsYou,
            Some(ReasonCode::OpenNeed),
        ));
    }
    if facts.policy == PolicyDecision::NeedsGate {
        return Some(lane_verdict(
            facts,
            LaneReadiness::NeedsYou,
            Some(ReasonCode::PolicyGatePending),
        ));
    }
    if facts.reconciliation == ExecutionReconciliation::Drifted {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Blocked,
            Some(ReasonCode::ExecutionDrifted),
        ));
    }
    // NotApplicable is emitted for lanes that have not claimed completion;
    // it intentionally cannot create a failing-check verdict.
    if facts.checks == CheckEvidence::Failing {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Blocked,
            Some(ReasonCode::ChecksFailing),
        ));
    }
    match facts.upstream {
        UpstreamEvidence::Unmet => {
            return Some(lane_verdict(
                facts,
                LaneReadiness::Blocked,
                Some(ReasonCode::UpstreamUnmet),
            ));
        }
        UpstreamEvidence::Unknown => {
            return Some(lane_verdict(
                facts,
                LaneReadiness::Unknown,
                Some(ReasonCode::RemoteUnknown),
            ));
        }
        UpstreamEvidence::Satisfied => {}
    }
    if let Some((readiness, reason)) =
        pull_request_verdict(&facts.pull_requests, facts.open_pr_snapshot_freshness)
    {
        return Some(lane_verdict(facts, readiness, Some(reason)));
    }
    if facts.reconciliation == ExecutionReconciliation::Unknown {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Unknown,
            Some(ReasonCode::RemoteUnknown),
        ));
    }
    // NotApplicable is not missing evidence: only an applicable run that
    // produced no result is ChecksUnknown.
    if facts.checks == CheckEvidence::NotProduced {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Unknown,
            Some(ReasonCode::ChecksUnknown),
        ));
    }
    match facts.direction_status.as_str() {
        "review" | "done" => Some(lane_verdict(facts, LaneReadiness::ReviewReady, None)),
        _ => Some(lane_verdict(
            facts,
            LaneReadiness::Unknown,
            Some(ReasonCode::InProgress),
        )),
    }
}

fn readiness_priority(readiness: LaneReadiness) -> u8 {
    match readiness {
        LaneReadiness::ReviewReady => 0,
        LaneReadiness::Unknown => 1,
        LaneReadiness::Blocked => 2,
        LaneReadiness::NeedsYou => 3,
        LaneReadiness::Failed => 4,
    }
}

fn issue_readiness_of(lane: LaneReadiness) -> IssueReadiness {
    match lane {
        LaneReadiness::ReviewReady => IssueReadiness::ReviewReady,
        LaneReadiness::Blocked => IssueReadiness::Blocked,
        LaneReadiness::NeedsYou => IssueReadiness::NeedsYou,
        LaneReadiness::Unknown => IssueReadiness::Unknown,
        LaneReadiness::Failed => IssueReadiness::Failed,
    }
}

fn push_unique_reason(out: &mut Vec<ReadinessReason>, reason: ReadinessReason) {
    if !out.iter().any(|existing| existing == &reason) {
        out.push(reason);
    }
}

/// Aggregate only active lane verdicts into one issue verdict.
pub fn issue_readiness(lane_facts: &[LaneFacts]) -> IssueReadinessDto {
    let lanes: Vec<LaneReadinessDto> = lane_facts.iter().filter_map(lane_readiness).collect();
    if lanes.is_empty() {
        return IssueReadinessDto {
            readiness: IssueReadiness::Unknown,
            reasons: vec![ReadinessReason {
                code: ReasonCode::NoActiveLanes,
                direction_id: None,
            }],
            active_lane_count: 0,
            lanes,
        };
    }

    let mut selected: Option<LaneReadiness> = None;
    for lane in &lanes {
        if lane.readiness == LaneReadiness::ReviewReady {
            continue;
        }
        let should_select = match selected {
            Some(current) => readiness_priority(lane.readiness) > readiness_priority(current),
            None => true,
        };
        if should_select {
            selected = Some(lane.readiness);
        }
    }

    let Some(selected) = selected else {
        return IssueReadinessDto {
            readiness: IssueReadiness::ReviewReady,
            reasons: Vec::new(),
            active_lane_count: lanes.len(),
            lanes,
        };
    };

    let mut reasons = Vec::new();
    for lane in &lanes {
        if lane.readiness != selected {
            continue;
        }
        for reason in &lane.reasons {
            push_unique_reason(&mut reasons, reason.clone());
        }
    }
    IssueReadinessDto {
        readiness: issue_readiness_of(selected),
        reasons,
        active_lane_count: lanes.len(),
        lanes,
    }
}

enum PlannedLaneSource {
    /// No live plan policy applies: persisted directions retain the established
    /// single-repo/legacy allowed path.
    Legacy,
    /// A durable proposal whose individual lane decisions are authoritative.
    Parsed(Vec<crate::planner::ProposedDirection>),
    /// A plan exists but cannot safely be understood as lane policy.
    Unavailable,
}

fn planned_lane_source(plan: Option<&plan::Model>) -> PlannedLaneSource {
    let Some(plan) = plan else {
        return PlannedLaneSource::Legacy;
    };
    if plan.status == "withdrawn" {
        return PlannedLaneSource::Legacy;
    }
    if !matches!(plan.status.as_str(), "proposed" | "confirmed") {
        return PlannedLaneSource::Unavailable;
    }
    let proposal = match serde_json::from_str::<crate::planner::Proposal>(&plan.proposal) {
        Ok(proposal) if !proposal.directions.is_empty() => proposal,
        Ok(_) | Err(_) => return PlannedLaneSource::Unavailable,
    };
    PlannedLaneSource::Parsed(proposal.directions)
}

fn direction_is_active(direction: &direction::Model) -> bool {
    !matches!(direction.status.as_str(), "inactive" | "cancelled")
}

fn proposal_lane_policy(proposed_lane: &crate::planner::ProposedDirection) -> PolicyDecision {
    match proposed_lane.decision.as_str() {
        "denied" => PolicyDecision::Denied,
        "" if proposed_lane.direction_id == 0 => PolicyDecision::NeedsGate,
        "" | "approved" => PolicyDecision::AllowedByPolicy,
        _ => PolicyDecision::NeedsGate,
    }
}

fn stricter_materialized_policy(
    current: PolicyDecision,
    candidate: PolicyDecision,
) -> PolicyDecision {
    match (current, candidate) {
        (PolicyDecision::Denied, _) | (_, PolicyDecision::Denied) => PolicyDecision::Denied,
        (PolicyDecision::NeedsGate, _) | (_, PolicyDecision::NeedsGate) => {
            PolicyDecision::NeedsGate
        }
        _ => PolicyDecision::AllowedByPolicy,
    }
}

fn virtual_lane_facts(
    direction_id: i32,
    name: String,
    policy: PolicyDecision,
    reconciliation: ExecutionReconciliation,
    pull_requests: Vec<PullRequestFacts>,
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
) -> LaneFacts {
    LaneFacts {
        direction_id,
        name,
        active: true,
        policy,
        worker_failed: false,
        has_open_ask: false,
        reconciliation,
        checks: CheckEvidence::NotApplicable,
        upstream: UpstreamEvidence::Satisfied,
        open_pr_snapshot_freshness,
        pull_requests,
        // Virtual lanes have no successful worker completion evidence. A
        // policy-allowed virtual lane must therefore land at InProgress after
        // all explicit gates have passed.
        direction_status: "working".to_string(),
    }
}

const CHECK_EVIDENCE_TTL: Duration = Duration::from_secs(10 * 60);
const READINESS_CHECK_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONCURRENT_CHECK_RUNNERS: usize = 2;
// Keep this aligned with host::monitor's private default. The readiness
// collector uses commands::env_secs too, so malformed values resolve exactly
// like the monitor's cadence configuration.
const PR_SWEEP_DEFAULT_SECS: u64 = 60;
const PR_OPEN_SNAPSHOT_TTL_SWEEPS: u64 = 3;

fn open_pr_snapshot_freshness(now_secs: i64, sweep_secs: u64) -> OpenPrSnapshotFreshness {
    if sweep_secs == 0 {
        return OpenPrSnapshotFreshness::Disabled;
    }
    let max_age_secs = sweep_secs
        .saturating_mul(PR_OPEN_SNAPSHOT_TTL_SWEEPS)
        .min(i64::MAX as u64) as i64;
    OpenPrSnapshotFreshness::MaxAge {
        now_secs,
        max_age_secs,
    }
}

fn current_open_pr_snapshot_freshness() -> Result<OpenPrSnapshotFreshness> {
    let sweep_secs = crate::commands::env_secs("WEFT_PR_SWEEP_SECS", PR_SWEEP_DEFAULT_SECS);
    if sweep_secs == 0 {
        return Ok(OpenPrSnapshotFreshness::Disabled);
    }
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?;
    let now_secs = i64::try_from(duration.as_secs())
        .map_err(|_| anyhow!("Unix timestamp exceeds i64 range"))?;
    Ok(open_pr_snapshot_freshness(now_secs, sweep_secs))
}

#[derive(Clone)]
struct CachedCheckEvidence {
    collected_at: Instant,
    path_signature: Vec<String>,
    head_signature: Vec<String>,
    evidence: CheckEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckTarget {
    path: String,
    head_sha: String,
}

/// Process-local coordination for readiness-triggered verification. The cache
/// is keyed by direction id, but also records the worktree path and HEAD-SHA
/// signatures that produced it so a recreated or newly committed checkout
/// cannot inherit prior evidence.
struct CheckFlight {
    cache: Mutex<HashMap<i32, CachedCheckEvidence>>,
    direction_locks: Mutex<HashMap<i32, Arc<AsyncMutex<()>>>>,
    runner_limit: Semaphore,
    ttl: Duration,
}

impl CheckFlight {
    fn new(ttl: Duration, max_concurrent_runners: usize) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            direction_locks: Mutex::new(HashMap::new()),
            runner_limit: Semaphore::new(max_concurrent_runners),
            ttl,
        }
    }

    fn signatures(targets: &[CheckTarget]) -> (Vec<String>, Vec<String>) {
        let paths = targets.iter().map(|target| target.path.clone()).collect();
        let heads = targets
            .iter()
            .map(|target| target.head_sha.clone())
            .collect();
        (paths, heads)
    }

    fn cached(&self, direction_id: i32, targets: &[CheckTarget]) -> Result<Option<CheckEvidence>> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("readiness check cache lock poisoned"))?;
        let Some(entry) = cache.get(&direction_id) else {
            return Ok(None);
        };
        let is_fresh = Instant::now().saturating_duration_since(entry.collected_at) < self.ttl;
        let (path_signature, head_signature) = Self::signatures(targets);
        if entry.path_signature == path_signature
            && entry.head_signature == head_signature
            && is_fresh
        {
            return Ok(Some(entry.evidence));
        }
        Ok(None)
    }

    fn direction_lock(&self, direction_id: i32) -> Result<Arc<AsyncMutex<()>>> {
        let mut direction_locks = self
            .direction_locks
            .lock()
            .map_err(|_| anyhow!("readiness check flight lock poisoned"))?;
        Ok(direction_locks
            .entry(direction_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone())
    }

    fn cache_result(
        &self,
        direction_id: i32,
        targets: &[CheckTarget],
        evidence: CheckEvidence,
    ) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("readiness check cache lock poisoned"))?;
        let (path_signature, head_signature) = Self::signatures(targets);
        cache.insert(
            direction_id,
            CachedCheckEvidence {
                collected_at: Instant::now(),
                path_signature,
                head_signature,
                evidence,
            },
        );
        Ok(())
    }

    async fn get_or_run<F, Fut>(
        &self,
        direction_id: i32,
        mut targets: Vec<CheckTarget>,
        runner: F,
    ) -> Result<CheckEvidence>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = Result<CheckEvidence>>,
    {
        targets.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.head_sha.cmp(&right.head_sha))
        });
        if let Some(evidence) = self.cached(direction_id, &targets)? {
            return Ok(evidence);
        }

        let direction_lock = self.direction_lock(direction_id)?;
        let _direction_guard = direction_lock.lock().await;
        if let Some(evidence) = self.cached(direction_id, &targets)? {
            return Ok(evidence);
        }

        let _runner_permit = self
            .runner_limit
            .acquire()
            .await
            .map_err(|_| anyhow!("readiness check runner semaphore closed"))?;
        let paths = targets.iter().map(|target| target.path.clone()).collect();
        let evidence = runner(paths).await?;
        self.cache_result(direction_id, &targets, evidence)?;
        Ok(evidence)
    }
}

fn check_flight() -> &'static CheckFlight {
    static CHECK_FLIGHT: OnceLock<CheckFlight> = OnceLock::new();
    CHECK_FLIGHT.get_or_init(|| CheckFlight::new(CHECK_EVIDENCE_TTL, MAX_CONCURRENT_CHECK_RUNNERS))
}

async fn latest_worker_failed(db: &Db, direction_id: i32) -> Result<bool> {
    let Some(session) = repo::latest_session_for_direction(db, direction_id).await? else {
        return Ok(false);
    };
    // engine::finalize_text_row persists the turn's terminal state by calling
    // repo::update_lead_message on an assistant/text row. Assistant/tool rows
    // describe individual tool calls, so their `error` cannot diagnose the
    // whole worker turn as failed.
    let latest = lead_message::Entity::find()
        .filter(lead_message::Column::SessionId.eq(session.id))
        .filter(lead_message::Column::Role.eq("assistant"))
        .filter(lead_message::Column::Kind.eq("text"))
        .order_by_desc(lead_message::Column::TurnId)
        .order_by_desc(lead_message::Column::Id)
        .one(&db.0)
        .await?;
    Ok(latest.is_some_and(|message| message.status == "error"))
}

async fn reconciliation_for(
    db: &Db,
    direction: &direction::Model,
) -> Result<ExecutionReconciliation> {
    let worktrees = repo::list_worktrees(db, Some(direction.id)).await?;
    if worktrees.is_empty() {
        return match direction.status.as_str() {
            "queued" | "planning" => Ok(ExecutionReconciliation::Matched),
            _ => Ok(ExecutionReconciliation::Unknown),
        };
    }
    if direction.branch.trim().is_empty() {
        return Ok(ExecutionReconciliation::Unknown);
    }

    let mut unknown = false;
    for worktree in worktrees {
        let path = Path::new(&worktree.path);
        if !path.is_dir() {
            unknown = true;
            continue;
        }
        match crate::git::current_branch(path) {
            Ok(branch) => {
                if branch != direction.branch {
                    return Ok(ExecutionReconciliation::Drifted);
                }
            }
            Err(_) => unknown = true,
        }
    }

    if unknown {
        return Ok(ExecutionReconciliation::Unknown);
    }
    Ok(ExecutionReconciliation::Matched)
}

async fn checks_for_with_runner<F, Fut>(
    db: &Db,
    direction: &direction::Model,
    flight: &CheckFlight,
    runner: F,
) -> Result<CheckEvidence>
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
{
    if !matches!(direction.status.as_str(), "review" | "done") {
        return Ok(CheckEvidence::NotApplicable);
    }

    let worktrees = repo::list_worktrees(db, Some(direction.id)).await?;
    let mut targets = Vec::new();
    for worktree in worktrees {
        let path = Path::new(&worktree.path);
        if !path.is_dir() {
            continue;
        }
        let Some(head_sha) = crate::git::head_commit_full(path) else {
            return Ok(CheckEvidence::NotProduced);
        };
        targets.push(CheckTarget {
            path: worktree.path,
            head_sha,
        });
    }
    if targets.is_empty() {
        return Ok(CheckEvidence::NotProduced);
    }

    flight.get_or_run(direction.id, targets, runner).await
}

fn check_output_tail(output: &str, max: usize) -> String {
    if output.len() <= max {
        return output.trim_end().to_string();
    }
    let mut start = output.len() - max;
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    let slice = &output[start..];
    let slice = slice
        .find('\n')
        .map(|index| &slice[index + 1..])
        .unwrap_or(slice);
    format!("…\n{}", slice.trim_end())
}

async fn run_bounded_check(
    cwd: &Path,
    check: &crate::check::Check,
    timeout: Duration,
) -> Result<Option<crate::check::CheckResult>> {
    let mut command = tokio::process::Command::new(&check.program);
    command
        .args(&check.args)
        .current_dir(cwd)
        .env("PATH", crate::detect::tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(Some(crate::check::CheckResult {
                name: check.name.clone(),
                status: "fail".to_string(),
                code: -1,
                output_tail: format!("could not run {}: {error}", check.program),
            }));
        }
    };
    // `wait_with_output` owns the child. When this deadline elapses, dropping
    // that future drops the kill-on-drop child; the unknown result must never
    // retain a CheckFlight direction lock or global runner permit.
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(Some(crate::check::CheckResult {
        name: check.name.clone(),
        status: if output.status.success() {
            "pass"
        } else {
            "fail"
        }
        .to_string(),
        code: output.status.code().unwrap_or(-1),
        output_tail: check_output_tail(&combined, 2_000),
    }))
}

async fn run_checks_with_timeout(
    cwd: &Path,
    checks: &[crate::check::Check],
    timeout: Duration,
) -> Result<CheckEvidence> {
    let mut evidence = CheckEvidence::Passed;
    for check in checks {
        let Some(result) = run_bounded_check(cwd, check, timeout).await? else {
            return Ok(CheckEvidence::NotProduced);
        };
        if result.status == "fail" {
            evidence = CheckEvidence::Failing;
        }
    }
    Ok(evidence)
}

async fn run_readiness_checks(paths: Vec<String>, timeout: Duration) -> Result<CheckEvidence> {
    let mut evidence = CheckEvidence::Passed;
    for path in paths {
        let checks = crate::check::infer_checks(Path::new(&path));
        let path_evidence = run_checks_with_timeout(Path::new(&path), &checks, timeout).await?;
        if path_evidence == CheckEvidence::NotProduced {
            return Ok(CheckEvidence::NotProduced);
        }
        if path_evidence == CheckEvidence::Failing {
            evidence = CheckEvidence::Failing;
            continue;
        }
        if path_evidence == CheckEvidence::NotProduced && evidence != CheckEvidence::Failing {
            evidence = CheckEvidence::NotProduced;
        }
    }
    Ok(evidence)
}

async fn checks_for(db: &Db, direction: &direction::Model) -> Result<CheckEvidence> {
    checks_for_with_runner(db, direction, check_flight(), |paths| async move {
        run_readiness_checks(paths, READINESS_CHECK_TIMEOUT).await
    })
    .await
}

async fn checks_after_reconciliation<F, Fut>(
    reconciliation: ExecutionReconciliation,
    collect_checks: F,
) -> Result<CheckEvidence>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
{
    if reconciliation == ExecutionReconciliation::Drifted {
        return Ok(CheckEvidence::NotApplicable);
    }
    collect_checks().await
}

fn parse_review_status(raw: &str) -> ReviewStatus {
    serde_json::from_str(raw).unwrap_or_else(|_| ReviewStatus::Unknown {
        reason: "stored review status was not successfully probed".to_string(),
    })
}

fn parse_conflict_status(raw: &str) -> ConflictStatus {
    serde_json::from_str(raw).unwrap_or_else(|_| ConflictStatus::Unknown {
        reason: "stored conflict status was not successfully probed".to_string(),
    })
}

fn parse_last_checked_at(raw: &str) -> Option<i64> {
    raw.trim().parse::<i64>().ok()
}

fn pull_request_facts(row: &pull_request::Model) -> PullRequestFacts {
    PullRequestFacts {
        id: row.id,
        lifecycle: crate::host::gate::parse_lifecycle(&row.lifecycle),
        ci: crate::host::gate::parse_ci(&row.ci_status),
        review: parse_review_status(&row.review_status),
        threads: crate::host::gate::parse_threads(&row.thread_status),
        conflict: parse_conflict_status(&row.conflict_status),
        probe_failed: !row.last_error.trim().is_empty() || row.probe_fail_count > 0,
        last_checked_at: parse_last_checked_at(&row.last_checked_at),
    }
}

fn upstream_evidence(status: UpstreamStatus) -> UpstreamEvidence {
    match status {
        UpstreamStatus::None | UpstreamStatus::Merged => UpstreamEvidence::Satisfied,
        UpstreamStatus::Pending { .. } => UpstreamEvidence::Unmet,
        UpstreamStatus::Unknown { .. } => UpstreamEvidence::Unknown,
    }
}

async fn collect_lane(
    db: &Db,
    direction: &direction::Model,
    policy: PolicyDecision,
    open_ask_direction_ids: &[i32],
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
) -> Result<LaneFacts> {
    let active = direction_is_active(direction);
    if !active || policy == PolicyDecision::Denied {
        return Ok(LaneFacts {
            direction_id: direction.id,
            name: direction.name.clone(),
            active,
            policy,
            worker_failed: false,
            has_open_ask: false,
            reconciliation: ExecutionReconciliation::Unknown,
            checks: CheckEvidence::NotApplicable,
            upstream: UpstreamEvidence::Satisfied,
            open_pr_snapshot_freshness,
            pull_requests: Vec::new(),
            direction_status: direction.status.clone(),
        });
    }
    let mut pull_requests = repo::list_pull_requests_for_direction(db, direction.id).await?;
    pull_requests.sort_by_key(|row| row.id);
    let pull_requests = pull_requests.iter().map(pull_request_facts).collect();
    let reconciliation = reconciliation_for(db, direction).await?;
    let checks = checks_after_reconciliation(reconciliation, || checks_for(db, direction)).await?;
    Ok(LaneFacts {
        direction_id: direction.id,
        name: direction.name.clone(),
        active,
        policy,
        worker_failed: latest_worker_failed(db, direction.id).await?,
        has_open_ask: open_ask_direction_ids.contains(&direction.id),
        reconciliation,
        checks,
        upstream: upstream_evidence(repo::upstream_merge_state(db, direction.id).await),
        open_pr_snapshot_freshness,
        pull_requests,
        direction_status: direction.status.clone(),
    })
}

async fn collect_unbound_pr_lane(
    db: &Db,
    thread_id: i32,
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
) -> Result<Option<LaneFacts>> {
    let rows = pull_request::Entity::find()
        .filter(pull_request::Column::ThreadId.eq(thread_id))
        .filter(pull_request::Column::DirectionId.eq(0))
        .order_by_asc(pull_request::Column::Id)
        .all(&db.0)
        .await?;
    let pull_requests: Vec<PullRequestFacts> = rows.iter().map(pull_request_facts).collect();
    if pull_request_verdict(&pull_requests, open_pr_snapshot_freshness).is_none() {
        return Ok(None);
    }
    Ok(Some(virtual_lane_facts(
        0,
        "unbound PR".to_string(),
        PolicyDecision::AllowedByPolicy,
        ExecutionReconciliation::Matched,
        pull_requests,
        open_pr_snapshot_freshness,
    )))
}

/// Collect live storage/process facts, then run the pure aggregation. This
/// function performs no writes and deliberately reuses the existing local
/// check runner and host parsers rather than inventing parallel semantics.
pub async fn collect(db: &Db, bus: &BusRegistry, thread_id: i32) -> Result<IssueReadinessDto> {
    if repo::get_thread(db, thread_id).await?.is_none() {
        return Err(anyhow!("thread {thread_id} not found"));
    }
    let plan = repo::get_plan(db, thread_id).await?;
    let open_ask_direction_ids: Vec<i32> = bus
        .open_asks(thread_id)
        .into_iter()
        .filter_map(|ask| ask.from.parse::<i32>().ok())
        .collect();
    let mut directions = repo::list_directions(db, thread_id).await?;
    directions.sort_by_key(|direction| direction.id);
    let open_pr_snapshot_freshness = current_open_pr_snapshot_freshness()?;
    let mut facts = Vec::with_capacity(directions.len() + 1);

    match planned_lane_source(plan.as_ref()) {
        PlannedLaneSource::Legacy => {
            for direction in &directions {
                facts.push(
                    collect_lane(
                        db,
                        direction,
                        PolicyDecision::AllowedByPolicy,
                        &open_ask_direction_ids,
                        open_pr_snapshot_freshness,
                    )
                    .await?,
                );
            }
        }
        PlannedLaneSource::Unavailable => {
            for direction in &directions {
                facts.push(
                    collect_lane(
                        db,
                        direction,
                        PolicyDecision::NeedsGate,
                        &open_ask_direction_ids,
                        open_pr_snapshot_freshness,
                    )
                    .await?,
                );
            }
        }
        PlannedLaneSource::Parsed(proposal_lanes) => {
            let referenced_direction_ids: HashSet<i32> = proposal_lanes
                .iter()
                .filter_map(|lane| (lane.direction_id != 0).then_some(lane.direction_id))
                .collect();
            let mut materialized_policies = HashMap::new();
            for proposed_lane in &proposal_lanes {
                if proposed_lane.direction_id == 0 {
                    continue;
                }
                let policy = proposal_lane_policy(proposed_lane);
                materialized_policies
                    .entry(proposed_lane.direction_id)
                    .and_modify(|current| {
                        *current = stricter_materialized_policy(*current, policy);
                    })
                    .or_insert(policy);
            }
            let directions_by_id: HashMap<i32, &direction::Model> = directions
                .iter()
                .map(|direction| (direction.id, direction))
                .collect();
            let mut handled_direction_ids = HashSet::new();

            for proposed_lane in proposal_lanes {
                let policy = proposal_lane_policy(&proposed_lane);
                if proposed_lane.direction_id == 0 {
                    if policy == PolicyDecision::Denied {
                        continue;
                    }
                    facts.push(virtual_lane_facts(
                        0,
                        proposed_lane.name,
                        policy,
                        ExecutionReconciliation::Matched,
                        Vec::new(),
                        open_pr_snapshot_freshness,
                    ));
                    continue;
                }
                if !handled_direction_ids.insert(proposed_lane.direction_id) {
                    continue;
                }
                let Some(&policy) = materialized_policies.get(&proposed_lane.direction_id) else {
                    return Err(anyhow!(
                        "proposal lane {} is missing its collected policy",
                        proposed_lane.direction_id
                    ));
                };
                if policy == PolicyDecision::Denied {
                    continue;
                }
                let Some(direction) = directions_by_id.get(&proposed_lane.direction_id) else {
                    facts.push(virtual_lane_facts(
                        proposed_lane.direction_id,
                        proposed_lane.name,
                        policy,
                        ExecutionReconciliation::Unknown,
                        Vec::new(),
                        open_pr_snapshot_freshness,
                    ));
                    continue;
                };
                facts.push(
                    collect_lane(
                        db,
                        direction,
                        policy,
                        &open_ask_direction_ids,
                        open_pr_snapshot_freshness,
                    )
                    .await?,
                );
            }

            for direction in &directions {
                if referenced_direction_ids.contains(&direction.id) {
                    continue;
                }
                facts.push(
                    collect_lane(
                        db,
                        direction,
                        PolicyDecision::AllowedByPolicy,
                        &open_ask_direction_ids,
                        open_pr_snapshot_freshness,
                    )
                    .await?,
                );
            }
        }
    }

    if let Some(unbound_pr_lane) =
        collect_unbound_pr_lane(db, thread_id, open_pr_snapshot_freshness).await?
    {
        facts.push(unbound_pr_lane);
    }
    Ok(issue_readiness(&facts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn facts() -> LaneFacts {
        LaneFacts {
            direction_id: 7,
            name: "implementation".to_string(),
            active: true,
            policy: PolicyDecision::AllowedByPolicy,
            worker_failed: false,
            has_open_ask: false,
            reconciliation: ExecutionReconciliation::Matched,
            checks: CheckEvidence::Passed,
            upstream: UpstreamEvidence::Satisfied,
            open_pr_snapshot_freshness: open_pr_snapshot_freshness(1_000, 60),
            pull_requests: Vec::new(),
            direction_status: "review".to_string(),
        }
    }

    fn pr() -> PullRequestFacts {
        PullRequestFacts {
            id: 1,
            lifecycle: Some(PrLifecycle::Open),
            ci: CiStatus::Passing,
            review: ReviewStatus::AwaitingApproval,
            threads: ThreadStatus::AllResolved,
            conflict: ConflictStatus::Clean,
            probe_failed: false,
            last_checked_at: Some(1_000),
        }
    }

    fn check_targets(path: &str, head_sha: &str) -> Vec<CheckTarget> {
        vec![CheckTarget {
            path: path.to_string(),
            head_sha: head_sha.to_string(),
        }]
    }

    fn verdict(facts: &LaneFacts) -> (LaneReadiness, Option<ReasonCode>) {
        let verdict = lane_readiness(facts).expect("active lane has a verdict");
        let reason = verdict.reasons.first().map(|reason| reason.code);
        (verdict.readiness, reason)
    }

    #[test]
    fn worker_failure_has_first_precedence() {
        let mut lane = facts();
        lane.worker_failed = true;
        lane.has_open_ask = true;
        lane.policy = PolicyDecision::NeedsGate;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Failed, Some(ReasonCode::WorkerFailed))
        );
    }

    #[test]
    fn open_ask_precedes_policy_and_drift() {
        let mut lane = facts();
        lane.has_open_ask = true;
        lane.policy = PolicyDecision::NeedsGate;
        lane.reconciliation = ExecutionReconciliation::Drifted;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::NeedsYou, Some(ReasonCode::OpenNeed))
        );
    }

    #[test]
    fn policy_gate_precedes_drift() {
        let mut lane = facts();
        lane.policy = PolicyDecision::NeedsGate;
        lane.reconciliation = ExecutionReconciliation::Drifted;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::NeedsYou, Some(ReasonCode::PolicyGatePending))
        );
    }

    #[test]
    fn drift_precedes_checks_and_cli_success() {
        let mut lane = facts();
        lane.direction_status = "done".to_string();
        lane.reconciliation = ExecutionReconciliation::Drifted;
        lane.checks = CheckEvidence::Failing;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::ExecutionDrifted))
        );
    }

    #[test]
    fn checks_and_upstream_follow_drift() {
        let mut lane = facts();
        lane.checks = CheckEvidence::Failing;
        lane.upstream = UpstreamEvidence::Unmet;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::ChecksFailing))
        );

        lane.checks = CheckEvidence::Passed;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::UpstreamUnmet))
        );

        lane.upstream = UpstreamEvidence::Unknown;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::RemoteUnknown))
        );
    }

    #[test]
    fn pr_axis_truth_table_is_fail_closed() {
        let cases = [
            (
                PullRequestFacts {
                    lifecycle: None,
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::RemoteUnknown,
            ),
            (
                PullRequestFacts {
                    lifecycle: Some(PrLifecycle::Closed),
                    ..pr()
                },
                LaneReadiness::Blocked,
                ReasonCode::PrClosedUnmerged,
            ),
            (
                PullRequestFacts {
                    ci: CiStatus::Pending,
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::PrCiPending,
            ),
            (
                PullRequestFacts {
                    ci: CiStatus::Failing,
                    ..pr()
                },
                LaneReadiness::Blocked,
                ReasonCode::PrCiFailing,
            ),
            (
                PullRequestFacts {
                    ci: CiStatus::Unknown {
                        reason: "unavailable".to_string(),
                    },
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::RemoteUnknown,
            ),
            (
                PullRequestFacts {
                    review: ReviewStatus::ChangesRequested,
                    ..pr()
                },
                LaneReadiness::Blocked,
                ReasonCode::PrReviewChangesRequested,
            ),
            (
                PullRequestFacts {
                    review: ReviewStatus::Unknown {
                        reason: "unavailable".to_string(),
                    },
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::RemoteUnknown,
            ),
            (
                PullRequestFacts {
                    threads: ThreadStatus::Unresolved { count: 1 },
                    ..pr()
                },
                LaneReadiness::Blocked,
                ReasonCode::PrThreadsUnresolved,
            ),
            (
                PullRequestFacts {
                    threads: ThreadStatus::Unknown {
                        reason: "unavailable".to_string(),
                    },
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::RemoteUnknown,
            ),
            (
                PullRequestFacts {
                    conflict: ConflictStatus::Conflicting,
                    ..pr()
                },
                LaneReadiness::Blocked,
                ReasonCode::PrConflict,
            ),
            (
                PullRequestFacts {
                    conflict: ConflictStatus::Unknown {
                        reason: "unavailable".to_string(),
                    },
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::RemoteUnknown,
            ),
            (
                PullRequestFacts {
                    probe_failed: true,
                    ..pr()
                },
                LaneReadiness::Unknown,
                ReasonCode::RemoteUnknown,
            ),
        ];

        for (pr, readiness, reason) in cases {
            let mut lane = facts();
            lane.pull_requests = vec![pr];
            assert_eq!(verdict(&lane), (readiness, Some(reason)));
        }
    }

    #[test]
    fn github_merged_fixture_shape_is_terminally_clear() {
        // Mirrors host/github.rs's real `gh pr view` merged fixture: GitHub
        // leaves `mergeable: "UNKNOWN"` and an empty review decision after a
        // merge, so all live axes must be skipped by the terminal lifecycle.
        let mut lane = facts();
        let mut merged = pr();
        merged.lifecycle = Some(PrLifecycle::Merged);
        merged.review = ReviewStatus::AwaitingApproval;
        merged.threads = ThreadStatus::Unknown {
            reason: "review threads have not been read yet".to_string(),
        };
        merged.conflict = ConflictStatus::Unknown {
            reason: "GitHub hasn't finished computing mergeability yet".to_string(),
        };
        merged.last_checked_at = None;
        merged.probe_failed = true;
        lane.pull_requests = vec![merged];

        assert_eq!(verdict(&lane), (LaneReadiness::ReviewReady, None));
    }

    #[test]
    fn open_pr_snapshot_ttl_is_fail_closed_without_affecting_terminal_rows() {
        let mut lane = facts();
        let mut tracked = pr();
        tracked.last_checked_at = Some(819);
        lane.pull_requests = vec![tracked.clone()];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::RemoteUnknown))
        );

        tracked.last_checked_at = Some(820);
        lane.pull_requests = vec![tracked.clone()];
        assert_eq!(verdict(&lane), (LaneReadiness::ReviewReady, None));

        tracked.lifecycle = Some(PrLifecycle::Merged);
        tracked.last_checked_at = Some(1);
        lane.pull_requests = vec![tracked.clone()];
        assert_eq!(verdict(&lane), (LaneReadiness::ReviewReady, None));

        tracked.lifecycle = Some(PrLifecycle::Open);
        tracked.last_checked_at = Some(1_000);
        lane.open_pr_snapshot_freshness = open_pr_snapshot_freshness(1_000, 0);
        lane.pull_requests = vec![tracked];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::RemoteUnknown))
        );
    }

    #[test]
    fn blank_or_invalid_pr_probe_timestamps_are_unknown() {
        assert_eq!(parse_last_checked_at(""), None);
        assert_eq!(parse_last_checked_at("not-a-timestamp"), None);
        assert_eq!(parse_last_checked_at(" 1000 "), Some(1_000));

        let mut lane = facts();
        let mut tracked = pr();
        tracked.last_checked_at = None;
        lane.pull_requests = vec![tracked];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::RemoteUnknown))
        );

        lane.pull_requests[0].last_checked_at = Some(i64::MIN);
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::RemoteUnknown))
        );
    }

    #[test]
    fn multi_pr_verdict_is_independent_of_input_order() {
        let mut unknown = pr();
        unknown.id = 3;
        unknown.ci = CiStatus::Pending;
        let mut blocked = pr();
        blocked.id = 9;
        blocked.ci = CiStatus::Failing;

        let mut forward = facts();
        forward.pull_requests = vec![unknown.clone(), blocked.clone()];
        let mut reverse = facts();
        reverse.pull_requests = vec![blocked, unknown];
        assert_eq!(
            verdict(&forward),
            (LaneReadiness::Blocked, Some(ReasonCode::PrCiFailing))
        );
        assert_eq!(verdict(&forward), verdict(&reverse));

        let mut lower_id = pr();
        lower_id.id = 2;
        lower_id.review = ReviewStatus::ChangesRequested;
        let mut higher_id = pr();
        higher_id.id = 7;
        higher_id.ci = CiStatus::Failing;
        forward.pull_requests = vec![higher_id.clone(), lower_id.clone()];
        reverse.pull_requests = vec![lower_id, higher_id];
        assert_eq!(
            verdict(&forward),
            (
                LaneReadiness::Blocked,
                Some(ReasonCode::PrReviewChangesRequested)
            )
        );
        assert_eq!(verdict(&forward), verdict(&reverse));
    }

    #[test]
    fn awaiting_approval_and_unconfigured_ci_remain_review_ready() {
        let mut lane = facts();
        let mut tracked = pr();
        tracked.ci = CiStatus::NotConfigured;
        lane.pull_requests = vec![tracked];
        assert_eq!(verdict(&lane), (LaneReadiness::ReviewReady, None));
    }

    #[test]
    fn unknown_evidence_beats_cli_success() {
        let mut lane = facts();
        lane.direction_status = "done".to_string();
        lane.reconciliation = ExecutionReconciliation::Unknown;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::RemoteUnknown))
        );

        lane.reconciliation = ExecutionReconciliation::Matched;
        lane.checks = CheckEvidence::NotProduced;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::ChecksUnknown))
        );
    }

    #[test]
    fn not_applicable_checks_fall_through_to_in_progress() {
        let mut lane = facts();
        lane.direction_status = "working".to_string();
        lane.checks = CheckEvidence::NotApplicable;

        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::InProgress))
        );
    }

    #[test]
    fn claimed_completion_check_failures_remain_blocking() {
        for status in ["review", "done"] {
            let mut lane = facts();
            lane.direction_status = status.to_string();
            lane.checks = CheckEvidence::Failing;

            assert_eq!(
                verdict(&lane),
                (LaneReadiness::Blocked, Some(ReasonCode::ChecksFailing))
            );
        }
    }

    #[test]
    fn ready_and_in_progress_terminal_rows_are_distinct() {
        let lane = facts();
        assert_eq!(verdict(&lane), (LaneReadiness::ReviewReady, None));

        let mut working = lane;
        working.direction_status = "working".to_string();
        assert_eq!(
            verdict(&working),
            (LaneReadiness::Unknown, Some(ReasonCode::InProgress))
        );
    }

    #[test]
    fn denied_lanes_are_excluded_and_zero_lanes_are_unknown() {
        let mut denied = facts();
        denied.policy = PolicyDecision::Denied;
        assert_eq!(lane_readiness(&denied), None);

        let issue = issue_readiness(&[denied]);
        assert_eq!(issue.readiness, IssueReadiness::Unknown);
        assert_eq!(issue.active_lane_count, 0);
        assert_eq!(issue.reasons[0].code, ReasonCode::NoActiveLanes);
    }

    #[test]
    fn virtual_plan_lanes_follow_the_policy_truth_table() {
        let pending = virtual_lane_facts(
            0,
            "pending implementation".to_string(),
            PolicyDecision::NeedsGate,
            ExecutionReconciliation::Matched,
            Vec::new(),
            open_pr_snapshot_freshness(1_000, 60),
        );
        let pending_verdict = lane_readiness(&pending).expect("pending virtual lane verdict");
        assert_eq!(pending_verdict.readiness, LaneReadiness::NeedsYou);
        assert_eq!(
            pending_verdict.reasons[0].code,
            ReasonCode::PolicyGatePending
        );
        assert_eq!(pending_verdict.reasons[0].direction_id, None);

        let approved = virtual_lane_facts(
            0,
            "approved implementation".to_string(),
            PolicyDecision::AllowedByPolicy,
            ExecutionReconciliation::Matched,
            Vec::new(),
            open_pr_snapshot_freshness(1_000, 60),
        );
        let approved_verdict = lane_readiness(&approved).expect("approved virtual lane verdict");
        assert_eq!(approved_verdict.readiness, LaneReadiness::Unknown);
        assert_eq!(approved_verdict.reasons[0].code, ReasonCode::InProgress);
        assert_eq!(approved_verdict.reasons[0].direction_id, None);
    }

    #[test]
    fn plan_lane_source_handles_withdrawn_and_unavailable_proposals() {
        let parsed_proposal = serde_json::to_string(&crate::planner::Proposal {
            rationale: "test proposal".to_string(),
            directions: vec![crate::planner::ProposedDirection {
                name: "implementation".to_string(),
                repo: "repo".to_string(),
                reason: "reason".to_string(),
                mandate: "impl-only".to_string(),
                base_branch: "main".to_string(),
                decision: "approved".to_string(),
                direction_id: 7,
            }],
        })
        .expect("proposal json");
        let confirmed = plan::Model {
            id: 1,
            thread_id: 1,
            proposal: parsed_proposal.clone(),
            status: "confirmed".to_string(),
            created_at: "0".to_string(),
        };
        assert!(matches!(
            planned_lane_source(Some(&confirmed)),
            PlannedLaneSource::Parsed(lanes) if lanes.len() == 1
        ));

        let withdrawn = plan::Model {
            status: "withdrawn".to_string(),
            ..confirmed.clone()
        };
        assert!(matches!(
            planned_lane_source(Some(&withdrawn)),
            PlannedLaneSource::Legacy
        ));

        let empty = plan::Model {
            proposal: serde_json::to_string(&crate::planner::Proposal::default())
                .expect("empty proposal json"),
            ..confirmed
        };
        assert!(matches!(
            planned_lane_source(Some(&empty)),
            PlannedLaneSource::Unavailable
        ));
    }

    #[test]
    fn unbound_pr_verdict_has_no_persisted_direction_attribution() {
        let mut unbound = virtual_lane_facts(
            0,
            "unbound PR".to_string(),
            PolicyDecision::AllowedByPolicy,
            ExecutionReconciliation::Matched,
            Vec::new(),
            open_pr_snapshot_freshness(1_000, 60),
        );
        let mut tracked = pr();
        tracked.ci = CiStatus::Failing;
        unbound.pull_requests = vec![tracked];

        let verdict = lane_readiness(&unbound).expect("unbound pr verdict");
        assert_eq!(verdict.readiness, LaneReadiness::Blocked);
        assert_eq!(verdict.reasons[0].code, ReasonCode::PrCiFailing);
        assert_eq!(verdict.reasons[0].direction_id, None);
    }

    #[test]
    fn aggregation_uses_the_highest_severity_and_its_lanes() {
        let mut failed = facts();
        failed.direction_id = 1;
        failed.worker_failed = true;
        let mut blocked = facts();
        blocked.direction_id = 2;
        blocked.checks = CheckEvidence::Failing;
        let issue = issue_readiness(&[failed, blocked]);
        assert_eq!(issue.readiness, IssueReadiness::Failed);
        assert_eq!(issue.reasons.len(), 1);
        assert_eq!(issue.reasons[0].direction_id, Some(1));
    }

    #[tokio::test]
    async fn check_flight_singleflights_reuses_matching_heads_and_reruns_on_head_change() {
        let flight = Arc::new(CheckFlight::new(CHECK_EVIDENCE_TTL, 2));
        let runs = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let flight = Arc::clone(&flight);
            let runs = Arc::clone(&runs);
            tasks.push(tokio::spawn(async move {
                flight
                    .get_or_run(
                        41,
                        check_targets("/tmp/check-flight", "head-a"),
                        move |_| {
                            let runs = Arc::clone(&runs);
                            async move {
                                runs.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                Ok(CheckEvidence::Passed)
                            }
                        },
                    )
                    .await
            }));
        }
        for task in tasks {
            let evidence = task
                .await
                .expect("singleflight task joins")
                .expect("singleflight result");
            assert_eq!(evidence, CheckEvidence::Passed);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let reuse_runs = Arc::clone(&runs);
        let reused = flight
            .get_or_run(
                41,
                check_targets("/tmp/check-flight", "head-a"),
                move |_| {
                    let reuse_runs = Arc::clone(&reuse_runs);
                    async move {
                        reuse_runs.fetch_add(1, Ordering::SeqCst);
                        Ok(CheckEvidence::Failing)
                    }
                },
            )
            .await
            .expect("fresh cache result");
        assert_eq!(reused, CheckEvidence::Passed);
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let changed_head_runs = Arc::clone(&runs);
        let changed_head = flight
            .get_or_run(
                41,
                check_targets("/tmp/check-flight", "head-b"),
                move |_| {
                    let changed_head_runs = Arc::clone(&changed_head_runs);
                    async move {
                        changed_head_runs.fetch_add(1, Ordering::SeqCst);
                        Ok(CheckEvidence::Failing)
                    }
                },
            )
            .await
            .expect("changed head reruns checks");
        assert_eq!(changed_head, CheckEvidence::Failing);
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn check_flight_reruns_after_ttl_expiry() {
        let flight = CheckFlight::new(Duration::ZERO, 2);
        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let runs = Arc::clone(&runs);
            let evidence = flight
                .get_or_run(
                    42,
                    check_targets("/tmp/check-expiry", "head-a"),
                    move |_| {
                        let runs = Arc::clone(&runs);
                        async move {
                            runs.fetch_add(1, Ordering::SeqCst);
                            Ok(CheckEvidence::Passed)
                        }
                    },
                )
                .await
                .expect("expired check result");
            assert_eq!(evidence, CheckEvidence::Passed);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn timed_out_readiness_check_is_not_produced_and_releases_the_runner_permit() {
        let root = tempfile::tempdir().expect("temporary check fixture");
        let flight = CheckFlight::new(Duration::ZERO, 1);
        let hanging_check = crate::check::Check {
            name: "hang".to_string(),
            program: "sleep".to_string(),
            args: vec!["30".to_string()],
        };
        let timed_out_path = root.path().to_path_buf();
        let timed_out_checks = vec![hanging_check];
        let timed_out = tokio::time::timeout(
            Duration::from_secs(1),
            flight.get_or_run(
                81,
                check_targets("/tmp/check-timeout", "head-a"),
                move |_| async move {
                    run_checks_with_timeout(
                        timed_out_path.as_path(),
                        &timed_out_checks,
                        Duration::from_millis(25),
                    )
                    .await
                },
            ),
        )
        .await
        .expect("timed-out check returns before the outer deadline")
        .expect("timed-out check result");
        assert_eq!(timed_out, CheckEvidence::NotProduced);

        let released = tokio::time::timeout(
            Duration::from_millis(250),
            flight.get_or_run(
                82,
                check_targets("/tmp/check-after-timeout", "head-a"),
                |_| async { Ok(CheckEvidence::Passed) },
            ),
        )
        .await
        .expect("timed-out runner released its global permit")
        .expect("post-timeout check result");
        assert_eq!(released, CheckEvidence::Passed);
    }

    #[tokio::test]
    async fn drifted_reconciliation_skips_the_check_flight() {
        let flight = Arc::new(CheckFlight::new(Duration::ZERO, 1));
        let runs = Arc::new(AtomicUsize::new(0));
        let skipped_flight = Arc::clone(&flight);
        let skipped_runs = Arc::clone(&runs);
        let evidence = checks_after_reconciliation(ExecutionReconciliation::Drifted, move || {
            let flight = Arc::clone(&skipped_flight);
            let runs = Arc::clone(&skipped_runs);
            async move {
                flight
                    .get_or_run(83, check_targets("/tmp/check-drift", "head-a"), move |_| {
                        let runs = Arc::clone(&runs);
                        async move {
                            runs.fetch_add(1, Ordering::SeqCst);
                            Ok(CheckEvidence::Passed)
                        }
                    })
                    .await
            }
        })
        .await
        .expect("drift check decision");
        assert_eq!(evidence, CheckEvidence::NotApplicable);
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        let released = tokio::time::timeout(
            Duration::from_millis(250),
            flight.get_or_run(
                84,
                check_targets("/tmp/check-after-drift", "head-a"),
                |_| async { Ok(CheckEvidence::Passed) },
            ),
        )
        .await
        .expect("drift skip did not retain a runner permit")
        .expect("post-drift check result");
        assert_eq!(released, CheckEvidence::Passed);
    }

    fn record_max(maximum: &AtomicUsize, candidate: usize) {
        let mut current = maximum.load(Ordering::SeqCst);
        while candidate > current {
            match maximum.compare_exchange_weak(
                current,
                candidate,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    #[tokio::test]
    async fn check_flight_limits_global_runners_to_two() {
        let flight = Arc::new(CheckFlight::new(Duration::ZERO, 2));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let tasks = (1..=5).map(|direction_id| {
            let flight = Arc::clone(&flight);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            async move {
                flight
                    .get_or_run(
                        direction_id,
                        check_targets(&format!("/tmp/check-{direction_id}"), "head-a"),
                        move |_| {
                            let active = Arc::clone(&active);
                            let maximum = Arc::clone(&maximum);
                            async move {
                                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                                record_max(&maximum, current);
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                active.fetch_sub(1, Ordering::SeqCst);
                                Ok(CheckEvidence::Passed)
                            }
                        },
                    )
                    .await
            }
        });
        let results = futures::future::join_all(tasks).await;
        for result in results {
            assert_eq!(result.expect("limited check result"), CheckEvidence::Passed);
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }
}
