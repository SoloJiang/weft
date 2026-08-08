//! AuthorityPolicy (issue #172): the single judgment for whether a candidate
//! Lane may auto-materialize, needs a human Gate, or is denied outright — and
//! the matching judgment for a CLI permission request bridged through the
//! same policy object. Two independent, separately versioned inputs feed one
//! deterministic decision:
//!
//! - the dynamic scope (a Lane's own repo/reason/base/mandate, versioned by
//!   `store::entities::plan_revision`);
//! - the AuthorityPolicy itself (`store::entities::authority_policy`), which
//!   the user tightens/revokes independently of any single plan.
//!
//! [`adjudicate_lane`] is the ONLY place that judgment is made for
//! materialize; [`bridge_decision`] is the ONLY place the same judgment is
//! made for a CLI permission ask. Neither ever WIDENS what today's default
//! (conservative) posture already allows — see each function's own doc.
//!
//! Both functions are PURE: they read only their arguments and never touch
//! the store. Every caller is responsible for fetching the CURRENT policy row
//! (`store::repo::get_active_authority_policy`) and CURRENT scope revision
//! (`store::repo::latest_plan_revision`) immediately before calling — there is
//! deliberately no verdict cache anywhere in this module, so a stale verdict
//! can never be replayed: the only way to get a verdict is to compute one
//! against whatever is current right now (issue's own "stale policy decision
//! 不可继续 materialize" boundary, satisfied by construction rather than by a
//! staleness comparison).

use serde::{Deserialize, Serialize};

/// Where an AuthorityPolicy applies. `Workspace` is the whole workspace's
/// default; `Project` narrows to one repo group (reserved for a future
/// narrower scope — issue #172 ships `Workspace` as the only scope any
/// production caller creates today, but `Project` parses/round-trips so
/// adding it later is additive, not a migration).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyScope {
    Workspace(i32),
    Project(i32),
}

impl PolicyScope {
    pub fn kind(self) -> &'static str {
        match self {
            PolicyScope::Workspace(_) => "workspace",
            PolicyScope::Project(_) => "project",
        }
    }

    pub fn id(self) -> i32 {
        match self {
            PolicyScope::Workspace(id) | PolicyScope::Project(id) => id,
        }
    }

    pub fn parse(kind: &str, id: i32) -> Option<Self> {
        match kind {
            "workspace" => Some(PolicyScope::Workspace(id)),
            "project" => Some(PolicyScope::Project(id)),
            _ => None,
        }
    }
}

/// The configurable rule set of one AuthorityPolicy revision. Every field
/// defaults to the MOST CONSERVATIVE reading (empty allow/deny lists, no auto
/// materialize) so a policy row saved with a partial/old JSON shape never
/// silently widens what it authorizes — every field is `#[serde(default)]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRules {
    /// Repo names this policy explicitly allows. Empty = no explicit
    /// allow-list configured — falls back to "any workspace-registered repo"
    /// (today's behavior; a Lane's repo is already known-repo-checked
    /// upstream of adjudication, see `LaneCandidate::repo_known`).
    #[serde(default)]
    pub allowed_repos: Vec<String>,
    /// Repo names this policy denies outright, regardless of `allowed_repos`.
    #[serde(default)]
    pub denied_repos: Vec<String>,
    /// Base branch names that always require a human Gate, even for an
    /// otherwise-authorized Lane (protected branches, release branches, …).
    #[serde(default)]
    pub protected_branches: Vec<String>,
    /// When true, a Lane that clears every other check auto-materializes even
    /// WITHOUT a prior human confirm/approve — the "Project 策略内的 Lane 自动
    /// 创建并 materialize" behavior the issue describes. Default `false`
    /// preserves today's confirm-gated flow exactly (see
    /// [`LaneCandidate::human_authorized`]'s own doc).
    #[serde(default)]
    pub auto_materialize: bool,
    /// CLI permission-bridge `action_key` patterns this policy auto-approves.
    /// A pattern is either an exact `action_key` or a `prefix*` glob. Empty by
    /// default — the bridge defers to the existing `AskRegistry`/human flow
    /// for every action until a policy explicitly opts one in.
    #[serde(default)]
    pub allow_actions: Vec<String>,
    /// CLI permission-bridge `action_key` patterns this policy auto-denies.
    /// Checked before `allow_actions`, so a narrower deny can always carve an
    /// exception out of a broader allow.
    #[serde(default)]
    pub deny_actions: Vec<String>,
}

/// One AuthorityPolicy revision, resolved from the `authority_policy` table
/// (or the hard-coded [`default_policy`] when no row exists / the active row
/// was revoked). `revision` is a monotonically increasing per-scope counter,
/// stored as a decimal string (mirrors `plan.created_at`'s OCC-token
/// convention used elsewhere in this store).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub id: i32,
    pub scope: PolicyScope,
    pub revision: String,
    pub rules: PolicyRules,
    pub source: String,
    pub created_at: String,
    pub revoked_at: String,
}

/// The hard-coded, conservative default used when no configured policy row
/// exists (or the last one was revoked) for a scope. Its rules are the
/// all-empty, `auto_materialize: false` default — identical, by construction,
/// to a freshly-`Default::default()`d [`PolicyRules`]. Revision `"0"` so a
/// REAL first configured policy (revision `"1"`) always compares as strictly
/// newer.
pub fn default_policy(scope: PolicyScope) -> PolicySnapshot {
    PolicySnapshot {
        id: 0,
        scope,
        revision: "0".to_string(),
        rules: PolicyRules::default(),
        source: "system".to_string(),
        created_at: String::new(),
        revoked_at: String::new(),
    }
}

/// A prior human Gate resolution recorded for one Lane at one EXACT policy
/// revision (see `store::repo::get_gate_decision`). Keying an override to the
/// policy revision it was granted under is what makes a later policy change
/// invalidate every outstanding Gate approval automatically — the "stale
/// policy decision" fail-closed rule, applied to the Gate override path too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateOverride {
    Approved,
    Denied,
}

/// A candidate Lane at adjudication time — everything [`adjudicate_lane`]
/// reads to reach a verdict. Built by the caller from a `direction` row (or a
/// not-yet-materialized `planner::ResolvedDirection`); never from raw
/// lead-supplied JSON directly — the trust boundary `planner::
/// save_proposal_value` already enforces upstream (scrubbing `decision`/
/// `direction_id`) is preserved, not re-implemented here.
#[derive(Clone, Debug)]
pub struct LaneCandidate<'a> {
    pub lane_id: &'a str,
    pub repo_known: bool,
    pub repo_name: &'a str,
    pub reason: &'a str,
    pub base_branch: &'a str,
    /// True once a human has already authorized this EXACT Lane's creation.
    /// Every `direction` row that reaches `materialize::materialize_direction`
    /// exists only because a human already called confirm/approve on it (both
    /// are the only writers of `direction` rows), so today's callers always
    /// pass `true` here — this is what keeps the default (conservative)
    /// policy a complete no-op on the existing single-repo confirm flow: no
    /// regression, because the fail-closed checks above never reject an
    /// already-known, already-reasoned, already-based Lane, and the default
    /// policy's `auto_materialize` is `false`. A future dynamic-scope path
    /// that creates Lanes ahead of any human click passes `false` here.
    pub human_authorized: bool,
    /// True when the human's per-lane decision on the original proposal was
    /// an explicit "denied" (`planner::ProposedDirection.decision ==
    /// "denied"`) — a human veto always outranks a permissive policy.
    pub human_denied: bool,
    /// True when this lane's id collides with another lane already
    /// adjudicated in the same batch. The caller owns identity tracking;
    /// this struct only carries the fact so `adjudicate_lane` fails closed on
    /// it the same uniform way as every other shape violation.
    pub duplicate_lane_id: bool,
    /// A prior human Gate resolution for this lane, already filtered by the
    /// caller to the CURRENT policy revision (a resolution recorded under an
    /// older revision must never be passed here — see [`GateOverride`]'s doc).
    pub gate_override: Option<GateOverride>,
}

/// The three-way outcome (issue #172's own vocabulary).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneDecision {
    AllowedByPolicy,
    NeedsGate,
    Denied,
}

/// A stable, machine-readable reason code — mirrors `readiness::ReasonCode`'s
/// own "never a free-text reason as the ONLY signal" posture. i18n keys and
/// audit payloads key off this, never off `hit_rule`'s free text alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictReason {
    UnknownRepo,
    MissingReason,
    InvalidBase,
    DuplicateLaneId,
    RepoDeniedByPolicy,
    RepoOutsideProjectScope,
    ProtectedBranch,
    HumanDenied,
    GateApprovedOverride,
    GateDeniedOverride,
    PolicyAutoMaterialize,
    HumanConfirmed,
    AwaitingGateDecision,
    ActionAllowedByPolicy,
    ActionDeniedByPolicy,
}

/// One materialize/permission decision, carrying enough of its own basis to
/// audit (issue #172's "reason 记录选仓依据…" + "保留完整 audit") and to key a
/// Gate override or an Evidence row by (`policy_revision`, `scope_revision`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneVerdict {
    pub decision: LaneDecision,
    pub reason: VerdictReason,
    pub hit_rule: Option<String>,
    pub policy_revision: String,
    pub scope_revision: String,
    pub decided_at: String,
    pub source: &'static str,
}

fn now_unix_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

fn matches_name(list: &[String], name: &str) -> bool {
    list.iter().any(|entry| entry.eq_ignore_ascii_case(name))
}

/// A base/ref name is well-formed enough to branch off — a defensive
/// syntactic check, NOT a git-protocol validator (git itself is the final
/// authority at `git worktree add` time). Blank is always valid (repo
/// default). Rejects shapes that are never a real ref name: leading `-`
/// (could be parsed as a flag by a naive shell-out), any control/whitespace
/// character, and the `..` two-dot sequence (git's own range/parent syntax,
/// never valid inside a single ref path component).
pub fn looks_like_valid_ref(base_branch: &str) -> bool {
    if base_branch.is_empty() {
        return true;
    }
    if base_branch.starts_with('-') {
        return false;
    }
    if base_branch.contains("..") {
        return false;
    }
    !base_branch.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The single AuthorityPolicy judgment for one candidate Lane (issue #172).
///
/// Fail-closed checks that do not depend on the policy at all run FIRST and
/// always deny — no policy, however permissive, can waive an unknown repo, a
/// missing reason, a malformed base, or a duplicate lane id (the issue's own
/// "异常与边界" list). Only once those pass does the configured
/// [`PolicyRules`] get a say; a recorded [`GateOverride`] (already filtered by
/// the caller to the current policy revision) settles a Gate outright before
/// the general `protected_branches`/`auto_materialize` rules run.
pub fn adjudicate_lane(
    policy: &PolicySnapshot,
    scope_revision: &str,
    lane: &LaneCandidate<'_>,
) -> LaneVerdict {
    let decided_at = now_unix_string();
    let build = |decision: LaneDecision, reason: VerdictReason, hit_rule: Option<String>| LaneVerdict {
        decision,
        reason,
        hit_rule,
        policy_revision: policy.revision.clone(),
        scope_revision: scope_revision.to_string(),
        decided_at: decided_at.clone(),
        source: "authority_policy",
    };

    if lane.duplicate_lane_id {
        return build(LaneDecision::Denied, VerdictReason::DuplicateLaneId, Some(lane.lane_id.to_string()));
    }
    if !lane.repo_known {
        return build(LaneDecision::Denied, VerdictReason::UnknownRepo, Some(lane.repo_name.to_string()));
    }
    if lane.reason.trim().is_empty() {
        return build(LaneDecision::Denied, VerdictReason::MissingReason, None);
    }
    if !looks_like_valid_ref(lane.base_branch) {
        return build(LaneDecision::Denied, VerdictReason::InvalidBase, Some(lane.base_branch.to_string()));
    }

    if matches_name(&policy.rules.denied_repos, lane.repo_name) {
        return build(
            LaneDecision::Denied,
            VerdictReason::RepoDeniedByPolicy,
            Some(lane.repo_name.to_string()),
        );
    }
    if !policy.rules.allowed_repos.is_empty() && !matches_name(&policy.rules.allowed_repos, lane.repo_name) {
        return build(
            LaneDecision::Denied,
            VerdictReason::RepoOutsideProjectScope,
            Some(lane.repo_name.to_string()),
        );
    }

    match lane.gate_override {
        Some(GateOverride::Approved) => {
            return build(LaneDecision::AllowedByPolicy, VerdictReason::GateApprovedOverride, None);
        }
        Some(GateOverride::Denied) => {
            return build(LaneDecision::Denied, VerdictReason::GateDeniedOverride, None);
        }
        None => {}
    }

    let protected_hit =
        !lane.base_branch.is_empty() && matches_name(&policy.rules.protected_branches, lane.base_branch);
    if protected_hit {
        return build(
            LaneDecision::NeedsGate,
            VerdictReason::ProtectedBranch,
            Some(lane.base_branch.to_string()),
        );
    }

    // A human veto always outranks a permissive policy — checked BEFORE
    // `auto_materialize` so a policy that auto-materializes everything can
    // never resurrect a Lane the human explicitly denied.
    if lane.human_denied {
        return build(LaneDecision::Denied, VerdictReason::HumanDenied, None);
    }
    if policy.rules.auto_materialize {
        return build(LaneDecision::AllowedByPolicy, VerdictReason::PolicyAutoMaterialize, None);
    }
    if lane.human_authorized {
        return build(LaneDecision::AllowedByPolicy, VerdictReason::HumanConfirmed, None);
    }
    build(LaneDecision::NeedsGate, VerdictReason::AwaitingGateDecision, None)
}

// ---- Permission Bridge ----

/// One CLI permission request, reduced to the stable identity the bridge
/// judges. `action_key` MUST be built via `ask::action_key` — the same
/// canonical, collision-resistant encoding every ask-creation path already
/// uses (issue #89), so a policy pattern written against one action can never
/// accidentally match an unrelated one.
#[derive(Clone, Copy, Debug)]
pub struct PermissionAction<'a> {
    pub action_key: &'a str,
}

/// The Permission Bridge's verdict for one CLI ask. `Allow`/`Deny` are
/// DECISIVE — the CLI gets an automatic answer with no human card at all;
/// `Defer` means the policy has no opinion and the existing `AskRegistry`
/// auto-decision + human-ask flow runs completely unchanged. The default (no
/// configured policy, or one with empty `allow_actions`/`deny_actions`)
/// ALWAYS defers — the bridge is purely additive, never a regression to
/// today's approval flow. "CLI 自身的 approval/sandbox 设置永不放宽 Weft
/// 策略": this function never reads anything the CLI itself claimed; the
/// bridge is Weft-side policy data only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeDecision {
    Allow,
    Deny,
    Defer,
}

fn action_pattern_matches(pattern: &str, action_key: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => action_key.starts_with(prefix),
        None => pattern == action_key,
    }
}

/// Map one CLI permission request to the SAME `AuthorityPolicy` the Lane
/// adjudicator consults (issue #172's "定义最小 Permission Bridge…先咨询同一
/// AuthorityPolicy 再落到 AskRegistry/人工"). `deny_actions` is checked before
/// `allow_actions`, so a policy author can always carve an exception out of a
/// broader allow with a narrower deny.
pub fn bridge_decision(policy: &PolicySnapshot, action: &PermissionAction<'_>) -> (BridgeDecision, LaneVerdict) {
    let decided_at = now_unix_string();
    let build = |decision: LaneDecision, decision_kind: BridgeDecision, reason: VerdictReason| {
        (
            decision_kind,
            LaneVerdict {
                decision,
                reason,
                hit_rule: Some(action.action_key.to_string()),
                policy_revision: policy.revision.clone(),
                scope_revision: String::new(),
                decided_at: decided_at.clone(),
                source: "permission_bridge",
            },
        )
    };
    if policy.rules.deny_actions.iter().any(|p| action_pattern_matches(p, action.action_key)) {
        return build(LaneDecision::Denied, BridgeDecision::Deny, VerdictReason::ActionDeniedByPolicy);
    }
    if policy.rules.allow_actions.iter().any(|p| action_pattern_matches(p, action.action_key)) {
        return build(LaneDecision::AllowedByPolicy, BridgeDecision::Allow, VerdictReason::ActionAllowedByPolicy);
    }
    (
        BridgeDecision::Defer,
        LaneVerdict {
            decision: LaneDecision::NeedsGate,
            reason: VerdictReason::AwaitingGateDecision,
            hit_rule: None,
            policy_revision: policy.revision.clone(),
            scope_revision: String::new(),
            decided_at,
            source: "permission_bridge",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_lane<'a>() -> LaneCandidate<'a> {
        LaneCandidate {
            lane_id: "lane-a",
            repo_known: true,
            repo_name: "svc",
            reason: "fix the bug",
            base_branch: "",
            human_authorized: true,
            human_denied: false,
            duplicate_lane_id: false,
            gate_override: None,
        }
    }

    #[test]
    fn default_policy_allows_a_human_authorized_known_repo_lane_zero_regression() {
        let policy = default_policy(PolicyScope::Workspace(1));
        let verdict = adjudicate_lane(&policy, "rev-1", &base_lane());
        assert_eq!(verdict.decision, LaneDecision::AllowedByPolicy);
        assert_eq!(verdict.reason, VerdictReason::HumanConfirmed);
        assert_eq!(verdict.policy_revision, "0");
        assert_eq!(verdict.scope_revision, "rev-1");
    }

    #[test]
    fn default_policy_needs_gate_when_not_yet_human_authorized() {
        let policy = default_policy(PolicyScope::Workspace(1));
        let mut lane = base_lane();
        lane.human_authorized = false;
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::NeedsGate);
        assert_eq!(verdict.reason, VerdictReason::AwaitingGateDecision);
    }

    #[test]
    fn unknown_repo_always_denied_regardless_of_policy() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.auto_materialize = true;
        let mut lane = base_lane();
        lane.repo_known = false;
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::UnknownRepo);
    }

    #[test]
    fn missing_reason_always_denied() {
        let policy = default_policy(PolicyScope::Workspace(1));
        let mut lane = base_lane();
        lane.reason = "   ";
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::MissingReason);
    }

    #[test]
    fn invalid_base_always_denied() {
        let policy = default_policy(PolicyScope::Workspace(1));
        let mut lane = base_lane();
        lane.base_branch = "-x";
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::InvalidBase);
    }

    #[test]
    fn duplicate_lane_id_always_denied() {
        let policy = default_policy(PolicyScope::Workspace(1));
        let mut lane = base_lane();
        lane.duplicate_lane_id = true;
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::DuplicateLaneId);
    }

    #[test]
    fn denied_repos_outrank_auto_materialize() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.auto_materialize = true;
        policy.rules.denied_repos = vec!["svc".to_string()];
        let verdict = adjudicate_lane(&policy, "rev-1", &base_lane());
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::RepoDeniedByPolicy);
    }

    #[test]
    fn repo_outside_explicit_allow_list_denied() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.allowed_repos = vec!["other".to_string()];
        let verdict = adjudicate_lane(&policy, "rev-1", &base_lane());
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::RepoOutsideProjectScope);
    }

    #[test]
    fn protected_branch_needs_gate_even_when_human_authorized() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.protected_branches = vec!["main".to_string()];
        let mut lane = base_lane();
        lane.base_branch = "main";
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::NeedsGate);
        assert_eq!(verdict.reason, VerdictReason::ProtectedBranch);
    }

    #[test]
    fn gate_override_approved_beats_protected_branch() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.protected_branches = vec!["main".to_string()];
        let mut lane = base_lane();
        lane.base_branch = "main";
        lane.gate_override = Some(GateOverride::Approved);
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::AllowedByPolicy);
        assert_eq!(verdict.reason, VerdictReason::GateApprovedOverride);
    }

    #[test]
    fn gate_override_denied_beats_a_permissive_policy() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.auto_materialize = true;
        let mut lane = base_lane();
        lane.gate_override = Some(GateOverride::Denied);
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::GateDeniedOverride);
    }

    #[test]
    fn auto_materialize_allows_a_lane_never_human_authorized() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.auto_materialize = true;
        let mut lane = base_lane();
        lane.human_authorized = false;
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::AllowedByPolicy);
        assert_eq!(verdict.reason, VerdictReason::PolicyAutoMaterialize);
    }

    #[test]
    fn human_denied_always_denies_even_under_auto_materialize() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.auto_materialize = true;
        let mut lane = base_lane();
        lane.human_denied = true;
        let verdict = adjudicate_lane(&policy, "rev-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::HumanDenied);
    }

    #[test]
    fn a_tightened_policy_denies_what_an_older_revision_allowed_stale_never_reused() {
        let permissive = {
            let mut p = default_policy(PolicyScope::Workspace(1));
            p.revision = "1".to_string();
            p
        };
        let before = adjudicate_lane(&permissive, "rev-1", &base_lane());
        assert_eq!(before.decision, LaneDecision::AllowedByPolicy);

        let tightened = {
            let mut p = default_policy(PolicyScope::Workspace(1));
            p.revision = "2".to_string();
            p.rules.denied_repos = vec!["svc".to_string()];
            p
        };
        // Re-adjudicating with the NEW policy revision — never the cached
        // `before` verdict — is the whole "stale never materializes" story:
        // there is no path that lets the old Allow verdict get used again.
        let after = adjudicate_lane(&tightened, "rev-1", &base_lane());
        assert_eq!(after.decision, LaneDecision::Denied);
        assert_eq!(after.policy_revision, "2");
    }

    #[test]
    fn looks_like_valid_ref_rejects_flag_like_and_range_like_and_whitespace() {
        assert!(looks_like_valid_ref(""));
        assert!(looks_like_valid_ref("main"));
        assert!(looks_like_valid_ref("feature/x"));
        assert!(!looks_like_valid_ref("-x"));
        assert!(!looks_like_valid_ref("a..b"));
        assert!(!looks_like_valid_ref("a b"));
        assert!(!looks_like_valid_ref("a\nb"));
    }

    #[test]
    fn bridge_defers_by_default_zero_regression_to_existing_ask_flow() {
        let policy = default_policy(PolicyScope::Workspace(1));
        let action = PermissionAction { action_key: "[\"cmd\",\"bash\",\"rm -rf /\"]" };
        let (decision, verdict) = bridge_decision(&policy, &action);
        assert_eq!(decision, BridgeDecision::Defer);
        assert_eq!(verdict.decision, LaneDecision::NeedsGate);
    }

    #[test]
    fn bridge_allows_an_explicitly_configured_action() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.allow_actions = vec!["[\"cmd\",\"bash\",\"git status\"]".to_string()];
        let action = PermissionAction { action_key: "[\"cmd\",\"bash\",\"git status\"]" };
        let (decision, verdict) = bridge_decision(&policy, &action);
        assert_eq!(decision, BridgeDecision::Allow);
        assert_eq!(verdict.decision, LaneDecision::AllowedByPolicy);
    }

    #[test]
    fn bridge_deny_pattern_outranks_a_broader_allow_pattern() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.allow_actions = vec!["[\"cmd\",\"bash\",*".to_string()];
        policy.rules.deny_actions = vec!["[\"cmd\",\"bash\",\"rm -rf /\"]".to_string()];
        let denied = PermissionAction { action_key: "[\"cmd\",\"bash\",\"rm -rf /\"]" };
        let (decision, _) = bridge_decision(&policy, &denied);
        assert_eq!(decision, BridgeDecision::Deny);

        let allowed = PermissionAction { action_key: "[\"cmd\",\"bash\",\"git status\"]" };
        let (decision, _) = bridge_decision(&policy, &allowed);
        assert_eq!(decision, BridgeDecision::Allow);
    }

    #[test]
    fn scope_kind_and_id_and_parse_round_trip() {
        let ws = PolicyScope::Workspace(7);
        assert_eq!(ws.kind(), "workspace");
        assert_eq!(ws.id(), 7);
        assert_eq!(PolicyScope::parse("workspace", 7), Some(ws));
        let proj = PolicyScope::Project(3);
        assert_eq!(proj.kind(), "project");
        assert_eq!(PolicyScope::parse("project", 3), Some(proj));
        assert_eq!(PolicyScope::parse("bogus", 1), None);
    }
}
