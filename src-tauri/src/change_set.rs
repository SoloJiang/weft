//! Issue #175: the Issue Change Set — one delivery overview for a whole issue.
//!
//! An issue's state is otherwise spread across its Directions, worktrees,
//! Evidence rows and Sessions, and the user has to open a Session to assemble
//! it. This module assembles it once, on the backend, so the first screen can
//! answer: what will be written, why those repos, in what order, how much of it
//! is trustworthy, and what is left.
//!
//! ## One collection, two projections
//!
//! [`crate::readiness::collect_facts_and_readiness`] already computes every
//! fact the verdict needs — including the Git signature probe of each lane's
//! registered worktrees — and then discards what the verdict did not use. This
//! view wants exactly those discarded facts. It takes them from the SAME
//! collection rather than re-deriving them, because two derivations are two
//! answers that can disagree — the drift `readiness` exists to prevent — and
//! because re-probing pays for the same `git` calls twice on the path that
//! paints the first screen. The verdict here is therefore always the verdict
//! `issue_readiness` would give, by construction rather than by agreement.
//!
//! ## What this module does NOT decide
//!
//! Nothing. Every field on a lane row is either a row read from the store or a
//! value `readiness` already decided. Adding a judgment here would create the
//! second opinion the paragraph above exists to rule out.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::readiness::{
    CheckEvidence, CheckExecution, ExecutionReconciliation, IssueReadiness, IssueReadinessDto,
    LaneCheckout, LaneFacts, LaneReadiness, PullRequestFacts, ReadinessReason, UpstreamEvidence,
};
use crate::store::repo;
use crate::store::Db;

/// How much of a lane's Evidence can still be believed.
///
/// Counts rather than one verdict: "3 fresh, 1 stale" and "1 fresh, 3 stale"
/// are different situations for a human deciding whether to look, and
/// collapsing them to a single word throws that away. `readiness` still owns
/// whether any of it BLOCKS — this is only how much there is.
///
/// `unknown` absorbs both "the collector could not sample this" and "this row
/// is revision-anchored and the lane was never probed", exactly as
/// `store::repo::evidence_freshness` already reports them to `list_evidence`.
/// Both mean the same thing to the reader: you cannot rely on it without
/// looking.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub fresh: usize,
    pub stale: usize,
    pub unknown: usize,
    /// Newest `observed_at` across this lane's rows, verbatim as the store
    /// recorded it. `None` when the lane has no Evidence at all — which is not
    /// the same as having Evidence that is merely old.
    pub newest_observed_at: Option<String>,
}

/// The declared-versus-observed pair for one lane's checkout.
///
/// Kept as declared fields beside the observed rows rather than as one
/// "drifted" boolean: the user needs to see WHAT differs to know what to do
/// about it, and `readiness` has already decided whether the difference blocks
/// (`reconciliation`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFacts {
    /// The ref the lane's branch was created off. Empty = the repo default,
    /// resolved live at materialize time.
    pub declared_base: String,
    /// The branch this lane's work is supposed to live on.
    pub declared_branch: String,
    /// What the shared probe found, or `None` when this collection never
    /// probed this lane. See [`LaneFacts::checkouts`]: `None` and `Some([])`
    /// are different states and the UI must not merge them.
    pub observed: Option<Vec<LaneCheckout>>,
}

/// One lane's row in the overview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSetLane {
    pub direction_id: i32,
    pub name: String,
    /// Which repository this lane writes to, and why it was chosen. `reason` is
    /// for explanation and audit — it has never gated anything.
    pub repo_name: String,
    pub reason: String,
    pub checkout: CheckoutFacts,
    /// Lanes this one waits on, so the first screen can show the order without
    /// the user reconstructing it lane by lane.
    pub depends_on: Vec<i32>,
    pub direction_status: String,
    pub reconciliation: ExecutionReconciliation,
    pub checks: CheckEvidence,
    pub upstream: UpstreamEvidence,
    /// The host rows behind the verdict, so the reader can see WHICH PR is
    /// red. Deliberately without the collection's open-PR snapshot TTL: that
    /// is a policy input `readiness` has already applied, and re-exporting it
    /// would invite a second reader to re-judge staleness and disagree.
    pub pull_requests: Vec<PullRequestFacts>,
    pub evidence: EvidenceSummary,
    /// Exactly what `issue_readiness` decided for this lane — never recomputed.
    pub readiness: LaneReadiness,
    pub reasons: Vec<ReadinessReason>,
}

/// The whole overview for one issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueChangeSet {
    pub readiness: IssueReadiness,
    pub reasons: Vec<ReadinessReason>,
    pub active_lane_count: usize,
    pub lanes: Vec<ChangeSetLane>,
}

/// How many Evidence rows one scan may return per issue.
///
/// The summary only needs counts, and an issue that has produced more rows than
/// this has long since answered "is there evidence here". Bounded so a
/// long-running issue cannot make the first screen slow. `list_evidence` orders
/// newest-first, so a truncated scan drops the OLDEST rows — the ones least
/// likely to change what the reader does.
const EVIDENCE_SCAN_LIMIT: u64 = 200;

/// Collect one issue's Change Set, running verification the same way the
/// desktop readiness command does.
pub async fn collect(
    db: &Db,
    bus: &crate::bus::BusRegistry,
    asks: &crate::ask::AskRegistry,
    thread_id: i32,
) -> Result<IssueChangeSet> {
    let (verdict, facts) = crate::readiness::collect_facts_and_readiness(
        db,
        bus,
        asks,
        thread_id,
        CheckExecution::RunAllowed,
    )
    .await?;
    project(db, thread_id, verdict, &facts).await
}

/// Join the collected facts with the rows that describe WHERE each lane writes.
///
/// Split out from [`collect`] so the projection can be exercised against
/// constructed facts without standing up a real collection.
async fn project(
    db: &Db,
    thread_id: i32,
    verdict: IssueReadinessDto,
    facts: &[LaneFacts],
) -> Result<IssueChangeSet> {
    let directions = repo::list_directions(db, thread_id).await?;
    let directions_by_id: HashMap<i32, &crate::store::entities::direction::Model> =
        directions.iter().map(|row| (row.id, row)).collect();
    let evidence = evidence_by_direction(db, thread_id, facts).await?;

    // `issue_readiness` builds its lane list by filtering these same facts in
    // order, so its rows are an ordered SUBSEQUENCE of `facts` and a cursor
    // walk pairs them exactly. Deliberately not a `direction_id` map: virtual
    // lanes — the unbound-PR row, the issue-wide ask row, every proposed lane
    // not yet materialized — all carry `direction_id == 0`, so keying on it
    // would give several distinct lanes one shared verdict.
    let mut next_verdict = 0usize;

    let mut lanes = Vec::with_capacity(verdict.lanes.len());
    for fact in facts {
        // A lane the verdict left out is one it does not count — an inactive or
        // policy-denied lane. Showing it here would contradict the
        // `active_lane_count` printed beside it.
        let Some(lane_verdict) = verdict.lanes.get(next_verdict).filter(|candidate| {
            candidate.direction_id == fact.direction_id && candidate.name == fact.name
        }) else {
            continue;
        };
        next_verdict += 1;
        // Virtual lanes (`direction_id == 0`: the unbound-PR row, the
        // issue-wide ask row) carry a verdict but no direction, so they have no
        // repo, no checkout and no dependencies to look up.
        let direction = directions_by_id.get(&fact.direction_id).copied();
        let repo_name = match direction {
            Some(direction) => repo_name_of(db, direction.repo_id).await,
            None => String::new(),
        };
        let depends_on = match direction {
            Some(direction) => upstream_direction_ids(db, direction.id).await?,
            None => Vec::new(),
        };
        lanes.push(ChangeSetLane {
            direction_id: fact.direction_id,
            name: fact.name.clone(),
            repo_name,
            reason: direction.map(|row| row.reason.clone()).unwrap_or_default(),
            checkout: CheckoutFacts {
                declared_base: direction
                    .map(|row| row.base_branch.clone())
                    .unwrap_or_default(),
                declared_branch: direction.map(|row| row.branch.clone()).unwrap_or_default(),
                observed: fact.checkouts.clone(),
            },
            depends_on,
            direction_status: fact.direction_status.clone(),
            reconciliation: fact.reconciliation,
            checks: fact.checks,
            upstream: fact.upstream,
            pull_requests: fact.pull_requests.clone(),
            evidence: evidence
                .get(&fact.direction_id)
                .cloned()
                .unwrap_or_default(),
            readiness: lane_verdict.readiness,
            reasons: lane_verdict.reasons.clone(),
        });
    }

    Ok(IssueChangeSet {
        readiness: verdict.readiness,
        reasons: verdict.reasons,
        active_lane_count: verdict.active_lane_count,
        lanes,
    })
}

/// A repo row that no longer resolves yields an empty name rather than an
/// error: the Change Set is a read of whatever is there, and one dangling
/// `repo_id` must not blank the whole overview.
async fn repo_name_of(db: &Db, repo_id: i32) -> String {
    repo::get_repo(db, repo_id)
        .await
        .ok()
        .flatten()
        .map(|row| row.name)
        .unwrap_or_default()
}

/// This lane's upstream lane ids — the resolved edges only.
///
/// A denied or unresolved edge carries `upstream_direction_id == 0` by
/// construction: it names no lane. Those edges are already reflected in the
/// lane's `upstream` evidence, which `readiness` reduced into the verdict;
/// drawing them here as dependency arrows would point at a lane that does not
/// exist.
async fn upstream_direction_ids(db: &Db, direction_id: i32) -> Result<Vec<i32>> {
    Ok(repo::direction_upstream_edges(db, direction_id)
        .await?
        .into_iter()
        .filter(|edge| edge.state == repo::UpstreamEdgeState::Resolved)
        .map(|edge| edge.upstream_direction_id)
        .filter(|id| *id != 0)
        .collect())
}

/// One scan of the issue's Evidence, bucketed per lane.
///
/// Read once for the whole issue rather than per lane: an issue with many lanes
/// would otherwise re-query the same table once per row on the path that paints
/// the first screen.
///
/// Freshness is judged against the head SHAs the collection ALREADY sampled
/// (`LaneFacts::checkouts`), keyed by repo name the same way evidence rows key
/// `source_ref`. A lane this collection did not probe supplies no SHA, and
/// `evidence_freshness` then fails closed to `unknown` for its
/// revision-anchored rows — the same answer `list_evidence` gives when it
/// cannot probe.
async fn evidence_by_direction(
    db: &Db,
    thread_id: i32,
    facts: &[LaneFacts],
) -> Result<HashMap<i32, EvidenceSummary>> {
    let mut head_shas_by_direction: HashMap<i32, HashMap<&str, &str>> = HashMap::new();
    for fact in facts {
        let entry = head_shas_by_direction.entry(fact.direction_id).or_default();
        for checkout in fact.checkouts.iter().flatten() {
            if let Some(observed) = checkout.observed.as_ref() {
                entry.insert(checkout.repo_name.as_str(), observed.head_sha.as_str());
            }
        }
    }

    let rows = repo::list_evidence(db, thread_id, None, EVIDENCE_SCAN_LIMIT).await?;
    let now_secs = now_unix_secs();
    let host_max_age_secs = repo::evidence_host_max_age_secs();
    let mut out: HashMap<i32, EvidenceSummary> = HashMap::new();
    for row in rows {
        let current_revision = head_shas_by_direction
            .get(&row.direction_id)
            .and_then(|shas| shas.get(row.source_ref.as_str()))
            .copied();
        let freshness =
            repo::evidence_freshness(&row, current_revision, now_secs, host_max_age_secs);
        let entry = out.entry(row.direction_id).or_default();
        match freshness {
            repo::EvidenceFreshness::Fresh => entry.fresh += 1,
            repo::EvidenceFreshness::Stale => entry.stale += 1,
            repo::EvidenceFreshness::Unknown => entry.unknown += 1,
        }
        // Rows arrive newest-first, so the first one seen for a lane is its
        // newest and must not be overwritten by the older ones behind it.
        if entry.newest_observed_at.is_none() {
            entry.newest_observed_at = Some(row.observed_at.clone());
        }
    }
    Ok(out)
}

/// A clock read that cannot fail. A system clock before the epoch would make
/// every host row look infinitely old; `0` makes it look infinitely NEW, which
/// is the wrong way to fail. Falling back to `i64::MAX` keeps the fail-closed
/// direction: unreadable clock ⇒ host evidence reads stale.
fn now_unix_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(_) => i64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readiness::{
        issue_readiness, CheckoutSignature, ExecutionReconciliation, OpenPrSnapshotFreshness,
        PolicyDecision,
    };
    use crate::store::entities::direction;

    fn freshness() -> OpenPrSnapshotFreshness {
        OpenPrSnapshotFreshness::MaxAge {
            now_secs: 1_000,
            max_age_secs: 180,
        }
    }

    fn lane_facts(direction_id: i32, name: &str) -> LaneFacts {
        LaneFacts {
            direction_id,
            name: name.to_string(),
            active: true,
            policy: PolicyDecision::AllowedByPolicy,
            worker_failed: false,
            worker_active: false,
            has_open_ask: false,
            reconciliation: ExecutionReconciliation::Matched,
            checks: CheckEvidence::Passed,
            upstream: UpstreamEvidence::Satisfied,
            open_pr_snapshot_freshness: freshness(),
            pull_requests: Vec::new(),
            direction_status: "done".to_string(),
            checkouts: None,
        }
    }

    fn checkout(repo_name: &str, branch: &str, head_sha: &str) -> LaneCheckout {
        LaneCheckout {
            repo_name: repo_name.to_string(),
            path: format!("/tmp/{repo_name}"),
            observed: Some(CheckoutSignature {
                branch: branch.to_string(),
                head_sha: head_sha.to_string(),
                dirty: false,
            }),
        }
    }

    async fn fixture(name: &str) -> (Db, i32, i32) {
        let db = Db::connect("sqlite::memory:").await.expect("memory db");
        let workspace = repo::create_workspace(&db, name).await.expect("workspace");
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "primary-repo",
            "/tmp/primary-repo",
            "main",
            "",
            true,
        )
        .await
        .expect("repo ref");
        let thread = repo::create_thread(&db, workspace.id, name, "feature/change-set", "claude")
            .await
            .expect("thread");
        (db, thread.id, repo_ref.id)
    }

    async fn direction_of(
        db: &Db,
        thread_id: i32,
        repo_id: i32,
        name: &str,
        reason: &str,
    ) -> direction::Model {
        repo::create_direction(db, thread_id, name, "claude", repo_id, reason, "impl-only", "main")
            .await
            .expect("direction")
    }

    /// The whole point of the module: the row the overview prints is the row
    /// the verdict decided, never a second derivation of it.
    #[tokio::test]
    async fn lane_rows_carry_the_verdicts_own_answer() {
        let (db, thread_id, repo_id) = fixture("change set verdict reuse").await;
        let direction = direction_of(&db, thread_id, repo_id, "impl", "needs the API").await;

        let mut fact = lane_facts(direction.id, "impl");
        fact.checks = CheckEvidence::Failing;
        let facts = vec![fact];
        let verdict = issue_readiness(&facts);

        let change_set = project(&db, thread_id, verdict.clone(), &facts)
            .await
            .expect("project");
        assert_eq!(change_set.readiness, verdict.readiness);
        assert_eq!(change_set.reasons, verdict.reasons);
        assert_eq!(change_set.active_lane_count, verdict.active_lane_count);
        assert_eq!(change_set.lanes.len(), 1);
        assert_eq!(change_set.lanes[0].readiness, verdict.lanes[0].readiness);
        assert_eq!(change_set.lanes[0].reasons, verdict.lanes[0].reasons);
        assert_eq!(change_set.lanes[0].repo_name, "primary-repo");
        assert_eq!(change_set.lanes[0].reason, "needs the API");
        assert_eq!(change_set.lanes[0].checkout.declared_base, "main");
    }

    /// Several virtual lanes share `direction_id == 0`. Pairing by id would
    /// hand them all one verdict; the cursor walk keeps each lane its own.
    #[tokio::test]
    async fn virtual_lanes_sharing_direction_zero_keep_distinct_verdicts() {
        let (db, thread_id, _repo_id) = fixture("change set virtual lanes").await;

        let mut ready = lane_facts(0, "unbound pr");
        ready.checks = CheckEvidence::NotApplicable;
        let mut blocked = lane_facts(0, "issue ask");
        blocked.has_open_ask = true;
        let facts = vec![ready, blocked];
        let verdict = issue_readiness(&facts);
        assert_eq!(verdict.lanes.len(), 2);
        assert_ne!(
            verdict.lanes[0].readiness, verdict.lanes[1].readiness,
            "fixture must produce two DIFFERENT verdicts for the join to be observable"
        );

        let change_set = project(&db, thread_id, verdict.clone(), &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes.len(), 2);
        assert_eq!(change_set.lanes[0].name, "unbound pr");
        assert_eq!(change_set.lanes[0].readiness, verdict.lanes[0].readiness);
        assert_eq!(change_set.lanes[1].name, "issue ask");
        assert_eq!(change_set.lanes[1].readiness, verdict.lanes[1].readiness);
    }

    /// A lane the verdict dropped must not appear beside an `active_lane_count`
    /// that does not count it.
    #[tokio::test]
    async fn a_lane_the_verdict_excluded_is_not_shown() {
        let (db, thread_id, repo_id) = fixture("change set excluded lane").await;
        let kept = direction_of(&db, thread_id, repo_id, "impl", "primary").await;
        let dropped = direction_of(&db, thread_id, repo_id, "abandoned", "stale").await;

        let mut inactive = lane_facts(dropped.id, "abandoned");
        inactive.active = false;
        let facts = vec![lane_facts(kept.id, "impl"), inactive];
        let verdict = issue_readiness(&facts);

        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes.len(), 1);
        assert_eq!(change_set.lanes[0].direction_id, kept.id);
        assert_eq!(change_set.active_lane_count, 1);
    }

    /// Declared and observed are reported side by side so the reader can see
    /// WHAT drifted, not just that something did.
    #[tokio::test]
    async fn declared_and_observed_checkouts_are_both_reported() {
        let (db, thread_id, repo_id) = fixture("change set drift").await;
        let direction = direction_of(&db, thread_id, repo_id, "impl", "primary").await;
        assert!(
            !direction.branch.is_empty(),
            "the fixture needs a declared branch to drift away from"
        );

        let mut fact = lane_facts(direction.id, "impl");
        fact.reconciliation = ExecutionReconciliation::Drifted;
        fact.checkouts = Some(vec![checkout("primary-repo", "weft/other", "sha-1")]);
        let facts = vec![fact];
        let verdict = issue_readiness(&facts);

        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        let lane = &change_set.lanes[0];
        assert_eq!(lane.checkout.declared_branch, direction.branch);
        assert_ne!(lane.checkout.declared_branch, "weft/other");
        assert_eq!(lane.reconciliation, ExecutionReconciliation::Drifted);
        let observed = lane.checkout.observed.as_ref().expect("probed");
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].observed.as_ref().map(|s| s.branch.as_str()),
            Some("weft/other")
        );
    }

    /// "We did not look" and "there is nothing there yet" need different next
    /// actions, so they must survive as different values.
    #[tokio::test]
    async fn not_probed_and_probed_empty_stay_distinct() {
        let (db, thread_id, repo_id) = fixture("change set probe states").await;
        let unprobed = direction_of(&db, thread_id, repo_id, "unprobed", "a").await;
        let empty = direction_of(&db, thread_id, repo_id, "empty", "b").await;

        let mut probed_empty = lane_facts(empty.id, "empty");
        probed_empty.checkouts = Some(Vec::new());
        let facts = vec![lane_facts(unprobed.id, "unprobed"), probed_empty];
        let verdict = issue_readiness(&facts);

        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes[0].checkout.observed, None);
        assert_eq!(change_set.lanes[1].checkout.observed, Some(Vec::new()));
    }

    /// Denied and unresolved edges name no lane; drawing them would point the
    /// dependency arrow at a direction that does not exist.
    #[tokio::test]
    async fn only_resolved_upstream_edges_become_dependencies() {
        let (db, thread_id, repo_id) = fixture("change set dependencies").await;
        let upstream = direction_of(&db, thread_id, repo_id, "api", "produces").await;
        let consumer = direction_of(&db, thread_id, repo_id, "ui", "consumes").await;
        repo::set_direction_upstreams(
            &db,
            consumer.id,
            &[
                repo::UpstreamEdge::resolved(upstream.id),
                repo::UpstreamEdge::denied(),
                repo::UpstreamEdge::unresolved(),
            ],
        )
        .await
        .expect("upstream edges");

        let facts = vec![lane_facts(upstream.id, "api"), lane_facts(consumer.id, "ui")];
        let verdict = issue_readiness(&facts);
        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes[0].depends_on, Vec::<i32>::new());
        assert_eq!(change_set.lanes[1].depends_on, vec![upstream.id]);
    }

    async fn append(db: &Db, thread_id: i32, direction_id: i32, kind: &str, revision: &str) {
        repo::append_evidence(
            db,
            repo::EvidenceWrite {
                thread_id,
                direction_id,
                kind,
                source: "test",
                source_ref: "primary-repo",
                revision,
                policy_revision: "",
                summary: kind,
                payload: "{}",
                collection_state: repo::EVIDENCE_COLLECTION_OK,
            },
        )
        .await
        .expect("evidence row");
    }

    /// Revision-anchored evidence is judged against the head SHA this same
    /// collection already sampled — no second probe, and no blanket `unknown`.
    #[tokio::test]
    async fn evidence_is_judged_against_the_sampled_head_sha() {
        let (db, thread_id, repo_id) = fixture("change set evidence freshness").await;
        let matching = direction_of(&db, thread_id, repo_id, "matching", "a").await;
        let drifted = direction_of(&db, thread_id, repo_id, "drifted", "b").await;
        append(&db, thread_id, matching.id, repo::EVIDENCE_KIND_VERIFICATION, "sha-live").await;
        append(&db, thread_id, drifted.id, repo::EVIDENCE_KIND_VERIFICATION, "sha-old").await;

        let mut matching_fact = lane_facts(matching.id, "matching");
        matching_fact.checkouts = Some(vec![checkout("primary-repo", "weft/a", "sha-live")]);
        let mut drifted_fact = lane_facts(drifted.id, "drifted");
        drifted_fact.checkouts = Some(vec![checkout("primary-repo", "weft/b", "sha-live")]);
        let facts = vec![matching_fact, drifted_fact];
        let verdict = issue_readiness(&facts);

        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes[0].evidence.fresh, 1);
        assert_eq!(change_set.lanes[0].evidence.stale, 0);
        assert_eq!(change_set.lanes[0].evidence.unknown, 0);
        assert_eq!(change_set.lanes[1].evidence.stale, 1);
        assert_eq!(change_set.lanes[1].evidence.fresh, 0);
    }

    /// A lane this collection never probed supplies no SHA, so its
    /// revision-anchored rows fail closed rather than being called fresh.
    #[tokio::test]
    async fn unprobed_lane_evidence_fails_closed_to_unknown() {
        let (db, thread_id, repo_id) = fixture("change set unprobed evidence").await;
        let direction = direction_of(&db, thread_id, repo_id, "impl", "a").await;
        append(&db, thread_id, direction.id, repo::EVIDENCE_KIND_VERIFICATION, "sha-live").await;

        let facts = vec![lane_facts(direction.id, "impl")];
        assert_eq!(facts[0].checkouts, None, "this lane must be unprobed");
        let verdict = issue_readiness(&facts);

        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes[0].evidence.unknown, 1);
        assert_eq!(change_set.lanes[0].evidence.fresh, 0);
    }

    /// The summary reports the NEWEST observation, not whichever row the scan
    /// happened to reach last.
    #[tokio::test]
    async fn newest_observed_at_wins_over_older_rows() {
        let (db, thread_id, repo_id) = fixture("change set newest evidence").await;
        let direction = direction_of(&db, thread_id, repo_id, "impl", "a").await;
        // Distinct kinds so `append_evidence` writes rows rather than
        // superseding one identity in place.
        append(&db, thread_id, direction.id, repo::EVIDENCE_KIND_DECISION, "").await;
        append(&db, thread_id, direction.id, repo::EVIDENCE_KIND_HANDOFF, "").await;

        let rows = repo::list_evidence(&db, thread_id, Some(direction.id), 10)
            .await
            .expect("rows");
        assert_eq!(rows.len(), 2, "newest-first ordering needs two rows");
        let newest = rows[0].observed_at.clone();

        let facts = vec![lane_facts(direction.id, "impl")];
        let verdict = issue_readiness(&facts);
        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(
            change_set.lanes[0].evidence.newest_observed_at,
            Some(newest)
        );
        assert_eq!(change_set.lanes[0].evidence.fresh, 2);
    }

    /// A lane with no rows at all reports `None`, which is not the same as
    /// having rows that are merely old.
    #[tokio::test]
    async fn a_lane_without_evidence_reports_none() {
        let (db, thread_id, repo_id) = fixture("change set no evidence").await;
        let direction = direction_of(&db, thread_id, repo_id, "impl", "a").await;

        let facts = vec![lane_facts(direction.id, "impl")];
        let verdict = issue_readiness(&facts);
        let change_set = project(&db, thread_id, verdict, &facts)
            .await
            .expect("project");
        assert_eq!(change_set.lanes[0].evidence, EvidenceSummary::default());
        assert_eq!(change_set.lanes[0].evidence.newest_observed_at, None);
    }
}
