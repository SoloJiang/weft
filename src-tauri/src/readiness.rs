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
//! | parseable `confirmed` proposal, `decision == ""` with `direction_id != 0` | the materialized direction with `AllowedByPolicy` for idempotent confirm/re-dispatch |
//! | parseable `proposed` proposal, `decision == ""` with `direction_id != 0` | the materialized direction with `NeedsGate`, including a reverted automatic reuse approval |
//! | parseable proposal, `decision == "approved"` with `direction_id != 0` | the materialized direction with `AllowedByPolicy` |
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
//! issue ready on its own. Likewise, an open durable BusRegistry human ask or
//! ephemeral AskRegistry permission ask whose scope cannot be mapped to a
//! persisted direction is collected as one virtual `issue ask` lane. `lead`
//! and the legacy empty scope are issue-level identities; an unknown
//! non-numeric or dangling numeric scope is treated the same way fail-closed,
//! rather than being dropped.
//!
//! | Lane facts, evaluated in this exact first-match order | Lane readiness | Reason |
//! | --- | --- | --- |
//! | inactive/cancelled, or policy denied | omitted | — |
//! | any repository slot's latest worker turn ended `error` | Failed | WorkerFailed |
//! | answerable BusRegistry or AskRegistry ask is open for the direction | NeedsYou | OpenNeed |
//! | policy needs a human gate | NeedsYou | PolicyGatePending |
//! | any repository slot's latest worker session is `running`, `starting`, or `stopped` while direction status is `review` or `done` | Unknown | InProgress |
//! | no worker is active, at least one tracked PR is `merged`, and the deterministic reduction of every tracked PR is clear | ReviewReady | — |
//! | worktree/branch reconciliation drifted | Blocked | ExecutionDrifted |
//! | an inferred check failed for a claimed-complete lane | Blocked | ChecksFailing |
//! | upstream evidence is Unmet (including a pending or unregistered upstream PR) | Blocked | UpstreamUnmet |
//! | upstream evidence is Unknown | Unknown | RemoteUnknown |
//! | tracked PR is closed without merge | Blocked | PrClosedUnmerged |
//! | tracked PR probe failed or lifecycle is unknown | Unknown | RemoteUnknown |
//! | tracked open PR has no valid successful timestamp, its snapshot is older than its TTL, or PR sweeping is disabled | Unknown | RemoteUnknown |
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
//! | checks were never produced for a claimed-complete lane (including an inferred zero-rung suite or a cache-only read with no fresh entry) | Unknown | ChecksUnknown |
//! | direction status is `review` or `done` | ReviewReady | — |
//! | direction status is `queued`, `planning`, or `working` | Unknown | InProgress |
//!
//! A CLI-reported `review`/`done` state is intentionally last: it cannot
//! override drift, remote uncertainty, or missing checks. A lane without a PR
//! skips the PR rows entirely; a PR is not a prerequisite for single-repo
//! review readiness. A latest worker session in any repository slot that still
//! occupies a claimed-complete lane (`running`, `starting`, or
//! terminal-takeover `stopped`) instead yields `Unknown[InProgress]`: that
//! state is neither a failed check nor missing remote evidence, and no verifier
//! may read its intermediate checkout. A
//! merged tracked PR is different only when every tracked PR row is clear:
//! after the three human gates above and with no active worker, that all-clear
//! merge set is terminal delivery evidence and makes the lane `ReviewReady`
//! even when its old worktree has been reclaimed or a predecessor remains
//! pending. A failing, conflicting, closed-unmerged, or unknown sibling PR
//! remains in the normal deterministic reduction and cannot be bypassed. An
//! unresolved worker failure, open ask, or policy gate is still reported first
//! because it needs a human response.
//!
//! Check collection is intentionally gated by claimed completion: only
//! `review` and `done` lanes invoke inferred checks. The collector records
//! `NotApplicable` for `queued`, `planning`, and `working` lanes; that evidence
//! skips both check-failing and check-unknown verdict rows. Before it invokes a
//! check runner, collection also short-circuits every decisive first-match gate:
//! worker failure, an open direction ask, a policy gate, an active worker on a
//! claimed-complete lane, an all-clear merged PR set, or reconciliation drift.
//! It records `NotApplicable` for those lanes, because the winner is already
//! known and a changing or mismatched checkout is not a safe target for
//! build/test work. This matches `materialize::remove_direction_worktree`'s
//! worktree-occupancy boundary exactly: `running`, `starting`, and `stopped`.
//! `engine::persist_activity` persists `running` while a worker turn is active
//! and `idle` once it drains; the frontend-only `busy` push state is not a
//! stored session value. Terminal writes retry with bounded backoff. While the
//! app is live, collection also reconciles each current session against its
//! engine under a 250ms bounded lock wait, so an idle engine cannot remain
//! blocked forever by a stale durable `running` row; a missing engine falls
//! back to durable state and a contended engine fails active. `reviving` is
//! only a revive-operation label and is not persisted as `session.status`.
//! This retains check.rs's
//! worker-done-means-checks-green contract without turning ordinary in-progress
//! work into automatic build/test execution.
//! An inferred zero-rung suite is `NotProduced`, never `Passed`: no configured
//! check is missing verification evidence rather than proof of delivery.
//!
//! Manifest inference itself runs off the async executor with a one-second
//! deadline and a process-wide two-task cap. A timed-out blocking task keeps
//! its permit until it really exits, so stalled network/FUSE mounts cannot
//! accumulate unbounded manifest readers; unavailable inference produces
//! `NotProduced`. Readiness checks then run each inferred rung in
//! a kill-on-drop Tokio child with a 120-second per-rung deadline. Every check
//! and Git-signature child is configured and registered with
//! `proc_registry::Owner::probe`. On Unix, a timeout, child-wait failure, or
//! output-reader failure while the direct child may still be alive invokes
//! `proc_registry::reap_bounded`: it snapshots the living descendant tree and
//! kills every distinct PGID before waiting the root. Its 250ms deadline covers
//! the blocking process-table scan, tree walk, group signals, and child wait;
//! a slow platform scan runs off the async executor and returns incomplete
//! rather than holding a readiness permit indefinitely. Every inferred check
//! also receives one unique inherited file-descriptor marker. The parent keeps
//! every marker close-on-exec and clears that flag only in its selected probe,
//! so concurrent checks cannot inherit one another's ownership token. After
//! the direct child exits, macOS/Linux scan same-user open vnode identities and
//! kill every matching PID and process group, retaining ownership across
//! `fork`, `exec`, `setsid`, and PPID reparenting. Finding any background
//! process discards the check result as `NotProduced`; it can never be
//! published as a pass. A normal sweep runs on a bounded blocking worker. If a
//! check future is cancelled, marker Drop transfers its still-open vnode to
//! one dedicated cleanup thread, so process-table enumeration never blocks a
//! Tokio worker and delayed cleanup cannot confuse a reused inode. Tools that
//! deliberately close unknown inherited descriptors remain outside this
//! unprivileged contract; they cannot be given a cross-platform
//! cgroup/Job-style ownership guarantee by this process.
//! Only a completed bounded reap disarms the root-group guard. On incomplete
//! cleanup readiness keeps that guard armed and explicitly tears down the
//! direct child before its registration is dropped.
//! Non-Unix platforms retain the direct-child kill-on-drop fallback. A
//! completed failing rung remains `Failing` even if a later rung times out or
//! cannot be awaited. Completed stdout and stderr are read continuously into
//! one bounded 2000-byte tail buffer, so a noisy check cannot accumulate
//! unbounded output before its deadline. Completed output uses the same
//! combined-output tail convention as `CheckResult`.
//! The process-local single-flight cache is valid for at most 10 minutes and
//! only while its sorted worktree-path, branch, HEAD-SHA, and clean-worktree
//! signatures match the newly collected values; any branch or HEAD change
//! immediately reruns checks.
//! A dirty worktree invalidates any prior entry and is intentionally
//! ineligible for delivery-readiness evidence: a boolean dirty marker cannot
//! distinguish content changes that occur during an already-dirty run. The
//! explicit `verify_direction` path may still run those checks for working-
//! phase feedback, but a dirty checkout cannot make a Lane review-ready.
//! Run-allowed collection samples every clean target again after its runner
//! returns and before it publishes evidence: a failed post-run sample or any
//! path/branch/HEAD/dirty mismatch invalidates the memo and publishes `NotProduced`
//! to the leader and matching followers. Requests that observed the same dirty
//! signature while its one execution is still in flight share that execution
//! only; its result is discarded as soon as the flight completes. Each
//! worktree's HEAD, branch, and dirty-state facts come from one bounded Git
//! signature sample with a 15-second total deadline. It uses the same
//! Tokio-child, process-group, and kill-on-timeout discipline as check
//! execution, rather than a blocking `Command::output` call. A failed or
//! timed-out sample is unavailable evidence: reconciliation is unknown,
//! checks are `NotProduced`, and no old cache entry may be reused or written
//! as `Passed`.
//!
//! The desktop `issue_readiness` command collects with `RunAllowed`. The
//! read-only global `issue_status` bus tool instead uses `CachedOnly`. It starts
//! neither inferred checks nor Git signature probes, because `git status` may
//! execute a repository-configured fsmonitor hook. It consumes durable
//! worker/plan/PR/upstream facts and leaves local reconciliation unknown and
//! applicable checks `NotProduced`, so the answer stays fail-closed without
//! running repository code.
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
//! PR gate to continue. A merged row is clear for its own terminal axes, but
//! never clears a sibling row. Otherwise their row verdicts are reduced
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

use crate::ask::AskRegistry;
use crate::bus::BusRegistry;
use crate::host::{
    CiStatus, ConflictStatus, PrLifecycle, ReviewStatus, ThreadStatus, UpstreamStatus,
};
use crate::store::{
    entities::{direction, lead_message, plan, pull_request, session},
    repo, Db,
};
use anyhow::{anyhow, Result};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, Mutex as AsyncMutex, OwnedMutexGuard, Semaphore};

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
    /// Applicable verification has no usable result. This includes an inferred
    /// zero-rung suite and a cache-only collection without fresh evidence.
    NotProduced,
    Passed,
    Failing,
}

/// Whether this collection caller may start repository commands. Read-only
/// tool surfaces consume durable facts only: even `git status` can execute a
/// configured fsmonitor hook, so CachedOnly launches neither Git nor checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckExecution {
    RunAllowed,
    CachedOnly,
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
    pub worker_active: bool,
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
    // A merged lifecycle clears this row's terminal evidence, stronger than
    // every live axis. GitHub's real merged fixture has `mergeable: "UNKNOWN"`
    // and an empty review decision after GitHub stops computing them, so
    // applying the open PR axes here would turn a completed delivery back into
    // RemoteUnknown. Sibling rows are still reduced independently below.
    if pr.lifecycle == Some(PrLifecycle::Merged) {
        return None;
    }

    // A closed lifecycle is terminal, actionable evidence even when the
    // partial live probe that refreshed another axis failed. The host monitor
    // can persist `closed` from its scalar PR probe after its review-thread
    // probe failed; later sweeps exclude closed rows, so treating that row as
    // generic RemoteUnknown would otherwise leave it stranded indefinitely.
    if pr.lifecycle == Some(PrLifecycle::Closed) {
        return Some((LaneReadiness::Blocked, ReasonCode::PrClosedUnmerged));
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

fn has_merged_pull_request(pull_requests: &[PullRequestFacts]) -> bool {
    pull_requests
        .iter()
        .any(|pr| pr.lifecycle == Some(PrLifecycle::Merged))
}

fn has_all_clear_merged_pull_request(
    pull_requests: &[PullRequestFacts],
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
) -> bool {
    has_merged_pull_request(pull_requests)
        && pull_request_verdict(pull_requests, open_pr_snapshot_freshness).is_none()
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
    if worker_active_preempts_completion(facts.worker_active, facts.direction_status.as_str()) {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Unknown,
            Some(ReasonCode::InProgress),
        ));
    }
    // A merge is terminal only after the checkout is no longer occupied. A
    // worker may still be active while the stored lifecycle says `working`;
    // do not let the merged-row shortcut advertise readiness over that work.
    if facts.worker_active
        && has_all_clear_merged_pull_request(
            &facts.pull_requests,
            facts.open_pr_snapshot_freshness,
        )
    {
        return Some(lane_verdict(
            facts,
            LaneReadiness::Unknown,
            Some(ReasonCode::InProgress),
        ));
    }
    // Merge is terminal delivery evidence only when every tracked row is clear.
    // It intentionally comes after the human-action gates above, but before
    // local execution, predecessor, and live-host evidence that may disappear
    // after an official Done card has reclaimed its worktree.
    if has_all_clear_merged_pull_request(&facts.pull_requests, facts.open_pr_snapshot_freshness) {
        return Some(lane_verdict(facts, LaneReadiness::ReviewReady, None));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProposalPolicyPhase {
    Proposed,
    Confirmed,
}

enum PlannedLaneSource {
    /// No live plan policy applies: persisted directions retain the established
    /// single-repo/legacy allowed path.
    Legacy,
    /// A durable proposal whose phase and individual lane decisions are
    /// authoritative.
    Parsed {
        phase: ProposalPolicyPhase,
        lanes: Vec<crate::planner::ProposedDirection>,
    },
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
    let phase = match plan.status.as_str() {
        "proposed" => ProposalPolicyPhase::Proposed,
        "confirmed" => ProposalPolicyPhase::Confirmed,
        _ => return PlannedLaneSource::Unavailable,
    };
    let proposal = match serde_json::from_str::<crate::planner::Proposal>(&plan.proposal) {
        Ok(proposal) if !proposal.directions.is_empty() => proposal,
        Ok(_) | Err(_) => return PlannedLaneSource::Unavailable,
    };
    PlannedLaneSource::Parsed {
        phase,
        lanes: proposal.directions,
    }
}

fn direction_is_active(direction: &direction::Model) -> bool {
    !matches!(direction.status.as_str(), "inactive" | "cancelled")
}

fn direction_claimed_completion(status: &str) -> bool {
    matches!(status, "review" | "done")
}

/// The caller's reason for sampling verification targets. Readiness collection
/// is intentionally stricter than an explicit verification request: only a
/// claimed-complete lane may cause collection to start checks, while the
/// established worker-completion path can explicitly verify an idle `working`
/// lane before its lifecycle advances to `review`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerificationTargetPurpose {
    ReadinessCollection,
    ExplicitDirectionVerification,
}

impl VerificationTargetPurpose {
    fn allows(self, direction_status: &str) -> bool {
        match self {
            Self::ReadinessCollection => direction_claimed_completion(direction_status),
            Self::ExplicitDirectionVerification => {
                direction_claimed_completion(direction_status) || direction_status == "working"
            }
        }
    }
}

fn worker_active_preempts_completion(worker_active: bool, direction_status: &str) -> bool {
    worker_active && direction_claimed_completion(direction_status)
}

fn proposal_lane_policy(
    phase: ProposalPolicyPhase,
    proposed_lane: &crate::planner::ProposedDirection,
) -> PolicyDecision {
    match proposed_lane.decision.as_str() {
        "denied" => PolicyDecision::Denied,
        "" if proposed_lane.direction_id == 0 => PolicyDecision::NeedsGate,
        "" if phase == ProposalPolicyPhase::Confirmed => PolicyDecision::AllowedByPolicy,
        "approved" => PolicyDecision::AllowedByPolicy,
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
        worker_active: false,
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
const CHECK_INFERENCE_TIMEOUT: Duration = Duration::from_secs(1);
const GIT_SIGNATURE_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const BOUNDED_PROCESS_REAP_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_CONCURRENT_CHECK_RUNNERS: usize = 2;
const MAX_CONCURRENT_CHECK_INFERENCES: usize = 2;
const MAX_CONCURRENT_GIT_PROBES: usize = 4;
const MAX_CONCURRENT_MARKER_SWEEPS: usize = 2;
const MARKER_SWEEP_TIMEOUT: Duration = Duration::from_millis(250);
const CHECK_OUTPUT_TAIL_BYTES: usize = 2_000;
const CHECK_OUTPUT_READ_BUFFER_BYTES: usize = 8 * 1024;
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

/// Executable verification results per write repo of a direction (§4.13).
///
/// This stays serializable because the explicit `verify_direction` IPC command
/// returns it directly. Readiness and explicit verification share this report
/// shape so one bounded execution can satisfy both callers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepoChecks {
    pub repo: String,
    pub worktree: String,
    pub checks: Vec<crate::check::CheckResult>,
}

#[derive(Clone)]
struct CachedCheckEvidence {
    collected_at: Instant,
    path_signature: Vec<String>,
    branch_signature: Vec<String>,
    head_signature: Vec<String>,
    dirty_signature: Vec<bool>,
    report: VerificationReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckTarget {
    repo: String,
    path: String,
    branch: String,
    head_sha: String,
    dirty: bool,
}

/// The complete local verification publication shared by explicit verification
/// and readiness. `evidence` keeps readiness's fail-closed reduction while
/// `repo_checks` preserves the established explicit-command IPC payload.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VerificationReport {
    evidence: CheckEvidence,
    repo_checks: Vec<RepoChecks>,
}

impl VerificationReport {
    fn from_evidence(evidence: CheckEvidence) -> Self {
        Self {
            evidence,
            repo_checks: Vec::new(),
        }
    }

    fn not_produced() -> Self {
        Self::from_evidence(CheckEvidence::NotProduced)
    }
}

type SharedCheckResult = std::result::Result<VerificationReport, String>;

struct InflightCheck {
    targets: Vec<CheckTarget>,
    receiver: watch::Receiver<Option<SharedCheckResult>>,
}

enum CheckFlightClaim<'a> {
    Leader(CheckFlightLeader<'a>),
    Follower(watch::Receiver<Option<SharedCheckResult>>),
    WaitForDifferentTargets(watch::Receiver<Option<SharedCheckResult>>),
}

/// Removes and resolves a claimed flight if its owner is cancelled. The
/// bounded child runner handles process cleanup; this guard makes sure peers
/// are not left waiting on a direction flight that will never publish.
struct CheckFlightLeader<'a> {
    flight: &'a CheckFlight,
    direction_id: i32,
    targets: Vec<CheckTarget>,
    sender: Option<watch::Sender<Option<SharedCheckResult>>>,
}

/// Holds the per-direction verification admission gate. A later engine-start
/// phase can acquire this guard without depending on runner internals.
pub struct VerificationAdmission {
    _guard: OwnedMutexGuard<()>,
}

impl CheckFlightLeader<'_> {
    fn finish(mut self, result: SharedCheckResult) -> Result<()> {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Some(result));
        }
        self.flight
            .remove_inflight(self.direction_id, &self.targets)
    }
}

impl Drop for CheckFlightLeader<'_> {
    fn drop(&mut self) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        let _ = sender.send(Some(Err(
            "readiness check flight was cancelled before publishing evidence".to_string(),
        )));
        let _ = self
            .flight
            .remove_inflight(self.direction_id, &self.targets);
    }
}

/// Process-local coordination for every local verification caller. The cache
/// is keyed by direction id and records the worktree path, branch, HEAD-SHA,
/// and dirty signatures that produced it so a recreated, switched, or newly
/// committed checkout cannot inherit prior evidence.
struct CheckFlight {
    cache: Mutex<HashMap<i32, CachedCheckEvidence>>,
    inflight: Mutex<HashMap<i32, InflightCheck>>,
    direction_locks: Mutex<HashMap<i32, Arc<AsyncMutex<()>>>>,
    runner_limit: Semaphore,
    ttl: Duration,
}

impl CheckFlight {
    fn new(ttl: Duration, max_concurrent_runners: usize) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            direction_locks: Mutex::new(HashMap::new()),
            runner_limit: Semaphore::new(max_concurrent_runners),
            ttl,
        }
    }

    fn signatures(targets: &[CheckTarget]) -> (Vec<String>, Vec<String>, Vec<String>, Vec<bool>) {
        let paths = targets.iter().map(|target| target.path.clone()).collect();
        let branches = targets.iter().map(|target| target.branch.clone()).collect();
        let heads = targets
            .iter()
            .map(|target| target.head_sha.clone())
            .collect();
        let dirty = targets.iter().map(|target| target.dirty).collect();
        (paths, branches, heads, dirty)
    }

    fn sort_targets(targets: &mut [CheckTarget]) {
        targets.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.branch.cmp(&right.branch))
                .then(left.head_sha.cmp(&right.head_sha))
                .then(left.dirty.cmp(&right.dirty))
                .then(left.repo.cmp(&right.repo))
        });
    }

    /// Repo display names are not verification identity. They may be renamed
    /// without changing the checkout a runner touched; path/branch/HEAD/dirty
    /// are the complete publish and cache boundary.
    fn targets_match(left: &[CheckTarget], right: &[CheckTarget]) -> bool {
        left.len() == right.len()
            && left.iter().zip(right).all(|(left, right)| {
                left.path == right.path
                    && left.branch == right.branch
                    && left.head_sha == right.head_sha
                    && left.dirty == right.dirty
            })
    }

    fn matching_cached_report(
        entry: &CachedCheckEvidence,
        path_signature: &[String],
        branch_signature: &[String],
        head_signature: &[String],
        dirty_signature: &[bool],
        ttl: Duration,
    ) -> Option<VerificationReport> {
        let is_fresh = Instant::now().saturating_duration_since(entry.collected_at) < ttl;
        if entry.path_signature == path_signature
            && entry.branch_signature == branch_signature
            && entry.head_signature == head_signature
            && entry.dirty_signature == dirty_signature
            && is_fresh
        {
            return Some(entry.report.clone());
        }
        None
    }

    fn cached(
        &self,
        direction_id: i32,
        targets: &[CheckTarget],
    ) -> Result<Option<VerificationReport>> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("readiness check cache lock poisoned"))?;
        let (path_signature, branch_signature, head_signature, dirty_signature) =
            Self::signatures(targets);
        // A dirty worktree may have changed after the cached pass, even when
        // HEAD did not. Invalidate rather than merely skip it so a later clean
        // status cannot resurrect evidence from before the dirty interval.
        if dirty_signature.iter().any(|dirty| *dirty) {
            cache.remove(&direction_id);
            return Ok(None);
        }
        let Some(entry) = cache.get(&direction_id) else {
            return Ok(None);
        };
        Ok(Self::matching_cached_report(
            entry,
            &path_signature,
            &branch_signature,
            &head_signature,
            &dirty_signature,
            self.ttl,
        ))
    }

    /// A read-only cache lookup for tools whose authority excludes starting or
    /// invalidating verification work. A dirty target simply cannot match;
    /// unlike `cached`, this leaves the process cache untouched.
    fn cached_read_only(
        &self,
        direction_id: i32,
        targets: &[CheckTarget],
    ) -> Result<Option<VerificationReport>> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("readiness check cache lock poisoned"))?;
        let (path_signature, branch_signature, head_signature, dirty_signature) =
            Self::signatures(targets);
        if dirty_signature.iter().any(|dirty| *dirty) {
            return Ok(None);
        }
        let Some(entry) = cache.get(&direction_id) else {
            return Ok(None);
        };
        Ok(Self::matching_cached_report(
            entry,
            &path_signature,
            &branch_signature,
            &head_signature,
            &dirty_signature,
            self.ttl,
        ))
    }

    /// Drop every memo for a direction after a run no longer matches the
    /// signature it started from. A later caller must establish fresh evidence
    /// instead of reviving a result from before the mutation interval.
    fn invalidate_cached(&self, direction_id: i32) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("readiness check cache lock poisoned"))?;
        cache.remove(&direction_id);
        Ok(())
    }

    fn claim_inflight(
        &self,
        direction_id: i32,
        targets: &[CheckTarget],
    ) -> Result<CheckFlightClaim<'_>> {
        let mut inflight = self
            .inflight
            .lock()
            .map_err(|_| anyhow!("readiness check flight map lock poisoned"))?;
        if let Some(existing) = inflight.get(&direction_id) {
            if Self::targets_match(&existing.targets, targets) {
                return Ok(CheckFlightClaim::Follower(existing.receiver.clone()));
            }
            return Ok(CheckFlightClaim::WaitForDifferentTargets(
                existing.receiver.clone(),
            ));
        }

        let (sender, receiver) = watch::channel::<Option<SharedCheckResult>>(None);
        inflight.insert(
            direction_id,
            InflightCheck {
                targets: targets.to_vec(),
                receiver,
            },
        );
        Ok(CheckFlightClaim::Leader(CheckFlightLeader {
            flight: self,
            direction_id,
            targets: targets.to_vec(),
            sender: Some(sender),
        }))
    }

    fn remove_inflight(&self, direction_id: i32, targets: &[CheckTarget]) -> Result<()> {
        let mut inflight = self
            .inflight
            .lock()
            .map_err(|_| anyhow!("readiness check flight map lock poisoned"))?;
        let should_remove = inflight
            .get(&direction_id)
            .is_some_and(|existing| Self::targets_match(&existing.targets, targets));
        if should_remove {
            inflight.remove(&direction_id);
        }
        Ok(())
    }

    fn shared_result(result: SharedCheckResult) -> Result<VerificationReport> {
        match result {
            Ok(evidence) => Ok(evidence),
            Err(message) => Err(anyhow!(message)),
        }
    }

    async fn wait_for_inflight(
        mut receiver: watch::Receiver<Option<SharedCheckResult>>,
    ) -> Result<VerificationReport> {
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return Self::shared_result(result);
            }
            if receiver.changed().await.is_err() {
                if let Some(result) = receiver.borrow().clone() {
                    return Self::shared_result(result);
                }
                return Err(anyhow!(
                    "readiness check flight ended without publishing evidence"
                ));
            }
        }
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

    async fn acquire_admission(&self, direction_id: i32) -> Result<VerificationAdmission> {
        let direction_lock = self.direction_lock(direction_id)?;
        Ok(VerificationAdmission {
            _guard: direction_lock.lock_owned().await,
        })
    }

    /// Cached-only callers have no authority to wait behind a worker start or
    /// a verification runner. They may read a memo only when they can join the
    /// same direction admission critical section immediately.
    fn cached_read_only_if_admitted(
        &self,
        direction_id: i32,
        targets: &[CheckTarget],
    ) -> Result<Option<VerificationReport>> {
        let direction_lock = self.direction_lock(direction_id)?;
        let Ok(_admission) = direction_lock.try_lock_owned() else {
            return Ok(None);
        };
        self.cached_read_only(direction_id, targets)
    }

    fn cache_result(
        &self,
        direction_id: i32,
        targets: &[CheckTarget],
        report: &VerificationReport,
    ) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("readiness check cache lock poisoned"))?;
        let (path_signature, branch_signature, head_signature, dirty_signature) =
            Self::signatures(targets);
        if dirty_signature.iter().any(|dirty| *dirty)
            || matches!(
                report.evidence,
                CheckEvidence::NotProduced | CheckEvidence::NotApplicable
            )
        {
            cache.remove(&direction_id);
            return Ok(());
        }
        cache.insert(
            direction_id,
            CachedCheckEvidence {
                collected_at: Instant::now(),
                path_signature,
                branch_signature,
                head_signature,
                dirty_signature,
                report: report.clone(),
            },
        );
        Ok(())
    }

    #[cfg(test)]
    async fn get_or_run<F, Fut>(
        &self,
        direction_id: i32,
        targets: Vec<CheckTarget>,
        runner: F,
    ) -> Result<CheckEvidence>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = Result<CheckEvidence>>,
    {
        let stable_targets = targets.clone();
        self.get_or_run_with_post_targets(direction_id, targets, runner, move |_| async move {
            Ok(stable_targets)
        })
        .await
    }

    /// Test-facing verdict wrapper around the shared report runner.
    #[cfg(test)]
    async fn get_or_run_with_post_targets<F, Fut, S, SFut>(
        &self,
        direction_id: i32,
        targets: Vec<CheckTarget>,
        runner: F,
        post_targets: S,
    ) -> Result<CheckEvidence>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: Future<Output = Result<CheckEvidence>>,
        S: FnOnce(Vec<CheckTarget>) -> SFut,
        SFut: Future<Output = Result<Vec<CheckTarget>>>,
    {
        let admission_targets = targets.clone();
        let report = self
            .get_or_run_report_with_admission_and_post_targets(
                direction_id,
                targets,
                move |targets| {
                    let paths = targets
                        .into_iter()
                        .map(|target| target.path)
                        .collect::<Vec<_>>();
                    async move {
                        runner(paths)
                            .await
                            .map(VerificationReport::from_evidence)
                    }
                },
                move |_| async move { Ok(admission_targets) },
                post_targets,
            )
            .await?;
        Ok(report.evidence)
    }

    /// Run verification under the per-direction admission gate, then validate
    /// that every target is still the exact path/branch/HEAD/dirty signature
    /// the runner observed. The runner permit intentionally lives only inside
    /// the execution block so bounded Git re-sampling cannot consume one of
    /// the two execution slots.
    async fn get_or_run_report_with_admission_and_post_targets<F, Fut, A, AFut, S, SFut>(
        &self,
        direction_id: i32,
        mut targets: Vec<CheckTarget>,
        runner: F,
        admission_targets: A,
        post_targets: S,
    ) -> Result<VerificationReport>
    where
        F: FnOnce(Vec<CheckTarget>) -> Fut,
        Fut: Future<Output = Result<VerificationReport>>,
        A: FnOnce(Vec<CheckTarget>) -> AFut,
        AFut: Future<Output = Result<Vec<CheckTarget>>>,
        S: FnOnce(Vec<CheckTarget>) -> SFut,
        SFut: Future<Output = Result<Vec<CheckTarget>>>,
    {
        Self::sort_targets(&mut targets);
        loop {
            match self.claim_inflight(direction_id, &targets)? {
                CheckFlightClaim::Follower(receiver) => {
                    return Self::wait_for_inflight(receiver).await;
                }
                CheckFlightClaim::WaitForDifferentTargets(receiver) => {
                    // A different worktree signature is currently running for
                    // this direction. It cannot contribute evidence to this
                    // caller, but waiting preserves the per-direction runner
                    // serialization before this caller samples its own result.
                    let _ = Self::wait_for_inflight(receiver).await;
                }
                CheckFlightClaim::Leader(leader) => {
                    let _admission = self.acquire_admission(direction_id).await?;
                    let mut admitted_targets = match admission_targets(targets.clone()).await {
                        Ok(targets) => targets,
                        Err(_) => {
                            let shared_result = match self.invalidate_cached(direction_id) {
                                Ok(()) => Ok(VerificationReport::not_produced()),
                                Err(error) => Err(error.to_string()),
                            };
                            leader.finish(shared_result.clone())?;
                            return Self::shared_result(shared_result);
                        }
                    };
                    Self::sort_targets(&mut admitted_targets);
                    if !Self::targets_match(&admitted_targets, &targets) {
                        let shared_result = match self.invalidate_cached(direction_id) {
                            Ok(()) => Ok(VerificationReport::not_produced()),
                            Err(error) => Err(error.to_string()),
                        };
                        leader.finish(shared_result.clone())?;
                        return Self::shared_result(shared_result);
                    }
                    // Even a cache hit must match a target sample taken after
                    // this caller owns admission. Otherwise a branch switch or
                    // worker start in the initial-sample-to-admission gap could
                    // publish a memo for a checkout that no longer exists.
                    if let Some(report) = self.cached(direction_id, &targets)? {
                        let shared_result = Ok(report.clone());
                        leader.finish(shared_result)?;
                        return Ok(report);
                    }

                    let runner_result = {
                        let _runner_permit = self
                            .runner_limit
                            .acquire()
                            .await
                            .map_err(|_| anyhow!("readiness check runner semaphore closed"))?;
                        runner(targets.clone()).await
                    };
                    let shared_result = match runner_result {
                        Ok(report) => match post_targets(targets.clone()).await {
                            Ok(mut observed_targets) => {
                                Self::sort_targets(&mut observed_targets);
                                if Self::targets_match(&observed_targets, &targets) {
                                    match self.cache_result(direction_id, &targets, &report) {
                                        Ok(()) => Ok(report),
                                        Err(error) => Err(error.to_string()),
                                    }
                                } else {
                                    match self.invalidate_cached(direction_id) {
                                        Ok(()) => Ok(VerificationReport::not_produced()),
                                        Err(error) => Err(error.to_string()),
                                    }
                                }
                            }
                            Err(_) => match self.invalidate_cached(direction_id) {
                                Ok(()) => Ok(VerificationReport::not_produced()),
                                Err(error) => Err(error.to_string()),
                            },
                        },
                        Err(error) => Err(error.to_string()),
                    };
                    leader.finish(shared_result.clone())?;
                    return Self::shared_result(shared_result);
                }
            }
        }
    }
}

fn check_flight() -> &'static CheckFlight {
    static CHECK_FLIGHT: OnceLock<CheckFlight> = OnceLock::new();
    CHECK_FLIGHT.get_or_init(|| CheckFlight::new(CHECK_EVIDENCE_TTL, MAX_CONCURRENT_CHECK_RUNNERS))
}

/// Acquire the shared per-direction verification admission gate.
///
/// This is intentionally independent of `lead_chat::engine`: the next phase
/// can use the same gate to prevent a worker start from racing verification.
pub async fn acquire_verification_admission(
    direction_id: i32,
) -> Result<VerificationAdmission> {
    check_flight().acquire_admission(direction_id).await
}

/// Drop cached verification evidence for one direction.
///
/// Callers that mutate a verification target can invalidate before a later
/// `RunAllowed` collection establishes fresh evidence.
pub fn invalidate_verification_memo(direction_id: i32) -> Result<()> {
    check_flight().invalidate_cached(direction_id)
}

#[derive(Clone, Copy, Debug, Default)]
struct WorkerSessionFacts {
    failed: bool,
    active: bool,
}

fn worker_session_occupies_worktree(status: &str) -> bool {
    matches!(status, "running" | "starting" | "stopped")
}

fn reconciled_worker_activity(status: &str, live_worker_activity: Option<bool>) -> bool {
    live_worker_activity.unwrap_or_else(|| worker_session_occupies_worktree(status))
}

async fn latest_worker_facts(db: &Db, direction_id: i32) -> Result<WorkerSessionFacts> {
    // A multi-repo direction dispatches one worker per worktree. Sort every
    // session newest-first, then retain the first row for each repository slot
    // so an old error is superseded by a later retry in the same repository,
    // while a current worker in another repository cannot be hidden by the
    // direction's globally newest session.
    let sessions = session::Entity::find()
        .filter(session::Column::DirectionId.eq(direction_id))
        .order_by_desc(session::Column::Id)
        .all(&db.0)
        .await?;
    let mut seen_repo_ids = HashSet::new();
    let mut facts = WorkerSessionFacts::default();
    let current_sessions = sessions
        .into_iter()
        .filter(|session| seen_repo_ids.insert(session.repo_id))
        .collect::<Vec<_>>();
    // A contended engine lookup waits at most 250ms. Probe repository slots
    // concurrently so a multi-repo direction still has one total live-state
    // budget rather than multiplying that wait by its repository count.
    let live_sessions = futures::future::join_all(current_sessions.into_iter().map(|session| {
        async move {
            let activity = crate::lead_chat::engine::live_worker_activity(session.id).await;
            (session, activity)
        }
    }))
    .await;

    for (session, live_worker_activity) in live_sessions {

        // Match materialize::remove_direction_worktree's exact worktree
        // safety boundary: a worker can own the checkout while it is starting,
        // running, or taken over in a human terminal (stopped). `reviving` is
        // a revive operation label, not a persisted session.status value.
        // A live engine is the authoritative checkout owner for this process.
        // In particular, an idle override reconciles a durable `running` row
        // left by a terminal SQLite write failure. A live lookup waits briefly
        // for a serialized terminal transition, then fails active if the lock
        // remains contended. Without a live engine (including after restart),
        // the durable status remains authoritative.
        let active = reconciled_worker_activity(session.status.as_str(), live_worker_activity);
        if active {
            facts.active = true;
            // The active turn supersedes this same session's prior terminal
            // text. Until the new turn ends, an older error is not the latest
            // worker outcome for this repository slot.
            continue;
        }

        // engine::finalize_text_row persists the turn's terminal state by
        // calling repo::update_lead_message on an assistant/text row.
        // Assistant/tool rows describe individual tool calls, so their `error`
        // cannot diagnose the whole worker turn as failed.
        let latest = lead_message::Entity::find()
            .filter(lead_message::Column::SessionId.eq(session.id))
            .filter(lead_message::Column::Role.eq("assistant"))
            .filter(lead_message::Column::Kind.eq("text"))
            .order_by_desc(lead_message::Column::TurnId)
            .order_by_desc(lead_message::Column::Id)
            .one(&db.0)
            .await?;
        if latest.is_some_and(|message| message.status == "error") {
            facts.failed = true;
        }
    }

    Ok(facts)
}

/// One bounded source of local Git facts. Branch reconciliation and the
/// worktree signature for check-flight reuse share its deadline and failure
/// boundary, rather than independently sampling an unbounded Git command.
#[derive(Clone, Debug)]
struct GitSignatureProbe {
    program: PathBuf,
    timeout: Duration,
    /// Production probes share the process-wide cap. Tests that need to prove
    /// a child actually starts may inject an isolated cap so unrelated
    /// parallel tests cannot consume the whole deadline before spawn.
    limit: Option<Arc<Semaphore>>,
}

impl GitSignatureProbe {
    fn readiness() -> Self {
        Self {
            program: PathBuf::from("git"),
            timeout: GIT_SIGNATURE_PROBE_TIMEOUT,
            limit: None,
        }
    }

    async fn sample(&self, path: &Path) -> Result<GitWorktreeSignature> {
        // This is deliberately one total budget, including time queued behind
        // other board cards, rather than 15 seconds per subcommand. A large
        // portfolio and a slow fsmonitor therefore cannot create an unbounded
        // backlog of overlapping readiness refreshes.
        let deadline = tokio::time::Instant::now() + self.timeout;
        // Board cards and multi-lane collection may sample concurrently. Keep
        // the process fan-out globally bounded across every issue rather than
        // multiplying one Git child per card, lane, and worktree.
        let limit = self
            .limit
            .clone()
            .unwrap_or_else(|| Arc::clone(git_probe_limit()));
        let _permit = match tokio::time::timeout_at(deadline, limit.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(anyhow!("readiness Git probe semaphore closed")),
            Err(_) => return Err(anyhow!("readiness Git probe deadline elapsed while queued")),
        };
        // One inherited marker spans status/branch/HEAD. A configured
        // fsmonitor hook can daemonize and let Git's direct process exit, so
        // PGID/PPID cleanup alone is not sufficient for signature probes.
        let process_marker = ReadinessProbeMarker::create()?;
        let porcelain = run_bounded_git_command(
            path,
            self.program.as_path(),
            &["status", "--porcelain"],
            deadline,
            Some(&process_marker),
        )
        .await?;
        let branch = run_bounded_git_command(
            path,
            self.program.as_path(),
            &["rev-parse", "--abbrev-ref", "HEAD"],
            deadline,
            Some(&process_marker),
        )
        .await?;
        let head = run_bounded_git_command(
            path,
            self.program.as_path(),
            &["rev-parse", "HEAD"],
            deadline,
            Some(&process_marker),
        )
        .await?;

        let branch = required_git_probe_output(branch.stdout, "branch")?;
        let head_sha = required_git_probe_output(head.stdout, "HEAD")?;
        match sweep_readiness_probe_marker_before(deadline, process_marker).await {
            Some(0) => {}
            Some(escaped) => {
                anyhow::bail!(
                    "readiness Git probe left {escaped} inherited background process(es)"
                );
            }
            None => {
                anyhow::bail!("readiness Git process ownership sweep exceeded its deadline");
            }
        }

        Ok(GitWorktreeSignature {
            branch,
            head_sha,
            dirty: !porcelain.stdout.is_empty(),
        })
    }
}

fn git_probe_limit() -> &'static Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_GIT_PROBES)))
}

#[derive(Clone, Debug)]
struct GitWorktreeSignature {
    branch: String,
    head_sha: String,
    dirty: bool,
}

#[derive(Clone, Debug)]
struct ProbedWorktree {
    repo: String,
    stored_path: String,
    signature: Option<GitWorktreeSignature>,
}

async fn probe_worktrees_for_direction(
    db: &Db,
    direction_id: i32,
    git_probe: &GitSignatureProbe,
) -> Result<Vec<ProbedWorktree>> {
    let worktrees = repo::list_worktrees(db, Some(direction_id)).await?;
    let probes = worktrees.into_iter().map(|worktree| async move {
        let repo_name = repo::get_repo(db, worktree.repo_id)
            .await?
            .map(|repo| repo.name)
            .unwrap_or_else(|| format!("repo {}", worktree.repo_id));
        let stored_path = worktree.path;
        let signature = git_probe.sample(Path::new(&stored_path)).await.ok();
        Ok(ProbedWorktree {
            repo: repo_name,
            stored_path,
            signature,
        })
    });
    futures::future::join_all(probes).await.into_iter().collect()
}

fn reconciliation_for(
    direction: &direction::Model,
    worktrees: &[ProbedWorktree],
) -> ExecutionReconciliation {
    if worktrees.is_empty() {
        return match direction.status.as_str() {
            "queued" | "planning" => ExecutionReconciliation::Matched,
            _ => ExecutionReconciliation::Unknown,
        };
    }
    if direction.branch.trim().is_empty() {
        return ExecutionReconciliation::Unknown;
    }

    let mut unknown = false;
    for worktree in worktrees {
        let Some(signature) = worktree.signature.as_ref() else {
            unknown = true;
            continue;
        };
        if signature.branch != direction.branch {
            return ExecutionReconciliation::Drifted;
        }
    }

    if unknown {
        return ExecutionReconciliation::Unknown;
    }
    ExecutionReconciliation::Matched
}

fn check_targets_for_worktrees(worktrees: &[ProbedWorktree]) -> Option<Vec<CheckTarget>> {
    if worktrees.is_empty() {
        return None;
    }

    let mut targets = Vec::with_capacity(worktrees.len());
    for worktree in worktrees {
        let Some(signature) = worktree.signature.as_ref() else {
            // A failed or timed-out signature cannot safely inherit a cached
            // pass: unknown cleanliness means NotProduced, fail-closed.
            return None;
        };
        targets.push(CheckTarget {
            repo: worktree.repo.clone(),
            path: worktree.stored_path.clone(),
            branch: signature.branch.clone(),
            head_sha: signature.head_sha.clone(),
            dirty: signature.dirty,
        });
    }
    Some(targets)
}

/// Delivery readiness may publish check evidence only for a clean checkout.
/// A dirty boolean is enough to invalidate old cache entries, but not enough
/// to prove that dirty contents stayed unchanged while a runner was active.
/// Explicit verification bypasses this collector boundary and can still
/// provide working-phase feedback for uncommitted edits.
fn targets_support_delivery_evidence(targets: &[CheckTarget]) -> bool {
    targets.iter().all(|target| !target.dirty)
}

/// Re-read the direction's active-worker state, then fetch the currently
/// registered worktrees and sample their live Git signatures. Leaders call
/// this while holding the admission gate, so a queued verification cannot run
/// against a worker that started or a target that changed before it acquired
/// the per-direction turn.
async fn verification_targets_for_direction(
    db: &Db,
    direction_id: i32,
    git_probe: &GitSignatureProbe,
    purpose: VerificationTargetPurpose,
) -> Result<Vec<CheckTarget>> {
    let direction = repo::get_direction(db, direction_id)
        .await?
        .ok_or_else(|| anyhow!("direction {direction_id} no longer exists"))?;
    if !purpose.allows(direction.status.as_str()) {
        anyhow::bail!(
            "verification was not produced for direction {direction_id}: completion was withdrawn"
        );
    }
    let worker = latest_worker_facts(db, direction_id).await?;
    // Verification target occupancy is stricter than lane-verdict precedence:
    // a working lane may be explicitly verified only after every repository
    // worker is idle. `starting`, `running`, and terminal-takeover `stopped`
    // all own the checkout regardless of the direction lifecycle label.
    if worker.active {
        anyhow::bail!(
            "verification was not produced for direction {direction_id}: worker is active"
        );
    }
    let worktrees = probe_worktrees_for_direction(db, direction_id, git_probe).await?;
    match reconciliation_for(&direction, &worktrees) {
        ExecutionReconciliation::Matched => {}
        ExecutionReconciliation::Drifted => {
            anyhow::bail!(
                "verification was not produced for direction {direction_id}: worktree branch drifted"
            );
        }
        ExecutionReconciliation::Unknown => {
            anyhow::bail!(
                "verification was not produced for direction {direction_id}: worktree branch is unknown"
            );
        }
    }
    check_targets_for_worktrees(&worktrees).ok_or_else(|| {
        anyhow!(
            "verification was not produced for direction {direction_id}: \
             worktree target is unavailable"
        )
    })
}

/// Re-sample every target that a readiness runner just touched. A result is
/// publishable only when path, branch, HEAD, and dirty state still match.
#[cfg(test)]
async fn resample_check_targets(
    targets: Vec<CheckTarget>,
    git_probe: &GitSignatureProbe,
) -> Result<Vec<CheckTarget>> {
    let mut observed = Vec::with_capacity(targets.len());
    for target in targets {
        let repo = target.repo;
        let path = target.path;
        let signature = git_probe.sample(Path::new(&path)).await?;
        observed.push(CheckTarget {
            repo,
            path,
            branch: signature.branch,
            head_sha: signature.head_sha,
            dirty: signature.dirty,
        });
    }
    Ok(observed)
}

#[cfg(test)]
async fn checks_for_with_runner<F, Fut>(
    direction: &direction::Model,
    worktrees: &[ProbedWorktree],
    flight: &CheckFlight,
    execution: CheckExecution,
    runner: F,
) -> Result<CheckEvidence>
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
{
    checks_for_with_runner_and_post_targets(
        direction,
        worktrees,
        flight,
        execution,
        runner,
        |targets| async move { Ok(targets) },
    )
    .await
}

#[cfg(test)]
async fn checks_for_with_runner_and_post_targets<F, Fut, S, SFut>(
    direction: &direction::Model,
    worktrees: &[ProbedWorktree],
    flight: &CheckFlight,
    execution: CheckExecution,
    runner: F,
    post_targets: S,
) -> Result<CheckEvidence>
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
    S: FnOnce(Vec<CheckTarget>) -> SFut,
    SFut: Future<Output = Result<Vec<CheckTarget>>>,
{
    if !direction_claimed_completion(direction.status.as_str()) {
        return Ok(CheckEvidence::NotApplicable);
    }

    let Some(targets) = check_targets_for_worktrees(worktrees) else {
        return Ok(CheckEvidence::NotProduced);
    };
    if !targets_support_delivery_evidence(&targets) {
        return Ok(CheckEvidence::NotProduced);
    }
    checks_for_targets_with_runner_and_post_targets(
        flight,
        execution,
        direction.id,
        targets,
        runner,
        post_targets,
    )
    .await
}

/// Apply the caller's authority boundary after the inexpensive target facts
/// have been gathered. Cached-only callers deliberately do not wait on an
/// in-flight runner: a pending execution is not persisted verification
/// evidence for a read-only request.
#[cfg(test)]
async fn checks_for_targets_with_runner<F, Fut>(
    flight: &CheckFlight,
    execution: CheckExecution,
    direction_id: i32,
    targets: Vec<CheckTarget>,
    runner: F,
) -> Result<CheckEvidence>
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
{
    checks_for_targets_with_runner_and_post_targets(
        flight,
        execution,
        direction_id,
        targets,
        runner,
        |targets| async move { Ok(targets) },
    )
    .await
}

#[cfg(test)]
async fn checks_for_targets_with_runner_and_post_targets<F, Fut, S, SFut>(
    flight: &CheckFlight,
    execution: CheckExecution,
    direction_id: i32,
    mut targets: Vec<CheckTarget>,
    runner: F,
    post_targets: S,
) -> Result<CheckEvidence>
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
    S: FnOnce(Vec<CheckTarget>) -> SFut,
    SFut: Future<Output = Result<Vec<CheckTarget>>>,
{
    CheckFlight::sort_targets(&mut targets);
    match execution {
        CheckExecution::RunAllowed => {
            flight
                .get_or_run_with_post_targets(direction_id, targets, runner, post_targets)
                .await
        }
        CheckExecution::CachedOnly => Ok(flight
            .cached_read_only_if_admitted(direction_id, &targets)?
            .map(|report| report.evidence)
            .unwrap_or(CheckEvidence::NotProduced)),
    }
}

/// A single, bounded combined stdout/stderr tail. Readers append as bytes
/// arrive; exact cross-pipe ordering is not observable from separate pipes,
/// but the captured content and memory bound are stable.
struct OutputTailBuffer {
    bytes: VecDeque<u8>,
    max_bytes: usize,
    truncated: bool,
}

impl OutputTailBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(max_bytes),
            max_bytes,
            truncated: false,
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.max_bytes == 0 {
            self.truncated = true;
            return;
        }
        if chunk.len() >= self.max_bytes {
            let discarded = chunk.len() > self.max_bytes || !self.bytes.is_empty();
            self.bytes.clear();
            let retained = &chunk[chunk.len() - self.max_bytes..];
            self.bytes.extend(retained.iter().copied());
            self.truncated |= discarded;
            return;
        }

        let overflow = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.max_bytes);
        if overflow > 0 {
            self.truncated = true;
            for _ in 0..overflow {
                let _ = self.bytes.pop_front();
            }
        }
        self.bytes.extend(chunk.iter().copied());
    }

    fn render(&self) -> String {
        let bytes: Vec<u8> = self.bytes.iter().copied().collect();
        let output = String::from_utf8_lossy(&bytes);
        if self.truncated {
            // Match CheckResult's old tail convention: do not surface a
            // leading fragment of a line when the ring has discarded bytes.
            let complete_line = output
                .find('\n')
                .map(|index| &output[index + 1..])
                .unwrap_or(output.as_ref());
            return format!("…\n{}", complete_line.trim_end());
        }
        output.trim_end().to_string()
    }
}

type SharedOutputTail = Arc<Mutex<OutputTailBuffer>>;

async fn drain_check_output<R>(mut reader: R, output_tail: SharedOutputTail) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = [0_u8; CHECK_OUTPUT_READ_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| anyhow!("could not read readiness check output: {error}"))?;
        if read == 0 {
            return Ok(());
        }
        {
            let mut output_tail = output_tail
                .lock()
                .map_err(|_| anyhow!("readiness check output buffer lock poisoned"))?;
            output_tail.append(&buffer[..read]);
        }
    }
}

async fn wait_for_check_output(
    reader: &mut tokio::task::JoinHandle<Result<()>>,
    stream: &str,
) -> Result<()> {
    reader
        .await
        .map_err(|error| anyhow!("readiness check {stream} reader task failed: {error}"))?
}

fn rendered_check_output_tail(output_tail: &SharedOutputTail) -> Result<String> {
    let output_tail = output_tail
        .lock()
        .map_err(|_| anyhow!("readiness check output buffer lock poisoned"))?;
    Ok(output_tail.render())
}

enum BoundedCheckOutcome {
    Completed(crate::check::CheckResult),
    NotProduced { output_tail: String },
}

/// A per-check ownership marker that survives `setsid` and PPID reparenting.
/// Normal completion performs an explicit sweep before publishing evidence;
/// Drop transfers cancellation cleanup to a dedicated blocking thread.
struct ReadinessProbeMarker(crate::proc_registry::InheritedProcessMarker);

impl ReadinessProbeMarker {
    fn create() -> Result<Self> {
        let marker = crate::proc_registry::InheritedProcessMarker::create("readiness-check")
            .map_err(|error| anyhow!("could not create readiness process marker: {error}"))?;
        Ok(Self(marker))
    }

    fn attach_to(&self, command: &mut tokio::process::Command) -> Result<()> {
        self.0
            .attach(command)
            .map_err(|error| anyhow!("could not attach readiness process marker: {error}"))
    }

    fn attach(command: &mut tokio::process::Command) -> Result<Self> {
        let marker = Self::create()?;
        marker.attach_to(command)?;
        Ok(marker)
    }

    fn sweep_and_disarm(&mut self) -> usize {
        self.0.sweep_and_disarm()
    }
}

fn marker_sweep_limit() -> &'static Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_MARKER_SWEEPS)))
}

async fn run_bounded_marker_sweep_with<F>(
    outer_deadline: tokio::time::Instant,
    limit: Arc<Semaphore>,
    sweep: F,
) -> Option<usize>
where
    F: FnOnce() -> usize + Send + 'static,
{
    let now = tokio::time::Instant::now();
    let sweep_deadline = std::cmp::min(outer_deadline, now + MARKER_SWEEP_TIMEOUT);
    if now >= sweep_deadline {
        return None;
    }
    let permit = match tokio::time::timeout_at(sweep_deadline, limit.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => return None,
    };
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        sweep()
    });
    match tokio::time::timeout_at(sweep_deadline, task).await {
        Ok(Ok(killed)) => Some(killed),
        Ok(Err(_)) | Err(_) => None,
    }
}

async fn sweep_readiness_probe_marker_before(
    deadline: tokio::time::Instant,
    mut marker: ReadinessProbeMarker,
) -> Option<usize> {
    run_bounded_marker_sweep_with(deadline, Arc::clone(marker_sweep_limit()), move || {
        marker.sweep_and_disarm()
    })
    .await
}

/// A Unix readiness subprocess owns a fresh process group. Keep the group
/// armed until every return path has swept it: a direct child can exit normally
/// while a redirected background descendant remains in that group.
#[cfg(unix)]
struct BoundedReadinessProcessGroup {
    pgid: Option<i32>,
}

#[cfg(unix)]
impl BoundedReadinessProcessGroup {
    fn from_child_id(child_id: Option<u32>) -> Self {
        Self {
            pgid: child_id.map(|pid| pid as i32),
        }
    }

    fn kill(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        crate::proc_registry::kill_group(pgid);
    }

    fn kill_but_keep_armed(&self) {
        let Some(pgid) = self.pgid else {
            return;
        };
        crate::proc_registry::kill_group(pgid);
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

#[cfg(unix)]
impl Drop for BoundedReadinessProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Apply the bounded reaper's ownership result. An incomplete tree cleanup is
/// deliberately not permission to disarm the root-group fallback: the bounded
/// registry call may have returned before it could discover an escaped group.
#[cfg(unix)]
fn settle_bounded_reap_outcome(
    process_group: &mut BoundedReadinessProcessGroup,
    outcome: crate::proc_registry::BoundedReapOutcome,
) -> bool {
    match outcome {
        crate::proc_registry::BoundedReapOutcome::Completed => {
            process_group.disarm();
            true
        }
        crate::proc_registry::BoundedReapOutcome::Incomplete => {
            // The registry already signalled this group as its mandatory
            // fallback. Signal again defensively, but keep the guard armed for
            // future cancellation and this frame's Drop path.
            process_group.kill_but_keep_armed();
            false
        }
    }
}

#[cfg(unix)]
async fn reap_bounded_readiness_process(
    child: &mut tokio::process::Child,
    registration: &mut Option<crate::proc_registry::Registration>,
    process_group: &mut BoundedReadinessProcessGroup,
) {
    // Keep the registration alive until the bounded reaper has actually waited
    // the direct child. On an incomplete outcome, direct teardown is explicit
    // and the armed root group survives until this frame drops.
    let outcome = match registration.as_ref() {
        Some(registration) => {
            crate::proc_registry::reap_bounded(child, registration, BOUNDED_PROCESS_REAP_TIMEOUT)
                .await
        }
        None => {
            // This is defensive only: all paths that reach tree-aware cleanup
            // retain their registration until `reap` completes. Preserve the
            // old root-group/direct-child fallback rather than panicking if
            // that invariant is ever broken.
            process_group.kill_but_keep_armed();
            let _ = child.start_kill();
            return;
        }
    };
    if settle_bounded_reap_outcome(process_group, outcome) {
        drop(registration.take());
    } else {
        // Keep `registration` in place until this immediate teardown path
        // explicitly targets the direct child. Its later Drop removes only
        // metadata; `kill_on_drop` and the still-armed group guard remain the
        // final cancellation fallbacks.
        let _ = child.start_kill();
    }
}

#[cfg(not(unix))]
async fn reap_bounded_readiness_process(
    child: &mut tokio::process::Child,
    registration: &mut Option<crate::proc_registry::Registration>,
) {
    let outcome = match registration.as_ref() {
        Some(registration) => {
            crate::proc_registry::reap_bounded(child, registration, BOUNDED_PROCESS_REAP_TIMEOUT)
                .await
        }
        None => {
            let _ = child.start_kill();
            return;
        }
    };
    if outcome == crate::proc_registry::BoundedReapOutcome::Incomplete {
        let _ = child.start_kill();
    }
    // Either bounded reap waited the root, or this is the established immediate
    // direct-child teardown path after `start_kill`.
    drop(registration.take());
}

struct BoundedProcessFailure {
    error: anyhow::Error,
}

enum BoundedReadinessWait {
    Completed(std::process::ExitStatus),
    /// The direct child may still be alive, so the caller must invoke the
    /// tree-aware `proc_registry::reap_bounded` before it tears down readers.
    ReapRequired(BoundedProcessFailure),
    /// The direct child was successfully waited already. Its ppid tree may
    /// have been reparented, so the caller can only sweep the known root group.
    ChildExited(BoundedProcessFailure),
}

/// Owns the two pipe-drain tasks for one bounded child. Tokio detaches a task
/// when its JoinHandle is dropped, so this owner aborts every still-pending
/// reader on early return or future cancellation. Controlled error paths call
/// `abort_pending` first to await that cancellation when possible.
struct BoundedReadinessOutputReaders {
    stdout: tokio::task::JoinHandle<Result<()>>,
    stderr: tokio::task::JoinHandle<Result<()>>,
    stdout_pending: bool,
    stderr_pending: bool,
}

impl BoundedReadinessOutputReaders {
    fn new(
        stdout: tokio::task::JoinHandle<Result<()>>,
        stderr: tokio::task::JoinHandle<Result<()>>,
    ) -> Self {
        Self {
            stdout,
            stderr,
            stdout_pending: true,
            stderr_pending: true,
        }
    }

    async fn abort_pending(&mut self) {
        if self.stdout_pending {
            self.stdout.abort();
            let _ = (&mut self.stdout).await;
            self.stdout_pending = false;
        }
        if self.stderr_pending {
            self.stderr.abort();
            let _ = (&mut self.stderr).await;
            self.stderr_pending = false;
        }
    }
}

impl Drop for BoundedReadinessOutputReaders {
    fn drop(&mut self) {
        if self.stdout_pending {
            self.stdout.abort();
        }
        if self.stderr_pending {
            self.stderr.abort();
        }
    }
}

/// Wait for the direct child and both bounded stream drains under one deadline.
/// A stream reader can fail before the child exits; preserve that distinction so
/// the caller invokes `proc_registry::reap` while the descendant tree is still
/// observable instead of first killing just its root process group.
async fn wait_for_bounded_readiness_process(
    child: &mut tokio::process::Child,
    registration: &mut Option<crate::proc_registry::Registration>,
    readers: &mut BoundedReadinessOutputReaders,
    deadline: tokio::time::Instant,
    process_label: &str,
    stdout_label: &str,
    stderr_label: &str,
) -> BoundedReadinessWait {
    let mut child_status = None;

    loop {
        if !readers.stdout_pending && !readers.stderr_pending {
            if let Some(status) = child_status.take() {
                return BoundedReadinessWait::Completed(status);
            }
        }

        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                let failure = BoundedProcessFailure {
                    error: anyhow!("{process_label} timed out"),
                };
                if child_status.is_some() {
                    return BoundedReadinessWait::ChildExited(failure);
                }
                return BoundedReadinessWait::ReapRequired(failure);
            }
            result = child.wait(), if child_status.is_none() => {
                match result {
                    Ok(status) => {
                        child_status = Some(status);
                        // The direct child is now reaped. Do not leave its
                        // registration visible while a background descendant
                        // keeps an output pipe open and the reader waits.
                        drop(registration.take());
                    }
                    Err(error) => {
                        return BoundedReadinessWait::ReapRequired(BoundedProcessFailure {
                            error: anyhow!("could not wait for {process_label}: {error}"),
                        });
                    }
                }
            }
            result = wait_for_check_output(&mut readers.stdout, stdout_label), if readers.stdout_pending => {
                readers.stdout_pending = false;
                if let Err(error) = result {
                    let failure = BoundedProcessFailure { error };
                    if child_status.is_some() {
                        return BoundedReadinessWait::ChildExited(failure);
                    }
                    return BoundedReadinessWait::ReapRequired(failure);
                }
            }
            result = wait_for_check_output(&mut readers.stderr, stderr_label), if readers.stderr_pending => {
                readers.stderr_pending = false;
                if let Err(error) = result {
                    let failure = BoundedProcessFailure { error };
                    if child_status.is_some() {
                        return BoundedReadinessWait::ChildExited(failure);
                    }
                    return BoundedReadinessWait::ReapRequired(failure);
                }
            }
        }
    }
}

struct GitProbeOutput {
    stdout: String,
}

fn required_git_probe_output(output: String, field: &str) -> Result<String> {
    let mut lines = output.lines();
    let Some(value) = lines.next() else {
        return Err(anyhow!("readiness git probe returned no {field}"));
    };
    if lines.next().is_some() {
        return Err(anyhow!(
            "readiness git probe returned an ambiguous {field} value"
        ));
    }
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("readiness git probe returned an empty {field}"));
    }
    Ok(value.to_string())
}

/// Run one cheap Git probe under the same cancellation discipline as inferred
/// checks. `deadline` is shared by status, branch, and HEAD commands, so the
/// complete signature collection has one bounded budget.
async fn run_bounded_git_command(
    cwd: &Path,
    program: &Path,
    args: &[&str],
    deadline: tokio::time::Instant,
    process_marker: Option<&ReadinessProbeMarker>,
) -> Result<GitProbeOutput> {
    if tokio::time::Instant::now() >= deadline {
        return Err(anyhow!("readiness git signature probe deadline elapsed"));
    }

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("PATH", crate::detect::tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(process_marker) = process_marker {
        process_marker.attach_to(&mut command)?;
    }
    let configured =
        crate::proc_registry::configure(&mut command, crate::proc_registry::Owner::probe());
    let mut child = command.spawn().map_err(|error| {
        anyhow!(
            "could not start readiness git probe {:?} at {}: {error}",
            args,
            cwd.display()
        )
    })?;
    let mut registration = Some(configured.register(&child));
    #[cfg(unix)]
    let mut process_group = BoundedReadinessProcessGroup::from_child_id(
        registration.as_ref().map(|registration| registration.pid()),
    );

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            #[cfg(unix)]
            reap_bounded_readiness_process(&mut child, &mut registration, &mut process_group).await;
            #[cfg(not(unix))]
            reap_bounded_readiness_process(&mut child, &mut registration).await;
            return Err(anyhow!("readiness git probe {:?} has no stdout pipe", args));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            #[cfg(unix)]
            reap_bounded_readiness_process(&mut child, &mut registration, &mut process_group).await;
            #[cfg(not(unix))]
            reap_bounded_readiness_process(&mut child, &mut registration).await;
            return Err(anyhow!("readiness git probe {:?} has no stderr pipe", args));
        }
    };
    let stdout_tail = Arc::new(Mutex::new(OutputTailBuffer::new(CHECK_OUTPUT_TAIL_BYTES)));
    let stderr_tail = Arc::new(Mutex::new(OutputTailBuffer::new(CHECK_OUTPUT_TAIL_BYTES)));
    let mut readers = BoundedReadinessOutputReaders::new(
        tokio::spawn(drain_check_output(stdout, Arc::clone(&stdout_tail))),
        tokio::spawn(drain_check_output(stderr, Arc::clone(&stderr_tail))),
    );

    match wait_for_bounded_readiness_process(
        &mut child,
        &mut registration,
        &mut readers,
        deadline,
        "readiness git probe",
        "git stdout",
        "git stderr",
    )
    .await
    {
        BoundedReadinessWait::Completed(status) => {
            #[cfg(unix)]
            if process_marker.is_some() {
                // The signature-level marker spans all three Git commands.
                // Keep same-group background helpers alive (but marked) until
                // that one final scan can both detect and invalidate them;
                // killing here would erase evidence that a hook escaped.
                process_group.disarm();
            } else {
                // Direct test/legacy callers without a marker retain the
                // established root-group cleanup fallback.
                process_group.kill();
            }
            let stdout = rendered_check_output_tail(&stdout_tail)?;
            let stderr = rendered_check_output_tail(&stderr_tail)?;
            if !status.success() {
                let detail = if stderr.is_empty() { &stdout } else { &stderr };
                return Err(anyhow!(
                    "readiness git probe {:?} failed at {} (code {}): {}",
                    args,
                    cwd.display(),
                    status.code().unwrap_or(-1),
                    detail
                ));
            }
            Ok(GitProbeOutput { stdout })
        }
        BoundedReadinessWait::ReapRequired(failure) => {
            #[cfg(unix)]
            reap_bounded_readiness_process(&mut child, &mut registration, &mut process_group).await;
            #[cfg(not(unix))]
            reap_bounded_readiness_process(&mut child, &mut registration).await;
            readers.abort_pending().await;
            Err(anyhow!(
                "readiness git probe {:?} failed at {}: {}",
                args,
                cwd.display(),
                failure.error,
            ))
        }
        BoundedReadinessWait::ChildExited(failure) => {
            #[cfg(unix)]
            // The direct child was already waited. Do not call `reap` with a
            // stale root PID; its ppid tree may have been reparented. The
            // armed group still clears ordinary same-group descendants.
            process_group.kill();
            readers.abort_pending().await;
            Err(anyhow!(
                "readiness git probe {:?} failed at {}: {}",
                args,
                cwd.display(),
                failure.error,
            ))
        }
    }
}

async fn run_bounded_check(
    cwd: &Path,
    check: &crate::check::Check,
    timeout: Duration,
) -> Result<BoundedCheckOutcome> {
    let mut command = tokio::process::Command::new(&check.program);
    command
        .args(&check.args)
        .current_dir(cwd)
        .env("PATH", crate::detect::tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let process_marker = ReadinessProbeMarker::attach(&mut command)?;
    let configured =
        crate::proc_registry::configure(&mut command, crate::proc_registry::Owner::probe());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(BoundedCheckOutcome::Completed(crate::check::CheckResult {
                name: check.name.clone(),
                status: "fail".to_string(),
                code: -1,
                output_tail: format!("could not run {}: {error}", check.program),
            }));
        }
    };
    let mut registration = Some(configured.register(&child));
    #[cfg(unix)]
    // `configure` makes the direct child its own group leader. This guard
    // remains armed if this future is abandoned before tree-aware reap can
    // await the child and its descendants.
    let mut process_group = BoundedReadinessProcessGroup::from_child_id(
        registration.as_ref().map(|registration| registration.pid()),
    );
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            #[cfg(unix)]
            reap_bounded_readiness_process(&mut child, &mut registration, &mut process_group).await;
            #[cfg(not(unix))]
            reap_bounded_readiness_process(&mut child, &mut registration).await;
            return Ok(BoundedCheckOutcome::NotProduced {
                output_tail: String::new(),
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            #[cfg(unix)]
            reap_bounded_readiness_process(&mut child, &mut registration, &mut process_group).await;
            #[cfg(not(unix))]
            reap_bounded_readiness_process(&mut child, &mut registration).await;
            return Ok(BoundedCheckOutcome::NotProduced {
                output_tail: String::new(),
            });
        }
    };
    let output_tail = Arc::new(Mutex::new(OutputTailBuffer::new(CHECK_OUTPUT_TAIL_BYTES)));
    let mut readers = BoundedReadinessOutputReaders::new(
        tokio::spawn(drain_check_output(stdout, Arc::clone(&output_tail))),
        tokio::spawn(drain_check_output(stderr, Arc::clone(&output_tail))),
    );

    // The wait and both pipe drains share one deadline. A background child may
    // keep a pipe open after its shell exits, so waiting for only `child.wait`
    // would still let readers outlive the check and retain its output stream.
    let deadline = tokio::time::Instant::now() + timeout;
    match wait_for_bounded_readiness_process(
        &mut child,
        &mut registration,
        &mut readers,
        deadline,
        "readiness check",
        "stdout",
        "stderr",
    )
    .await
    {
        BoundedReadinessWait::Completed(status) => {
            // Inspect the inherited ownership marker before the ordinary root
            // group sweep can make a same-group background process disappear.
            // The full process/fd-table scan runs on a blocking worker and
            // shares this check's outer deadline. A deferred scan is itself
            // unavailable evidence; its owned marker continues cleanup after
            // this direction admission is released.
            let escaped_processes =
                sweep_readiness_probe_marker_before(deadline, process_marker).await;
            #[cfg(unix)]
            // The direct child is already reaped, so this only sweeps residual
            // root-group members (for example `sh -c 'server >/dev/null 2>&1
            // &'`). A reparented child that actively escaped into another
            // group is no longer discoverable through the exited root's ppid
            // tree. Do this for both pass and fail exits so a successful check
            // cannot leave a background process mutating the checkout.
            process_group.kill();
            let output_tail = rendered_check_output_tail(&output_tail)?;
            let Some(escaped_processes) = escaped_processes else {
                let detail = "verification discarded: process ownership sweep exceeded its deadline";
                return Ok(BoundedCheckOutcome::NotProduced {
                    output_tail: if output_tail.is_empty() {
                        detail.to_string()
                    } else {
                        format!("{output_tail}\n{detail}")
                    },
                });
            };
            if escaped_processes > 0 {
                let detail = format!(
                    "verification discarded: check left {escaped_processes} background process(es)"
                );
                return Ok(BoundedCheckOutcome::NotProduced {
                    output_tail: if output_tail.is_empty() {
                        detail
                    } else {
                        format!("{output_tail}\n{detail}")
                    },
                });
            }
            Ok(BoundedCheckOutcome::Completed(crate::check::CheckResult {
                name: check.name.clone(),
                status: if status.success() { "pass" } else { "fail" }.to_string(),
                code: status.code().unwrap_or(-1),
                output_tail,
            }))
        }
        BoundedReadinessWait::ReapRequired(_failure) => {
            #[cfg(unix)]
            reap_bounded_readiness_process(&mut child, &mut registration, &mut process_group).await;
            #[cfg(not(unix))]
            reap_bounded_readiness_process(&mut child, &mut registration).await;
            readers.abort_pending().await;
            // Marker Drop enqueues the fallback scan on the dedicated cleanup
            // worker; never enumerate the process table on this async task.
            drop(process_marker);
            Ok(BoundedCheckOutcome::NotProduced {
                output_tail: rendered_check_output_tail(&output_tail)?,
            })
        }
        BoundedReadinessWait::ChildExited(_failure) => {
            #[cfg(unix)]
            // The root already exited, so only its known process group can be
            // swept safely. See the normal-completion comment above.
            process_group.kill();
            readers.abort_pending().await;
            drop(process_marker);
            Ok(BoundedCheckOutcome::NotProduced {
                output_tail: rendered_check_output_tail(&output_tail)?,
            })
        }
    }
}

async fn run_checks_with_timeout_report(
    cwd: &Path,
    checks: &[crate::check::Check],
    timeout: Duration,
) -> Result<(CheckEvidence, Vec<crate::check::CheckResult>)> {
    if checks.is_empty() {
        // `infer_checks` deliberately declines to invent a runner. That is no
        // verification evidence for a claimed-complete lane, not a vacuous
        // passing suite.
        return Ok((CheckEvidence::NotProduced, Vec::new()));
    }
    let mut saw_not_produced = false;
    let mut saw_failure = false;
    let mut results = Vec::with_capacity(checks.len());
    for check in checks {
        match run_bounded_check(cwd, check, timeout).await? {
            BoundedCheckOutcome::Completed(result) => {
                if result.status == "fail" {
                    saw_failure = true;
                }
                results.push(result);
            }
            BoundedCheckOutcome::NotProduced { output_tail } => {
                saw_not_produced = true;
                results.push(crate::check::CheckResult {
                    name: check.name.clone(),
                    status: "fail".to_string(),
                    code: -1,
                    output_tail: if output_tail.is_empty() {
                        "verification did not produce a bounded check result".to_string()
                    } else {
                        format!(
                            "verification did not produce a bounded check result: {output_tail}"
                        )
                    },
                });
            }
        }
    }
    if saw_failure {
        return Ok((CheckEvidence::Failing, results));
    }
    if saw_not_produced {
        return Ok((CheckEvidence::NotProduced, results));
    }
    Ok((CheckEvidence::Passed, results))
}

#[cfg(test)]
async fn run_checks_with_timeout(
    cwd: &Path,
    checks: &[crate::check::Check],
    timeout: Duration,
) -> Result<CheckEvidence> {
    let (evidence, _) = run_checks_with_timeout_report(cwd, checks, timeout).await?;
    Ok(evidence)
}

#[cfg(test)]
async fn run_readiness_checks(paths: Vec<String>, timeout: Duration) -> Result<CheckEvidence> {
    let mut saw_not_produced = false;
    let mut saw_failure = false;
    for path in paths {
        let checks = crate::check::infer_checks(Path::new(&path));
        let path_evidence = run_checks_with_timeout(Path::new(&path), &checks, timeout).await?;
        if path_evidence == CheckEvidence::Failing {
            saw_failure = true;
        } else if path_evidence == CheckEvidence::NotProduced {
            saw_not_produced = true;
        }
    }
    if saw_failure {
        return Ok(CheckEvidence::Failing);
    }
    if saw_not_produced {
        return Ok(CheckEvidence::NotProduced);
    }
    Ok(CheckEvidence::Passed)
}

fn check_inference_limit() -> &'static Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_CHECK_INFERENCES)))
}

async fn run_bounded_check_inference_with<F>(
    deadline: tokio::time::Instant,
    limit: Arc<Semaphore>,
    infer: F,
) -> Option<Vec<crate::check::Check>>
where
    F: FnOnce() -> Vec<crate::check::Check> + Send + 'static,
{
    if tokio::time::Instant::now() >= deadline {
        return None;
    }
    let permit = match tokio::time::timeout_at(deadline, limit.acquire_owned()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) | Err(_) => return None,
    };
    let task = tokio::task::spawn_blocking(move || {
        // A timed-out blocking task keeps this permit until the filesystem
        // call really returns. Later callers fail closed at their own deadline
        // instead of spawning an unbounded queue against the same stalled mount.
        let _permit = permit;
        infer()
    });
    match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(checks)) => Some(checks),
        Ok(Err(_)) | Err(_) => None,
    }
}

async fn infer_readiness_checks(
    path: PathBuf,
    timeout: Duration,
) -> Option<Vec<crate::check::Check>> {
    let budget = std::cmp::min(timeout, CHECK_INFERENCE_TIMEOUT);
    let deadline = tokio::time::Instant::now() + budget;
    run_bounded_check_inference_with(deadline, Arc::clone(check_inference_limit()), move || {
        crate::check::infer_checks(&path)
    })
    .await
}

async fn run_verification_checks(
    targets: Vec<CheckTarget>,
    timeout: Duration,
) -> Result<VerificationReport> {
    if targets.is_empty() {
        return Ok(VerificationReport::not_produced());
    }

    let mut saw_not_produced = false;
    let mut saw_failure = false;
    let mut repo_checks = Vec::with_capacity(targets.len());
    for target in targets {
        let repo = target.repo;
        let worktree = target.path;
        let Some(checks) = infer_readiness_checks(PathBuf::from(&worktree), timeout).await else {
            saw_not_produced = true;
            repo_checks.push(RepoChecks {
                repo,
                worktree,
                checks: Vec::new(),
            });
            continue;
        };
        let (evidence, checks) =
            run_checks_with_timeout_report(Path::new(&worktree), &checks, timeout).await?;
        if evidence == CheckEvidence::Failing {
            saw_failure = true;
        } else if evidence == CheckEvidence::NotProduced {
            saw_not_produced = true;
        }
        repo_checks.push(RepoChecks {
            repo,
            worktree,
            checks,
        });
    }

    let evidence = if saw_failure {
        CheckEvidence::Failing
    } else if saw_not_produced {
        CheckEvidence::NotProduced
    } else {
        CheckEvidence::Passed
    };
    Ok(VerificationReport {
        evidence,
        repo_checks,
    })
}

async fn run_shared_verification(
    db: &Db,
    direction_id: i32,
    targets: Vec<CheckTarget>,
    git_probe: &GitSignatureProbe,
    purpose: VerificationTargetPurpose,
) -> Result<VerificationReport> {
    let admission_probe = git_probe.clone();
    let post_probe = git_probe.clone();
    check_flight()
        .get_or_run_report_with_admission_and_post_targets(
            direction_id,
            targets,
            |targets| async move {
                run_verification_checks(targets, READINESS_CHECK_TIMEOUT).await
            },
            move |_| async move {
                verification_targets_for_direction(db, direction_id, &admission_probe, purpose)
                    .await
            },
            move |_| async move {
                verification_targets_for_direction(db, direction_id, &post_probe, purpose).await
            },
        )
        .await
}

async fn checks_for(
    db: &Db,
    direction: &direction::Model,
    worktrees: &[ProbedWorktree],
    execution: CheckExecution,
    git_probe: &GitSignatureProbe,
) -> Result<CheckEvidence> {
    if !direction_claimed_completion(direction.status.as_str()) {
        return Ok(CheckEvidence::NotApplicable);
    }

    let Some(mut targets) = check_targets_for_worktrees(worktrees) else {
        return Ok(CheckEvidence::NotProduced);
    };
    if !targets_support_delivery_evidence(&targets) {
        return Ok(CheckEvidence::NotProduced);
    }
    CheckFlight::sort_targets(&mut targets);
    match execution {
        CheckExecution::RunAllowed => Ok(
            run_shared_verification(
                db,
                direction.id,
                targets,
                git_probe,
                VerificationTargetPurpose::ReadinessCollection,
            )
            .await?
            .evidence,
        ),
        CheckExecution::CachedOnly => Ok(check_flight()
            .cached_read_only_if_admitted(direction.id, &targets)?
            .map(|report| report.evidence)
            .unwrap_or(CheckEvidence::NotProduced)),
    }
}

/// Run bounded local verification for one direction and return the established
/// explicit-command report shape. Missing/deferred evidence is an error rather
/// than a misleading empty successful response.
pub async fn verify_direction(db: &Db, direction_id: i32) -> Result<Vec<RepoChecks>> {
    let git_probe = GitSignatureProbe::readiness();
    let targets = verification_targets_for_direction(
        db,
        direction_id,
        &git_probe,
        VerificationTargetPurpose::ExplicitDirectionVerification,
    )
    .await?;
    let report = run_shared_verification(
        db,
        direction_id,
        targets,
        &git_probe,
        VerificationTargetPurpose::ExplicitDirectionVerification,
    )
    .await?;
    if matches!(
        report.evidence,
        CheckEvidence::NotProduced | CheckEvidence::NotApplicable
    ) || report.repo_checks.is_empty()
    {
        return Err(anyhow!(
            "verification was not produced for direction {direction_id}"
        ));
    }
    Ok(report.repo_checks)
}

fn checks_are_preempted(facts: &LaneFacts) -> bool {
    facts.worker_failed
        || facts.has_open_ask
        || facts.policy == PolicyDecision::NeedsGate
        || worker_active_preempts_completion(facts.worker_active, facts.direction_status.as_str())
        || has_all_clear_merged_pull_request(&facts.pull_requests, facts.open_pr_snapshot_freshness)
        || facts.reconciliation == ExecutionReconciliation::Drifted
}

async fn checks_after_decisive_gates<F, Fut>(
    facts: &LaneFacts,
    collect_checks: F,
) -> Result<CheckEvidence>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<CheckEvidence>>,
{
    if checks_are_preempted(facts) {
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

/// Attribute a human or permission ask to one persisted direction only when
/// its scope is a current numeric direction id for this thread. Everything
/// else is issue-wide: a stale or unfamiliar ask must not disappear from
/// delivery readiness just because it cannot be assigned precisely.
fn collect_open_ask_scope(
    scope: &str,
    direction_ids: &HashSet<i32>,
    open_ask_direction_ids: &mut HashSet<i32>,
    has_issue_open_ask: &mut bool,
) {
    match scope.parse::<i32>() {
        Ok(direction_id) if direction_ids.contains(&direction_id) => {
            open_ask_direction_ids.insert(direction_id);
        }
        _ => {
            *has_issue_open_ask = true;
        }
    }
}

async fn collect_lane(
    db: &Db,
    direction: &direction::Model,
    policy: PolicyDecision,
    open_ask_direction_ids: &HashSet<i32>,
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
    check_execution: CheckExecution,
    git_probe: &GitSignatureProbe,
) -> Result<LaneFacts> {
    let active = direction_is_active(direction);
    if !active || policy == PolicyDecision::Denied {
        return Ok(LaneFacts {
            direction_id: direction.id,
            name: direction.name.clone(),
            active,
            policy,
            worker_failed: false,
            worker_active: false,
            has_open_ask: false,
            reconciliation: ExecutionReconciliation::Unknown,
            checks: CheckEvidence::NotApplicable,
            upstream: UpstreamEvidence::Satisfied,
            open_pr_snapshot_freshness,
            pull_requests: Vec::new(),
            direction_status: direction.status.clone(),
        });
    }
    let worker = latest_worker_facts(db, direction.id).await?;
    let has_open_ask = open_ask_direction_ids.contains(&direction.id);
    let mut facts = LaneFacts {
        direction_id: direction.id,
        name: direction.name.clone(),
        active,
        policy,
        worker_failed: worker.failed,
        worker_active: worker.active,
        has_open_ask,
        // Local Git facts are deliberately unknown until the only shared
        // signature probe has sampled the registered worktrees.
        reconciliation: ExecutionReconciliation::Unknown,
        checks: CheckEvidence::NotApplicable,
        // Any early return below is decided before upstream evidence. Keep
        // every uncollected fact fail-closed rather than inventing success.
        upstream: UpstreamEvidence::Unknown,
        open_pr_snapshot_freshness,
        pull_requests: Vec::new(),
        direction_status: direction.status.clone(),
    };

    // Match lane_readiness's first-match order before touching local Git. At
    // this point a positive preemption can only be worker failure, an open
    // ask, a policy gate, or an occupied claimed-complete worker; no Git fact
    // can change any of those verdicts.
    if checks_are_preempted(&facts) {
        return Ok(facts);
    }

    let mut pull_requests = repo::list_pull_requests_for_direction(db, direction.id).await?;
    pull_requests.sort_by_key(|row| row.id);
    facts.pull_requests = pull_requests.iter().map(pull_request_facts).collect();

    // An all-clear merged PR is the remaining terminal gate that precedes
    // local reconciliation. It needs stored PR facts, but no local Git probe.
    if checks_are_preempted(&facts) {
        return Ok(facts);
    }

    // Global/tool status reads are read-only in the stronger sense: a Git
    // signature probe may execute a repository-configured fsmonitor hook.
    // CachedOnly therefore consumes durable worker/plan/PR/upstream facts but
    // starts no repository subprocess at all. Without a fresh local sample,
    // reconciliation remains Unknown and completed-lane checks fail closed.
    if check_execution == CheckExecution::CachedOnly {
        facts.checks = if direction_claimed_completion(direction.status.as_str()) {
            CheckEvidence::NotProduced
        } else {
            CheckEvidence::NotApplicable
        };
        facts.upstream = upstream_evidence(repo::upstream_merge_state(db, direction.id).await);
        return Ok(facts);
    }

    let worktrees = probe_worktrees_for_direction(db, direction.id, git_probe).await?;
    facts.reconciliation = reconciliation_for(direction, &worktrees);
    // Keep collection's cost aligned with `lane_readiness` first-match order.
    // Once a human-action, active-worker, all-clear terminal merge, or drift
    // gate decides the outcome, starting a build/test process cannot add useful
    // delivery evidence or may race a changing checkout.
    let checks = checks_after_decisive_gates(&facts, || {
        checks_for(db, direction, &worktrees, check_execution, git_probe)
    })
    .await?;
    facts.checks = checks;
    facts.upstream = upstream_evidence(repo::upstream_merge_state(db, direction.id).await);
    Ok(facts)
}

fn virtual_issue_ask_lane(open_pr_snapshot_freshness: OpenPrSnapshotFreshness) -> LaneFacts {
    let mut facts = virtual_lane_facts(
        0,
        "issue ask".to_string(),
        PolicyDecision::AllowedByPolicy,
        ExecutionReconciliation::Matched,
        Vec::new(),
        open_pr_snapshot_freshness,
    );
    facts.has_open_ask = true;
    facts
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

enum PendingLaneCollection {
    Immediate(LaneFacts),
    Direction {
        direction: direction::Model,
        policy: PolicyDecision,
    },
}

async fn resolve_pending_lanes(
    db: &Db,
    pending: Vec<PendingLaneCollection>,
    open_ask_direction_ids: &HashSet<i32>,
    open_pr_snapshot_freshness: OpenPrSnapshotFreshness,
    check_execution: CheckExecution,
    git_probe: &GitSignatureProbe,
) -> Result<Vec<LaneFacts>> {
    let tasks = pending.into_iter().map(|pending_lane| async move {
        match pending_lane {
            PendingLaneCollection::Immediate(facts) => Ok(facts),
            PendingLaneCollection::Direction { direction, policy } => {
                collect_lane(
                    db,
                    &direction,
                    policy,
                    open_ask_direction_ids,
                    open_pr_snapshot_freshness,
                    check_execution,
                    git_probe,
                )
                .await
            }
        }
    });
    futures::future::join_all(tasks).await.into_iter().collect()
}

/// Collect live storage/process facts, then run the pure aggregation. This
/// function performs no writes and deliberately reuses the existing local
/// check runner and host parsers rather than inventing parallel semantics.
pub async fn collect(
    db: &Db,
    bus: &BusRegistry,
    asks: &AskRegistry,
    thread_id: i32,
) -> Result<IssueReadinessDto> {
    collect_with_check_execution(db, bus, asks, thread_id, CheckExecution::RunAllowed).await
}

/// Collect one issue with an explicit verification execution boundary. The
/// desktop command is allowed to refresh verification evidence; read-only
/// callers consume durable facts without starting repository subprocesses.
pub async fn collect_with_check_execution(
    db: &Db,
    bus: &BusRegistry,
    asks: &AskRegistry,
    thread_id: i32,
    check_execution: CheckExecution,
) -> Result<IssueReadinessDto> {
    if repo::get_thread(db, thread_id).await?.is_none() {
        return Err(anyhow!("thread {thread_id} not found"));
    }
    let plan = repo::get_plan(db, thread_id).await?;
    let mut directions = repo::list_directions(db, thread_id).await?;
    directions.sort_by_key(|direction| direction.id);
    let direction_ids: HashSet<i32> = directions.iter().map(|direction| direction.id).collect();
    let mut open_ask_direction_ids = HashSet::new();
    let mut has_issue_open_ask = false;
    for ask in bus.open_asks(thread_id) {
        collect_open_ask_scope(
            &ask.from,
            &direction_ids,
            &mut open_ask_direction_ids,
            &mut has_issue_open_ask,
        );
    }
    for ask in asks.open_in(thread_id) {
        collect_open_ask_scope(
            &ask.dir,
            &direction_ids,
            &mut open_ask_direction_ids,
            &mut has_issue_open_ask,
        );
    }
    let open_pr_snapshot_freshness = current_open_pr_snapshot_freshness()?;
    let git_probe = GitSignatureProbe::readiness();
    let mut pending = Vec::with_capacity(directions.len() + 2);

    match planned_lane_source(plan.as_ref()) {
        PlannedLaneSource::Legacy => {
            for direction in &directions {
                pending.push(PendingLaneCollection::Direction {
                    direction: direction.clone(),
                    policy: PolicyDecision::AllowedByPolicy,
                });
            }
        }
        PlannedLaneSource::Unavailable => {
            for direction in &directions {
                pending.push(PendingLaneCollection::Direction {
                    direction: direction.clone(),
                    policy: PolicyDecision::NeedsGate,
                });
            }
        }
        PlannedLaneSource::Parsed {
            phase,
            lanes: proposal_lanes,
        } => {
            let referenced_direction_ids: HashSet<i32> = proposal_lanes
                .iter()
                .filter_map(|lane| (lane.direction_id != 0).then_some(lane.direction_id))
                .collect();
            let mut materialized_policies = HashMap::new();
            for proposed_lane in &proposal_lanes {
                if proposed_lane.direction_id == 0 {
                    continue;
                }
                let policy = proposal_lane_policy(phase, proposed_lane);
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
                let policy = proposal_lane_policy(phase, &proposed_lane);
                if proposed_lane.direction_id == 0 {
                    if policy == PolicyDecision::Denied {
                        continue;
                    }
                    pending.push(PendingLaneCollection::Immediate(virtual_lane_facts(
                        0,
                        proposed_lane.name,
                        policy,
                        ExecutionReconciliation::Matched,
                        Vec::new(),
                        open_pr_snapshot_freshness,
                    )));
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
                    pending.push(PendingLaneCollection::Immediate(virtual_lane_facts(
                        proposed_lane.direction_id,
                        proposed_lane.name,
                        policy,
                        ExecutionReconciliation::Unknown,
                        Vec::new(),
                        open_pr_snapshot_freshness,
                    )));
                    continue;
                };
                pending.push(PendingLaneCollection::Direction {
                    direction: (*direction).clone(),
                    policy,
                });
            }

            for direction in &directions {
                if referenced_direction_ids.contains(&direction.id) {
                    continue;
                }
                pending.push(PendingLaneCollection::Direction {
                    direction: direction.clone(),
                    policy: PolicyDecision::AllowedByPolicy,
                });
            }
        }
    }

    let mut facts = resolve_pending_lanes(
        db,
        pending,
        &open_ask_direction_ids,
        open_pr_snapshot_freshness,
        check_execution,
        &git_probe,
    )
    .await?;

    if let Some(unbound_pr_lane) =
        collect_unbound_pr_lane(db, thread_id, open_pr_snapshot_freshness).await?
    {
        facts.push(unbound_pr_lane);
    }
    if has_issue_open_ask {
        facts.push(virtual_issue_ask_lane(open_pr_snapshot_freshness));
    }
    Ok(issue_readiness(&facts))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    struct ReaderTaskDropMarker {
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for ReaderTaskDropMarker {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn facts() -> LaneFacts {
        LaneFacts {
            direction_id: 7,
            name: "implementation".to_string(),
            active: true,
            policy: PolicyDecision::AllowedByPolicy,
            worker_failed: false,
            worker_active: false,
            has_open_ask: false,
            reconciliation: ExecutionReconciliation::Matched,
            checks: CheckEvidence::Passed,
            upstream: UpstreamEvidence::Satisfied,
            open_pr_snapshot_freshness: open_pr_snapshot_freshness(1_000, 60),
            pull_requests: Vec::new(),
            direction_status: "review".to_string(),
        }
    }

    async fn multi_repo_worker_fixture() -> (Db, direction::Model, i32, i32, i32) {
        let db = Db::connect("sqlite::memory:")
            .await
            .expect("memory readiness db");
        let workspace = repo::create_workspace(&db, "multi-repo worker readiness")
            .await
            .expect("workspace");
        let first_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "first-repo",
            "/tmp/first-repo",
            "main",
            "",
            true,
        )
        .await
        .expect("first repo");
        let second_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "second-repo",
            "/tmp/second-repo",
            "main",
            "",
            true,
        )
        .await
        .expect("second repo");
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "multi-repo worker readiness",
            "feature/multi-repo-worker-readiness",
            "claude",
        )
        .await
        .expect("thread");
        let mut direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "claude",
            first_repo.id,
            "exercise one current session per repository slot",
            "impl-only",
            "main",
        )
        .await
        .expect("direction");
        repo::set_direction_status(&db, direction.id, "review")
            .await
            .expect("review direction");
        direction.status = "review".to_string();

        (db, direction, thread.id, first_repo.id, second_repo.id)
    }

    #[tokio::test]
    async fn live_idle_worker_reconciles_a_failed_terminal_status_write() {
        let (db, direction, _thread_id, first_repo_id, _second_repo_id) =
            multi_repo_worker_fixture().await;
        let worker = repo::create_session(
            &db,
            direction.id,
            first_repo_id,
            "claude",
            "/tmp/live-idle-worker",
        )
        .await
        .expect("worker session");
        repo::set_session_status(&db, worker.id, "running")
            .await
            .expect("simulate stale durable running status");

        let durable = latest_worker_facts(&db, direction.id)
            .await
            .expect("durable worker facts");
        assert!(durable.active, "the stale durable row remains conservative");

        assert!(
            !reconciled_worker_activity("running", Some(false)),
            "an idle live engine must unblock verification after its idle SQLite write failed"
        );
        assert!(
            reconciled_worker_activity("idle", Some(true)),
            "a live turn must remain active even before a durable status refresh"
        );
    }

    #[tokio::test]
    async fn admission_target_recheck_refuses_a_worker_that_started_after_collection() {
        let db = Db::connect("sqlite::memory:").await.expect("memory db");
        let workspace = repo::create_workspace(&db, "admission recheck")
            .await
            .expect("workspace");
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "admission-recheck-repo",
            "/tmp/admission-recheck-repo",
            "main",
            "",
            true,
        )
        .await
        .expect("repo ref");
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "admission recheck",
            "feature/admission-recheck",
            "claude",
        )
        .await
        .expect("thread");
        let direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "claude",
            repo_ref.id,
            "exercise admission recheck",
            "impl-only",
            "main",
        )
        .await
        .expect("direction");
        repo::set_direction_status(&db, direction.id, "review")
            .await
            .expect("review direction");
        let worker = repo::create_session(
            &db,
            direction.id,
            repo_ref.id,
            "claude",
            "/tmp/admission-recheck-worker",
        )
        .await
        .expect("worker session");
        repo::set_session_status(&db, worker.id, "running")
            .await
            .expect("worker running");

        let error = verification_targets_for_direction(
            &db,
            direction.id,
            &GitSignatureProbe::readiness(),
            VerificationTargetPurpose::ReadinessCollection,
        )
        .await
        .expect_err("active worker must preempt the admitted verification target probe");
        assert!(error.to_string().contains("worker is active"));
    }

    #[tokio::test]
    async fn multi_repo_current_worker_statuses_preempt_verification() {
        let (db, direction, _thread_id, first_repo_id, second_repo_id) =
            multi_repo_worker_fixture().await;
        let first_worker = repo::create_session(
            &db,
            direction.id,
            first_repo_id,
            "claude",
            "/tmp/first-repo-worker",
        )
        .await
        .expect("first worker");
        // Insert the other repository's idle session second so it is the
        // direction-wide newest row. The first slot must still preempt
        // verification whenever it owns its own worktree.
        let second_worker = repo::create_session(
            &db,
            direction.id,
            second_repo_id,
            "claude",
            "/tmp/second-repo-worker",
        )
        .await
        .expect("second worker");
        repo::set_session_status(&db, second_worker.id, "idle")
            .await
            .expect("second worker idle");

        for status in ["running", "starting", "stopped"] {
            repo::set_session_status(&db, first_worker.id, status)
                .await
                .expect("set first worker status");

            let worker = latest_worker_facts(&db, direction.id)
                .await
                .expect("aggregate worker facts");
            assert!(worker.active, "{status} worker must occupy its repo slot");
            assert!(!worker.failed, "fixture has no terminal worker error");

            let error = verification_targets_for_direction(
                &db,
                direction.id,
                &GitSignatureProbe::readiness(),
                VerificationTargetPurpose::ReadinessCollection,
            )
            .await
            .expect_err("an active worker in either repo must preempt verification");
            assert!(error.to_string().contains("worker is active"), "{status}");
        }
    }

    #[tokio::test]
    async fn working_idle_worker_is_explicitly_verifiable_while_readiness_collection_skips_runner()
    {
        let root = tempfile::tempdir().expect("temporary working verification fixture");
        std::fs::write(root.path().join("README.md"), "working verification\n")
            .expect("write fixture readme");
        git_in(root.path(), &["init", "--quiet", "-b", "main"]);
        git_in(
            root.path(),
            &["config", "user.email", "readiness@example.invalid"],
        );
        git_in(root.path(), &["config", "user.name", "Readiness Test"]);
        git_in(root.path(), &["add", "README.md"]);
        git_in(root.path(), &["commit", "--quiet", "-m", "fixture"]);

        let db = Db::connect("sqlite::memory:")
            .await
            .expect("memory readiness db");
        let workspace = repo::create_workspace(&db, "working verification workspace")
            .await
            .expect("workspace");
        let root_path = root.path().display().to_string();
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "working-verification-repo",
            &root_path,
            "main",
            "",
            true,
        )
        .await
        .expect("repo reference");
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "working verification",
            "feature/working-verification",
            "claude",
        )
        .await
        .expect("thread");
        let mut direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "claude",
            repo_ref.id,
            "verify after an idle worker finishes",
            "impl-only",
            "main",
        )
        .await
        .expect("direction");
        git_in(
            root.path(),
            &["checkout", "--quiet", "-b", direction.branch.as_str()],
        );
        repo::set_direction_status(&db, direction.id, "working")
            .await
            .expect("working direction");
        direction.status = "working".to_string();
        repo::record_worktree(
            &db,
            repo_ref.id,
            direction.id,
            &direction.branch,
            &root_path,
            false,
            false,
            "",
        )
        .await
        .expect("worktree row");
        let worker = repo::create_session(&db, direction.id, repo_ref.id, "claude", &root_path)
            .await
            .expect("idle worker session");
        repo::set_session_status(&db, worker.id, "idle")
            .await
            .expect("idle worker status");
        assert!(
            !latest_worker_facts(&db, direction.id)
                .await
                .expect("idle worker facts")
                .active,
            "an idle worker does not occupy the verification target"
        );

        let probe = GitSignatureProbe::readiness();
        for active_status in ["starting", "running", "stopped"] {
            repo::set_session_status(&db, worker.id, active_status)
                .await
                .expect("active working worker status");
            let error = verification_targets_for_direction(
                &db,
                direction.id,
                &probe,
                VerificationTargetPurpose::ExplicitDirectionVerification,
            )
            .await
            .expect_err("working verification must wait for every active worker state");
            assert!(error.to_string().contains("worker is active"));
        }
        repo::set_session_status(&db, worker.id, "idle")
            .await
            .expect("restore idle worker status");
        let explicit_targets = verification_targets_for_direction(
            &db,
            direction.id,
            &probe,
            VerificationTargetPurpose::ExplicitDirectionVerification,
        )
        .await
        .expect("working idle worker remains explicitly verifiable");
        assert_eq!(explicit_targets.len(), 1);
        // `verify_direction` uses this same explicit target purpose for its
        // initial, admission, and post-run samples. Keep the runner injected
        // so this regression proves that authorization boundary without a
        // checker mutating a fixture signature during the test itself.
        let explicit_flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let direction_id = direction.id;
        let db_for_admission = &db;
        let db_for_post = &db;
        let admission_probe = probe.clone();
        let post_probe = probe.clone();
        let explicit_report = explicit_flight
            .get_or_run_report_with_admission_and_post_targets(
                direction_id,
                explicit_targets.clone(),
                |targets| async move {
                    Ok(VerificationReport {
                        evidence: CheckEvidence::Passed,
                        repo_checks: targets
                            .into_iter()
                            .map(|target| RepoChecks {
                                repo: target.repo,
                                worktree: target.path,
                                checks: vec![crate::check::CheckResult {
                                    name: "injected".to_string(),
                                    status: "pass".to_string(),
                                    code: 0,
                                    output_tail: String::new(),
                                }],
                            })
                            .collect(),
                    })
                },
                move |_| async move {
                    verification_targets_for_direction(
                        db_for_admission,
                        direction_id,
                        &admission_probe,
                        VerificationTargetPurpose::ExplicitDirectionVerification,
                    )
                    .await
                },
                move |_| async move {
                    verification_targets_for_direction(
                        db_for_post,
                        direction_id,
                        &post_probe,
                        VerificationTargetPurpose::ExplicitDirectionVerification,
                    )
                    .await
                },
            )
            .await
            .expect("working idle worker verifies through the explicit shared path");
        assert_eq!(explicit_report.repo_checks.len(), 1);
        assert!(
            explicit_report.repo_checks[0]
                .checks
                .iter()
                .all(|check| check.status == "pass"),
            "explicit verification publishes the successful checks for checksByDirection"
        );

        let worktrees = probe_worktrees_for_direction(&db, direction.id, &probe)
            .await
            .expect("working worktree facts");
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let runs = Arc::new(AtomicUsize::new(0));
        let attempted_runs = Arc::clone(&runs);
        let evidence = checks_for_with_runner(
            &direction,
            &worktrees,
            &flight,
            CheckExecution::RunAllowed,
            move |_| {
                attempted_runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Passed) }
            },
        )
        .await
        .expect("working readiness collection result");
        assert_eq!(evidence, CheckEvidence::NotApplicable);
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "readiness collection must not run checks for a working lane"
        );

        let collection_error = verification_targets_for_direction(
            &db,
            direction.id,
            &probe,
            VerificationTargetPurpose::ReadinessCollection,
        )
        .await
        .expect_err("readiness collection retains claimed-completion target guard");
        assert!(collection_error
            .to_string()
            .contains("completion was withdrawn"));

        git_in(
            root.path(),
            &["checkout", "--quiet", "-b", "feature/unexpected"],
        );
        let drift_error = verification_targets_for_direction(
            &db,
            direction.id,
            &probe,
            VerificationTargetPurpose::ExplicitDirectionVerification,
        )
        .await
        .expect_err("explicit verification must reject the wrong checked-out branch");
        assert!(drift_error.to_string().contains("worktree branch drifted"));
    }

    #[tokio::test]
    async fn multi_repo_current_worker_errors_are_aggregated_without_stale_error_latching() {
        let (db, direction, thread_id, first_repo_id, second_repo_id) =
            multi_repo_worker_fixture().await;
        let stale_first = repo::create_session(
            &db,
            direction.id,
            first_repo_id,
            "claude",
            "/tmp/first-repo-stale",
        )
        .await
        .expect("stale first worker");
        repo::set_session_status(&db, stale_first.id, "idle")
            .await
            .expect("stale first worker idle");
        repo::insert_lead_message(
            &db,
            thread_id,
            Some(stale_first.id),
            1,
            "assistant",
            "text",
            r#"{"text":"first attempt failed"}"#,
            "error",
        )
        .await
        .expect("stale first worker error");

        let failing_second = repo::create_session(
            &db,
            direction.id,
            second_repo_id,
            "claude",
            "/tmp/second-repo-failing",
        )
        .await
        .expect("failing second worker");
        repo::set_session_status(&db, failing_second.id, "idle")
            .await
            .expect("failing second worker idle");
        repo::insert_lead_message(
            &db,
            thread_id,
            Some(failing_second.id),
            1,
            "assistant",
            "text",
            r#"{"text":"second attempt failed"}"#,
            "error",
        )
        .await
        .expect("failing second worker error");

        let replacement_first = repo::create_session(
            &db,
            direction.id,
            first_repo_id,
            "claude",
            "/tmp/first-repo-retry",
        )
        .await
        .expect("replacement first worker");
        repo::set_session_status(&db, replacement_first.id, "idle")
            .await
            .expect("replacement first worker idle");
        repo::insert_lead_message(
            &db,
            thread_id,
            Some(replacement_first.id),
            2,
            "assistant",
            "text",
            r#"{"text":"first retry completed"}"#,
            "complete",
        )
        .await
        .expect("replacement first worker completion");

        // The globally newest session is the successful first-repo retry, but
        // the second repository's current terminal error remains actionable.
        let worker = latest_worker_facts(&db, direction.id)
            .await
            .expect("aggregate worker facts");
        assert!(worker.failed);
        let open_asks = HashSet::new();
        let lane = collect_lane(
            &db,
            &direction,
            PolicyDecision::AllowedByPolicy,
            &open_asks,
            open_pr_snapshot_freshness(1_000, 60),
            CheckExecution::RunAllowed,
            &GitSignatureProbe::readiness(),
        )
        .await
        .expect("worker failure must preempt lane collection");
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Failed, Some(ReasonCode::WorkerFailed))
        );

        let replacement_second = repo::create_session(
            &db,
            direction.id,
            second_repo_id,
            "claude",
            "/tmp/second-repo-retry",
        )
        .await
        .expect("replacement second worker");
        repo::set_session_status(&db, replacement_second.id, "idle")
            .await
            .expect("replacement second worker idle");
        repo::insert_lead_message(
            &db,
            thread_id,
            Some(replacement_second.id),
            2,
            "assistant",
            "text",
            r#"{"text":"second retry completed"}"#,
            "complete",
        )
        .await
        .expect("replacement second worker completion");

        let recovered = latest_worker_facts(&db, direction.id)
            .await
            .expect("aggregate recovered worker facts");
        assert!(!recovered.active);
        assert!(
            !recovered.failed,
            "errors from replaced repository sessions must not latch forever"
        );
    }

    #[tokio::test]
    async fn active_retry_does_not_latch_a_prior_error_from_the_same_session() {
        let (db, direction, thread_id, first_repo_id, _) = multi_repo_worker_fixture().await;
        let worker = repo::create_session(
            &db,
            direction.id,
            first_repo_id,
            "claude",
            "/tmp/same-session-retry",
        )
        .await
        .expect("same-session worker");
        repo::set_session_status(&db, worker.id, "idle")
            .await
            .expect("failed turn becomes idle");
        repo::insert_lead_message(
            &db,
            thread_id,
            Some(worker.id),
            1,
            "assistant",
            "text",
            r#"{"text":"first turn failed"}"#,
            "error",
        )
        .await
        .expect("first turn error");
        assert!(
            latest_worker_facts(&db, direction.id)
                .await
                .expect("failed worker facts")
                .failed
        );

        repo::set_session_status(&db, worker.id, "running")
            .await
            .expect("same session starts retry");
        let retry = latest_worker_facts(&db, direction.id)
            .await
            .expect("active retry facts");
        assert!(retry.active);
        assert!(
            !retry.failed,
            "the prior turn error must not preempt the active retry"
        );
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
        check_targets_with_dirty(path, head_sha, false)
    }

    fn check_targets_with_dirty(path: &str, head_sha: &str, dirty: bool) -> Vec<CheckTarget> {
        vec![CheckTarget {
            repo: "test-repo".to_string(),
            path: path.to_string(),
            branch: "main".to_string(),
            head_sha: head_sha.to_string(),
            dirty,
        }]
    }

    fn git_in(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("run git test fixture command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn sampled_check_target(path: &Path, stored_path: String) -> CheckTarget {
        let signature = GitSignatureProbe::readiness()
            .sample(path)
            .await
            .expect("sample test worktree signature");
        CheckTarget {
            repo: "test-repo".to_string(),
            path: stored_path,
            branch: signature.branch,
            head_sha: signature.head_sha,
            dirty: signature.dirty,
        }
    }

    fn shell_check(name: &str, script: &str) -> crate::check::Check {
        crate::check::Check {
            name: name.to_string(),
            program: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
        }
    }

    #[cfg(unix)]
    fn shell_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    #[cfg(unix)]
    fn process_is_live(pid: i32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output();
        let Ok(output) = output else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        !state.is_empty() && !state.starts_with('Z')
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn process_group_id(pid: i32) -> Option<i32> {
        let output = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stalled_manifest_inference_is_deadline_bounded_and_caps_blocking_tasks() {
        let limit = Arc::new(Semaphore::new(1));
        let release = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let first_release = Arc::clone(&release);
        let first_started = Arc::clone(&started);
        let first_limit = Arc::clone(&limit);
        let first = tokio::spawn(async move {
            run_bounded_check_inference_with(
                tokio::time::Instant::now() + Duration::from_millis(100),
                first_limit,
                move || {
                    first_started.notify_one();
                    while !first_release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Vec::new()
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("blocking manifest fixture starts");
        assert_eq!(
            first.await.expect("bounded inference task joins"),
            None,
            "a stalled manifest read must release the async caller at its deadline"
        );

        let second_started = Arc::new(AtomicBool::new(false));
        let second_observed = Arc::clone(&second_started);
        let second = run_bounded_check_inference_with(
            tokio::time::Instant::now() + Duration::from_millis(20),
            Arc::clone(&limit),
            move || {
                second_observed.store(true, Ordering::SeqCst);
                Vec::new()
            },
        )
        .await;
        assert_eq!(second, None);
        assert!(
            !second_started.load(Ordering::SeqCst),
            "a timed-out manifest read must retain its permit and cap later blocking work"
        );

        release.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(1), async {
            while limit.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("stalled manifest fixture releases its blocking permit");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn marker_sweep_timeout_does_not_block_the_async_executor_or_spawn_past_the_cap() {
        let limit = Arc::new(Semaphore::new(1));
        let release = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let first_release = Arc::clone(&release);
        let first_started = Arc::clone(&started);
        let first_limit = Arc::clone(&limit);
        let first = tokio::spawn(async move {
            run_bounded_marker_sweep_with(
                tokio::time::Instant::now() + Duration::from_millis(100),
                first_limit,
                move || {
                    first_started.notify_one();
                    while !first_release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    0
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("blocking marker fixture starts");
        assert_eq!(
            first.await.expect("bounded sweep task joins"),
            None,
            "a stalled process-table scan must release the async caller at its deadline"
        );

        let second_started = Arc::new(AtomicBool::new(false));
        let second_observed = Arc::clone(&second_started);
        let second = run_bounded_marker_sweep_with(
            tokio::time::Instant::now() + Duration::from_millis(20),
            Arc::clone(&limit),
            move || {
                second_observed.store(true, Ordering::SeqCst);
                0
            },
        )
        .await;
        assert_eq!(second, None);
        assert!(
            !second_started.load(Ordering::SeqCst),
            "a timed-out scan must retain its permit and cap later blocking work"
        );

        release.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(1), async {
            while limit.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("stalled marker fixture releases its blocking permit");
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_bounded_reap_keeps_the_root_group_fallback_armed() {
        // pgid 0 is deliberately harmless to `kill_group`, while still making
        // the guard's armed/disarmed ownership transition observable without
        // spawning a process.
        let mut process_group = BoundedReadinessProcessGroup::from_child_id(Some(0));
        assert!(
            !settle_bounded_reap_outcome(
                &mut process_group,
                crate::proc_registry::BoundedReapOutcome::Incomplete,
            ),
            "incomplete cleanup must not disarm the root-group fallback"
        );
        assert_eq!(process_group.pgid, Some(0));

        assert!(
            settle_bounded_reap_outcome(
                &mut process_group,
                crate::proc_registry::BoundedReapOutcome::Completed,
            ),
            "only a fully completed bounded reap may disarm the fallback"
        );
        assert_eq!(process_group.pgid, None);
    }

    #[tokio::test]
    async fn dropping_output_reader_owner_aborts_pending_reader_tasks() {
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));

        let stdout_started = Arc::clone(&started);
        let stdout_dropped = Arc::clone(&dropped);
        let stdout = tokio::spawn(async move {
            let _marker = ReaderTaskDropMarker {
                dropped: stdout_dropped,
            };
            stdout_started.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<()>>().await
        });
        let stderr_started = Arc::clone(&started);
        let stderr_dropped = Arc::clone(&dropped);
        let stderr = tokio::spawn(async move {
            let _marker = ReaderTaskDropMarker {
                dropped: stderr_dropped,
            };
            stderr_started.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<()>>().await
        });

        let readers = BoundedReadinessOutputReaders::new(stdout, stderr);
        tokio::time::timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reader tasks start");
        drop(readers);
        tokio::time::timeout(Duration::from_secs(1), async {
            while dropped.load(Ordering::SeqCst) != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping the owner aborts both pending reader tasks");
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
    fn active_worker_keeps_claimed_completion_in_progress_before_merge_or_drift() {
        let mut lane = facts();
        let mut merged = pr();
        merged.lifecycle = Some(PrLifecycle::Merged);
        lane.worker_active = true;
        lane.pull_requests = vec![merged];
        lane.reconciliation = ExecutionReconciliation::Drifted;
        lane.checks = CheckEvidence::Failing;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::InProgress))
        );

        lane.has_open_ask = true;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::NeedsYou, Some(ReasonCode::OpenNeed))
        );
    }

    #[test]
    fn active_working_worker_prevents_terminal_merge_readiness() {
        let mut lane = facts();
        let mut merged = pr();
        merged.lifecycle = Some(PrLifecycle::Merged);
        lane.direction_status = "working".to_string();
        lane.worker_active = true;
        lane.pull_requests = vec![merged];

        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Unknown, Some(ReasonCode::InProgress))
        );
        assert!(
            checks_are_preempted(&lane),
            "an occupied merged lane must not launch verification"
        );
    }

    #[test]
    fn worker_occupancy_matches_the_worktree_reclaim_safety_boundary() {
        for status in ["running", "starting", "stopped"] {
            assert!(worker_session_occupies_worktree(status), "{status}");
        }
        for status in ["idle", "reviving", "complete", "error"] {
            assert!(!worker_session_occupies_worktree(status), "{status}");
        }
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
    fn closed_pr_precedes_partial_probe_failure() {
        let mut closed = pr();
        closed.lifecycle = Some(PrLifecycle::Closed);
        closed.probe_failed = true;
        closed.threads = ThreadStatus::Unknown {
            reason: "review-thread probe failed".to_string(),
        };

        let mut lane = facts();
        lane.pull_requests = vec![closed];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::PrClosedUnmerged))
        );
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
    fn all_clear_merged_pr_is_terminal_after_human_gates() {
        let mut lane = facts();
        let mut merged = pr();
        merged.lifecycle = Some(PrLifecycle::Merged);
        lane.pull_requests = vec![merged];
        lane.reconciliation = ExecutionReconciliation::Drifted;
        lane.checks = CheckEvidence::NotProduced;
        lane.upstream = UpstreamEvidence::Unmet;
        assert_eq!(verdict(&lane), (LaneReadiness::ReviewReady, None));

        lane.has_open_ask = true;
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::NeedsYou, Some(ReasonCode::OpenNeed))
        );
    }

    #[test]
    fn merged_pr_requires_every_tracked_row_to_be_clear() {
        let mut lane = facts();
        let mut merged = pr();
        merged.lifecycle = Some(PrLifecycle::Merged);

        let mut ci_failing = pr();
        ci_failing.id = 2;
        ci_failing.ci = CiStatus::Failing;
        lane.pull_requests = vec![merged.clone(), ci_failing];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::PrCiFailing))
        );
        assert!(
            !checks_are_preempted(&lane),
            "a non-clear sibling PR must not skip applicable checks"
        );

        let mut closed_unmerged = pr();
        closed_unmerged.id = 3;
        closed_unmerged.lifecycle = Some(PrLifecycle::Closed);
        lane.pull_requests = vec![merged.clone(), closed_unmerged];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::PrClosedUnmerged))
        );

        let mut conflicting = pr();
        conflicting.id = 4;
        conflicting.conflict = ConflictStatus::Conflicting;
        lane.pull_requests = vec![merged, conflicting];
        assert_eq!(
            verdict(&lane),
            (LaneReadiness::Blocked, Some(ReasonCode::PrConflict))
        );
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
            PlannedLaneSource::Parsed {
                phase: ProposalPolicyPhase::Confirmed,
                lanes,
            } if lanes.len() == 1
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
    fn proposed_materialized_pending_lane_keeps_the_policy_gate_until_confirmed() {
        let reused_lane = crate::planner::ProposedDirection {
            name: "reused implementation".to_string(),
            repo: "repo".to_string(),
            reason: "reuse rollback fixture".to_string(),
            mandate: "impl-only".to_string(),
            base_branch: "main".to_string(),
            decision: String::new(),
            direction_id: 7,
        };
        assert_eq!(
            proposal_lane_policy(ProposalPolicyPhase::Proposed, &reused_lane),
            PolicyDecision::NeedsGate,
            "a proposed materialized reuse lane must not bypass the human gate"
        );
        assert_eq!(
            proposal_lane_policy(ProposalPolicyPhase::Confirmed, &reused_lane),
            PolicyDecision::AllowedByPolicy,
            "confirmed idempotent re-dispatch keeps the established allowed path"
        );

        let mut approved = reused_lane;
        approved.decision = "approved".to_string();
        assert_eq!(
            proposal_lane_policy(ProposalPolicyPhase::Proposed, &approved),
            PolicyDecision::AllowedByPolicy
        );
    }

    #[test]
    fn ask_scope_attribution_keeps_unknown_permission_scopes_issue_wide() {
        let direction_ids = HashSet::from([7]);
        let mut lane_asks = HashSet::new();
        let mut has_issue_ask = false;

        collect_open_ask_scope(
            "7",
            &direction_ids,
            &mut lane_asks,
            &mut has_issue_ask,
        );
        collect_open_ask_scope(
            "",
            &direction_ids,
            &mut lane_asks,
            &mut has_issue_ask,
        );
        collect_open_ask_scope(
            "lead",
            &direction_ids,
            &mut lane_asks,
            &mut has_issue_ask,
        );
        collect_open_ask_scope(
            "999",
            &direction_ids,
            &mut lane_asks,
            &mut has_issue_ask,
        );
        collect_open_ask_scope(
            "permission-session",
            &direction_ids,
            &mut lane_asks,
            &mut has_issue_ask,
        );

        assert_eq!(lane_asks, HashSet::from([7]));
        assert!(
            has_issue_ask,
            "empty, lead, stale, and non-numeric permission scopes must not be dropped"
        );
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
    async fn check_flight_invalidates_passing_evidence_for_uncommitted_worktree_changes() {
        let root = tempfile::tempdir().expect("temporary git worktree fixture");
        std::fs::write(root.path().join("README.md"), "green\n").expect("write initial file");
        git_in(root.path(), &["init", "--quiet"]);
        git_in(
            root.path(),
            &["config", "user.email", "readiness@example.invalid"],
        );
        git_in(root.path(), &["config", "user.name", "Readiness Test"]);
        git_in(root.path(), &["add", "README.md"]);
        git_in(root.path(), &["commit", "--quiet", "-m", "fixture"]);

        let stored_path = root.path().display().to_string();
        let clean_target = sampled_check_target(root.path(), stored_path.clone()).await;
        assert!(!clean_target.dirty, "freshly committed worktree is clean");
        let clean_head = clean_target.head_sha.clone();

        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let runs = Arc::new(AtomicUsize::new(0));
        let initial_runs = Arc::clone(&runs);
        let initial = flight
            .get_or_run(43, vec![clean_target], move |_| {
                let initial_runs = Arc::clone(&initial_runs);
                async move {
                    initial_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(CheckEvidence::Passed)
                }
            })
            .await
            .expect("initial clean check");
        assert_eq!(initial, CheckEvidence::Passed);

        std::fs::write(root.path().join("README.md"), "red\n").expect("make uncommitted change");
        let dirty_target = sampled_check_target(root.path(), stored_path.clone()).await;
        assert_eq!(
            dirty_target.head_sha, clean_head,
            "uncommitted changes preserve HEAD"
        );
        assert!(
            dirty_target.dirty,
            "modified tracked file makes target dirty"
        );

        let dirty_runs = Arc::clone(&runs);
        let dirty = flight
            .get_or_run(43, vec![dirty_target], move |_| {
                let dirty_runs = Arc::clone(&dirty_runs);
                async move {
                    dirty_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(CheckEvidence::Failing)
                }
            })
            .await
            .expect("dirty worktree reruns checks");
        assert_eq!(dirty, CheckEvidence::Failing);
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        std::fs::write(root.path().join("README.md"), "green\n").expect("restore tracked file");
        let restored_target = sampled_check_target(root.path(), stored_path).await;
        assert!(!restored_target.dirty, "restored worktree is clean");
        let restored_runs = Arc::clone(&runs);
        let restored = flight
            .get_or_run(43, vec![restored_target], move |_| {
                let restored_runs = Arc::clone(&restored_runs);
                async move {
                    restored_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(CheckEvidence::Passed)
                }
            })
            .await
            .expect("clean state after dirty interval reruns checks");
        assert_eq!(restored, CheckEvidence::Passed);
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn post_run_mutation_is_not_published_or_cached() {
        let root = tempfile::tempdir().expect("temporary post-run mutation fixture");
        std::fs::write(root.path().join("README.md"), "green\n").expect("write initial file");
        git_in(root.path(), &["init", "--quiet"]);
        git_in(
            root.path(),
            &["config", "user.email", "readiness@example.invalid"],
        );
        git_in(root.path(), &["config", "user.name", "Readiness Test"]);
        git_in(root.path(), &["add", "README.md"]);
        git_in(root.path(), &["commit", "--quiet", "-m", "fixture"]);

        let stored_path = root.path().display().to_string();
        let pre_targets = vec![sampled_check_target(root.path(), stored_path).await];
        let changed_path = root.path().join("README.md");
        let probe = GitSignatureProbe::readiness();
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);

        let evidence = checks_for_targets_with_runner_and_post_targets(
            &flight,
            CheckExecution::RunAllowed,
            88,
            pre_targets.clone(),
            move |_| async move {
                std::fs::write(changed_path, "changed during readiness check\n")
                    .map_err(|error| anyhow!("mutate check target: {error}"))?;
                Ok(CheckEvidence::Passed)
            },
            move |targets| async move { resample_check_targets(targets, &probe).await },
        )
        .await
        .expect("post-run mutation result");

        assert_eq!(evidence, CheckEvidence::NotProduced);
        assert_eq!(
            flight
                .cached_read_only(88, &pre_targets)
                .expect("cache lookup after mutation"),
            None,
            "mutated checks must not leave a memo under their pre-run signature"
        );
    }

    #[tokio::test]
    async fn same_head_branch_switch_is_not_published_or_cached() {
        let root = tempfile::tempdir().expect("temporary branch-switch fixture");
        std::fs::write(root.path().join("README.md"), "green\n").expect("write initial file");
        git_in(root.path(), &["init", "--quiet"]);
        git_in(
            root.path(),
            &["config", "user.email", "readiness@example.invalid"],
        );
        git_in(root.path(), &["config", "user.name", "Readiness Test"]);
        git_in(root.path(), &["add", "README.md"]);
        git_in(root.path(), &["commit", "--quiet", "-m", "fixture"]);

        let stored_path = root.path().display().to_string();
        let pre_targets = vec![sampled_check_target(root.path(), stored_path).await];
        let pre_head = pre_targets[0].head_sha.clone();
        let pre_branch = pre_targets[0].branch.clone();
        let switched_path = root.path().to_path_buf();
        let probe = GitSignatureProbe::readiness();
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);

        let evidence = checks_for_targets_with_runner_and_post_targets(
            &flight,
            CheckExecution::RunAllowed,
            881,
            pre_targets.clone(),
            move |_| async move {
                git_in(&switched_path, &["checkout", "--quiet", "-b", "same-head-switch"]);
                Ok(CheckEvidence::Passed)
            },
            move |targets| async move { resample_check_targets(targets, &probe).await },
        )
        .await
        .expect("same-HEAD branch-switch result");

        let after = GitSignatureProbe::readiness()
            .sample(root.path())
            .await
            .expect("sample switched branch");
        assert_eq!(after.head_sha, pre_head, "branch switch keeps the same HEAD");
        assert_ne!(after.branch, pre_branch, "fixture must change branches");
        assert_eq!(evidence, CheckEvidence::NotProduced);
        assert_eq!(
            flight
                .cached_read_only(881, &pre_targets)
                .expect("cache lookup after branch switch"),
            None,
            "same-HEAD branch switches must not publish pre-switch evidence"
        );
    }

    #[tokio::test]
    async fn failed_post_run_probe_invalidates_existing_memo_and_returns_not_produced() {
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let old_targets = check_targets("/tmp/post-run-probe", "old-head");
        let primed = flight
            .get_or_run(89, old_targets.clone(), |_| async {
                Ok(CheckEvidence::Passed)
            })
            .await
            .expect("prime old memo");
        assert_eq!(primed, CheckEvidence::Passed);

        let pre_targets = check_targets("/tmp/post-run-probe", "new-head");
        let evidence = flight
            .get_or_run_with_post_targets(
                89,
                pre_targets,
                |_| async { Ok(CheckEvidence::Failing) },
                |_| async { Err(anyhow!("post-run Git probe failed")) },
            )
            .await
            .expect("failed post-run probe becomes unavailable evidence");

        assert_eq!(evidence, CheckEvidence::NotProduced);
        assert_eq!(
            flight
                .cached_read_only(89, &old_targets)
                .expect("old memo lookup"),
            None,
            "a failed post-run probe must invalidate every prior direction memo"
        );
    }

    #[tokio::test]
    async fn matching_check_flight_followers_receive_not_produced_publication() {
        let flight = Arc::new(CheckFlight::new(CHECK_EVIDENCE_TTL, 1));
        let targets = check_targets("/tmp/post-run-followers", "head-a");
        let sampling_started = Arc::new(tokio::sync::Notify::new());
        let allow_sampling = Arc::new(tokio::sync::Notify::new());
        let leader_flight = Arc::clone(&flight);
        let leader_targets = targets.clone();
        let leader_started = Arc::clone(&sampling_started);
        let leader_allow = Arc::clone(&allow_sampling);
        let leader = tokio::spawn(async move {
            leader_flight
                .get_or_run_with_post_targets(
                    90,
                    leader_targets,
                    |_| async { Ok(CheckEvidence::Passed) },
                    move |mut post_targets| async move {
                        leader_started.notify_one();
                        leader_allow.notified().await;
                        post_targets[0].dirty = true;
                        Ok(post_targets)
                    },
                )
                .await
        });

        sampling_started.notified().await;
        let follower = match flight
            .claim_inflight(90, &targets)
            .expect("claim matching follower")
        {
            CheckFlightClaim::Follower(receiver) => receiver,
            CheckFlightClaim::Leader(_) | CheckFlightClaim::WaitForDifferentTargets(_) => {
                panic!("matching target must subscribe as a follower")
            }
        };

        allow_sampling.notify_one();
        let leader = leader
            .await
            .expect("leader post-run task joins")
            .expect("leader post-run result");
        assert_eq!(leader, CheckEvidence::NotProduced);
        assert_eq!(
            CheckFlight::wait_for_inflight(follower)
                .await
                .expect("follower publication")
                .evidence,
            CheckEvidence::NotProduced
        );
    }

    #[tokio::test]
    async fn post_run_sampling_does_not_hold_the_global_runner_permit() {
        let flight = Arc::new(CheckFlight::new(CHECK_EVIDENCE_TTL, 1));
        let sampling_started = Arc::new(tokio::sync::Notify::new());
        let allow_sampling = Arc::new(tokio::sync::Notify::new());
        let first_flight = Arc::clone(&flight);
        let first_started = Arc::clone(&sampling_started);
        let first_allow = Arc::clone(&allow_sampling);
        let first = tokio::spawn(async move {
            first_flight
                .get_or_run_with_post_targets(
                    90,
                    check_targets("/tmp/post-run-first", "head-a"),
                    |_| async { Ok(CheckEvidence::Passed) },
                    move |targets| async move {
                        first_started.notify_one();
                        first_allow.notified().await;
                        Ok(targets)
                    },
                )
                .await
        });

        sampling_started.notified().await;
        let second_flight = Arc::clone(&flight);
        let second = tokio::time::timeout(
            Duration::from_millis(250),
            second_flight.get_or_run(
                91,
                check_targets("/tmp/post-run-second", "head-a"),
                |_| async { Ok(CheckEvidence::Passed) },
            ),
        )
        .await
        .expect("post-run sampling must not retain the only runner permit")
        .expect("second readiness check result");
        assert_eq!(second, CheckEvidence::Passed);

        allow_sampling.notify_one();
        let first = first
            .await
            .expect("first post-run sampling task joins")
            .expect("first readiness check result");
        assert_eq!(first, CheckEvidence::Passed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn decisive_gates_return_before_a_stalled_git_probe_starts() {
        use sea_orm::{ActiveModelTrait, Set};

        let root = tempfile::tempdir().expect("temporary decisive gate fixture");
        let probe_started = root.path().join("stalled-git-probe-started");
        let quoted_probe_started = shell_single_quote(&probe_started.to_string_lossy());
        let git_stub = root.path().join("git-stall.sh");
        std::fs::write(
            &git_stub,
            format!("#!/bin/sh\nprintf 'started\\n' > {quoted_probe_started}\nsleep 30\n"),
        )
        .expect("write stalled git stub");
        let mut permissions = std::fs::metadata(&git_stub)
            .expect("stalled git stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&git_stub, permissions).expect("make stalled git stub executable");

        let db = Db::connect("sqlite::memory:")
            .await
            .expect("memory readiness db");
        let workspace = repo::create_workspace(&db, "decisive gate workspace")
            .await
            .expect("workspace");
        let root_path = root.path().display().to_string();
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "decisive-gate-repo",
            &root_path,
            "main",
            "",
            true,
        )
        .await
        .expect("repo reference");
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "decisive gate stalled Git probe",
            "feature",
            "claude",
        )
        .await
        .expect("thread");
        let git_probe = GitSignatureProbe {
            program: git_stub,
            timeout: Duration::from_secs(1),
            limit: Some(Arc::new(Semaphore::new(1))),
        };

        let cases = [
            (
                "worker-failed",
                PolicyDecision::AllowedByPolicy,
                Some("idle"),
                true,
                false,
                false,
                CheckExecution::RunAllowed,
                LaneReadiness::Failed,
                Some(ReasonCode::WorkerFailed),
            ),
            (
                "open-ask",
                PolicyDecision::AllowedByPolicy,
                None,
                false,
                true,
                false,
                CheckExecution::CachedOnly,
                LaneReadiness::NeedsYou,
                Some(ReasonCode::OpenNeed),
            ),
            (
                "policy-gate",
                PolicyDecision::NeedsGate,
                None,
                false,
                false,
                false,
                CheckExecution::RunAllowed,
                LaneReadiness::NeedsYou,
                Some(ReasonCode::PolicyGatePending),
            ),
            (
                "occupied-worker",
                PolicyDecision::AllowedByPolicy,
                Some("running"),
                false,
                false,
                false,
                CheckExecution::CachedOnly,
                LaneReadiness::Unknown,
                Some(ReasonCode::InProgress),
            ),
            (
                "merged-pr",
                PolicyDecision::AllowedByPolicy,
                None,
                false,
                false,
                true,
                CheckExecution::RunAllowed,
                LaneReadiness::ReviewReady,
                None,
            ),
        ];

        for (
            name,
            policy,
            worker_status,
            worker_failed,
            has_open_ask,
            has_all_clear_merged_pr,
            check_execution,
            expected_readiness,
            expected_reason,
        ) in cases
        {
            let worktree_path = root.path().join(format!("{name}-worktree"));
            std::fs::create_dir(&worktree_path).expect("worktree fixture directory");
            let worktree_path = worktree_path.display().to_string();
            let mut direction = repo::create_direction(
                &db,
                thread.id,
                name,
                "claude",
                repo_ref.id,
                "probe preemption fixture",
                "impl-only",
                "main",
            )
            .await
            .expect("direction");
            repo::record_worktree(
                &db,
                repo_ref.id,
                direction.id,
                &direction.branch,
                &worktree_path,
                false,
                false,
                "",
            )
            .await
            .expect("worktree row");
            repo::set_direction_status(&db, direction.id, "review")
                .await
                .expect("review status");
            direction.status = "review".to_string();

            let mut open_asks = HashSet::new();
            if has_open_ask {
                open_asks.insert(direction.id);
            }
            if let Some(worker_status) = worker_status {
                let session =
                    repo::create_session(&db, direction.id, repo_ref.id, "claude", &worktree_path)
                        .await
                        .expect("worker session");
                repo::set_session_status(&db, session.id, worker_status)
                    .await
                    .expect("set worker session status");
                if worker_failed {
                    repo::insert_lead_message(
                        &db,
                        thread.id,
                        Some(session.id),
                        1,
                        "assistant",
                        "text",
                        r#"{"text":"worker failed"}"#,
                        "error",
                    )
                    .await
                    .expect("failed assistant text row");
                }
            }
            if has_all_clear_merged_pr {
                pull_request::ActiveModel {
                    thread_id: Set(thread.id),
                    direction_id: Set(direction.id),
                    repo_id: Set(repo_ref.id),
                    host_kind: Set("github".to_string()),
                    host_base: Set("github.com".to_string()),
                    host_owner: Set("example".to_string()),
                    host_repo: Set("decisive-gate".to_string()),
                    number: Set(direction.id),
                    url: Set(format!(
                        "https://github.com/example/decisive-gate/pull/{}",
                        direction.id
                    )),
                    title: Set("terminal decisive gate".to_string()),
                    head_sha: Set(String::new()),
                    base_ref: Set("main".to_string()),
                    lifecycle: Set("merged".to_string()),
                    ci_status: Set(serde_json::to_string(&CiStatus::Passing).expect("ci json")),
                    review_status: Set(
                        serde_json::to_string(&ReviewStatus::Approved).expect("review json")
                    ),
                    thread_status: Set(
                        serde_json::to_string(&ThreadStatus::AllResolved).expect("thread json")
                    ),
                    conflict_status: Set(
                        serde_json::to_string(&ConflictStatus::Clean).expect("conflict json")
                    ),
                    merge_readiness: Set(String::new()),
                    last_checked_at: Set(String::new()),
                    last_error: Set(String::new()),
                    probe_fail_count: Set(0),
                    created_at: Set("0".to_string()),
                    ..Default::default()
                }
                .insert(&db.0)
                .await
                .expect("merged pull request");
            }

            let lane = tokio::time::timeout(
                Duration::from_millis(250),
                collect_lane(
                    &db,
                    &direction,
                    policy,
                    &open_asks,
                    open_pr_snapshot_freshness(1_000, 60),
                    check_execution,
                    &git_probe,
                ),
            )
            .await
            .expect("decisive readiness must not wait for the stalled Git probe")
            .expect("decisive readiness collection");

            assert_eq!(
                verdict(&lane),
                (expected_readiness, expected_reason),
                "{name} verdict"
            );
            assert_eq!(
                lane.reconciliation,
                ExecutionReconciliation::Unknown,
                "{name} must leave unprobed reconciliation fail-closed"
            );
            assert_eq!(
                lane.checks,
                CheckEvidence::NotApplicable,
                "{name} must not collect check evidence"
            );
            assert!(
                !probe_started.exists(),
                "{name} must return before starting the Git signature probe"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_git_signature_probe_keeps_collection_bounded_and_fail_closed() {
        let root = tempfile::tempdir().expect("temporary git probe fixture");
        std::fs::write(root.path().join("README.md"), "probe fixture\n")
            .expect("write fixture readme");
        git_in(root.path(), &["init", "--quiet", "-b", "main"]);
        git_in(
            root.path(),
            &["config", "user.email", "readiness@example.invalid"],
        );
        git_in(root.path(), &["config", "user.name", "Readiness Test"]);
        git_in(root.path(), &["add", "README.md"]);
        git_in(root.path(), &["commit", "--quiet", "-m", "fixture"]);

        let db = Db::connect("sqlite::memory:")
            .await
            .expect("memory readiness db");
        let workspace = repo::create_workspace(&db, "readiness probe workspace")
            .await
            .expect("workspace");
        let root_path = root.path().display().to_string();
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "readiness-probe-repo",
            &root_path,
            "main",
            "",
            true,
        )
        .await
        .expect("repo reference");
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "stalled git signature probe",
            "feature",
            "claude",
        )
        .await
        .expect("thread");
        let mut direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "claude",
            repo_ref.id,
            "probe readiness collection",
            "impl-only",
            "main",
        )
        .await
        .expect("direction");
        repo::record_worktree(
            &db,
            repo_ref.id,
            direction.id,
            &direction.branch,
            &root_path,
            false,
            false,
            "",
        )
        .await
        .expect("worktree row");
        repo::set_direction_status(&db, direction.id, "review")
            .await
            .expect("review status");
        direction.status = "review".to_string();
        assert_eq!(
            repo::list_worktrees(&db, Some(direction.id))
                .await
                .expect("recorded worktrees")
                .len(),
            1,
            "collector must have one worktree to sample"
        );

        let check_counter = root.path().join("readiness-check-count");
        std::fs::write(
            root.path().join("readiness-check.sh"),
            "#!/bin/sh\nprintf 'run\\n' >> readiness-check-count\n",
        )
        .expect("write check runner");
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"build":"sh ./readiness-check.sh"}}"#,
        )
        .expect("write package manifest");

        let pid_file = root.path().join("stalled-git-probe.pids");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        let git_stub = root.path().join("git-stall.sh");
        std::fs::write(
            &git_stub,
            format!(
                "#!/bin/sh\nsleep 30 & child=$!\nprintf '%s %s\\n' \"$$\" \"$child\" > {quoted_pid_file}\nwait\n"
            ),
        )
        .expect("write stalled git stub");
        let mut permissions = std::fs::metadata(&git_stub)
            .expect("stalled git stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&git_stub, permissions).expect("make stalled git stub executable");

        let git_probe = GitSignatureProbe {
            program: git_stub,
            timeout: Duration::from_secs(1),
            limit: Some(Arc::new(Semaphore::new(1))),
        };
        // The app prewarms this cached PATH at startup. Do the same before the
        // bounded sample so this test measures the injected Git child rather
        // than an unrelated one-time login-shell PATH discovery.
        let _ = crate::detect::tool_path();
        let no_open_asks = HashSet::new();
        let lane = tokio::time::timeout(
            Duration::from_secs(3),
            collect_lane(
                &db,
                &direction,
                PolicyDecision::AllowedByPolicy,
                &no_open_asks,
                open_pr_snapshot_freshness(1_000, 60),
                CheckExecution::RunAllowed,
                &git_probe,
            ),
        )
        .await
        .expect("stalled probe collection returns before outer deadline")
        .expect("stalled probe collection");

        assert_eq!(lane.reconciliation, ExecutionReconciliation::Unknown);
        assert_eq!(lane.checks, CheckEvidence::NotProduced);
        let verdict = lane_readiness(&lane).expect("active lane verdict");
        assert_eq!(verdict.readiness, LaneReadiness::Unknown);
        assert_eq!(verdict.reasons[0].code, ReasonCode::RemoteUnknown);
        assert!(
            !check_counter.exists(),
            "a failed signature probe must not start inferred checks"
        );

        // Prime a separate flight to prove unavailable facts bypass both a
        // prior passing memo and a new runner.
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let stale_target = CheckTarget {
            repo: "test-repo".to_string(),
            path: root_path.clone(),
            branch: "main".to_string(),
            head_sha: "old-head".to_string(),
            dirty: false,
        };
        let cached = flight
            .get_or_run(direction.id, vec![stale_target], |_| async {
                Ok(CheckEvidence::Passed)
            })
            .await
            .expect("prime passing memo");
        assert_eq!(cached, CheckEvidence::Passed);
        let runner_calls = Arc::new(AtomicUsize::new(0));
        let attempted_calls = Arc::clone(&runner_calls);
        let unavailable = vec![ProbedWorktree {
            repo: "test-repo".to_string(),
            stored_path: root_path,
            signature: None,
        }];
        let evidence = checks_for_with_runner(
            &direction,
            &unavailable,
            &flight,
            CheckExecution::RunAllowed,
            move |_| {
                attempted_calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Passed) }
            },
        )
        .await
        .expect("unavailable signature evidence");
        assert_eq!(evidence, CheckEvidence::NotProduced);
        assert_eq!(runner_calls.load(Ordering::SeqCst), 0);

        let recorded = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let Ok(recorded) = recorded else {
            // Under the full parallel suite the one-second probe budget may
            // expire before the injected shell is scheduled at all. That is
            // still the bounded, fail-closed behavior this test owns. The
            // escaped-descendant tests below require a recorded PID and cover
            // cleanup after an actual child has started.
            return;
        };
        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(pids.len(), 2, "stub shell and child must be recorded");
        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            pids.iter().all(|pid| !process_is_live(*pid)),
            "stalled git probe must leave no live process in its group: {pids:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn git_signature_probe_reaps_a_parent_exited_fsmonitor_descendant() {
        let perl_status = std::process::Command::new("perl")
            .args(["-MPOSIX", "-e", "exit 0"])
            .status();
        let Ok(perl_status) = perl_status else {
            eprintln!("perl unavailable — skipping parent-exit Git marker test");
            return;
        };
        if !perl_status.success() {
            eprintln!("perl POSIX unavailable — skipping parent-exit Git marker test");
            return;
        }

        let root = tempfile::tempdir().expect("temporary parent-exit Git fixture");
        let git_stub = root.path().join("git-fsmonitor-parent-exit.sh");
        let pid_file = root.path().join("git-fsmonitor-parent-exit.pid");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        let perl_script = r#"my $file = shift; my $pid = fork(); die $! unless defined $pid; if (!$pid) { POSIX::setsid(); open(STDIN, '<', '/dev/null'); open(STDOUT, '>', '/dev/null'); open(STDERR, '>', '/dev/null'); open(my $fh, '>', $file) or die $!; print {$fh} "$$\n"; close($fh); exec('sleep', '30'); } exit 0;"#;
        let quoted_perl_script = shell_single_quote(perl_script);
        std::fs::write(
            &git_stub,
            format!(
                "#!/bin/sh\nif [ \"$1\" = status ]; then\n  perl -MPOSIX -e {quoted_perl_script} {quoted_pid_file}\n  exit $?\nfi\nif [ \"$2\" = --abbrev-ref ]; then\n  printf 'main\\n'\nelse\n  printf '0123456789012345678901234567890123456789\\n'\nfi\n"
            ),
        )
        .expect("write parent-exit Git stub");
        let mut permissions = std::fs::metadata(&git_stub)
            .expect("parent-exit Git stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&git_stub, permissions)
            .expect("make parent-exit Git stub executable");

        let probe = GitSignatureProbe {
            program: git_stub,
            timeout: Duration::from_secs(2),
            limit: Some(Arc::new(Semaphore::new(1))),
        };
        let error = probe
            .sample(root.path())
            .await
            .expect_err("a daemonized fsmonitor descendant invalidates the Git signature");
        assert!(
            error.to_string().contains("background process")
                || error.to_string().contains("ownership sweep"),
            "unexpected marker error: {error}"
        );
        let child_pid = std::fs::read_to_string(&pid_file)
            .expect("fsmonitor descendant records pid")
            .trim()
            .parse::<i32>()
            .expect("numeric fsmonitor descendant pid");
        for _ in 0..80 {
            if !process_is_live(child_pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if let Some(group) = process_group_id(child_pid) {
            crate::proc_registry::kill_group(group);
        }
        panic!("parent-exited fsmonitor descendant survived marker cleanup: {child_pid}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn timed_out_git_probe_reaps_an_escaped_descendant_process_group() {
        // Keep the same optional-perl convention as proc_registry's own
        // escape-group coverage: the production cleanup is portable, while
        // this Unix fixture needs POSIX::setpgid to prove a distinct PGID.
        let perl_status = std::process::Command::new("perl")
            .args(["-MPOSIX", "-e", "exit 0"])
            .status();
        let Ok(perl_status) = perl_status else {
            eprintln!("perl unavailable — skipping escaped Git probe test");
            return;
        };
        if !perl_status.success() {
            eprintln!("perl POSIX unavailable — skipping escaped Git probe test");
            return;
        }

        let root = tempfile::tempdir().expect("temporary escaped Git probe fixture");
        let pid_file = root.path().join("escaped-git-probe.pids");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        let perl_script = r#"my $file = shift; my $pid = fork(); if (!$pid) { POSIX::setpgid(0, 0); open(my $fh, ">>", $file) or die $!; print {$fh} "$$\n"; close($fh); exec("sleep", "30"); } sleep 30;"#;
        let quoted_perl_script = shell_single_quote(perl_script);
        let git_stub = root.path().join("git-escape.sh");
        std::fs::write(
            &git_stub,
            format!(
                "#!/bin/sh\nprintf '%s ' \"$$\" > {quoted_pid_file}\nperl -MPOSIX -e {quoted_perl_script} {quoted_pid_file} &\nwait\n"
            ),
        )
        .expect("write escaped Git stub");
        let mut permissions = std::fs::metadata(&git_stub)
            .expect("escaped Git stub metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&git_stub, permissions).expect("make escaped Git stub executable");

        let cwd = root.path().to_path_buf();
        let program = git_stub.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let probe = tokio::spawn(async move {
            run_bounded_git_command(
                cwd.as_path(),
                program.as_path(),
                &["status", "--porcelain"],
                deadline,
                None,
            )
            .await
        });

        let recorded = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                let _ = probe.await;
                panic!("escaped Git probe did not record its descendants: {error}");
            }
        };

        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(
            pids.len(),
            2,
            "root and escaped descendant must be recorded"
        );
        assert!(
            pids.iter().all(|pid| process_is_live(*pid)),
            "the direct child and escaped descendant must still be alive before timeout: {pids:?}"
        );
        let root_group = process_group_id(pids[0]).expect("root process group");
        let escaped_group = process_group_id(pids[1]).expect("escaped process group");
        assert_ne!(
            root_group, escaped_group,
            "POSIX::setpgid must move the descendant out of the root group"
        );

        let outcome = probe.await.expect("escaped Git probe task joins");
        assert!(outcome.is_err(), "probe must return through its deadline");

        let mut reaped = false;
        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !reaped {
            // Preserve fixture hygiene on a regression without masking the
            // assertion: tree-aware cleanup itself had already been observed.
            crate::proc_registry::kill_group(root_group);
            crate::proc_registry::kill_group(escaped_group);
        }
        assert!(
            reaped,
            "tree-aware Git timeout cleanup must reap root and escaped PGID: {pids:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn timed_out_readiness_check_reaps_an_escaped_descendant_process_group() {
        let perl_status = std::process::Command::new("perl")
            .args(["-MPOSIX", "-e", "exit 0"])
            .status();
        let Ok(perl_status) = perl_status else {
            eprintln!("perl unavailable — skipping escaped readiness check test");
            return;
        };
        if !perl_status.success() {
            eprintln!("perl POSIX unavailable — skipping escaped readiness check test");
            return;
        }

        let root = tempfile::tempdir().expect("temporary escaped readiness check fixture");
        let pid_file = root.path().join("escaped-readiness-check.pids");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        let perl_script = r#"my $file = shift; my $pid = fork(); if (!$pid) { POSIX::setpgid(0, 0); open(my $fh, ">>", $file) or die $!; print {$fh} "$$\n"; close($fh); exec("sleep", "30"); } sleep 30;"#;
        let quoted_perl_script = shell_single_quote(perl_script);
        let script = format!(
            "printf '%s ' \"$$\" > {quoted_pid_file}\nperl -MPOSIX -e {quoted_perl_script} {quoted_pid_file} &\nwait"
        );
        let check = shell_check("escaped-process-group", &script);
        let cwd = root.path().to_path_buf();
        let runner = tokio::spawn(async move {
            run_bounded_check(cwd.as_path(), &check, Duration::from_secs(2)).await
        });

        let recorded = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                let _ = runner.await;
                panic!("escaped readiness check did not record its descendants: {error}");
            }
        };

        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(
            pids.len(),
            2,
            "root and escaped descendant must be recorded"
        );
        assert!(
            pids.iter().all(|pid| process_is_live(*pid)),
            "the direct check child and escaped descendant must be alive before timeout: {pids:?}"
        );
        let root_group = process_group_id(pids[0]).expect("root process group");
        let escaped_group = process_group_id(pids[1]).expect("escaped process group");
        assert_ne!(
            root_group, escaped_group,
            "POSIX::setpgid must move the check descendant out of the root group"
        );

        let outcome = tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .expect("escaped readiness check returns before outer deadline")
            .expect("escaped readiness check task joins")
            .expect("escaped readiness check execution");
        match outcome {
            BoundedCheckOutcome::NotProduced { .. } => {}
            BoundedCheckOutcome::Completed(_) => {
                panic!("timed-out escaped readiness check unexpectedly completed")
            }
        }

        let mut reaped = false;
        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !reaped {
            crate::proc_registry::kill_group(root_group);
            crate::proc_registry::kill_group(escaped_group);
        }
        assert!(
            reaped,
            "tree-aware readiness check timeout cleanup must reap root and escaped PGID: {pids:?}"
        );
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
    async fn not_produced_evidence_is_not_cached_and_recovery_reruns() {
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let targets = check_targets("/tmp/not-produced-recovery", "head-a");
        let runs = Arc::new(AtomicUsize::new(0));

        let first_runs = Arc::clone(&runs);
        let first = flight
            .get_or_run(421, targets.clone(), move |_| {
                first_runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::NotProduced) }
            })
            .await
            .expect("not-produced verification result");
        assert_eq!(first, CheckEvidence::NotProduced);
        assert_eq!(
            flight
                .cached_read_only(421, &targets)
                .expect("not-produced cache lookup"),
            None,
            "not-produced verification must not occupy the ten-minute cache"
        );

        let recovery_runs = Arc::clone(&runs);
        let recovered = flight
            .get_or_run(421, targets, move |_| {
                recovery_runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Passed) }
            })
            .await
            .expect("recovered verification reruns");
        assert_eq!(recovered, CheckEvidence::Passed);
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cached_only_does_not_read_a_memo_while_admission_is_held() {
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let targets = check_targets("/tmp/cached-only-admission", "head-a");
        let primed = flight
            .get_or_run(422, targets.clone(), |_| async {
                Ok(CheckEvidence::Passed)
            })
            .await
            .expect("prime cached evidence");
        assert_eq!(primed, CheckEvidence::Passed);

        let admission = flight
            .acquire_admission(422)
            .await
            .expect("hold worker-style admission");
        let runner_calls = Arc::new(AtomicUsize::new(0));
        let attempted_runner = Arc::clone(&runner_calls);
        let observed = tokio::time::timeout(
            Duration::from_millis(100),
            checks_for_targets_with_runner(
                &flight,
                CheckExecution::CachedOnly,
                422,
                targets.clone(),
                move |_| {
                    let attempted_runner = Arc::clone(&attempted_runner);
                    async move {
                        attempted_runner.fetch_add(1, Ordering::SeqCst);
                        Ok(CheckEvidence::Failing)
                    }
                },
            ),
        )
        .await
        .expect("cached-only must not wait for admission")
        .expect("cached-only admission result");

        assert_eq!(observed, CheckEvidence::NotProduced);
        assert_eq!(runner_calls.load(Ordering::SeqCst), 0);
        flight
            .invalidate_cached(422)
            .expect("worker-style memo invalidation");
        drop(admission);
        assert_eq!(
            flight
                .cached_read_only(422, &targets)
                .expect("memo lookup after invalidation"),
            None
        );
    }

    #[tokio::test]
    async fn worker_start_invalidation_prevents_cached_only_reuse_of_prior_memo() {
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let targets = check_targets("/tmp/cached-only-after-worker-start", "head-a");
        let primed = flight
            .get_or_run(424, targets.clone(), |_| async {
                Ok(CheckEvidence::Passed)
            })
            .await
            .expect("prime cached evidence");
        assert_eq!(primed, CheckEvidence::Passed);

        // This is the worker direct-start sequence: acquire the shared gate,
        // invalidate the older memo, then reserve/persist while it is held.
        let worker_admission = flight
            .acquire_admission(424)
            .await
            .expect("worker start admission");
        flight
            .invalidate_cached(424)
            .expect("worker start invalidates prior memo");
        drop(worker_admission);

        let runner_calls = Arc::new(AtomicUsize::new(0));
        let attempted_runner = Arc::clone(&runner_calls);
        let evidence = checks_for_targets_with_runner(
            &flight,
            CheckExecution::CachedOnly,
            424,
            targets,
            move |_| {
                attempted_runner.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Failing) }
            },
        )
        .await
        .expect("cached-only lookup after worker start");

        assert_eq!(evidence, CheckEvidence::NotProduced);
        assert_eq!(runner_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn run_allowed_cached_hit_waits_for_admission_and_rechecks_memo() {
        let flight = Arc::new(CheckFlight::new(CHECK_EVIDENCE_TTL, 1));
        let targets = check_targets("/tmp/run-allowed-admission", "head-a");
        let primed = flight
            .get_or_run(423, targets.clone(), |_| async {
                Ok(CheckEvidence::Passed)
            })
            .await
            .expect("prime cached evidence");
        assert_eq!(primed, CheckEvidence::Passed);

        let admission = flight
            .acquire_admission(423)
            .await
            .expect("hold worker-style admission");
        let runner_calls = Arc::new(AtomicUsize::new(0));
        let waiter_flight = Arc::clone(&flight);
        let waiter_targets = targets.clone();
        let attempted_runner = Arc::clone(&runner_calls);
        let mut cached_hit = tokio::spawn(async move {
            waiter_flight
                .get_or_run(423, waiter_targets, move |_| {
                    let attempted_runner = Arc::clone(&attempted_runner);
                    async move {
                        attempted_runner.fetch_add(1, Ordering::SeqCst);
                        Ok(CheckEvidence::Failing)
                    }
                })
                .await
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut cached_hit)
                .await
                .is_err(),
            "a RunAllowed cached hit must wait behind the admission gate"
        );
        flight
            .invalidate_cached(423)
            .expect("worker-style memo invalidation");
        drop(admission);

        let evidence = cached_hit
            .await
            .expect("cached-hit task joins")
            .expect("RunAllowed result after invalidation");
        assert_eq!(evidence, CheckEvidence::Failing);
        assert_eq!(runner_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn run_allowed_cached_hit_revalidates_targets_after_admission() {
        let flight = CheckFlight::new(CHECK_EVIDENCE_TTL, 1);
        let targets = check_targets("/tmp/run-allowed-target-recheck", "head-a");
        let primed = flight
            .get_or_run(425, targets.clone(), |_| async {
                Ok(CheckEvidence::Passed)
            })
            .await
            .expect("prime cached evidence");
        assert_eq!(primed, CheckEvidence::Passed);

        let runner_calls = Arc::new(AtomicUsize::new(0));
        let attempted_runner = Arc::clone(&runner_calls);
        let report = flight
            .get_or_run_report_with_admission_and_post_targets(
                425,
                targets.clone(),
                move |_| {
                    attempted_runner.fetch_add(1, Ordering::SeqCst);
                    async {
                        Ok(VerificationReport::from_evidence(
                            CheckEvidence::Failing,
                        ))
                    }
                },
                |mut admitted_targets| async move {
                    admitted_targets[0].head_sha = "head-b".to_string();
                    Ok(admitted_targets)
                },
                |post_targets| async move { Ok(post_targets) },
            )
            .await
            .expect("mismatched admitted target is fail-closed");

        assert_eq!(report.evidence, CheckEvidence::NotProduced);
        assert_eq!(runner_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            flight
                .cached_read_only(425, &targets)
                .expect("cache lookup after target mismatch"),
            None,
            "the pre-admission memo must be invalidated"
        );
    }

    #[tokio::test]
    async fn cached_only_checks_never_start_a_runner_and_reuse_a_fresh_memo() {
        let flight = Arc::new(CheckFlight::new(CHECK_EVIDENCE_TTL, 1));
        let runs = Arc::new(AtomicUsize::new(0));
        let post_samples = Arc::new(AtomicUsize::new(0));
        let without_cache_runs = Arc::clone(&runs);
        let without_cache_samples = Arc::clone(&post_samples);
        let without_cache = checks_for_targets_with_runner_and_post_targets(
            &flight,
            CheckExecution::CachedOnly,
            57,
            check_targets("/tmp/cached-only", "head-a"),
            move |_| {
                without_cache_runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Failing) }
            },
            move |targets| {
                without_cache_samples.fetch_add(1, Ordering::SeqCst);
                async move { Ok(targets) }
            },
        )
        .await
        .expect("cached-only check without memo");
        assert_eq!(without_cache, CheckEvidence::NotProduced);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(
            post_samples.load(Ordering::SeqCst),
            0,
            "cached-only must not re-sample targets"
        );

        let inflight_runs = Arc::new(AtomicUsize::new(0));
        let runner_started = Arc::new(tokio::sync::Notify::new());
        let allow_runner = Arc::new(tokio::sync::Notify::new());
        let inflight_flight = Arc::clone(&flight);
        let started = Arc::clone(&runner_started);
        let allow = Arc::clone(&allow_runner);
        let active_runs = Arc::clone(&inflight_runs);
        let active = tokio::spawn(async move {
            inflight_flight
                .get_or_run(
                    58,
                    check_targets("/tmp/cached-only-inflight", "head-a"),
                    move |_| async move {
                        active_runs.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        allow.notified().await;
                        Ok(CheckEvidence::Passed)
                    },
                )
                .await
        });
        runner_started.notified().await;

        let cached_only_runs = Arc::clone(&inflight_runs);
        let during_inflight = tokio::time::timeout(
            Duration::from_millis(100),
            checks_for_targets_with_runner(
                &flight,
                CheckExecution::CachedOnly,
                58,
                check_targets("/tmp/cached-only-inflight", "head-a"),
                move |_| async move {
                    cached_only_runs.fetch_add(1, Ordering::SeqCst);
                    Ok(CheckEvidence::Failing)
                },
            ),
        )
        .await
        .expect("cached-only must not wait for an in-flight runner")
        .expect("cached-only in-flight result");
        assert_eq!(during_inflight, CheckEvidence::NotProduced);
        assert_eq!(
            inflight_runs.load(Ordering::SeqCst),
            1,
            "cached-only must not start a second runner while a matching flight is pending"
        );
        allow_runner.notify_one();
        assert_eq!(
            active
                .await
                .expect("in-flight runner task joins")
                .expect("in-flight runner result"),
            CheckEvidence::Passed
        );

        let initial_runs = Arc::clone(&runs);
        let initial = checks_for_targets_with_runner(
            &flight,
            CheckExecution::RunAllowed,
            57,
            check_targets("/tmp/cached-only", "head-a"),
            move |_| {
                initial_runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Passed) }
            },
        )
        .await
        .expect("allowed check primes memo");
        assert_eq!(initial, CheckEvidence::Passed);
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let cached_runs = Arc::clone(&runs);
        let cached = checks_for_targets_with_runner(
            &flight,
            CheckExecution::CachedOnly,
            57,
            check_targets("/tmp/cached-only", "head-a"),
            move |_| {
                cached_runs.fetch_add(1, Ordering::SeqCst);
                async { Ok(CheckEvidence::Failing) }
            },
        )
        .await
        .expect("cached-only check reuses fresh memo");
        assert_eq!(cached, CheckEvidence::Passed);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn zero_rung_check_suite_is_not_produced() {
        let root = tempfile::tempdir().expect("temporary no-rung fixture");
        let evidence = run_readiness_checks(
            vec![root.path().display().to_string()],
            Duration::from_millis(25),
        )
        .await
        .expect("no-rung readiness check");

        assert_eq!(evidence, CheckEvidence::NotProduced);
    }

    #[tokio::test]
    async fn bounded_check_evidence_keeps_observed_failures_sticky() {
        let root = tempfile::tempdir().expect("temporary check fixture");

        let failure_then_timeout = run_checks_with_timeout(
            root.path(),
            &[
                shell_check("fail", "exit 1"),
                shell_check("hang", "sleep 30"),
            ],
            Duration::from_millis(25),
        )
        .await
        .expect("failure followed by timeout");
        assert_eq!(failure_then_timeout, CheckEvidence::Failing);

        let all_pass = run_checks_with_timeout(
            root.path(),
            &[
                shell_check("pass-one", "exit 0"),
                shell_check("pass-two", "exit 0"),
            ],
            Duration::from_millis(250),
        )
        .await
        .expect("all passing checks");
        assert_eq!(all_pass, CheckEvidence::Passed);

        let timeout_only = run_checks_with_timeout(
            root.path(),
            &[shell_check("hang-only", "sleep 30")],
            Duration::from_millis(25),
        )
        .await
        .expect("timeout-only check");
        assert_eq!(timeout_only, CheckEvidence::NotProduced);
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
    async fn noisy_check_streams_a_bounded_tail_before_its_deadline() {
        let root = tempfile::tempdir().expect("temporary noisy check fixture");
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            run_bounded_check(
                root.path(),
                &shell_check(
                    "noisy",
                    "while :; do printf 'readiness-output-readiness-output-readiness-output\\n'; done",
                ),
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("noisy check returns after its deadline")
        .expect("noisy check execution");
        let output_tail = match outcome {
            BoundedCheckOutcome::NotProduced { output_tail } => output_tail,
            BoundedCheckOutcome::Completed(_) => panic!("noisy check unexpectedly completed"),
        };
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "noisy process must be killed by the bounded deadline"
        );
        assert!(
            output_tail.as_bytes().len() <= CHECK_OUTPUT_TAIL_BYTES + "…\n".len(),
            "tail must stay bounded, got {} bytes",
            output_tail.as_bytes().len()
        );
        assert!(
            output_tail.starts_with("…\n"),
            "large output must report that its retained tail was truncated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_readiness_check_reaps_the_entire_process_group() {
        let root = tempfile::tempdir().expect("temporary check fixture");
        let pid_file = root.path().join("check-processes.pid");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        let script = format!(
            "sleep 30 & child=$!; printf '%s %s\\n' \"$$\" \"$child\" > {quoted_pid_file}; wait"
        );
        let evidence = run_checks_with_timeout(
            root.path(),
            &[shell_check("spawn-descendant", &script)],
            Duration::from_secs(1),
        )
        .await
        .expect("timed process-group check");
        assert_eq!(evidence, CheckEvidence::NotProduced);

        let recorded = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("check shell records its process-group members before timeout");
        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(
            pids.len(),
            2,
            "shell and spawned descendant must be recorded"
        );

        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            pids.iter().all(|pid| !process_is_live(*pid)),
            "timeout must leave no live process in the check's group: {pids:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_check_runner_aborts_pipe_readers_and_cleans_the_background_holder() {
        let root = tempfile::tempdir().expect("temporary cancelled-check fixture");
        let pid_file = root.path().join("cancelled-check-processes.pid");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        // The background child inherits both pipe writers and keeps them open
        // while the enclosing check future is cancelled.
        let script = format!(
            "sleep 30 & child=$!; printf '%s %s\\n' \"$$\" \"$child\" > {quoted_pid_file}; wait"
        );
        let check = shell_check("cancelled-pipe-holder", &script);
        let cwd = root.path().to_path_buf();
        let runner = tokio::spawn(async move {
            run_bounded_check(cwd.as_path(), &check, Duration::from_secs(30)).await
        });

        let recorded = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                runner.abort();
                let _ = runner.await;
                panic!("cancelled check did not record its pipe holder: {error}");
            }
        };
        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(pids.len(), 2, "shell and pipe holder must be recorded");

        runner.abort();
        let join = runner.await;
        let Err(error) = join else {
            panic!("cancelling the readiness check task must abort it");
        };
        assert!(error.is_cancelled(), "outer readiness task is cancelled");

        let mut cleaned = false;
        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                cleaned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !cleaned {
            crate::proc_registry::kill_group(pids[0]);
        }
        assert!(
            cleaned,
            "dropping the check future must not leave a pipe-holding background descendant: {pids:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_check_child_drops_its_registration_before_pipe_drains_finish() {
        let root = tempfile::tempdir().expect("temporary registration-drop fixture");
        let pid_file = root.path().join("registration-drop-processes.pid");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        // The shell exits after recording its PID, but its background child
        // retains the pipe writers so the runner remains in reader-drain work.
        let script = format!(
            "sleep 30 & child=$!; printf '%s %s\\n' \"$$\" \"$child\" > {quoted_pid_file}; exit 0"
        );
        let check = shell_check("registration-drop-pipe-holder", &script);
        let cwd = root.path().to_path_buf();
        let runner = tokio::spawn(async move {
            run_bounded_check(cwd.as_path(), &check, Duration::from_secs(30)).await
        });

        let recorded = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let recorded = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                runner.abort();
                let _ = runner.await;
                panic!("registration-drop check did not record its child: {error}");
            }
        };
        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(pids.len(), 2, "shell and pipe holder must be recorded");
        let root_pid = pids[0] as u32;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let registered = crate::proc_registry::registered();
                if !registered
                    .iter()
                    .any(|registration| registration.pid == root_pid)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("successful child wait drops its registration before pipe drains finish");

        runner.abort();
        let _ = runner.await;
        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        crate::proc_registry::kill_group(pids[0]);
        assert!(
            pids.iter().all(|pid| !process_is_live(*pid)),
            "registration-drop fixture must not leak its pipe holder: {pids:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_readiness_check_reaps_redirected_background_descendants() {
        let root = tempfile::tempdir().expect("temporary completed-check fixture");
        let pid_file = root.path().join("completed-check-processes.pid");
        let quoted_pid_file = shell_single_quote(&pid_file.to_string_lossy());
        let script = format!(
            "sleep 30 >/dev/null 2>&1 & child=$!; printf '%s %s\\n' \"$$\" \"$child\" > {quoted_pid_file}; exit 0"
        );

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            run_bounded_check(
                root.path(),
                &shell_check("completed-background-descendant", &script),
                Duration::from_secs(5),
            ),
        )
        .await
        .expect("completed check must not wait for its redirected background child")
        .expect("completed check execution");
        match outcome {
            BoundedCheckOutcome::NotProduced { output_tail } => assert!(
                output_tail.contains("left 1 background process"),
                "daemonizing checks must be discarded with an ownership reason: {output_tail}"
            ),
            BoundedCheckOutcome::Completed(_) => {
                panic!("a check that leaves a background process must not publish pass evidence")
            }
        }

        let recorded = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if let Ok(recorded) = std::fs::read_to_string(&pid_file) {
                    if recorded.split_whitespace().count() == 2 {
                        return recorded;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("completed check shell records its process-group members");
        let pids: Vec<i32> = recorded
            .split_whitespace()
            .map(|pid| pid.parse::<i32>().expect("numeric process id"))
            .collect();
        assert_eq!(
            pids.len(),
            2,
            "shell and redirected background descendant must be recorded"
        );

        let mut reaped = false;
        for _ in 0..40 {
            if pids.iter().all(|pid| !process_is_live(*pid)) {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !reaped {
            crate::proc_registry::kill_group(pids[0]);
        }
        assert!(
            reaped,
            "completed check must leave no live process in the check's group: {pids:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn completed_check_reaps_a_setsid_child_after_the_direct_parent_exits() {
        let perl_status = std::process::Command::new("perl")
            .args(["-MPOSIX", "-e", "exit 0"])
            .status();
        let Ok(perl_status) = perl_status else {
            eprintln!("perl unavailable — skipping completed setsid check test");
            return;
        };
        if !perl_status.success() {
            eprintln!("perl POSIX unavailable — skipping completed setsid check test");
            return;
        }

        let root = tempfile::tempdir().expect("temporary completed setsid fixture");
        let pid_file = root.path().join("completed-setsid-child.pid");
        let perl_script = r#"my $file = shift; my $pid = fork(); if (!$pid) { POSIX::setsid(); open(STDOUT, '>', '/dev/null'); open(STDERR, '>', '/dev/null'); open(my $fh, '>', $file) or die $!; print {$fh} "$$\n"; close($fh); exec('sleep', '30'); } select(undef, undef, undef, 0.1); exit 0;"#;
        let check = crate::check::Check {
            name: "completed-setsid-child".to_string(),
            program: "perl".to_string(),
            args: vec![
                "-MPOSIX".to_string(),
                "-e".to_string(),
                perl_script.to_string(),
                pid_file.to_string_lossy().into_owned(),
            ],
        };

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            run_bounded_check(root.path(), &check, Duration::from_secs(5)),
        )
        .await
        .expect("setsid parent-exit check returns")
        .expect("setsid parent-exit execution");
        match outcome {
            BoundedCheckOutcome::NotProduced { output_tail } => assert!(
                output_tail.contains("background process"),
                "escaped ownership must invalidate evidence: {output_tail}"
            ),
            BoundedCheckOutcome::Completed(_) => {
                panic!("an escaped setsid child must prevent pass evidence")
            }
        }

        let child_pid = std::fs::read_to_string(&pid_file)
            .expect("setsid child records pid")
            .trim()
            .parse::<i32>()
            .expect("numeric setsid child pid");
        let mut reaped = false;
        for _ in 0..40 {
            if !process_is_live(child_pid) {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !reaped {
            if let Some(group) = process_group_id(child_pid) {
                crate::proc_registry::kill_group(group);
            }
        }
        assert!(reaped, "escaped setsid child must be reaped: {child_pid}");
    }

    #[tokio::test]
    async fn decisive_gates_skip_the_check_flight() {
        let flight = Arc::new(CheckFlight::new(Duration::ZERO, 1));
        let runs = Arc::new(AtomicUsize::new(0));
        let skipped_flight = Arc::clone(&flight);
        let skipped_runs = Arc::clone(&runs);
        let mut lane = facts();
        lane.reconciliation = ExecutionReconciliation::Drifted;
        let evidence = checks_after_decisive_gates(&lane, move || {
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
        .expect("decisive skip did not retain a runner permit")
        .expect("post-drift check result");
        assert_eq!(released, CheckEvidence::Passed);
    }

    #[tokio::test]
    async fn open_ask_short_circuits_a_never_finishing_check_runner() {
        let runs = Arc::new(AtomicUsize::new(0));
        let attempted_runs = Arc::clone(&runs);
        let mut lane = facts();
        lane.has_open_ask = true;
        let evidence = tokio::time::timeout(
            Duration::from_millis(100),
            checks_after_decisive_gates(&lane, move || {
                attempted_runs.fetch_add(1, Ordering::SeqCst);
                async { std::future::pending::<Result<CheckEvidence>>().await }
            }),
        )
        .await
        .expect("open ask returns without awaiting the runner")
        .expect("open ask check decision");
        assert_eq!(evidence, CheckEvidence::NotApplicable);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
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
