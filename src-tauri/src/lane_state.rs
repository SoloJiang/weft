//! Issue #172: the ONE authority state of a Lane.
//!
//! Every surface that cares whether a lane may proceed — the Gate panel, Gate
//! resolution, the dispatch set, worker admission, readiness — used to derive
//! that answer itself, from its own combination of decision evidence, worktree
//! validity, session existence, plan membership, policy revision, upstream
//! lanes and lifecycle status. Seven inputs, six call sites, each subtly
//! different. Every combination one of them missed was a real defect: a card
//! that could not be dismissed, a worker started behind a producer that never
//! ran, a finished task offered a resume button, an obsolete card that still
//! materialized removed work.
//!
//! So the inputs are read in exactly one place and collapse into one
//! discriminated value, which callers map exhaustively (CLAUDE.md: derive ONE
//! discriminated value, do not re-derive the same booleans at every call site).
//! Adding a state now forces every surface to say what it does with it, which
//! is the property that was missing.

use anyhow::Result;

use crate::authority::{LaneDecision, LaneVerdict};
use crate::store::Db;

/// What the workspace's authority says about one lane, right now.
///
/// Ordering of the arms is the order the resolver decides them, and that order
/// is itself load-bearing: a finished lane is finished whatever the policy now
/// says, an out-of-scope lane must not be actionable even if it would be
/// allowed, and a lane behind a blocked producer is not "ready" merely because
/// its own verdict is clean.
#[derive(Clone, Debug)]
pub enum LaneAuthorityState {
    /// Binds no write repo, so authority has nothing to say about it.
    NotApplicable,
    /// Terminal lifecycle. Never actionable and never dispatchable — a policy
    /// change can re-adjudicate completed work, and acting on that would start
    /// a second worker on a finished task.
    Finished,
    /// The worker claimed completion and the lane is waiting on review
    /// (`readiness::direction_claimed_completion`), with nothing live on it.
    ///
    /// Distinct from `ReadyToStart`, which it otherwise looks exactly like — a
    /// valid checkout and no live session. The difference is that its work is
    /// DONE pending review, so a Resume card here invites a second worker onto
    /// finished work. Distinct from `Finished` too: `done` is settled, this is
    /// still open, so it does not release a consumer waiting on it.
    AwaitingReview,
    /// Switched OFF by a human (`inactive` / `cancelled`, the statuses
    /// `readiness::direction_is_active` excludes).
    ///
    /// Deliberately NOT folded into `Finished`, even though both are terminal
    /// and neither is actionable: `Finished` satisfies a consumer's
    /// prerequisite and this does not. A lane that was switched off produced
    /// nothing, so anything waiting on it is blocked, not released.
    Deactivated,
    /// The planner owned this lane and the current proposal no longer names it.
    /// Not the user's reviewed scope any more, so nothing may act on it.
    /// Standalone lanes (`create_direction`, never in any proposal) are never
    /// out of scope.
    OutOfScope,
    /// Refused: a `denied_repos` rule, or a human's Gate veto.
    Denied(Box<LaneVerdict>),
    /// A rule flagged it and no human has resolved it yet. This is the card
    /// that asks for a decision.
    AwaitingGate(Box<LaneVerdict>),
    /// Allowed, but a prerequisite is not runnable — gated, denied, missing its
    /// checkout, or itself blocked. Starting it would run work behind a
    /// producer that never ran.
    BlockedUpstream { blocker: i32 },
    /// Allowed, but has no checkout a worker could run in. Recoverable: the
    /// human can ask for it to be set up.
    NeedsMaterialize(Box<LaneVerdict>),
    /// Allowed, materialized, and no worker has ever run. This is the only
    /// state a lane may be dispatched from.
    ReadyToStart(Box<LaneVerdict>),
    /// Allowed, materialized, and a worker is live.
    Running,
}

impl LaneAuthorityState {
    /// Whether a worker may be started for this lane now.
    pub fn is_dispatchable(&self) -> bool {
        matches!(self, Self::ReadyToStart(_))
    }

    /// Whether a worker may be RUNNING for this lane — admission accepts an
    /// already-live lane so a reconnect is not treated as a fresh start.
    pub fn admits_worker(&self) -> bool {
        matches!(self, Self::ReadyToStart(_) | Self::Running)
    }

    /// Whether there is a decision here for a human to make — i.e. whether this
    /// state produces a Gate card, and therefore whether a Gate resolution may
    /// be recorded against it.
    ///
    /// ONE predicate for both, deliberately. Rendering the card and accepting
    /// the click were separate judgements, and they drifted: the resolver
    /// rejected only `OutOfScope`, so a lane that reached `done`, `inactive` or
    /// `cancelled` while its card sat open could still be approved — and since
    /// the panel's reload signature is just the thread's direction ids, a
    /// status change never removes the stale card. The approval then recorded an
    /// override that `materialize_direction` honors, creating a checkout for
    /// terminal work. A card that can be shown and a card that can be resolved
    /// are now the same set by construction.
    pub fn offers_decision(&self) -> bool {
        matches!(
            self,
            Self::AwaitingGate(_) | Self::NeedsMaterialize(_) | Self::ReadyToStart(_)
        )
    }

    /// Whether this lane's LIFECYCLE and SCOPE still admit an authority
    /// decision at all — as opposed to what that decision currently is.
    ///
    /// `offers_decision` cannot be re-used to re-validate after a decision is
    /// recorded, because recording one deliberately changes the answer: a
    /// denial moves the lane to `Denied`, which offers nothing. This is the
    /// half that a human's own click must NOT change, so it is the half worth
    /// re-reading across a write — a lane marked done, deactivated or dropped
    /// from scope between the check and the write must not go on to
    /// materialize.
    pub fn is_in_play(&self) -> bool {
        !matches!(
            self,
            Self::NotApplicable
                | Self::Finished
                | Self::Deactivated
                | Self::AwaitingReview
                | Self::OutOfScope
        )
    }

    /// A stable, low-cardinality name for the arm. Errors and logs want to say
    /// WHICH refusal this was without printing a whole verdict struct (a
    /// `{:?}` of one carries rule text into places nobody reviewed it for).
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Finished => "finished",
            Self::Deactivated => "deactivated",
            Self::AwaitingReview => "awaiting_review",
            Self::OutOfScope => "out_of_scope",
            Self::Denied(_) => "denied",
            Self::AwaitingGate(_) => "awaiting_gate",
            Self::BlockedUpstream { .. } => "blocked_upstream",
            Self::NeedsMaterialize(_) => "needs_materialize",
            Self::ReadyToStart(_) => "ready_to_start",
            Self::Running => "running",
        }
    }

    /// The verdict behind this state, when there is one. `Finished`,
    /// `Deactivated`, `OutOfScope`, `BlockedUpstream`, `Running` and
    /// `NotApplicable` are lifecycle or graph facts rather than judgments.
    pub fn verdict(&self) -> Option<&LaneVerdict> {
        match self {
            Self::Denied(v) | Self::AwaitingGate(v) | Self::NeedsMaterialize(v) | Self::ReadyToStart(v) => {
                Some(v)
            }
            Self::NotApplicable
            | Self::Finished
            | Self::Deactivated
            | Self::AwaitingReview
            | Self::OutOfScope
            | Self::BlockedUpstream { .. }
            | Self::Running => None,
        }
    }
}

/// This lane's own state, ignoring its prerequisites.
///
/// Split from the full resolve so the upstream walk can ask about ancestors
/// without recursing: an ancestor's LOCAL state is all a consumer needs, and
/// the walk covers the chain. That also makes a cyclic edge set terminate by
/// construction rather than by a depth guard.
async fn local_state(
    db: &Db,
    direction_id: i32,
    scopes: &mut ScopeCache,
) -> Result<LaneAuthorityState> {
    let Some(dir) = crate::store::repo::get_direction(db, direction_id).await? else {
        return Ok(LaneAuthorityState::NotApplicable);
    };
    if dir.status == "done" {
        return Ok(LaneAuthorityState::Finished);
    }
    // Switched off by a human. Checked before adjudication because a
    // deactivated lane must not be offered a recovery card or admit a worker
    // however permissive the policy now is.
    if !crate::readiness::direction_is_active(&dir) {
        return Ok(LaneAuthorityState::Deactivated);
    }
    if !scopes.admits(db, dir.thread_id, direction_id).await? {
        return Ok(LaneAuthorityState::OutOfScope);
    }
    // Judged FRESH every time rather than read from the newest evidence row and
    // compared against the active revision. That comparison is what produced
    // the stale-card deadlock (a card frozen at a dead revision whose approval
    // was rejected forever) and the fallback-to-an-older-row bug; asking the
    // adjudicator now cannot be stale by construction. `judge_lane` records
    // nothing, so this is safe on a hot path.
    let Some(verdict) = crate::materialize::judge_lane(db, direction_id).await? else {
        return Ok(LaneAuthorityState::NotApplicable);
    };
    match verdict.decision {
        LaneDecision::Denied => return Ok(LaneAuthorityState::Denied(Box::new(verdict))),
        LaneDecision::NeedsGate => return Ok(LaneAuthorityState::AwaitingGate(Box::new(verdict))),
        LaneDecision::AllowedByPolicy => {}
    }
    if !crate::materialize::lane_has_valid_checkout(db, direction_id).await? {
        return Ok(LaneAuthorityState::NeedsMaterialize(Box::new(verdict)));
    }
    // A session that EXITED leaves the lane as stopped as one that never
    // started, so history is not the question — whether anything could still
    // receive work is.
    let live = crate::store::repo::sessions_for_direction(db, direction_id)
        .await?
        .iter()
        .any(|session| matches!(session.status.as_str(), "running" | "idle" | "starting"));
    if live {
        return Ok(LaneAuthorityState::Running);
    }
    // Checked HERE, after the liveness test rather than alongside `done` at the
    // top, so a lane whose worker is still attached keeps reconnecting. Only a
    // `review` lane with nothing live is the one at issue: it has a valid
    // checkout and no session, which is indistinguishable from `ReadyToStart`
    // by those inputs alone — so it drew a stranded-lane Resume card and an
    // approval could dispatch a SECOND worker onto work that already claimed
    // completion. `readiness::direction_claimed_completion` has always treated
    // `review` as complete; this is the same judgement, in the one place lane
    // state is derived.
    if crate::readiness::direction_claimed_completion(&dir.status) {
        return Ok(LaneAuthorityState::AwaitingReview);
    }
    Ok(LaneAuthorityState::ReadyToStart(Box::new(verdict)))
}

/// The full state of one lane, including its prerequisite chain.
pub async fn lane_authority_state(db: &Db, direction_id: i32) -> Result<LaneAuthorityState> {
    let mut scopes = ScopeCache::default();
    lane_authority_state_cached(db, direction_id, &mut scopes).await
}

/// Resolve MANY lanes sharing one scope read.
///
/// `list_lane_gates` resolves every direction on a thread; resolving them one
/// at a time re-read and re-parsed the thread's whole plan history per lane,
/// which is quadratic in (directions x revisions) on the path that paints the
/// board. Same answers, one scope load per thread.
pub async fn lane_authority_states(
    db: &Db,
    direction_ids: &[i32],
) -> Result<Vec<(i32, LaneAuthorityState)>> {
    let mut scopes = ScopeCache::default();
    let mut out = Vec::with_capacity(direction_ids.len());
    for &direction_id in direction_ids {
        let state = lane_authority_state_cached(db, direction_id, &mut scopes).await?;
        out.push((direction_id, state));
    }
    Ok(out)
}

async fn lane_authority_state_cached(
    db: &Db,
    direction_id: i32,
    scopes: &mut ScopeCache,
) -> Result<LaneAuthorityState> {
    let own = local_state(db, direction_id, scopes).await?;
    // Only a lane that is otherwise ready can be held back by a producer.
    // A gated, denied, finished or out-of-scope lane already has its answer.
    if !matches!(
        own,
        LaneAuthorityState::ReadyToStart(_) | LaneAuthorityState::NeedsMaterialize(_)
    ) {
        return Ok(own);
    }
    // The WHOLE chain, not one hop. Confirm materializes a gated lane's
    // dependents and withholds only their dispatch, so a direct producer can
    // look perfectly ready while ITS producer is still gated.
    let mut seen: std::collections::HashSet<i32> = [direction_id].into_iter().collect();
    let mut frontier = vec![direction_id];
    while let Some(current) = frontier.pop() {
        for upstream in crate::store::repo::upstream_direction_ids(db, current).await? {
            if !seen.insert(upstream) {
                continue;
            }
            let state = local_state(db, upstream, scopes).await?;
            // A producer is satisfactory only once it is FINISHED or RUNNING.
            // Anything else blocks: gated and denied obviously, but also
            // `ReadyToStart` — a producer that has not run yet is exactly the
            // case confirm creates when it materializes a gated lane's
            // dependents and withholds their dispatch.
            //
            // A satisfactory producer ends the walk down that edge rather than
            // continuing into ITS producers: whether the thing C waits for has
            // run is the whole question, and if B is finished or live, what B
            // once waited for is settled. That is why the frontier only grows
            // through unsatisfactory edges, which is also what bounds it.
            match state {
                LaneAuthorityState::Finished | LaneAuthorityState::Running => continue,
                _ => return Ok(LaneAuthorityState::BlockedUpstream { blocker: upstream }),
            }
        }
    }
    Ok(own)
}

/// One thread's reviewed scope, read ONCE.
///
/// `list_lane_gates` resolves every direction on a thread, and each resolve used
/// to re-query and re-parse the thread's entire plan history — quadratic in
/// (directions x revisions), both of which grow with every re-proposal, on the
/// path that paints the board. The answer is identical for every lane on the
/// thread, so it is computed once and consulted per lane.
struct ThreadScope {
    /// `None` when the thread has no stored plan at all: nothing to filter
    /// against, so every lane is in scope.
    current: Option<std::collections::HashSet<i32>>,
    /// Every direction id any plan revision has ever named.
    planner_owned: std::collections::HashSet<i32>,
}

impl ThreadScope {
    /// An unreadable plan is an ERROR, never "no plan": reading it as absent
    /// switches the check off, which is the permissive direction.
    ///
    /// That applies to HISTORICAL revisions too. Skipping one (`.ok()`) answers
    /// "this revision named no lanes", which can only push `planner_owned`
    /// toward false — and false means standalone, which switches the check off
    /// for exactly the obsolete cards it exists to catch. Every revision is
    /// parsed, unpaged: a page bound would reclassify a lane referenced only by
    /// an older proposal as standalone, and short-circuiting on the first hit
    /// would make whether a corrupt row is reached depend on iteration order.
    async fn load(db: &Db, thread_id: i32) -> Result<Self> {
        let current = match crate::store::repo::get_plan(db, thread_id).await? {
            None => None,
            Some(plan) => {
                let parsed: serde_json::Value = serde_json::from_str(&plan.proposal)
                    .map_err(|error| anyhow::anyhow!("stored proposal is unreadable: {error}"))?;
                Some(direction_ids_in(&parsed).ok_or_else(|| {
                    anyhow::anyhow!("stored proposal has no directions array")
                })?)
            }
        };
        let mut planner_owned = std::collections::HashSet::new();
        for proposal in crate::store::repo::all_plan_revision_proposals(db, thread_id).await? {
            let value: serde_json::Value = serde_json::from_str(&proposal).map_err(|error| {
                anyhow::anyhow!("a stored scope revision is unreadable: {error}")
            })?;
            let ids = direction_ids_in(&value).ok_or_else(|| {
                anyhow::anyhow!("a stored scope revision has no directions array")
            })?;
            planner_owned.extend(ids);
        }
        Ok(Self { current, planner_owned })
    }

    /// Whether this lane still belongs to the thread's CURRENT reviewed scope.
    ///
    /// A lane the planner owns but the current proposal no longer names is
    /// obsolete. A standalone lane (`create_direction`, never referenced by any
    /// plan revision) is always in scope.
    fn admits(&self, direction_id: i32) -> bool {
        let Some(current) = &self.current else {
            return true;
        };
        current.contains(&direction_id) || !self.planner_owned.contains(&direction_id)
    }
}

fn direction_ids_in(proposal: &serde_json::Value) -> Option<std::collections::HashSet<i32>> {
    Some(
        proposal
            .get("directions")?
            .as_array()?
            .iter()
            .filter_map(|d| d.get("direction_id").and_then(|v| v.as_i64()))
            .map(|id| id as i32)
            .collect(),
    )
}

/// Thread scopes loaded so far in one resolution.
///
/// Keyed by thread rather than held as a single snapshot because the upstream
/// walk can cross threads, and answering a lane against another thread's scope
/// would be a correctness bug, not just a miss.
#[derive(Default)]
pub struct ScopeCache {
    by_thread: std::collections::HashMap<i32, ThreadScope>,
}

impl ScopeCache {
    async fn admits(&mut self, db: &Db, thread_id: i32, direction_id: i32) -> Result<bool> {
        if !self.by_thread.contains_key(&thread_id) {
            let scope = ThreadScope::load(db, thread_id).await?;
            self.by_thread.insert(thread_id, scope);
        }
        Ok(self
            .by_thread
            .get(&thread_id)
            .is_some_and(|scope| scope.admits(direction_id)))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(decision: LaneDecision) -> Box<LaneVerdict> {
        Box::new(LaneVerdict {
            decision,
            reason: crate::authority::VerdictReason::ProtectedBranch,
            hit_rule: None,
            policy_revision: "1".to_string(),
            scope_revision: "1".to_string(),
            decided_at: String::new(),
            source: "test",
        })
    }

    /// Every arm, enumerated ONCE. The behaviour tests below assert over this
    /// whole set rather than over their own hand-listed tables, so a new arm
    /// added to the enum without a decision here fails them instead of silently
    /// inheriting whatever `matches!` happens to say.
    fn all_states() -> Vec<LaneAuthorityState> {
        vec![
            LaneAuthorityState::NotApplicable,
            LaneAuthorityState::Finished,
            LaneAuthorityState::Deactivated,
            LaneAuthorityState::AwaitingReview,
            LaneAuthorityState::OutOfScope,
            LaneAuthorityState::Denied(verdict(LaneDecision::Denied)),
            LaneAuthorityState::AwaitingGate(verdict(LaneDecision::NeedsGate)),
            LaneAuthorityState::BlockedUpstream { blocker: 7 },
            LaneAuthorityState::NeedsMaterialize(verdict(LaneDecision::AllowedByPolicy)),
            LaneAuthorityState::ReadyToStart(verdict(LaneDecision::AllowedByPolicy)),
            LaneAuthorityState::Running,
        ]
    }

    fn labels_where(predicate: impl Fn(&LaneAuthorityState) -> bool) -> Vec<&'static str> {
        all_states()
            .iter()
            .filter(|state| predicate(state))
            .map(|state| state.label())
            .collect()
    }

    /// Exactly one state is dispatchable. This is the property the whole module
    /// exists for: before it, six call sites each decided "may this start?"
    /// from their own mix of inputs, and every combination one of them missed
    /// was a real defect — a finished task offered a resume button, a worker
    /// started behind a producer that never ran, an obsolete card that still
    /// materialized removed work.
    #[test]
    fn only_ready_to_start_is_dispatchable() {
        assert_eq!(
            labels_where(LaneAuthorityState::is_dispatchable),
            vec!["ready_to_start"]
        );
    }

    /// Admission is deliberately WIDER than dispatch by exactly one state: a
    /// lane whose worker is already live must reconnect rather than be refused
    /// as if it were a fresh start.
    #[test]
    fn admission_accepts_ready_and_running_only() {
        assert_eq!(
            labels_where(LaneAuthorityState::admits_worker),
            vec!["ready_to_start", "running"]
        );
    }

    /// The states a human may resolve are exactly the states that show a card.
    ///
    /// These were two separate judgements and they drifted: the panel rendered a
    /// card for three states while the resolver refused only `OutOfScope`, so a
    /// lane that reached `done`/`inactive`/`cancelled` with its card open could
    /// still be approved into a fresh checkout. Anything dispatchable is also
    /// resolvable — that is the stranded-lane recovery path.
    #[test]
    fn resolvable_states_are_exactly_the_states_that_show_a_card() {
        assert_eq!(
            labels_where(LaneAuthorityState::offers_decision),
            vec!["awaiting_gate", "needs_materialize", "ready_to_start"]
        );
        for state in all_states() {
            if state.is_dispatchable() {
                assert!(state.offers_decision(), "{} must stay resolvable", state.label());
            }
        }
    }

    /// A lane awaiting review is not a lane to recover. It looks exactly like
    /// `ReadyToStart` by checkout and session alone — which is how it drew a
    /// stranded-lane Resume card whose approval could start a SECOND worker on
    /// work that already claimed completion.
    #[test]
    fn a_lane_awaiting_review_is_not_recoverable_and_not_startable() {
        let reviewing = LaneAuthorityState::AwaitingReview;
        assert!(!reviewing.is_dispatchable());
        assert!(!reviewing.admits_worker());
        assert!(!reviewing.offers_decision());
        assert!(reviewing.verdict().is_none());
        // Not `Finished` either: `done` is settled and releases a consumer,
        // review is still open and must not.
        assert!(!matches!(reviewing, LaneAuthorityState::Finished));
    }

    /// `is_in_play` is the half a human's own decision cannot change, which is
    /// what makes it the right thing to re-read across the write that records
    /// one. Every state that offers a decision is in play; the reverse does
    /// not hold, and `Denied` is exactly why — recording a denial moves the
    /// lane there, so re-validating with `offers_decision` would reject the
    /// very write that had just succeeded.
    #[test]
    fn in_play_is_the_half_a_decision_cannot_change() {
        assert_eq!(
            labels_where(LaneAuthorityState::is_in_play),
            vec![
                "denied",
                "awaiting_gate",
                "blocked_upstream",
                "needs_materialize",
                "ready_to_start",
                "running"
            ]
        );
        for state in all_states() {
            if state.offers_decision() {
                assert!(state.is_in_play(), "{} offers a decision but is not in play", state.label());
            }
        }
        assert!(
            LaneAuthorityState::Denied(verdict(LaneDecision::Denied)).is_in_play(),
            "a denial must not invalidate the write that produced it"
        );
    }

    /// Labels reach error messages and logs, so two arms sharing one would make
    /// two different refusals indistinguishable to whoever is diagnosing them.
    #[test]
    fn every_state_has_its_own_label() {
        let labels: Vec<&str> = all_states().iter().map(|state| state.label()).collect();
        let unique: std::collections::HashSet<&&str> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    /// Only judgments carry a verdict; lifecycle and graph facts do not, and
    /// callers must not invent one for them.
    #[test]
    fn only_judged_states_carry_a_verdict() {
        assert_eq!(
            labels_where(|state| state.verdict().is_some()),
            vec!["denied", "awaiting_gate", "needs_materialize", "ready_to_start"]
        );
    }
}
