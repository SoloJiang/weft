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
    /// True when the stored `rules` JSON did not parse and `rules` is therefore
    /// the empty default rather than what the user actually configured. Every
    /// field being `#[serde(default)]` only makes a MISSING field safe; one
    /// wrong field TYPE fails the whole parse, and silently reading that as
    /// "no rules" would drop `denied_repos`/`protected_branches`/`deny_actions`
    /// — turning a corrupt row into a strictly MORE permissive policy than the
    /// user wrote. Adjudication fails closed on this flag instead.
    #[serde(default)]
    pub rules_unreadable: bool,
}

/// Build the snapshot for one stored `authority_policy` row. The ONLY
/// construction path any caller should use: a row whose `rules` JSON does not
/// parse resolves to empty rules AND `rules_unreadable: true`, so the
/// fail-closed handling lives in one place rather than depending on each call
/// site remembering not to `unwrap_or_default()` the parse.
pub fn snapshot_from_row(
    row: crate::store::entities::authority_policy::Model,
    scope: PolicyScope,
) -> PolicySnapshot {
    let (rules, rules_unreadable) = match serde_json::from_str::<PolicyRules>(&row.rules) {
        Ok(rules) => (rules, false),
        Err(_) => (PolicyRules::default(), true),
    };
    PolicySnapshot {
        id: row.id,
        scope,
        revision: row.revision,
        rules,
        source: row.source,
        created_at: row.created_at,
        revoked_at: row.revoked_at,
        rules_unreadable,
    }
}

/// The hard-coded, conservative default used when a scope has NEVER had a
/// configured policy row. A scope whose policy was revoked resolves to
/// [`revoked_policy`] instead — see that function for why the two must differ.
/// Its rules are the
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
        rules_unreadable: false,
    }
}

/// The policy in force for a scope whose configured policy was REVOKED.
///
/// Distinct from [`default_policy`] on purpose, and the distinction is a safety
/// property rather than bookkeeping. `revoke_authority_policy` documents that
/// revoking "can only make a scope MORE conservative, never more permissive" —
/// but the hard-coded default has empty `denied_repos` and empty
/// `protected_branches`, so falling back to it *removes* every rule the revoked
/// policy carried. Revoking a policy that denied a repo, or gated a release
/// branch, would hand that exact Lane an `allowed_by_policy` verdict.
///
/// So a revoked scope resolves to "a human decides", not "everything is
/// allowed": [`adjudicate_lane`] reads the non-empty `revoked_at` and answers
/// `NeedsGate`, with the same Gate-override escape the unreadable-policy branch
/// has, so nothing is stranded. The rules are empty because none can be
/// trusted, which also makes [`bridge_decision`] defer every CLI ask to the
/// existing human flow.
///
/// A scope that never had a policy at all is NOT this — it keeps
/// [`default_policy`], which is what leaves the feature inert for an
/// installation that has never configured one.
///
/// `revision` is the revoked row's own, so a Gate decision recorded under it
/// and a later real policy still compare in the right order.
pub fn revoked_policy(scope: PolicyScope, revision: String, revoked_at: String) -> PolicySnapshot {
    PolicySnapshot {
        id: 0,
        scope,
        // DISTINCT from the revision the revoked row carried, because a revoke
        // stamps `revoked_at` on that row without allocating a new revision.
        // Reusing it left a Gate card rendered under the ACTIVE policy still
        // valid: both revision checks in `resolve_lane_gate` accepted the stale
        // card, the approval was recorded under that same revision, and
        // adjudication then honoured its own override and materialized the lane
        // — a revoke that a pre-revoke card could walk straight through. A human
        // may still approve a gated lane in a revoked scope; they just have to
        // do it on a card that post-dates the revoke.
        revision: format!("{revision}:revoked"),
        rules: PolicyRules::default(),
        source: "system".to_string(),
        created_at: String::new(),
        // The discriminator `adjudicate_lane` reads. Never empty here: a
        // revoked row always carries a stamp, and an empty one would make this
        // snapshot indistinguishable from the permissive default.
        revoked_at: match revoked_at.is_empty() {
            true => "revoked".to_string(),
            false => revoked_at,
        },
        rules_unreadable: false,
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
    UnreadablePolicy,
    HumanDenied,
    GateApprovedOverride,
    GateDeniedOverride,
    PolicyAutoMaterialize,
    RevokedPolicy,
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

/// The bare branch name behind any spelling git accepts for it:
/// `refs/remotes/origin/main`, `refs/heads/main`, `origin/main` and `main` all
/// collapse to `main`.
///
/// Protection rules MUST compare on this. `add_worktree_synced` performs the
/// same collapse before resolving a base, so a lane naming `origin/main`
/// branches from exactly the ref a `protected_branches: ["main"]` rule means to
/// gate — while an exact-string comparison sees two different names and lets it
/// through. The rule side is normalized too, so configuring `refs/heads/main`
/// protects `main` rather than nothing at all.
fn bare_branch_name(name: &str) -> &str {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix("refs/remotes/") {
        return rest.strip_prefix("origin/").unwrap_or(rest);
    }
    if let Some(rest) = name.strip_prefix("refs/heads/") {
        return rest;
    }
    name.strip_prefix("origin/").unwrap_or(name)
}

/// `matches_name` over branch spellings — both sides collapsed to the bare name.
fn matches_branch(list: &[String], name: &str) -> bool {
    let name = bare_branch_name(name);
    list.iter().any(|entry| bare_branch_name(entry).eq_ignore_ascii_case(name))
}

/// A base/ref name is a plain BRANCH NAME, not a git revision expression.
///
/// This is a permission boundary, not merely hygiene. `protected_branches` is
/// matched by NAME (`bare_branch_name` collapses `origin/` and `refs/…`
/// spellings), while git resolves a revision EXPRESSION to a commit — so an
/// untrusted proposal naming `main~0`, `main^0` or `main@{0}` used to pass this
/// check, miss a `protected_branches: ["main"]` rule because the literals
/// differ, and materialize onto the protected branch's own commit with no Gate.
/// Resolving the expression here is not an option: `adjudicate_lane` is pure by
/// design and reaches neither git nor the store. Refusing the expression is,
/// and it fails CLOSED — a rejected base is `Denied(InvalidBase)`, never a
/// silent allow.
///
/// So the accepted shape is git's own `check-ref-format` for a branch name,
/// which excludes exactly the revision-expression metacharacters: `~` and `^`
/// (ancestry), `:` (path/range), `@{` (reflog), plus `?`, `*`, `[`, `\` and the
/// control/whitespace and `..` cases this already refused. Blank stays valid
/// (the repo default). `/` stays valid, so `origin/main` and `refs/heads/main`
/// still work, as does a raw commit SHA.
/// Whether this is a git object id rather than a branch name — 7 to 40 hex
/// digits, the abbreviated and full SHA-1 forms git accepts interchangeably.
fn looks_like_commit_id(value: &str) -> bool {
    let len = value.len();
    (7..=40).contains(&len) && value.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn looks_like_valid_ref(base_branch: &str) -> bool {
    if base_branch.is_empty() {
        return true;
    }
    if base_branch.starts_with('-') || base_branch.starts_with('/') {
        return false;
    }
    if base_branch.ends_with('/') || base_branch.ends_with('.') || base_branch.ends_with(".lock") {
        return false;
    }
    if base_branch.contains("..") || base_branch.contains("//") || base_branch.contains("@{") {
        return false;
    }
    // A lone `@` is not a ref name in git either.
    if base_branch == "@" {
        return false;
    }
    // A COMMIT ID is not a branch name, and accepting one re-opens the very
    // bypass the metacharacter rules above close. `protected_branches` is
    // matched by NAME; a full or abbreviated SHA of `main` resolves to the same
    // commit, matches no name, and materializes onto the protected branch's own
    // tip with no Gate. Nothing that resolves to a commit without naming a
    // branch may pass this check.
    //
    // Fails CLOSED for the pathological case of a branch genuinely named like a
    // SHA: it is refused rather than silently exempted. Git itself warns about
    // such names for the same ambiguity reason.
    if looks_like_commit_id(base_branch) {
        return false;
    }
    // Only BRANCH namespaces. `refs/tags/x` is a ref that resolves to a commit
    // without naming a branch, so a tag pointing at protected `main` matched no
    // `protected_branches` entry and materialized from the protected commit —
    // the same bypass as a revision expression or a raw SHA, one namespace over.
    // `git.rs` accepts a `refs/...` start point verbatim, so this is the only
    // place that can refuse it.
    //
    // Enumerating what is ALLOWED rather than what is denied: `refs/heads/…` is
    // a branch, and anything else under `refs/` (tags, notes, remotes-as-refs,
    // replace, stash, and whatever git adds next) is not. Chasing prefixes one
    // at a time is what let this class recur three times.
    if base_branch.starts_with("refs/") && !base_branch.starts_with("refs/heads/") {
        return false;
    }
    // Git's PSEUDO-refs — `HEAD`, `FETCH_HEAD`, `ORIG_HEAD`, `MERGE_HEAD`,
    // `CHERRY_PICK_HEAD` — resolve to a commit without naming a branch, so they
    // dodge `protected_branches` exactly as a tag or a SHA does, and
    // `git::resolve_base_ref` accepts them verbatim. They share one shape: git
    // requires a pseudo-ref to be all upper case (with `_` and digits), which no
    // conventional branch name uses.
    //
    // Matching the SHAPE rather than a list is the point. Enumerating `HEAD`,
    // then `FETCH_HEAD`, then the next one is how this class reached a fourth
    // spelling. A branch genuinely named `RELEASE` is refused too — fail closed,
    // and git itself discourages such names for this ambiguity.
    let bare = bare_branch_name(base_branch);
    if !bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
    {
        return false;
    }
    !base_branch.chars().any(|c| {
        c.is_whitespace()
            || c.is_control()
            || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    })
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

    // A policy whose rules did not parse is UNKNOWN, not empty: every check
    // below reads `policy.rules`, and running them against the empty default
    // would answer "nothing is denied" for a row that may well have denied
    // this exact repo or branch. Fail closed to a human Gate — but honor a
    // Gate resolution already recorded at this same revision, so the corrupt
    // row leaves a human path forward instead of stranding the Lane.
    if policy.rules_unreadable {
        return match lane.gate_override {
            Some(GateOverride::Approved) => {
                build(LaneDecision::AllowedByPolicy, VerdictReason::GateApprovedOverride, None)
            }
            Some(GateOverride::Denied) => {
                build(LaneDecision::Denied, VerdictReason::GateDeniedOverride, None)
            }
            None => build(LaneDecision::NeedsGate, VerdictReason::UnreadablePolicy, None),
        };
    }

    // A REVOKED policy is the same kind of unknown, for the same reason: the
    // rules below are empty not because nothing was forbidden but because
    // nothing can be consulted. Answering from them would let "undo my policy"
    // silently un-deny a repo and un-protect a branch — the one thing
    // `revoke_authority_policy` promises revoking cannot do. Fail closed to a
    // human, honoring a Gate resolution already recorded at this revision so
    // the Lane still has a way forward. See `authority::revoked_policy`.
    if !policy.revoked_at.is_empty() {
        return match lane.gate_override {
            Some(GateOverride::Approved) => {
                build(LaneDecision::AllowedByPolicy, VerdictReason::GateApprovedOverride, None)
            }
            Some(GateOverride::Denied) => {
                build(LaneDecision::Denied, VerdictReason::GateDeniedOverride, None)
            }
            None => build(LaneDecision::NeedsGate, VerdictReason::RevokedPolicy, None),
        };
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

    let protected_hit = !lane.base_branch.is_empty()
        && matches_branch(&policy.rules.protected_branches, lane.base_branch);
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
    // Unknown rules cannot decisively answer anything: with the empty default
    // both lists below are vacuously unmatched, so state the intent outright
    // rather than leaving it to fall through. Defer is exactly the pre-#172
    // AskRegistry/human flow, so a corrupt row costs a human card, never an
    // auto-approval of something `deny_actions` may have named.
    if policy.rules_unreadable {
        return build(LaneDecision::NeedsGate, BridgeDecision::Defer, VerdictReason::UnreadablePolicy);
    }
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

    /// A protection rule must survive every spelling git accepts for the same
    /// branch. `add_worktree_synced` collapses all of these to `main` before
    /// resolving the base, so an exact-string comparison let a lane branch from
    /// protected `main` by simply naming it `origin/main`.
    #[test]
    fn protected_branches_match_across_ref_spellings() {
        for spelling in [
            "main",
            "origin/main",
            "refs/heads/main",
            "refs/remotes/origin/main",
            "MAIN",
        ] {
            assert!(
                matches_branch(&["main".to_string()], spelling),
                "{spelling} should match a protected `main`"
            );
        }
        // The configured side is normalized too.
        assert!(matches_branch(&["refs/heads/main".to_string()], "main"));
        assert!(matches_branch(&["origin/main".to_string()], "main"));
        // A genuinely different branch still does not match.
        assert!(!matches_branch(&["main".to_string()], "release/main-2"));
        assert!(!matches_branch(&["main".to_string()], "feature"));
    }

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
        // Spellings git resolves to the SAME commit as `main`. Accepting these
        // let a lane dodge `protected_branches: ["main"]`, which is matched by
        // name and cannot see through a revision expression.
        for dodge in [
            "main~0", "main^0", "main^", "main~1", "main@{0}", "main^{}", "main^{commit}",
            "HEAD^", "main:path", "ma?in", "ma*in", "ma[in", "ma\\in", "@",
        ] {
            assert!(!looks_like_valid_ref(dodge), "{dodge:?} must not pass as a branch name");
        }
        // …while ordinary branch spellings and a raw SHA still do.
        assert!(looks_like_valid_ref("origin/main"));
        assert!(looks_like_valid_ref("refs/heads/main"));
        assert!(looks_like_valid_ref("release/2.0"));
        // A commit id is NOT a branch name: the full SHA of `main` resolves to
        // the same commit and would dodge `protected_branches: ["main"]`.
        assert!(!looks_like_valid_ref("9fddf70ced1a2b3c4d5e6f70819a2b3c4d5e6f70"));
        assert!(!looks_like_valid_ref("9fddf70"));
        assert!(!looks_like_valid_ref("9FDDF70CED1A2B3C"));
        // …while ordinary names that merely contain hex characters still pass.
        assert!(looks_like_valid_ref("beef"));
        assert!(looks_like_valid_ref("feature/9fddf70"));
        assert!(looks_like_valid_ref("release-2024"));
        // Tag and other non-branch namespaces resolve to a commit without
        // naming a branch, so they dodge `protected_branches` the same way.
        // Pseudo-refs resolve to a commit without naming a branch.
        assert!(!looks_like_valid_ref("HEAD"));
        assert!(!looks_like_valid_ref("FETCH_HEAD"));
        assert!(!looks_like_valid_ref("ORIG_HEAD"));
        assert!(!looks_like_valid_ref("MERGE_HEAD"));
        assert!(!looks_like_valid_ref("refs/tags/main-tip"));
        assert!(!looks_like_valid_ref("refs/tags/v1.0"));
        assert!(!looks_like_valid_ref("refs/remotes/origin/main"));
        assert!(!looks_like_valid_ref("refs/notes/commits"));
        assert!(!looks_like_valid_ref("main.lock"));
        assert!(!looks_like_valid_ref("main/"));
        assert!(!looks_like_valid_ref("/main"));
    }

    /// The bypass end-to-end: a protected `main` must still Gate a lane whose
    /// base is a revision expression pointing at it.
    #[test]
    fn a_revision_expression_cannot_dodge_a_protected_branch() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules.protected_branches = vec!["main".to_string()];
        for base in [
            "main",
            "origin/main",
            "refs/heads/main",
            "main~0",
            "main^0",
            "9fddf70ced1a2b3c4d5e6f70819a2b3c4d5e6f70",
            "9fddf70",
            "refs/tags/main-tip",
            "HEAD",
        ] {
            let lane = LaneCandidate {
                lane_id: "l1",
                repo_known: true,
                repo_name: "repo",
                reason: "why",
                base_branch: base,
                human_authorized: true,
                human_denied: false,
                duplicate_lane_id: false,
                gate_override: None,
            };
            let verdict = adjudicate_lane(&policy, "1", &lane);
            assert_ne!(
                verdict.decision,
                LaneDecision::AllowedByPolicy,
                "{base:?} must not materialize without a Gate, got {verdict:?}"
            );
        }
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

    // ---- unreadable rules fail closed ----

    fn row_with_rules(rules: &str) -> crate::store::entities::authority_policy::Model {
        crate::store::entities::authority_policy::Model {
            id: 7,
            scope: "workspace".to_string(),
            scope_id: 1,
            revision: "3".to_string(),
            rules: rules.to_string(),
            source: "user".to_string(),
            created_at: String::new(),
            revoked_at: String::new(),
        }
    }

    #[test]
    fn a_partial_rules_shape_still_parses_and_is_not_flagged() {
        // Every field is `#[serde(default)]`, so a MISSING field stays safe —
        // this is the case the struct's own doc promises, and it must keep
        // working or every older policy row would start gating.
        let snap = snapshot_from_row(row_with_rules(r#"{"denied_repos":["api"]}"#), PolicyScope::Workspace(1));
        assert!(!snap.rules_unreadable);
        assert_eq!(snap.rules.denied_repos, vec!["api".to_string()]);
    }

    #[test]
    fn unparseable_rules_are_flagged_rather_than_read_as_no_rules() {
        // One wrong field TYPE fails the whole parse. Reading that as the empty
        // default would silently DROP denied_repos/protected_branches.
        let snap = snapshot_from_row(
            row_with_rules(r#"{"denied_repos":"api","protected_branches":["main"]}"#),
            PolicyScope::Workspace(1),
        );
        assert!(snap.rules_unreadable);
        assert!(snap.rules.denied_repos.is_empty(), "the empty rules are NOT the user's");
    }

    #[test]
    fn an_unreadable_policy_gates_instead_of_allowing() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules_unreadable = true;
        let lane = base_lane();
        let verdict = adjudicate_lane(&policy, "1-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::NeedsGate);
        assert_eq!(verdict.reason, VerdictReason::UnreadablePolicy);
    }

    #[test]
    fn an_unreadable_policy_still_honors_a_gate_resolution_at_this_revision() {
        // Otherwise a corrupt row would strand every lane with no way forward.
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules_unreadable = true;
        let mut lane = base_lane();
        lane.gate_override = Some(GateOverride::Approved);
        assert_eq!(adjudicate_lane(&policy, "1-1", &lane).decision, LaneDecision::AllowedByPolicy);
        lane.gate_override = Some(GateOverride::Denied);
        assert_eq!(adjudicate_lane(&policy, "1-1", &lane).decision, LaneDecision::Denied);
    }

    #[test]
    fn an_unreadable_policy_defers_the_bridge_rather_than_deciding() {
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules_unreadable = true;
        // Even a key the (lost) rules might have denied must not be auto-allowed.
        let (bridge, _) = bridge_decision(&policy, &PermissionAction { action_key: "Run: rm -rf /" });
        assert_eq!(bridge, BridgeDecision::Defer);
    }

    #[test]
    fn a_shape_violation_outranks_an_unreadable_policy() {
        // The policy-independent fail-closed checks still run first: an
        // unreadable policy must not soften a missing reason into a Gate.
        let mut policy = default_policy(PolicyScope::Workspace(1));
        policy.rules_unreadable = true;
        let mut lane = base_lane();
        lane.reason = "   ";
        let verdict = adjudicate_lane(&policy, "1-1", &lane);
        assert_eq!(verdict.decision, LaneDecision::Denied);
        assert_eq!(verdict.reason, VerdictReason::MissingReason);
    }
}
