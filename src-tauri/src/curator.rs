//! The workspace Curator (ARCHITECTURE §4.9, §4.11): a hybrid pipeline where
//! deterministic manifest edges (`source="manifest"`) provide the high-confidence
//! lib floor, and a read-only coding agent fills the runtime/infra relations on
//! top. Precedence: user > manifest > agent. Findings persist on `repo_profile`;
//! the graph is rebuilt from them.

use crate::git;
use crate::profile::{self, AgentRelation, Component, Edge};
use crate::store::entities::{repo_profile, repo_ref};
use crate::store::{repo, Db};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// A profile as the UI sees it: decoded fields + repo name.
/// `analyzed` is false for a repo the agent hasn't classified yet (rendered as
/// an "analyzing" placeholder); such a node has an empty `tier`.
#[derive(Clone, Debug, Serialize)]
pub struct ProfileView {
    pub repo_id: i32,
    pub repo_name: String,
    /// "frontend" | "backend" | "" (unclassified / analyzing).
    pub tier: String,
    pub stack: Vec<String>,
    pub summary: String,
    /// "agent" | "user" | "" (placeholder).
    pub source: String,
    pub profiled_commit: String,
    pub analyzed: bool,
    pub components: Vec<Component>,
    /// Live analysis lifecycle from the run-state registry: "idle" | "running" |
    /// "failed". Distinct from `analyzed` (which reflects a persisted canonical
    /// tier): a repo can be `analyzed=false` while idle (never started), running,
    /// or failed — the UI renders each differently instead of one eternal spinner.
    pub analysis_state: String,
    /// The error from the last failed analysis (only set when `analysis_state ==
    /// "failed"`), surfaced in the detail panel alongside a manual retry.
    pub analysis_error: Option<String>,
    /// Role category within the tier (free-text, agent-assigned). "" until classified.
    pub category: String,
    /// Feature domains owned by this repo (agent-assigned).
    pub domains: Vec<String>,
}

/// The workspace dependency graph: every repo (placeholders included) + the
/// agent-inferred edges between them.
#[derive(Clone, Debug, Serialize)]
pub struct Graph {
    pub nodes: Vec<ProfileView>,
    pub edges: Vec<Edge>,
}

fn json_strs(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn arr(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn comps(s: &str) -> Vec<Component> {
    serde_json::from_str(s).unwrap_or_default()
}

// Summary and tier are independently owned: a user may pin one without pinning
// the other, so calibrating the tier must not freeze an agent-written summary
// (and vice versa). The single `source` column encodes which of the two the user
// owns — "user" (both, also the legacy value), "user_summary", "user_tier", or
// "agent" (neither) — so no migration is needed.
fn owns_summary(source: &str) -> bool {
    matches!(source, "user" | "user_summary")
}
fn owns_tier(source: &str) -> bool {
    matches!(source, "user" | "user_tier")
}
fn combine_source(summary_owned: bool, tier_owned: bool) -> &'static str {
    match (summary_owned, tier_owned) {
        (true, true) => "user",
        (true, false) => "user_summary",
        (false, true) => "user_tier",
        (false, false) => "agent",
    }
}

/// Apply a user calibration to the opinion fields. `summary`/`tier` are each
/// `Some` only for the field the user actually changed, so editing one pins ONLY
/// that field's ownership; the other keeps its prior value and ownership and can
/// still be refreshed by the agent.
pub async fn edit_profile(
    db: &Db,
    repo_id: i32,
    summary: Option<&str>,
    tier: Option<&str>,
) -> Result<repo_profile::Model> {
    // Don't resurrect a deleted repo: a stale edit (e.g. an input blur racing
    // delete_repo) must not recreate an orphaned profile row, since repo_profile
    // has no enforced foreign key.
    if repo::get_repo(db, repo_id).await?.is_none() {
        return Err(anyhow::anyhow!("repo {repo_id} no longer exists"));
    }
    // A repo can be edited while it is still an "analyzing" placeholder; upsert a
    // row rather than erroring so the human's calibration always persists, with
    // factual fields defaulted until the agent fills them.
    let existing = repo::get_repo_profile(db, repo_id).await?;
    let prior_source = existing.as_ref().map(|p| p.source.as_str()).unwrap_or("agent");
    let (stack, components, commit) = match &existing {
        Some(p) => (p.stack.clone(), p.components.clone(), p.profiled_commit.clone()),
        None => ("[]".to_string(), "[]".to_string(), String::new()),
    };

    let new_summary = match summary {
        Some(s) => s.to_string(),
        None => existing.as_ref().map(|p| p.summary.clone()).unwrap_or_default(),
    };
    // A provided tier is canonicalized; an empty string is an intentional clear,
    // while a non-empty legacy value (service/app/…) is kept verbatim so it still
    // qualifies for the legacy backfill. An absent tier keeps the prior value.
    let new_tier = match tier {
        Some(t) => match profile::normalize_tier(t) {
            Some(canon) => canon,
            None if t.trim().is_empty() => String::new(),
            None => t.to_string(),
        },
        None => existing.as_ref().map(|p| p.role.clone()).unwrap_or_default(),
    };
    // A manual canonical-tier edit cascades to this repo's components, so the
    // expanded (per-component) view matches the overview. Tolerate a malformed
    // components blob (treat as none). Other edits leave components untouched.
    let components = if let Some(t) = tier {
        if let Some(canon) = profile::normalize_tier(t) {
            match serde_json::from_str::<Vec<profile::Component>>(&components) {
                Ok(mut comps) => {
                    for c in &mut comps {
                        c.tier = canon.clone();
                    }
                    serde_json::to_string(&comps).unwrap_or(components)
                }
                Err(_) => components,
            }
        } else {
            components
        }
    } else {
        components
    };
    let source = combine_source(
        owns_summary(prior_source) || summary.is_some(),
        owns_tier(prior_source) || tier.is_some(),
    );
    let saved =
        repo::upsert_repo_profile(db, repo_id, &new_tier, &stack, &new_summary, &components, source, &commit)
            .await?;
    // A manual edit where the user PROVIDES a canonical tier RECOVERS a repo whose
    // analysis failed: clear any lingering `failed` run-state so it stops reading as
    // a retryable failure (failed overrides analyzed in the UI). Gate on `tier`
    // being supplied, not just the resulting tier being canonical — a summary-only
    // edit (`tier == None`) INHERITS the prior canonical role without re-classifying,
    // so clearing there would silently drop a real (e.g. stale-refresh) failure with
    // no re-run. No-op if it wasn't failed.
    if tier.is_some() && profile::normalize_tier(&new_tier).is_some() {
        clear_failure(repo_id);
        // The graph/detail views now read `analysis_state` from the PERSISTED
        // profile row (not the in-memory map), and `upsert_repo_profile` above
        // preserves that column unchanged. Clearing only the in-memory failure
        // would leave the DB column at "failed" — the repo keeps rendering as a
        // retryable failure and stays skipped from auto-passes. Persist "idle" too.
        let _ = repo::set_analysis_state(db, repo_id, "idle", None).await;
    }
    // A manual profile edit changes the map's INVENTORY surface (a tier edit
    // cascades to component tiers; a summary edit changes the narrative). Relations
    // have their own invalidation chokepoint in `set_repo_relations`, but inventory
    // edits don't pass through it, so invalidate the workspace map doc here too.
    // (Agent-driven inventory writes via `persist_repo_class` are always followed by
    // `analyze_relations`' fresh-markdown write in the same pass, so this manual path
    // is the only standalone inventory mutation.) Regenerates on the next pass.
    if let Some(r) = repo::get_repo(db, repo_id).await? {
        let _ = repo::clear_repo_map_doc(db, r.workspace_id).await;
    }
    Ok(saved)
}

/// One repo as the UI sees it. `profile == None` means the agent hasn't analyzed
/// this repo yet → an "analyzing" placeholder node.
fn view_of(repo: &repo_ref::Model, profile: Option<&repo_profile::Model>) -> ProfileView {
    let Some(p) = profile else {
        return ProfileView {
            repo_id: repo.id,
            repo_name: repo.name.clone(),
            tier: String::new(),
            stack: Vec::new(),
            summary: String::new(),
            source: String::new(),
            profiled_commit: String::new(),
            analyzed: false,
            components: Vec::new(),
            // No profile row yet → no persisted state; default to idle.
            analysis_state: "idle".to_string(),
            analysis_error: None,
            category: String::new(),
            domains: Vec::new(),
        };
    };
    ProfileView {
        repo_id: repo.id,
        repo_name: repo.name.clone(),
        tier: p.role.clone(),
        stack: arr(&p.stack),
        summary: p.summary.clone(),
        source: p.source.clone(),
        profiled_commit: p.profiled_commit.clone(),
        // "Analyzed" means the agent reached a real classification. A row can
        // exist with an empty/legacy tier — an eager placeholder, a
        // calibration/summary edit before analysis, or a pre-upgrade row — and
        // those must still read as "analyzing", not a finished unclassified node.
        analyzed: profile::normalize_tier(&p.role).is_some(),
        components: comps(&p.components),
        // Read from the persisted profile columns; in-memory run_phase/run_error
        // are now only used for the analyze_workspace failed-repo gate.
        analysis_state: p.analysis_state.clone(),
        analysis_error: p.analysis_error.clone(),
        category: p.category.clone(),
        domains: arr(&p.domains),
    }
}

/// Every repo in a workspace as the UI sees it, placeholders included (a repo
/// with no agent profile yet is returned with `analyzed=false`).
pub async fn list(db: &Db, workspace_id: i32) -> Result<Vec<ProfileView>> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut out = Vec::new();
    for r in &repos {
        let p = repo::get_repo_profile(db, r.id).await?;
        out.push(view_of(r, p.as_ref()));
    }
    Ok(out)
}

/// The workspace dependency graph: one node per repo (placeholders for repos the
/// agent hasn't classified yet) and the agent-inferred edges between them. There
/// is no manifest floor anymore — every edge comes from a stored agent relation
/// (`agent_edges` already drops self-links, stale targets, and `rejected`
/// tombstones).
pub async fn graph(db: &Db, workspace_id: i32) -> Result<Graph> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut nodes = Vec::new();
    let mut relations: Vec<(i32, Vec<AgentRelation>)> = Vec::new();
    for r in &repos {
        let p = repo::get_repo_profile(db, r.id).await?;
        if let Some(pp) = &p {
            // Tolerate malformed/empty JSON: a bad relations blob just means no
            // agent edges for that repo, never a failed graph.
            relations.push((r.id, serde_json::from_str(&pp.relations).unwrap_or_default()));
        }
        nodes.push(view_of(r, p.as_ref()));
    }
    let node_ids: std::collections::HashSet<i32> = nodes.iter().map(|n| n.repo_id).collect();
    let edges = relations
        .iter()
        .flat_map(|(id, rels)| profile::agent_edges(*id, rels, &node_ids))
        .collect();
    maybe_schedule_backfill(db, workspace_id, &nodes);
    Ok(Graph { nodes, edges })
}

/// Schedule the one-shot legacy backfill for an upgraded workspace whose rows
/// lack a canonical tier. Called from `graph()` so EVERY read path (the Tauri repo
/// map AND the planner's MCP `get_repo_map`) covers it, not just the UI. No-op
/// outside a running app (so unit tests never spawn an agent) and at most once per
/// workspace per process (so a failed/slow analyzer can't storm).
fn maybe_schedule_backfill(db: &Db, workspace_id: i32, nodes: &[ProfileView]) {
    if crate::APP_HANDLE.get().is_none() {
        return;
    }
    // A node needs the agent if its tier isn't canonical — a non-empty legacy
    // value (service/app/…), an empty tier from an old "Other"/summary-only edit,
    // OR a placeholder whose add-time pass never finished (e.g. the app exited or
    // the tool failed mid-run). The one-shot `try_claim_backfill` guard + the
    // coalescer make scheduling this at most one (deduped) pass per session, so
    // including fresh placeholders just rides along with the add's own pass.
    let needs_backfill = nodes
        .iter()
        .any(|n| profile::normalize_tier(&n.tier).is_none());
    if needs_backfill && try_claim_backfill(workspace_id) {
        let db = db.clone();
        tauri::async_runtime::spawn(async move {
            // Auto pass (fires on graph reads) → not forced: leave failed repos be.
            analyze_workspace_coalesced(&db, workspace_id, false).await;
        });
    }
}

// ─────────────────────────── agent curator ───────────────────────────
//
// The whole curator: a bounded, read-only agent classifies each repo's tier and
// surfaces monorepo sub-components (per-repo deep pass), then reports cross-repo
// relations (HTTP, gRPC, queues, shared infra, and declared libs). Findings
// persist on `repo_profile` and rebuild into `graph()`'s nodes + edges.

/// One relation as the curator agent reports it: flat, with an explicit `from`
/// (the stored `AgentRelation` is per-producer, so `from` is implicit there).
/// Lenient: missing fields default. `from`/`to` are optional so one malformed row
/// (missing an endpoint) is dropped by `persist_relations` rather than rejecting
/// the entire reply — this agent pass is the only source of cross-repo edges.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CuratorRelation {
    #[serde(default)]
    pub from: Option<i32>,
    #[serde(default)]
    pub to: Option<i32>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub via: String,
    // Lenient: an agent may emit a float (0.8), a word ("high"), or an
    // out-of-range number; coerce/clamp to 0-100 instead of rejecting the whole
    // `relations` payload over one bad value.
    #[serde(default, deserialize_with = "lenient_confidence")]
    pub confidence: u8,
    /// Free-text explanation from the agent for why this dependency exists.
    #[serde(default)]
    pub rationale: String,
}

/// Coerce a relation's `confidence` from whatever shape the agent emitted (int,
/// float, numeric string, or garbage) to a clamped 0-100 `u8`, defaulting to 0.
fn lenient_confidence<'de, D>(d: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let n = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(num) => num.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    Ok(n.clamp(0.0, 100.0).round() as u8)
}

#[derive(Debug, serde::Deserialize)]
struct CuratorWire {
    // Required (no serde default): a reply without a `relations` array is treated
    // as unparseable, so a malformed/timed-out turn never reads as "no relations".
    relations: Vec<CuratorRelation>,
    /// RedIM-style markdown document synthesized by the analyst. Optional:
    /// tolerate an agent that omits it — the doc update is skipped, not a failure.
    #[serde(default)]
    repo_map_markdown: Option<String>,
}

/// The parsed result of the curator agent's cross-repo pass. Relations are always
/// present (possibly empty); the markdown doc may be absent if the agent omitted it.
pub struct CuratorOutput {
    pub relations: Vec<CuratorRelation>,
    /// A RedIM-style workspace doc synthesized by the analyst. `None` when the
    /// agent did not emit one (degrade gracefully — skip the doc update).
    pub repo_map_markdown: Option<String>,
}

/// Every balanced top-level `{...}` substring, in order. Byte-scans for the ASCII
/// structural chars (`{ } " \`) — which never collide with UTF-8 continuation
/// bytes — and is string-literal aware, so braces inside strings (and earlier
/// prose/config objects) don't fool the depth counter.
fn json_objects(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'{' {
            i += 1;
            continue;
        }
        let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
        let mut end = None;
        let mut j = i;
        while j < b.len() {
            let c = b[j];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(j);
                    break;
                }
            }
            j += 1;
        }
        match end {
            Some(e) => {
                out.push(&text[i..=e]);
                i = e + 1;
            }
            // An unbalanced `{` (e.g. a stray brace in prose / a code snippet)
            // must not abort the scan — skip just this `{` and keep looking, so a
            // valid JSON object later in the reply is still found.
            None => i += 1,
        }
    }
    out
}

/// Extract the curator agent's output from its free-form reply (tolerant of
/// markdown fences / surrounding prose). Scans EVERY balanced object and returns
/// the LAST that deserializes as a relations payload — the prompt asks for the
/// JSON as the final thing, and an earlier prose/config `{...}` must not hide it.
/// `None` when no object has a `relations` array (timed-out/malformed reply) so
/// the caller leaves the graph intact; `Some` with an empty `relations` is an
/// explicit "no relations". `repo_map_markdown` defaults to `None` when absent —
/// the caller skips the doc update gracefully rather than failing.
pub fn parse_curator_output(text: &str) -> Option<CuratorOutput> {
    json_objects(text)
        .into_iter()
        .rev()
        .find_map(|obj| serde_json::from_str::<CuratorWire>(obj).ok())
        .map(|w| CuratorOutput {
            relations: w.relations,
            // Treat an explicit null or omitted key as None (skip doc update).
            repo_map_markdown: w.repo_map_markdown.filter(|s| !s.trim().is_empty()),
        })
}

/// The per-repo deep pass's strict-JSON result. `tier` is required (no serde
/// default), so a reply without it reads as unparseable and the prior profile is
/// left intact rather than blanked. `name` captures a stray `name` key: the
/// repo-level schema has no `name`, but a COMPONENT does, so an object with one is
/// a nested component that `json_objects` scanned into (a truncated top-level
/// object missing its brace) and is rejected. We choose this over a `path`/`deps`
/// discriminator because the failure mode is SAFE — a misclassified object leaves
/// the placeholder for retry, rather than persisting a component's tier/summary as
/// the whole repo (which `needs_classification` would then skip).
#[derive(Debug, Default, serde::Deserialize)]
struct RepoClassWire {
    // `tier` has no serde default by design — a reply without one is unparseable.
    // `Default::default()` yields "" which tests treat as an unparseable tier; that
    // is acceptable because the struct's `Default` is only used in test constructors
    // via `..Default::default()` for the new optional fields.
    tier: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    summary: String,
    // `Option` so persistence can tell an OMITTED factual field (a partial reply —
    // keep the prior value) from an explicit empty array (clear the prior value).
    #[serde(default)]
    stack: Option<Vec<String>>,
    #[serde(default)]
    components: Option<Vec<Component>>,
    // `Option` so persistence can distinguish "absent" (preserve prior) from
    // "present but empty" (explicit clear). `#[serde(default)]` makes a missing
    // JSON key deserialize as None. (Finding 4)
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    domains: Option<Vec<String>>,
    // Transient surface-info fields: parsed so the agent is prompted to reason
    // about them, but not persisted to their own columns in v1 (may be folded
    // into domains / fed to the analyst later) — hence allow(dead_code).
    #[allow(dead_code)]
    #[serde(default)]
    exposed: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    consumes: Vec<String>,
}

/// Extract the per-repo classification from the agent's free-form reply, same
/// tolerance as `parse_curator_output`: scan every balanced object, take the LAST
/// that carries a `tier` and is NOT a component object (those carry a `name`).
/// `None` for a timed-out/malformed reply.
fn parse_repo_class(text: &str) -> Option<RepoClassWire> {
    json_objects(text)
        .into_iter()
        .rev()
        .filter_map(|obj| serde_json::from_str::<RepoClassWire>(obj).ok())
        .find(|w| w.name.is_none())
}

const CURATOR_SYSTEM_PROMPT: &str = "You are a read-only repository analyst. You \
may read code and configuration as deeply as you need, but you must never modify, \
create, or delete files, and never run mutating commands.";

const CURATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Read-only sandbox args for a `codex exec` curator run.
///
/// Codex `exec` no longer accepts `--ask-for-approval` (removed from the `exec`
/// subcommand in current CLIs — passing it makes codex exit at arg-parse with
/// `unexpected argument`, which silently stranded every analysis). The
/// non-interactive "never prompt" intent is now expressed via the
/// `approval_policy` config override; `exec` is non-interactive anyway, so with a
/// read-only sandbox it can neither prompt nor escalate.
///
/// `--skip-git-repo-check`: the cross-repo relation pass runs from the repos'
/// common-ancestor dir, which usually isn't itself a git repo, and `codex exec`
/// otherwise refuses to start outside one. Harmless for the per-repo pass.
fn codex_exec_read_only_args() -> Vec<String> {
    vec![
        "--sandbox".into(),
        "read-only".into(),
        "-c".into(),
        "approval_policy=\"never\"".into(),
        "--skip-git-repo-check".into(),
    ]
}

/// The shared definition of the two architectural tiers, embedded in both the
/// per-repo classification prompt and the cross-repo relations prompt so the
/// agent applies one consistent taxonomy.
const TIER_GUIDE: &str = "Tiers:\n\
- frontend: user-facing client — web SPA/MPA, mobile, desktop UI, static site.\n\
- backend: anything server-side — API gateways / BFFs / aggregators / edge \
services, REST/gRPC services, workers, batch jobs, databases-of-record, and the \
shared libraries / IDL that back them.";

/// Per-repo DEEP classification prompt. The agent runs with cwd AT this repo and
/// is told to read widely (subdirectories, monorepo packages) before emitting one
/// strict-JSON object: tier + summary + stack + components + category + domains +
/// exposed + consumes. Manifest signals (requires/provides) are injected as hints
/// so the agent confirms/augments rather than guessing cold.
fn build_repo_class_prompt(repo_name: &str, cwd: &std::path::Path) -> String {
    let manifest = crate::manifest::scan_repo(cwd);
    let manifest_hint = build_manifest_hint(&manifest);
    format!(
        "Analyze the repository at the current working directory (name: {repo_name}) \
DEEPLY and READ-ONLY. Do NOT stop at the top-level manifest: read the source \
layout, entry points, configs, and — if this is a monorepo — its packages/apps/\
services subdirectories, so your classification reflects what the code actually \
does.\n\n{TIER_GUIDE}\n\n{manifest_hint}\
Classify the repository into exactly one top-level tier. \
If the repo is a monorepo containing two or more deployable/publishable internal \
packages or services, list each as a component with its own tier; a single-purpose \
repo has no components.\n\nAs the LAST thing in your reply, output a single JSON \
object and nothing after it:\n\
{{\"tier\":\"frontend|backend\",\"summary\":\"<one line; name the key \
internal modules if it is a monorepo>\",\"stack\":[\"<language/framework tags>\"],\
\"components\":[{{\"name\":\"<package/service>\",\"path\":\"<relative path>\",\
\"tier\":\"frontend|backend\",\"summary\":\"<one line>\",\
\"deps\":[\"<sibling component name it depends on>\"]}}],\
\"category\":\"<role within its tier, e.g. gateway|biz|core|common|idl|support|app|sdk|web — best fit, free text>\",\
\"domains\":[\"<owned feature domains, e.g. orders|payments|auth>\"],\
\"exposed\":[\"<HTTP/gRPC routes, queues, or dbs this repo offers>\"],\
\"consumes\":[\"<other services/APIs this repo calls>\"]}}\n\
Rules: pick the single tier that best fits the repo as a whole. `components` is \
[] unless this is a monorepo with 2+ internal packages/services. `deps` lists only \
SIBLING components in THIS repo. Keep summaries to one line. `category` is \
free-text — use the examples as guidance, not a constraint. `domains` and `exposed`/\
`consumes` may be [] if not applicable."
    )
}

/// Format the manifest signals as a human-readable hint block for the prompt.
/// Returns an empty string when the manifest carries no signal (no provides/requires).
fn build_manifest_hint(manifest: &crate::manifest::ManifestInfo) -> String {
    let has_provides = !manifest.provides.is_empty();
    let has_requires = !manifest.requires.is_empty();
    if !has_provides && !has_requires {
        return String::new();
    }
    let mut hint = String::from("Manifest signals (use as starting hints — confirm or augment from the code):\n");
    if has_provides {
        hint.push_str(&format!("- This repo provides: {}\n", manifest.provides.join(", ")));
    }
    if has_requires {
        hint.push_str(&format!("- Known declared dependencies: {}\n", manifest.requires.join(", ")));
    }
    hint.push('\n');
    hint
}

/// Format the manifest edges already seeded for a repo as a structured hint block.
/// Returns an empty string when the repo has no stored agent relations with
/// source="manifest" (the typical case when no manifests were found).
fn build_manifest_edge_hint(relations_json: &str) -> String {
    let rels: Vec<crate::profile::AgentRelation> =
        serde_json::from_str(relations_json).unwrap_or_default();
    let manifest: Vec<_> = rels.iter().filter(|r| r.source == "manifest").collect();
    if manifest.is_empty() {
        return String::new();
    }
    let mut out = String::from("  manifest edges (high-confidence lib deps from on-disk manifests):\n");
    for r in manifest {
        out.push_str(&format!("    → to={} kind={} via={:?}\n", r.to, r.kind, r.via));
    }
    out
}

/// Build the workspace-wide `provides_name → repo_id` map, SKIPPING ambiguous
/// names (claimed by 2+ distinct repos). A first-wins pick on a duplicated
/// package/crate/artifact name (forks, copied services, common unscoped names)
/// would resolve to whichever repo iterated first — a concrete-but-arbitrary
/// target. A wrong hint is worse than none (the agent infers the real one), so
/// ambiguous names resolve to nothing. Shared by the deterministic seed pass and
/// the analyst prompt's hint builder so the two never disagree.
fn unambiguous_provider_map<'a, I>(repo_infos: I) -> std::collections::HashMap<String, i32>
where
    I: IntoIterator<Item = (i32, &'a crate::manifest::ManifestInfo)>,
{
    use std::collections::HashMap;
    let mut providers: HashMap<String, Vec<i32>> = HashMap::new();
    for (id, info) in repo_infos {
        for name in &info.provides {
            let ids = providers.entry(name.clone()).or_default();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    providers
        .into_iter()
        .filter_map(|(name, ids)| match ids.as_slice() {
            [only] => Some((name, *only)),
            _ => None,
        })
        .collect()
}

/// Cross-repo relations prompt: lists every classified repo (id/name/tier/category/
/// domains/path/summary) along with its already-seeded manifest edges, then asks
/// for STRICT JSON with relations+rationale and a repo_map_markdown doc.
///
/// Manifest edges are computed DIRECTLY from on-disk manifests (not from the
/// persisted relations column), so the hint is always non-empty on a first
/// analysis even before seed_manifest_relations has persisted anything. The
/// agent pass's output is still merged/persisted after it completes, so seeding
/// after the agent doesn't overwrite the manifest floor. (Finding 3)
fn build_curator_prompt(repos: &[(repo_ref::Model, repo_profile::Model)]) -> String {
    use std::collections::HashMap;

    // Build workspace-wide provides_name → repo_id map from on-disk manifests.
    let mut repo_infos: Vec<(&repo_ref::Model, crate::manifest::ManifestInfo)> = repos
        .iter()
        .map(|(r, _)| (r, crate::manifest::scan_repo(Path::new(&r.local_git_path))))
        .collect();
    // Same ambiguity filtering as seed_manifest_relations: the agent consumes
    // these hints before persistence, so an arbitrary first-wins edge to a
    // duplicated provider could steer it into persisting a wrong agent relation.
    let provides_map = unambiguous_provider_map(repo_infos.iter().map(|(r, info)| (r.id, info)));

    // For each repo, build a fresh manifest-edge hint from on-disk deps.
    let mut live_manifest_hints: HashMap<i32, String> = HashMap::new();
    for (r, info) in &mut repo_infos {
        let edges: Vec<_> = info
            .requires
            .iter()
            .filter_map(|req| {
                let target_id = *provides_map.get(req)?;
                if target_id == r.id { None } else { Some((target_id, req.as_str())) }
            })
            .collect();
        if !edges.is_empty() {
            let mut hint = String::from("  manifest edges (high-confidence lib deps from on-disk manifests):\n");
            for (to, via) in &edges {
                hint.push_str(&format!("    → to={} kind=lib via={:?}\n", to, via));
            }
            live_manifest_hints.insert(r.id, hint);
        }
    }

    let mut lines = String::new();
    for (r, p) in repos {
        let tier = if p.role.is_empty() { "unknown" } else { p.role.as_str() };
        let category = p.category.as_str();
        let domains: Vec<String> = serde_json::from_str(&p.domains).unwrap_or_default();
        let domains_str = if domains.is_empty() {
            String::new()
        } else {
            format!(" domains=[{}]", domains.join(", "))
        };
        // Use the live on-disk manifest hint (always available, even on first
        // analysis). Fall back to the persisted hint as a secondary source only when
        // the live scan yielded nothing (e.g. checkout temporarily unreadable).
        let manifest_hint = {
            let live = live_manifest_hints.get(&r.id).cloned().unwrap_or_default();
            if live.is_empty() {
                build_manifest_edge_hint(&p.relations)
            } else {
                live
            }
        };
        lines.push_str(&format!(
            "- id={} name={:?} tier={} category={:?}{} path={:?}\n  summary: {}\n{}",
            r.id, r.name, tier, category, domains_str, r.local_git_path, p.summary, manifest_hint
        ));
    }
    format!(
        "Map how these repositories in one workspace depend on each other. \
Dependencies come in two flavors and BOTH matter equally:\n\
- CODE (declared): one repo imports another's package/module — kind `lib`.\n\
- BUSINESS / RUNTIME: one repo CONSUMES another's running surface — calls its \
HTTP/REST or gRPC API, publishes to or consumes a queue/topic it owns, or reads a \
database/infra it owns — kinds `http`/`grpc`/`queue`/`infra`. A runtime dependency \
is REAL even when the two repos share NO code: e.g. a frontend that calls a \
backend's REST endpoint depends on that backend, with no package dependency at \
all.\n\
To find the runtime/business edges (the easy-to-miss ones), CORRELATE a consumer's \
OUTBOUND calls — fetch / HTTP client usage, base URLs or service hosts in config / \
env, gRPC stubs, queue publishes — against another repo's EXPOSED surface — its \
routes / handlers, gRPC service definitions, queue consumers, DB schemas. The \
consumer is `from`, the producer it talks to is `to`. Use the tier flow as a prior: \
a frontend almost always depends on the backend that serves its data — assert such \
an edge whenever the code supports it, even with no shared package.\n\n\
{TIER_GUIDE}\n\nRepositories (manifest edges are pre-seeded high-confidence hints — \
confirm, augment, or add runtime edges from code):\n{lines}\n\
Read each repo's code and config at its path (READ-ONLY — change nothing). Then, as \
the LAST thing in your reply, output a single JSON object and nothing after it:\n\
{{\"relations\":[{{\"from\":<id>,\"to\":<id>,\"kind\":\"http|grpc|queue|infra|lib\",\
\"via\":\"<short evidence>\",\"confidence\":<0-100>,\
\"rationale\":\"<one sentence: why this dependency exists>\"}}],\
\"repo_map_markdown\":\"<markdown string>\"}}\n\
The `repo_map_markdown` value MUST be a RedIM-style workspace map with these four \
sections:\n\
1. **Inventory** — grouped by tier→category; one bullet per repo with its purpose \
and owned domains.\n\
2. **Dependency layers** — bullet list of directed dependency flows \
(frontend→backend, backend→infra, etc.) with a sentence of rationale each.\n\
3. **Sibling/lateral edges** — same-tier dependencies with rationale.\n\
4. **Domain-ownership index** — alphabetical: `domain: [repo names]`.\n\
Rules: `from` and `to` MUST be ids from the list above and must differ. Use kind \
`lib` for a declared package/module dependency, the runtime kinds otherwise. \
Include a relation when you have concrete evidence — for a runtime edge that means a \
matching endpoint / topic / host / contract across the two repos. `via` is a short \
label (e.g. \"POST /orders\", \"orders-topic\", \"shared postgres\", \
\"@acme/api-client\"). `rationale` is one sentence explaining why the dependency \
exists. If you find no cross-repo dependencies, output `{{\"relations\":[], \
\"repo_map_markdown\":\"<still include the inventory and domain index>\"}}`. \
`repo_map_markdown` must be a JSON string (escape newlines as \\\\n, quotes as \\\\\")."
    )
}

/// A streaming chunk from a curator agent run, forwarded to the caller's sink so
/// the analysis process can stream into the UI.
enum AnalysisEvent<'a> {
    /// A piece of assistant text — a token delta on the app-server transport, or
    /// the full message at completion on the exec transport.
    Delta(&'a str),
}

/// Accumulates a streamed agent turn's assistant text + terminal signals. The
/// "what counts as a clean vs failed turn" judgment lives — and is unit-tested —
/// HERE, once, instead of being re-hand-rolled per transport. That duplication was
/// the source of the repeated transport edge-case bugs (errored turn, mid-stream
/// EOF, timeout, empty output); both the app-server and exec runners now feed this
/// one collector so the two paths can't drift.
#[derive(Default)]
struct TurnCollector {
    texts: Vec<String>,
    deltas: String,
    turn_failed: bool,
    saw_turn_end: bool,
    saw_delta: bool,
}

impl TurnCollector {
    /// Feed one parsed event, forwarding any streamed text to `sink`. Returns true
    /// once the turn END is seen (the caller should stop reading).
    fn push<F: FnMut(AnalysisEvent)>(
        &mut self,
        ev: crate::lead_chat::proto::ChatEvent,
        sink: &mut F,
    ) -> bool {
        use crate::lead_chat::proto::ChatEvent;
        match ev {
            ChatEvent::TextDelta { text } => {
                sink(AnalysisEvent::Delta(&text));
                self.deltas.push_str(&text);
                self.saw_delta = true;
                false
            }
            ChatEvent::Assistant { texts, .. } => {
                // Forward the full message to the sink ONLY if it wasn't already
                // streamed as deltas. claude exec (`--include-partial-messages`)
                // sends BOTH the token deltas AND a final `assistant` message with
                // the same text — forwarding both would double it in the transcript.
                // codex exec / opencode send only the full message (no deltas), and
                // the app-server sends only deltas, so each streams exactly once.
                if !self.saw_delta {
                    for s in &texts {
                        sink(AnalysisEvent::Delta(s));
                    }
                }
                self.texts.extend(texts);
                false
            }
            ChatEvent::TurnEnd { is_error, .. } => {
                self.saw_turn_end = true;
                self.turn_failed = is_error;
                true
            }
            _ => false,
        }
    }

    /// The accumulated assistant text — full messages if any arrived, else the
    /// streamed deltas (the app-server delivers agent text only as deltas).
    fn text(&self) -> String {
        if self.texts.is_empty() {
            self.deltas.clone()
        } else {
            self.texts.join("\n")
        }
    }

    /// Decide the turn result: `Ok(text)` ONLY for a clean, non-error turn with
    /// usable text. `reached_end` = the stream terminated on its own (not a
    /// timeout). `require_turn_end` = treat a natural EOF WITHOUT a TurnEnd as a
    /// failure: true for the app-server (it always emits a TurnEnd, so its absence
    /// means a mid-turn disconnect), false for exec (opencode emits no TurnEnd, so a
    /// clean EOF is its normal completion).
    fn outcome(&self, label: &str, reached_end: bool, require_turn_end: bool) -> Result<String> {
        let text = self.text();
        let ended = if require_turn_end { self.saw_turn_end } else { reached_end };
        if self.turn_failed || !ended || text.trim().is_empty() {
            anyhow::bail!(
                "{label} turn did not complete cleanly (error={}, ended={ended})",
                self.turn_failed
            );
        }
        Ok(text)
    }
}

/// Run the resolved agent once over `cwd`, read-only, streaming text chunks to
/// `on_event` and returning the final assistant text. codex prefers the
/// app-server transport (token-by-token deltas, the same transport the chat
/// engine uses); on ANY app-server failure — or for a non-codex tool, or when
/// app-server is disabled — it falls back to `codex exec`/claude with corrected
/// read-only args. Best-effort, bounded by `CURATOR_TIMEOUT`; never writes files.
async fn run_streaming_agent<F: FnMut(AnalysisEvent)>(
    tool: &str,
    cwd: &Path,
    prompt: &str,
    on_event: &mut F,
) -> Result<String> {
    if tool == "codex" && crate::adapters::codex_prefers_appserver() {
        match run_codex_appserver(cwd, prompt, on_event).await {
            Ok(text) => return Ok(text),
            Err(e) => {
                eprintln!("[weft][curator] app-server unavailable ({e}) — exec fallback");
            }
        }
    }
    run_exec(tool, cwd, prompt, on_event).await
}

/// codex app-server transport: spawn a per-run app-server in `cwd` with read-only
/// config overrides, start one thread + turn, and stream the agent's text deltas
/// to `on_event` while accumulating the final reply. Auto-declines any approval
/// ask so the turn can't hang. Mirrors the engine's app-server path but is
/// ephemeral — no weft thread row, no persistence.
async fn run_codex_appserver<F: FnMut(AnalysisEvent)>(
    cwd: &Path,
    prompt: &str,
    on_event: &mut F,
) -> Result<String> {
    use crate::codex_app_server::{codex_approval_reply, Client, ThreadMsg};
    // Deliberately do NOT pre-trust `cwd`: this curator scan runs in the user's
    // CANONICAL repo (not a Weft-managed worktree), so silently writing it into
    // codex's trusted-folders config — permanently bypassing codex's own folder
    // trust prompt for later sessions — would be wrong. The exec path skips
    // `adapter.prepare` for the same reason. The scan is read-only + best-effort:
    // an untrusted repo simply yields nothing (→ a retryable failed state), and
    // the graph stays intact, rather than us escalating trust behind the user's back.
    let program = crate::tool_command::command_for("codex");
    // app-server has no per-turn CLI flags; the read-only + never-prompt policy is
    // applied as config overrides at spawn (the exec args' equivalent).
    let read_only = [
        "-c".to_string(),
        "sandbox_mode=\"read-only\"".to_string(),
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
    ];
    let client = Client::connect_session(&program, &read_only, cwd).await?;
    let cwd_s = cwd.to_string_lossy().into_owned();
    // codex has no thread/start system-prompt field, so prepend it to the turn
    // (exactly as the exec adapter and the engine's first-turn text do).
    let full = format!("{CURATOR_SYSTEM_PROMPT}\n\n{prompt}");
    // Run the whole turn inside one scope whose result we capture, so
    // `client.shutdown()` ALWAYS runs once `connect_session` succeeded — even when
    // thread/start or turn/start fails. A bare `?` here would return early and
    // leak the spawned `codex app-server` child + its read-loop.
    let outcome = async {
        let thread = client.start_thread(&cwd_s).await?;
        let mut rx = client.subscribe(&thread).await;
        let turn = client.start_turn(&thread, &full).await?;
        client.set_active_turn(&thread, &turn).await;
        let mut collector = TurnCollector::default();
        let collect = async {
            while let Some(msg) = rx.recv().await {
                match msg {
                    ThreadMsg::Event(ev) => {
                        if collector.push(ev, on_event) {
                            break;
                        }
                    }
                    ThreadMsg::Approval { id, method, .. } => {
                        // Decline every ask immediately with the SHAPE its kind needs:
                        // a permission ask requires `{permissions:{}}` (a `{decision}`
                        // reply no-ops it, so the turn would hang until CURATOR_TIMEOUT).
                        // Read-only curator → always deny.
                        let _ = client
                            .reply_result(
                                &id,
                                codex_approval_reply(method.contains("permissions"), false, None),
                            )
                            .await;
                    }
                    _ => {}
                }
            }
        };
        let _ = tokio::time::timeout(CURATOR_TIMEOUT, collect).await;
        // require_turn_end = true: the app-server always emits a TurnEnd, so a
        // timeout OR a mid-turn channel close (EOF without one) is a failure → Err,
        // so `run_streaming_agent` falls back to exec instead of returning partial
        // output as success.
        collector.outcome("codex app-server", true, true)
    }
    .await;
    // Reap the child here (not just kill_on_drop): the curator opens a fresh
    // per-session app-server for EVERY repo + the relation pass, so over a large
    // workspace plain `shutdown` would leave a trail of unreaped children.
    client.shutdown_and_reap().await;
    outcome
}

/// Exec transport (codex/claude/opencode): spawn a one-shot per-turn child,
/// read-only, feeding `prompt` and streaming text to `on_event`. Reuses the
/// per-tool adapter for argv + line parsing (claude reads stdin as a stream-json
/// envelope; per-turn tools carry the message on argv). Bounded by
/// `CURATOR_TIMEOUT`: a timeout or early exit returns whatever was collected.
async fn run_exec<F: FnMut(AnalysisEvent)>(
    tool: &str,
    cwd: &Path,
    prompt: &str,
    on_event: &mut F,
) -> Result<String> {
    use crate::adapters::{adapter_for, AdapterContext};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let adapter = adapter_for(tool).ok_or_else(|| anyhow::anyhow!("unknown tool {tool}"))?;
    // NB: deliberately do NOT call adapter.prepare(cwd) here. prepare() writes the
    // tool's folder-trust config, and this one-shot runs in the user's CANONICAL
    // repo — silently trusting it (bypassing the tool's own onboarding) would be
    // wrong. The analysis is read-only and best-effort; if the tool needs trust
    // and doesn't have it, the run simply yields nothing and the graph is intact.
    //
    // Enforce read-only at the TOOL level, not just via the prompt: this runs in
    // the user's real checkout, so a model that decides to edit must be stopped by
    // the sandbox. Codex defaults to workspace-write (can edit) — pin read-only;
    // claude runs in plan mode (no edits). opencode has no portable flag here, so
    // it falls back to the prompt's read-only instruction (best-effort).
    let read_only: Vec<String> = match tool {
        "codex" => codex_exec_read_only_args(),
        "claude" => vec!["--permission-mode".into(), "plan".into()],
        _ => vec![],
    };
    let ctx = AdapterContext {
        cwd,
        system_prompt: CURATOR_SYSTEM_PROMPT,
        extra_args: &read_only,
        native_id: None,
        message: prompt,
        slash_commands: &[],
    };
    let (program, argv) = adapter.build_argv(&ctx)?;
    let command = crate::tool_command::effective(None, &program);
    let mut child = tokio::process::Command::new(&command)
        .args(&argv)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // claude reads the message from stdin as a stream-json user envelope
        // (matches engine::write_user); per-turn tools already have it on argv.
        // Closing stdin (drop) signals EOF so the one-shot turn runs to the end.
        if !adapter.per_turn() {
            let env = serde_json::json!({
                "type": "user",
                "message": { "role": "user", "content": [{ "type": "text", "text": prompt }] }
            });
            let _ = stdin.write_all(format!("{env}\n").as_bytes()).await;
            let _ = stdin.flush().await;
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("curator child stdout not piped"))?;
    let mut reader = BufReader::new(stdout).lines();
    let mut collector = TurnCollector::default();
    let collect = async {
        while let Some(line) = reader.next_line().await? {
            if collector.push(adapter.parse_line(&line), on_event) {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    // `completed` = the stream ended cleanly on its own (a TurnEnd break, or EOF)
    // rather than the timeout firing OR a reader error. `Ok(Ok(()))` is the only
    // clean case: `Err(_)` is the timeout, `Ok(Err(_))` is a mid-stream read error
    // (non-UTF-8 / pipe I/O) — both must count as not-cleanly-ended.
    let completed = matches!(tokio::time::timeout(CURATOR_TIMEOUT, collect).await, Ok(Ok(())));
    // Close stdout so a child blocked on a full pipe unblocks, then SIGKILL and
    // reap (kill().await waits for exit — avoids a zombie on timeout). On a clean
    // EOF the child has already exited and this is a no-op.
    drop(reader);
    let _ = child.kill().await;
    // A timeout, an arg-parse / early exit (empty text), or an errored turn must
    // surface as `Err` so a failed reprofile shows the failed/retryable state
    // instead of being masked as `done`. codex (turn.completed/turn.failed) and
    // claude (result) emit a terminal event, so for them a stdout close BEFORE one
    // is a crash/early-exit → require it (like the app-server path). opencode's exec
    // stream emits none, so ONLY it may complete on a clean EOF.
    let require_turn_end = tool != "opencode";
    collector.outcome(tool, completed, require_turn_end)
}

/// Group the agent's flat relations by producer (`from`), keep only those whose
/// `from`/`to` are repos in this workspace and differ, then persist each repo's
/// relations — clearing repos the agent didn't mention so a re-run replaces
/// rather than accretes.
async fn persist_relations(
    db: &Db,
    profiled: &[(repo_ref::Model, repo_profile::Model)],
    relations: &[CuratorRelation],
) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    let ids: HashSet<i32> = profiled.iter().map(|(r, _)| r.id).collect();
    let mut by_from: HashMap<i32, Vec<crate::profile::AgentRelation>> = HashMap::new();
    for rel in relations {
        // Drop a malformed row (missing endpoint, self-edge, or unknown repo)
        // without discarding the rest of the pass.
        let (Some(from), Some(to)) = (rel.from, rel.to) else {
            continue;
        };
        if from == to || !ids.contains(&from) || !ids.contains(&to) {
            continue;
        }
        // Normalize the agent's kind to the canonical lowercase set; drop anything
        // unrecognized so the graph can't carry an edge calibrate_edges can't match.
        let Some(kind) = crate::profile::normalize_relation_kind(&rel.kind) else {
            continue;
        };
        by_from
            .entry(from)
            .or_default()
            .push(crate::profile::AgentRelation {
                to,
                kind,
                via: rel.via.clone(),
                confidence: rel.confidence,
                source: "agent".to_string(),
                rejected: false,
                rationale: rel.rationale.clone(),
            });
    }
    for (r, _) in profiled {
        let fresh = by_from.remove(&r.id).unwrap_or_default();
        // Re-read the CURRENT relations (not the pre-run snapshot): the agent run
        // can take up to ~180s, and a human may have calibrated an edge meanwhile.
        // Reloading here preserves those user pins/tombstones across the merge.
        let existing: Vec<crate::profile::AgentRelation> = repo::get_repo_profile(db, r.id)
            .await?
            .map(|p| serde_json::from_str(&p.relations).unwrap_or_default())
            .unwrap_or_default();
        let merged = crate::profile::merge_relations(&existing, &[], &fresh);
        let json = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into());
        repo::set_repo_relations(db, r.id, &json).await?;
    }
    Ok(())
}

/// Workspaces whose one-shot legacy backfill has already been attempted this
/// session, so a failed/timed-out migration (which leaves the legacy tier in
/// place) doesn't re-queue an agent pass on every `repo_graph` read.
fn backfilled() -> &'static std::sync::Mutex<std::collections::HashSet<i32>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i32>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Claim the one-shot legacy backfill for a workspace: returns `true` exactly
/// once per workspace per process. The caller schedules a migration pass only on
/// that first claim, so an unavailable/slow analyzer can't cause a retry storm.
/// The user can still trigger a fresh pass manually via Analyze deps.
fn try_claim_backfill(workspace_id: i32) -> bool {
    backfilled()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(workspace_id)
}

/// Nudge any open repo map to reload so background-inferred classifications and
/// edges appear without a manual refresh.
fn emit_graph_updated(workspace_id: i32) {
    if let Some(app) = crate::APP_HANDLE.get() {
        use tauri::Emitter;
        let _ = app.emit("repo-graph-updated", workspace_id);
    }
}

// ─────────────────────── per-repo analysis run state ───────────────────────
//
// An honest, in-memory lifecycle for each repo's classification pass, so the UI
// can tell "actively running" from "failed" from "never analyzed" — instead of
// the old derived-only "analyzing" placeholder that could silently strand a repo
// forever when a pass failed. Lives only for the process; on restart a repo with
// no canonical tier is simply re-analyzed.

#[derive(Clone)]
struct RunInfo {
    /// "running" | "failed". Absence in the map == "idle".
    /// Error details are now persisted to the DB via set_analysis_state.
    phase: &'static str,
}

fn run_states() -> &'static std::sync::Mutex<std::collections::HashMap<i32, RunInfo>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, RunInfo>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn run_lock() -> std::sync::MutexGuard<'static, std::collections::HashMap<i32, RunInfo>> {
    run_states().lock().unwrap_or_else(|e| e.into_inner())
}

/// Mark a repo's analysis as running. Returns `false` if it was ALREADY running,
/// so a manual reprofile racing the background pass can't double-spawn the agent.
fn run_begin(id: i32) -> bool {
    let mut g = run_lock();
    if matches!(g.get(&id), Some(r) if r.phase == "running") {
        return false;
    }
    g.insert(id, RunInfo { phase: "running" });
    true
}

/// Analysis finished successfully: the persisted canonical tier now drives the
/// `analyzed` state, so the transient run entry is dropped (→ "idle").
fn run_finish_ok(id: i32) {
    run_lock().remove(&id);
}

/// Analysis failed/timed out: mark the in-memory phase as "failed" so
/// analyze_workspace skips auto re-runs until a forced (user-initiated) pass.
/// Error details are persisted to the DB via set_analysis_state.
fn run_finish_err(id: i32, _error: String) {
    run_lock().insert(id, RunInfo { phase: "failed" });
}

fn run_phase(id: i32) -> &'static str {
    run_lock().get(&id).map(|r| r.phase).unwrap_or("idle")
}

/// Forget a repo's FAILED run-state — e.g. after re-adding a live checkout for a
/// repo that failed because its old checkout was gone, so the next pass classifies
/// the fresh checkout instead of the anti-storm skip suppressing it. No-op unless
/// the repo is `failed`, so an in-flight `running` run is never disturbed. (A
/// user-initiated "Analyze deps" retries failures via the pass's `force` flag, not
/// by clearing state up front — clearing-then-hoping-the-pass-runs-it was fragile.)
pub fn clear_failure(repo_id: i32) {
    let mut g = run_lock();
    if matches!(g.get(&repo_id), Some(r) if r.phase == "failed") {
        g.remove(&repo_id);
    }
}

/// Drop a repo's run-state entirely, whatever its phase — called when the repo is
/// DELETED, so the process-global registry doesn't keep a stale `failed`/`running`
/// entry for a repo that no longer exists. (Unlike `clear_failure`, this also drops
/// a `running` entry; a background pass racing the delete finishes harmlessly via
/// its deleted-repo guards.)
pub fn run_forget(repo_id: i32) {
    run_lock().remove(&repo_id);
}

/// The success criterion for an analysis run: the repo carries a usable (canonical)
/// tier classification FOR THE TREE THIS RUN ANALYZED (`analyzed_commit`). A run
/// persists `profiled_commit = analyzed_commit` only when it actually (re)classifies
/// (a canonical tier, or a user-pinned tier kept on a real upsert); an unparseable
/// or no-op reply leaves the PRIOR `profiled_commit`.
///
/// So requiring `profiled_commit == analyzed_commit` distinguishes the two cases a
/// bare tier-check conflates:
/// - HEAD unchanged + garbage reply → the prior canonical tier is still for THIS
///   tree (`profiled_commit` already == HEAD) → success, prior result preserved.
/// - HEAD moved + failed/garbage reply → the prior tier is for an OLD commit
///   (`profiled_commit` != the new HEAD) → FAILURE (retryable), instead of showing
///   the stale tier as a fresh `done` and re-running forever without converging.
async fn classified_for(db: &Db, repo_id: i32, analyzed_commit: &str) -> bool {
    repo::get_repo_profile(db, repo_id)
        .await
        .ok()
        .flatten()
        .map(|p| profile::normalize_tier(&p.role).is_some() && p.profiled_commit == analyzed_commit)
        .unwrap_or(false)
}

#[cfg(test)]
fn run_state_clear_all_for_test() {
    run_lock().clear();
}

/// Serialize the tests that mutate the process-global run-state registry. cargo
/// runs tests in parallel, and they share both the registry AND repo id 1 (each
/// fresh in-memory DB starts its autoincrement at 1), so without this a
/// `clear_all`/`run_finish_err` in one test could race another's assertion. Every
/// run-state test takes this lock first. Poison-tolerant (a panicking test must not
/// wedge the rest).
#[cfg(test)]
fn test_run_state_guard() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

/// Emit one analysis lifecycle/stream event for a repo so an open detail panel
/// can render the live transcript + status. No-op outside a running app.
fn emit_repo_analysis(
    workspace_id: i32,
    repo_id: i32,
    phase: &str,
    text: Option<&str>,
    error: Option<&str>,
) {
    if let Some(app) = crate::APP_HANDLE.get() {
        use tauri::Emitter;
        let _ = app.emit(
            "repo-analysis",
            serde_json::json!({
                "workspaceId": workspace_id,
                "repoId": repo_id,
                "phase": phase,
                "text": text,
                "error": error,
            }),
        );
    }
}

/// Persist one repo's deep-pass classification: tier + stack + summary +
/// components. Factual fields (stack/components) always refresh; the opinion
/// fields (tier/summary) are preserved when the user has pinned them
/// (source = "user"). Component tiers are normalized to the canonical set.
async fn persist_repo_class(
    db: &Db,
    repo: &repo_ref::Model,
    wire: RepoClassWire,
    commit: &str,
) -> Result<()> {
    // The per-repo agent pass can run for minutes; if the repo was removed
    // meanwhile, the captured `repo_ref` is stale. Skip rather than recreate an
    // orphaned profile row (repo_profile has no enforced foreign key).
    if repo::get_repo(db, repo.id).await?.is_none() {
        return Ok(());
    }
    let prior = repo::get_repo_profile(db, repo.id).await?;
    let prior_source = prior.as_ref().map(|p| p.source.as_str()).unwrap_or("agent");
    let summary_owned = owns_summary(prior_source);
    let tier_owned = owns_tier(prior_source);

    // Factual fields: an OMITTED field (None — a partial reply) keeps the prior
    // value so it isn't erased; an explicit array (Some, even empty) is persisted,
    // so a real later reply CAN clear stale facts. Nameless components are dropped
    // (a malformed sub-object) while the rest of the list is kept; their tiers are
    // normalized.
    let comps_json = match wire.components {
        Some(cs) => {
            let filtered: Vec<Component> = cs
                .into_iter()
                .filter(|c| !c.name.trim().is_empty())
                .map(|mut c| {
                    c.tier = profile::normalize_tier(&c.tier).unwrap_or_default();
                    c
                })
                .collect();
            serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".into())
        }
        None => prior.as_ref().map(|p| p.components.clone()).unwrap_or_else(|| "[]".into()),
    };
    let stack_json = match &wire.stack {
        Some(s) => json_strs(s),
        None => prior.as_ref().map(|p| p.stack.clone()).unwrap_or_else(|| "[]".into()),
    };

    let agent_tier = profile::normalize_tier(&wire.tier);
    // A truncated reply may omit the summary; fall back to the prior summary
    // rather than blanking an existing one (and then skipping the repo forever).
    let agent_summary = if wire.summary.trim().is_empty() {
        prior.as_ref().map(|p| p.summary.clone()).unwrap_or_default()
    } else {
        wire.summary
    };

    // Tier: a user-pinned valid tier is kept (still owned); a user-pinned
    // legacy/empty tier adopts the agent's valid tier (no longer owned — it's the
    // agent's value now). When the user doesn't own the tier, require a valid
    // agent tier — a missing/non-canonical one leaves the prior profile (or
    // placeholder) intact rather than persisting an analyzed-but-unclassified row.
    let (tier, tier_still_owned) = match prior.as_ref().filter(|_| tier_owned) {
        Some(p) => match profile::normalize_tier(&p.role) {
            Some(valid) => (valid, true),
            None => match agent_tier.clone() {
                Some(a) => (a, false),
                None => (p.role.clone(), true), // legacy kept, still pending migration
            },
        },
        None => {
            let Some(tier) = agent_tier else {
                return Ok(());
            };
            (tier, false)
        }
    };

    // Summary: keep a non-empty user-pinned summary (still owned); otherwise take
    // the agent's (no longer owned — covers an unowned field and an owned-but-blank
    // placeholder). `agent_summary` already falls back to the prior value when the
    // agent omitted it.
    let (summary, summary_still_owned) = match prior.as_ref().filter(|_| summary_owned) {
        Some(p) if !p.summary.trim().is_empty() => (p.summary.clone(), true),
        _ => (agent_summary, false),
    };

    // A classification with no usable summary (the agent omitted it and there was
    // no prior/user summary to fall back to) is incomplete: like a missing tier,
    // leave the placeholder/prior intact rather than persisting a canonical tier +
    // commit that `needs_classification` would then skip, stranding a blank node.
    // (A non-empty value here is either user-pinned or agent-provided, so this only
    // catches truly empty results.)
    if summary.trim().is_empty() {
        return Ok(());
    }

    // Persist only the ownership the user STILL holds: a field whose value was
    // taken from the agent is no longer user-pinned, so a later pass can refresh it.
    let source = combine_source(summary_still_owned, tier_still_owned);

    repo::upsert_repo_profile(
        db, repo.id, &tier, &stack_json, &summary, &comps_json, source, &commit,
    )
    .await?;

    // Persist category + domains when the agent provided them (Some = present,
    // even empty — clears prior; None = absent — preserves prior). (Finding 4)
    let has_category = wire.category.is_some();
    let has_domains = wire.domains.is_some();
    if has_category || has_domains {
        let prior_category = prior.as_ref().map(|p| p.category.as_str()).unwrap_or("");
        let prior_domains_json = prior.as_ref().map(|p| p.domains.as_str()).unwrap_or("[]");
        let final_category = match wire.category {
            Some(ref c) => c.trim().to_string(),
            None => prior_category.to_string(),
        };
        let final_domains_json = match wire.domains {
            Some(ref d) => json_strs(d),
            None => prior_domains_json.to_string(),
        };
        let _ = repo::set_repo_category_domains(db, repo.id, &final_category, &final_domains_json).await;
    }
    Ok(())
}

/// Deep, read-only per-repo pass: run the agent with cwd AT the repo, STREAM its
/// process into the repo's detail panel, parse its classification, and persist it.
/// Tracks an honest run-state (running → done/failed) so a failed pass surfaces an
/// error + manual retry instead of an eternal "analyzing" placeholder. Best-effort
/// — a timed-out/unparseable reply leaves the prior profile (or placeholder)
/// intact. No-op if the checkout is gone, or if this repo is already analyzing.
pub async fn profile_repo_agent(db: &Db, repo: &repo_ref::Model) -> Result<()> {
    let cwd = Path::new(&repo.local_git_path);
    if !cwd.exists() {
        // The checkout is gone (e.g. a retry after it was moved/deleted). Don't
        // silently return Ok — that would drop a previously-failed repo to idle and
        // lose its retryable error. Mark a visible, retryable failure instead.
        // (Background passes pre-filter missing checkouts, so this only fires on an
        // explicit reprofile_repo retry.) Store a stable CODE, not an English
        // sentence: the agent/transport errors are inherently dynamic English
        // diagnostics, but THIS is our own known, user-facing case, so the UI
        // localizes it (see `analysisErrorCheckoutMissing` in the i18n files).
        const GONE: &str = "checkout-missing";
        run_finish_err(repo.id, GONE.into());
        let _ = repo::set_analysis_state(db, repo.id, "failed", Some(GONE)).await;
        emit_repo_analysis(repo.workspace_id, repo.id, "failed", None, Some(GONE));
        return Ok(());
    }
    // Dedupe: a manual reprofile racing the background pass must not double-spawn
    // the agent for the same repo.
    if !run_begin(repo.id) {
        return Ok(());
    }
    let (ws, rid) = (repo.workspace_id, repo.id);
    let _ = repo::set_analysis_state(db, rid, "running", None).await;
    emit_repo_analysis(ws, rid, "started", None, None);
    // Also refresh the graph so an UNSELECTED card flips to `running` immediately:
    // the per-repo `started` stream is only observed for the selected repo, and
    // `repo-graph-updated` otherwise fires only when the (minutes-long) run ends —
    // without this, a reprofiled-but-unselected card sits stale for the whole run.
    emit_graph_updated(ws);
    let tool = crate::tools::default_tool(db).await;
    let prompt = build_repo_class_prompt(&repo.name, cwd);
    // Capture HEAD BEFORE the (minutes-long) run so the stored `profiled_commit`
    // reflects the tree the agent actually classified. If the checkout advances
    // during the run, a later pass sees HEAD != profiled_commit and reclassifies,
    // instead of saving the new SHA against a stale classification.
    let head_before = git::head_commit(cwd).unwrap_or_default();
    let res = run_streaming_agent(&tool, cwd, &prompt, &mut |ev| match ev {
        AnalysisEvent::Delta(t) => emit_repo_analysis(ws, rid, "delta", Some(t), None),
    })
    .await;
    // Persistence runs inside the result so a persist/DB error still FINALIZES the
    // run-state (as failed, retryable) — never propagate via `?` and strand the
    // repo at a leaked "running" (the very stuck-state this whole change kills).
    let outcome: Result<()> = match res {
        Err(e) => Err(e),
        Ok(text) => {
            // Best-effort persist; a genuine DB error is itself a failure. Note
            // persist_repo_class may legitimately write NOTHING — an unparseable
            // reply, OR a parseable-but-unusable one (non-canonical tier / blank
            // summary, which it intentionally drops) — so success can't be inferred
            // from its `Ok`.
            let persisted = match parse_repo_class(&text) {
                Some(wire) => persist_repo_class(db, repo, wire, &head_before).await,
                None => Ok(()),
            };
            match persisted {
                Err(e) => Err(e),
                Ok(()) => {
                    // Success iff the repo carries a usable canonical tier FOR THE
                    // TREE THIS RUN ANALYZED (head_before) — freshly written this run,
                    // or a prior one still valid because HEAD hasn't moved. An
                    // unparseable / no-op reply leaves the prior commit, so a failed
                    // refresh of a MOVED-HEAD repo is a real failure (visible +
                    // retryable), not the stale tier shown as a fresh `done`.
                    if classified_for(db, repo.id, &head_before).await {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("analyzer returned no usable classification"))
                    }
                }
            }
        }
    };
    match outcome {
        Ok(()) => {
            run_finish_ok(rid);
            let _ = repo::set_analysis_state(db, rid, "idle", None).await;
            emit_repo_analysis(ws, rid, "done", None, None);
        }
        Err(e) => {
            // Surface the failure (visible + retryable) instead of silently leaving
            // the placeholder to read as a perpetual "analyzing".
            let msg = e.to_string();
            run_finish_err(rid, msg.clone());
            let _ = repo::set_analysis_state(db, rid, "failed", Some(&msg)).await;
            emit_repo_analysis(ws, rid, "failed", None, Some(&msg));
        }
    }
    Ok(())
}

/// Run the agent curator over a workspace (ARCHITECTURE §4.9). Two stages, both
/// read-only and best-effort:
///   1. a DEEP per-repo pass classifies each repo's tier + summary + components,
///      emitting a graph refresh after each so nodes light up progressively;
///   2. a cross-repo pass (only with ≥2 repos) infers the relations between them.
/// Any single failure leaves that repo's prior state intact.
pub async fn analyze_workspace(db: &Db, workspace_id: i32, force: bool) -> Result<()> {
    let repos = repo::list_repos(db, workspace_id).await?;
    // Only analyze repos whose checkout still exists on disk.
    let existing: Vec<repo_ref::Model> = repos
        .into_iter()
        .filter(|r| Path::new(&r.local_git_path).exists())
        .collect();
    if existing.is_empty() {
        return Ok(());
    }

    // Stage 1: deep per-repo classification, progressive. Skip a repo that's
    // already classified (canonical tier) AND unchanged since (profiled_commit ==
    // HEAD) — re-running the minutes-long agent for every unchanged checkout when
    // a single repo is added would stall a large workspace for a long time.
    for r in &existing {
        // Honor the persisted failed state: after a restart the in-memory run-state
        // map is empty, so run_phase() returns "idle" for a repo whose
        // analysis_state="failed" was persisted to the DB. Consulting both sources
        // ensures a restart doesn't auto-retry a previously-failed repo — only a
        // forced (user-initiated) pass does. (Finding 1)
        let in_mem_failed = run_phase(r.id) == "failed";
        let db_failed = repo::get_repo_profile(db, r.id)
            .await
            .ok()
            .flatten()
            .map(|p| p.analysis_state.as_str() == "failed")
            .unwrap_or(false);
        let failed = in_mem_failed || db_failed;
        // Don't probe HEAD for a failed repo — its retry is gated on `force`, not on
        // whether it changed (see should_analyze).
        let needs = !failed && needs_classification(db, r).await;
        if !should_analyze(failed, force, needs) {
            continue;
        }
        let _ = profile_repo_agent(db, r).await;
        emit_graph_updated(workspace_id);
    }

    // Stage 2: cross-repo relations — needs at least two repos to relate.
    analyze_relations(db, workspace_id).await
}

/// Whether stage-1 should (re)classify a repo this pass. A FAILED repo is retried
/// only on a `force`d (user-initiated) pass — never on an auto pass, which runs on
/// every graph read (so a persistently-failing repo can't storm). A non-failed
/// repo runs iff it `needs` (re)classification (unclassified, or HEAD moved).
fn should_analyze(failed: bool, force: bool, needs: bool) -> bool {
    if failed {
        force
    } else {
        needs
    }
}

/// Whether a repo needs the deep classifier on an automatic pass: a repo with no
/// profile, no canonical tier yet, or whose HEAD moved since it was last profiled
/// (a stale classification). An unchanged, already-classified repo is skipped.
async fn needs_classification(db: &Db, repo: &repo_ref::Model) -> bool {
    let Ok(Some(p)) = repo::get_repo_profile(db, repo.id).await else {
        return true;
    };
    if profile::normalize_tier(&p.role).is_none() {
        return true; // unclassified / legacy → (re)classify
    }
    match git::head_commit(Path::new(&repo.local_git_path)) {
        Ok(head) => head != p.profiled_commit, // changed since last profiled
        Err(_) => false, // can't tell HEAD (not a git repo) → don't churn
    }
}

/// Whether a profile row is a COMPLETED deep classification rather than a bare
/// placeholder or a user-picked-tier-only node: it needs a canonical tier AND a
/// non-blank summary. `persist_repo_class` refuses to persist a classification
/// with an empty summary, so a non-blank summary marks a real deep pass (or a
/// user-pinned one — also real signal); a tier-only placeholder has a blank one.
/// Only such repos take part in the cross-repo relation pass.
fn is_fully_profiled(p: &repo_profile::Model) -> bool {
    profile::normalize_tier(&p.role).is_some() && !p.summary.trim().is_empty()
}

/// The deepest directory that contains every given path, or `None` when there is
/// no meaningful shared ancestor (different roots, or only the filesystem root).
/// Used as the relation pass's cwd so a sandboxed tool can read all repos.
fn common_ancestor(paths: &[&str]) -> Option<std::path::PathBuf> {
    let first = paths.first()?;
    let mut common: Vec<std::path::Component> = Path::new(first).components().collect();
    for p in &paths[1..] {
        let comps: Vec<std::path::Component> = Path::new(p).components().collect();
        let n = common.iter().zip(comps.iter()).take_while(|(a, b)| a == b).count();
        common.truncate(n);
    }
    // Require at least a root + one named segment (e.g. "/home"): bare "/" or no
    // common prefix is too broad / meaningless, so fall back to a single repo.
    if common.len() < 2 {
        return None;
    }
    let mut out = std::path::PathBuf::new();
    for c in &common {
        out.push(c.as_os_str());
    }
    Some(out)
}

/// Whether a directory is too broad to safely sandbox a read-only scan in — i.e.
/// it IS the user's home directory or an ancestor of it (`/`, `/home`, `/Users`,
/// `~`). Running there would let the tool read unrelated private files, so the
/// caller falls back to a single repo instead.
fn is_too_broad(dir: &Path) -> bool {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    match home {
        // `dir` is too broad when home is the same dir or nested under it.
        Some(h) => h.starts_with(dir),
        // Without a known home, be conservative: anything shallow is suspect.
        None => dir.components().count() < 4,
    }
}

/// Deterministic manifest edge pass: scan each existing-checkout repo's on-disk
/// manifests, build a workspace-wide `provides_name → repo_id` map, and for
/// each repo emit high-confidence `lib` edges to any workspace repo it depends
/// on. Persists via `merge_relations` (user > manifest > agent), so existing
/// user/agent relations are preserved.
///
/// Tolerant of missing checkouts, missing manifests, parse errors, and DB
/// errors — any single failure leaves that repo's prior relations intact.
pub async fn seed_manifest_relations(db: &Db, workspace_id: i32) -> Result<()> {
    use std::path::Path;

    let repos = repo::list_repos(db, workspace_id).await?;

    // Collect manifest info for every repo that has an existing checkout.
    let mut repo_manifests: Vec<(repo_ref::Model, crate::manifest::ManifestInfo)> = Vec::new();
    for r in &repos {
        if !Path::new(&r.local_git_path).exists() {
            continue;
        }
        let info = crate::manifest::scan_repo(Path::new(&r.local_git_path));
        repo_manifests.push((r.clone(), info));
    }

    // Workspace-wide provides_name → repo_id map, skipping ambiguous names (see
    // `unambiguous_provider_map`) so a duplicated package never seeds an arbitrary
    // first-wins lib edge.
    let name_to_repo =
        unambiguous_provider_map(repo_manifests.iter().map(|(r, info)| (r.id, info)));

    // For each repo, generate manifest lib edges for its requires that resolve to
    // a DIFFERENT workspace repo, then merge with the existing relations.
    for (r, info) in &repo_manifests {
        let mut fresh_manifest: Vec<crate::profile::AgentRelation> = Vec::new();
        for req in &info.requires {
            if let Some(&target_id) = name_to_repo.get(req) {
                if target_id == r.id {
                    continue; // skip self-dependency
                }
                fresh_manifest.push(crate::profile::AgentRelation {
                    to: target_id,
                    kind: "lib".into(),
                    via: req.clone(),
                    confidence: 100,
                    source: "manifest".into(),
                    rejected: false,
                    ..Default::default()
                });
            }
        }

        // Reload existing relations so user/agent edges accumulated since the
        // last pass are preserved through the merge.
        let existing: Vec<crate::profile::AgentRelation> = repo::get_repo_profile(db, r.id)
            .await
            .unwrap_or(None)
            .map(|p| serde_json::from_str(&p.relations).unwrap_or_default())
            .unwrap_or_default();

        // Keep agent relations from the existing set as "fresh_agent" so they
        // survive the merge. User relations are already handled by merge_relations.
        let existing_agent: Vec<crate::profile::AgentRelation> = existing
            .iter()
            .filter(|rel| rel.source == "agent" || rel.source.is_empty())
            .cloned()
            .collect();

        let merged =
            crate::profile::merge_relations(&existing, &fresh_manifest, &existing_agent);
        let json = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into());
        // Only persist if there is a profile row; skip silently if not.
        let _ = repo::set_repo_relations(db, r.id, &json).await;
    }

    Ok(())
}

/// Whether agent-generated markdown built from `profiled_ids` still reflects the
/// FINAL relation graph. The analyst prompt lists only the fully-profiled repos,
/// but `seed_manifest_relations` scans every checked-out repo and can add a `lib`
/// edge to/from an UNPROFILED provider the prompt never saw. When that happens the
/// markdown omits a node/edge now present in the graph, so it must not be
/// republished (the doc stays cleared until a later pass profiles everything).
///
/// `current_ids` is the set of repos that still exist in the workspace. An edge to
/// a DELETED repo is stale — the graph drops it by current node id (see `graph`),
/// so it must be ignored here too; otherwise a lingering user-pinned edge to a
/// removed repo would block republishing the map forever.
///
/// Returns false iff any non-rejected edge to a CURRENT repo is incident to a repo
/// outside the profiled set.
fn markdown_covers_graph(
    profiled_ids: &std::collections::HashSet<i32>,
    current_ids: &std::collections::HashSet<i32>,
    relations: &[(i32, Vec<AgentRelation>)],
) -> bool {
    for (owner, rels) in relations {
        let owner_profiled = profiled_ids.contains(owner);
        for e in rels {
            if e.rejected {
                continue;
            }
            // Stale edge to a deleted repo: not in the graph, so it can't make the
            // markdown incomplete.
            if !current_ids.contains(&e.to) {
                continue;
            }
            if !owner_profiled || !profiled_ids.contains(&e.to) {
                return false;
            }
        }
    }
    true
}

/// The cross-repo relations pass: reload the (classified) profiles, infer the
/// runtime/infra/lib relations between them, and persist. Needs ≥2 profiled
/// repos. A timed-out / unparseable reply leaves existing relations intact
/// (never persists an empty set, which would drop all agent edges).
async fn analyze_relations(db: &Db, workspace_id: i32) -> Result<()> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut profiled: Vec<(repo_ref::Model, repo_profile::Model)> = Vec::new();
    for r in &repos {
        if !Path::new(&r.local_git_path).exists() {
            continue;
        }
        if let Some(p) = repo::get_repo_profile(db, r.id).await? {
            // Only relate FULLY-PROFILED repos. A canonical tier ALONE isn't enough:
            // a user can pick a tier on a still-analyzing placeholder, and if the
            // deep classifier then fails/times out the row keeps that canonical tier
            // but a blank summary/stack/components. Feeding such a blank node to the
            // relation agent — and letting it reach the ≥2 threshold — lets a
            // degraded prompt return an explicit empty set, which `merge_relations`
            // turns into "drop every agent edge", wiping the OTHER repos' real edges.
            if is_fully_profiled(&p) {
                profiled.push((r.clone(), p));
            }
        }
    }
    if profiled.len() < 2 {
        // Still run the manifest pass even with < 2 fully-profiled repos: repos
        // with a checkout but pending/partial classification still benefit from
        // deterministic lib edges (the graph shows them as placeholder nodes).
        let _ = seed_manifest_relations(db, workspace_id).await;
        // A cross-repo map needs ≥2 profiled repos. If the workspace previously had
        // enough and dropped below (a repo deleted, or classifiers still pending),
        // any stored map describes repos/edges no longer in the graph — clear it so
        // the map pane shows the empty/regenerate state instead of stale markdown.
        let _ = repo::clear_repo_map_doc(db, workspace_id).await;
        emit_graph_updated(workspace_id);
        return Ok(());
    }
    let prompt = build_curator_prompt(&profiled);
    // Run from the repos' common-ancestor directory, not just the first repo, so a
    // sandboxed tool (codex read-only) can actually read EVERY repo's path — the
    // sandbox is scoped to cwd, and a sibling repo outside it would be unreadable,
    // so cross-repo edges would be missed. But REFUSE a too-broad ancestor (the
    // home directory or above): the read-only sandbox would then expose unrelated
    // private files under it. In that case fall back to the first repo (codex may
    // miss some cross-repo file reads, but it never reads outside a repo).
    let paths: Vec<&str> = profiled.iter().map(|(r, _)| r.local_git_path.as_str()).collect();
    let cwd = common_ancestor(&paths)
        .filter(|anc| !is_too_broad(anc))
        .unwrap_or_else(|| Path::new(&profiled[0].0.local_git_path).to_path_buf());
    let tool = crate::tools::default_tool(db).await;
    // The relations pass is workspace-level, not tied to one repo's detail panel,
    // so its stream is discarded — it shares the runner only for the transport fix.
    let mut fresh_markdown: Option<String> = None;
    if let Ok(text) = run_streaming_agent(&tool, &cwd, &prompt, &mut |_| {}).await {
        if let Some(output) = parse_curator_output(&text) {
            persist_relations(db, &profiled, &output.relations).await?;
            fresh_markdown = output.repo_map_markdown;
        }
    }
    // Manifest pass AFTER the agent pass: `seed_manifest_relations` reloads the
    // current relations (now including fresh agent edges) and merges manifest lib
    // edges on top via the user>manifest>agent precedence. Running it last ensures
    // the agent pass's `persist_relations` (which calls `merge_relations(&_, &[],
    // &fresh_agent)`) doesn't overwrite the manifest edges.
    let _ = seed_manifest_relations(db, workspace_id).await;
    // Write the map doc LAST, after every relation mutation above. `set_repo_relations`
    // invalidates the stored doc on each write, so persisting markdown here is what
    // repopulates it. If the agent omitted markdown (parse_curator_output permits it),
    // the doc stays cleared rather than serving the pre-pass narrative.
    //
    // But only publish when the markdown still covers the FINAL graph: the analyst
    // saw only `profiled`, while `seed_manifest_relations` may have just added a lib
    // edge to/from an UNPROFILED provider the prompt never listed. Republishing the
    // pre-seed markdown would then serve a doc missing that node/edge (permanently,
    // if the provider is failed). In that case leave the doc cleared for a later pass.
    if let Some(md) = fresh_markdown {
        let profiled_ids: std::collections::HashSet<i32> =
            profiled.iter().map(|(r, _)| r.id).collect();
        // Repos that still exist in the workspace — edges to anything else are stale
        // (the graph filters them out), so they must not block republishing.
        let current_ids: std::collections::HashSet<i32> = repos.iter().map(|r| r.id).collect();
        let mut final_relations: Vec<(i32, Vec<AgentRelation>)> = Vec::new();
        for r in &repos {
            if let Ok(Some(p)) = repo::get_repo_profile(db, r.id).await {
                let rels: Vec<AgentRelation> = serde_json::from_str(&p.relations).unwrap_or_default();
                final_relations.push((r.id, rels));
            }
        }
        if markdown_covers_graph(&profiled_ids, &current_ids, &final_relations) {
            let _ = repo::set_repo_map_doc(db, workspace_id, &md).await;
        }
    }
    emit_graph_updated(workspace_id);
    Ok(())
}

/// Re-profile a single repo on an explicit user action: force the deep classifier
/// for just this repo, then refresh the workspace's cross-repo relations. The
/// WHOLE operation holds the per-workspace `pass_gate` lock, so it is serialized
/// against any background workspace pass — a concurrent pass can't classify the
/// same repo in parallel and have its older result overwrite this forced one, and
/// the relation refresh can't run two agents at once. We classify directly (not
/// via the full coalescer) so a failed/timed-out forced classify isn't immediately
/// retried by the workspace pass's stage-1 (which would block for two timeouts).
pub async fn reprofile_repo(db: &Db, repo: &repo_ref::Model) -> Result<()> {
    let gate = pass_gate(repo.workspace_id);
    let _g = gate.lock.lock().await;
    // NB: do NOT clear the failed state up front. `profile_repo_agent` clears it via
    // `run_begin` only once a run actually STARTS (after its checkout-exists guard),
    // so a retry whose checkout is gone keeps a visible failed state instead of
    // being silently dropped to idle. (run_begin overwrites a `failed` entry with
    // `running`, and reprofile bypasses the auto-pass `failed`-skip by calling here.)
    profile_repo_agent(db, repo).await?;
    emit_graph_updated(repo.workspace_id);
    let _ = analyze_relations(db, repo.workspace_id).await;
    Ok(())
}

/// On startup, repos whose `analysis_state` was persisted as "running" at shutdown
/// have no live process. Re-kick their per-repo classification so a repo
/// interrupted mid-analysis resumes instead of spinning forever in a stale state.
/// Per-workspace: await all stuck classifiers concurrently, then refresh relations.
/// Called by `resume_running_analyses`; each invocation is spawned so startup stays
/// non-blocking. `profile_repo_agent`'s `run_begin` guard dedupes any concurrent pass.
async fn resume_workspace(db: Db, repos: Vec<repo_ref::Model>, workspace_id: i32) {
    use futures::future::join_all;
    // Serialize the WHOLE resume (classifiers + relation pass) under the workspace
    // pass-gate, like reprofile_repo. Otherwise, if a backfill/manual pass already
    // owns these repos' `run_begin`, the classifier futures return immediately
    // (deduped) and join_all reaches `analyze_relations` while that pass's own
    // relation agent is still running — two concurrent relation passes, and the
    // stale one finishing last would overwrite the newer relations/map doc.
    // `profile_repo_agent` gates on `run_begin`, not `pass_gate`, so holding the
    // gate here can't deadlock.
    let gate = pass_gate(workspace_id);
    let _g = gate.lock.lock().await;
    let futs = repos.into_iter().map(|r| {
        let db2 = db.clone();
        async move { let _ = profile_repo_agent(&db2, &r).await; }
    });
    join_all(futs).await;
    let _ = analyze_relations(&db, workspace_id).await;
}

/// Resume any repo whose analysis was interrupted mid-run (persisted state =
/// "running"). Groups stuck repos by workspace and, per workspace, spawns ONE task
/// that (a) awaits all stuck classifiers concurrently, THEN (b) refreshes relations —
/// so edges + the map doc are never written before the classifiers finish.
pub async fn resume_running_analyses(db: &Db) {
    let stuck = match repo::repos_with_analysis_state(db, "running").await {
        Ok(v) => v,
        Err(_) => return,
    };
    // Group by workspace so each workspace gets exactly one classify→relations task.
    let mut by_workspace: std::collections::HashMap<i32, Vec<repo_ref::Model>> =
        std::collections::HashMap::new();
    for r in stuck {
        by_workspace.entry(r.workspace_id).or_default().push(r);
    }
    for (ws_id, repos) in by_workspace {
        let db2 = db.clone();
        tauri::async_runtime::spawn(async move {
            resume_workspace(db2, repos, ws_id).await;
        });
    }
}

/// Per-workspace serialization for analysis passes: an async lock (two passes for
/// one workspace never overlap) plus a `dirty` flag so a request that lands while
/// a pass is running coalesces into one rerun instead of a parallel pass.
#[derive(Clone)]
struct PassGate {
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Sticky across the drain window: a forced (user-initiated) request anywhere
    /// while a pass is pending makes the next drained run force-retry failures.
    force: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

fn pass_gate(workspace_id: i32) -> PassGate {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, PassGate>>> =
        std::sync::OnceLock::new();
    let map = M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut g = map.lock().unwrap_or_else(|e| e.into_inner());
    g.entry(workspace_id)
        .or_insert_with(|| PassGate {
            lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            force: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
        .clone()
}

/// Run an analysis pass for a workspace, SERIALIZED (two never overlap, so a
/// background pass and a manual reprofile/analyze can't clobber each other's
/// relations) and AWAITABLE (returns only once this request's work has actually
/// completed — callers can refresh the map afterwards and see fresh edges).
///
/// Coalescing: every caller marks the workspace `dirty` and then takes the lock.
/// The lock holder drains — it reruns while `dirty` was set during its run — so a
/// batch add (many requests) collapses into ~one pass, while a request that lands
/// mid-pass is still covered before any waiter is released. Spawn it
/// fire-and-forget when you don't want to block (e.g. after adding a repo).
pub async fn analyze_workspace_coalesced(db: &Db, workspace_id: i32, force: bool) {
    use std::sync::atomic::Ordering;
    let gate = pass_gate(workspace_id);
    gate.dirty.store(true, Ordering::SeqCst);
    if force {
        gate.force.store(true, Ordering::SeqCst);
    }
    let _g = gate.lock.lock().await;
    // Drain: run until no new request landed during the previous run. The holder
    // covers waiters' requests, so once it exits they acquire the lock, find
    // `dirty` already cleared, and return immediately — having awaited completion.
    // A forced request anywhere in the drain window forces that run (then resets),
    // so an explicit "Analyze deps" retries failures even if it coalesced with an
    // auto pass.
    while gate.dirty.swap(false, Ordering::SeqCst) {
        let forced = gate.force.swap(false, Ordering::SeqCst);
        let _ = analyze_workspace(db, workspace_id, forced).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::AgentRelation;
    use crate::store::{repo, Db};

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    #[test]
    fn codex_exec_read_only_args_drop_ask_for_approval() {
        // Regression: current codex `exec` rejects `--ask-for-approval`, which made
        // every analysis spawn exit at arg-parse and stranded repos at "分析中".
        let a = super::codex_exec_read_only_args();
        assert!(
            !a.iter().any(|s| s == "--ask-for-approval"),
            "codex exec no longer accepts --ask-for-approval"
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--sandbox" && w[1] == "read-only"),
            "read-only sandbox preserved"
        );
        assert!(
            a.iter().any(|s| s.starts_with("approval_policy")),
            "never-prompt expressed via approval_policy config override"
        );
        assert!(a.iter().any(|s| s == "--skip-git-repo-check"));
    }

    #[test]
    fn turn_collector_decides_outcome_and_streams() {
        use crate::lead_chat::proto::ChatEvent;
        let te = |is_error| ChatEvent::TurnEnd { is_error, context_tokens: None };
        let delta = |s: &str| ChatEvent::TextDelta { text: s.to_string() };
        let asst = |s: &str| ChatEvent::Assistant { texts: vec![s.to_string()], tools: vec![] };
        let mut noop = |_: super::AnalysisEvent| {};

        // Clean app-server turn: deltas then a non-error TurnEnd → Ok with the text.
        let mut c = super::TurnCollector::default();
        assert!(!c.push(delta("hel"), &mut noop));
        assert!(c.push(te(false), &mut noop), "TurnEnd ends the read");
        assert_eq!(c.outcome("t", true, true).unwrap(), "hel");

        // Errored turn → Err.
        let mut c = super::TurnCollector::default();
        c.push(delta("x"), &mut noop);
        c.push(te(true), &mut noop);
        assert!(c.outcome("t", true, true).is_err(), "errored turn fails");

        // App-server EOF WITHOUT a TurnEnd (mid-crash) → Err even with partial text.
        let mut c = super::TurnCollector::default();
        c.push(delta("partial"), &mut noop);
        assert!(c.outcome("t", true, true).is_err(), "require_turn_end: no TurnEnd → fail");

        // Exec clean EOF without a TurnEnd (opencode) WITH text → Ok.
        let mut c = super::TurnCollector::default();
        c.push(asst("done"), &mut noop);
        assert_eq!(c.outcome("t", true, false).unwrap(), "done");

        // Timeout (reached_end = false) → Err.
        let mut c = super::TurnCollector::default();
        c.push(asst("partial"), &mut noop);
        assert!(c.outcome("t", false, false).is_err(), "timeout fails");

        // Empty output → Err.
        let mut c = super::TurnCollector::default();
        c.push(te(false), &mut noop);
        assert!(c.outcome("t", true, true).is_err(), "empty output fails");

        // claude shape: token deltas THEN a final full `assistant` message carrying
        // the SAME text → streamed to the sink ONCE (the final message is not
        // re-forwarded after deltas), while `text()` returns the full message.
        let mut got = String::new();
        let mut sink = |e: super::AnalysisEvent| match e {
            super::AnalysisEvent::Delta(t) => got.push_str(t),
        };
        let mut c = super::TurnCollector::default();
        c.push(delta("hel"), &mut sink);
        c.push(delta("lo"), &mut sink);
        c.push(asst("hello"), &mut sink);
        assert_eq!(got, "hello", "claude's final full message is not double-streamed");
        assert_eq!(c.text(), "hello", "final text is the full message");

        // codex-exec / opencode shape: a full message with NO prior deltas → streamed.
        let mut got2 = String::new();
        let mut sink2 = |e: super::AnalysisEvent| match e {
            super::AnalysisEvent::Delta(t) => got2.push_str(t),
        };
        let mut c2 = super::TurnCollector::default();
        c2.push(asst("done"), &mut sink2);
        assert_eq!(got2, "done", "a full message with no deltas streams once");
    }

    #[test]
    fn run_state_transitions_and_dedupe() {
        let _g = super::test_run_state_guard();
        super::run_state_clear_all_for_test();
        assert!(super::run_begin(101), "first begin starts it");
        assert!(!super::run_begin(101), "second begin deduped while running");
        assert_eq!(super::run_phase(101), "running");
        super::run_finish_err(101, "boom".into());
        assert_eq!(super::run_phase(101), "failed");
        assert!(super::run_begin(101), "a failed repo can be restarted");
        super::run_finish_ok(101);
        assert_eq!(super::run_phase(101), "idle");
    }

    #[test]
    fn should_analyze_gates_failed_on_force() {
        // A failed repo is retried ONLY on a forced (user) pass — never on an auto
        // pass — regardless of whether it "needs" reclassification. A non-failed repo
        // runs iff it needs (re)classification; force is irrelevant to it.
        assert!(!super::should_analyze(true, false, true), "failed + auto → skip even if needs");
        assert!(!super::should_analyze(true, false, false), "failed + auto → skip");
        assert!(super::should_analyze(true, true, false), "failed + forced → retry even if unchanged");
        assert!(super::should_analyze(false, false, true), "not failed + needs → run");
        assert!(!super::should_analyze(false, true, false), "not failed + unchanged → skip even forced");
    }

    // ─── Finding 1: persisted failed state gates the auto-pass ─────────────

    #[tokio::test]
    async fn auto_pass_skips_db_failed_repo_without_force() {
        // After a restart the in-memory run-state is empty (idle), but a repo with
        // persisted analysis_state="failed" must NOT be auto-retried — only a forced
        // pass may retry it. Finding 1.
        let _g = super::test_run_state_guard();
        super::run_state_clear_all_for_test();
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_f1").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc", "/nonexistent/svc/checkout", "main", "", true)
            .await
            .unwrap();

        // Seed a profile row that looks like a previously-failed repo (simulate the
        // state that persists across a restart: analysis_state="failed" in the DB,
        // but the in-memory run-state map is empty → run_phase() returns "idle").
        repo::upsert_repo_profile(&db, r.id, "", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        repo::set_analysis_state(&db, r.id, "failed", Some("timeout")).await.unwrap();

        // The combined gate should treat this repo as failed (db-backed).
        let in_mem_failed = super::run_phase(r.id) == "failed"; // false: map is empty
        let db_failed = repo::get_repo_profile(&db, r.id)
            .await
            .unwrap()
            .map(|p| p.analysis_state == "failed")
            .unwrap_or(false);
        let failed = in_mem_failed || db_failed;
        // Non-forced auto-pass skips failed repos.
        assert!(!super::should_analyze(failed, false, false), "db-failed + auto → skip");
        // Forced pass retries them.
        assert!(super::should_analyze(failed, true, false), "db-failed + forced → retry");
    }

    // ─── Finding 1 (Codex r2): resume groups by workspace + classify before relations

    #[tokio::test]
    async fn resume_groups_stuck_repos_by_workspace() {
        // Verify that resume_running_analyses groups stuck repos per workspace so each
        // workspace gets exactly one classify→relations task (no races). We check the
        // grouping logic — the same HashMap logic used inside resume_running_analyses.
        let db = mem().await;
        let ws1 = repo::create_workspace(&db, "ws1_f2").await.unwrap();
        let ws2 = repo::create_workspace(&db, "ws2_f2").await.unwrap();
        let r1 = repo::add_repo_ref(&db, ws1.id, "r1", "/tmp/r1", "main", "", true).await.unwrap();
        let r2 = repo::add_repo_ref(&db, ws1.id, "r2", "/tmp/r2", "main", "", true).await.unwrap();
        let r3 = repo::add_repo_ref(&db, ws2.id, "r3", "/tmp/r3", "main", "", true).await.unwrap();

        repo::set_analysis_state(&db, r1.id, "running", None).await.unwrap();
        repo::set_analysis_state(&db, r2.id, "running", None).await.unwrap();
        repo::set_analysis_state(&db, r3.id, "running", None).await.unwrap();

        let stuck = repo::repos_with_analysis_state(&db, "running").await.unwrap();
        // Mirror the HashMap grouping from resume_running_analyses.
        let mut by_workspace: std::collections::HashMap<i32, Vec<_>> =
            std::collections::HashMap::new();
        for r in &stuck {
            by_workspace.entry(r.workspace_id).or_default().push(r.id);
        }
        // Two distinct workspace groups → two classify→relations tasks spawned.
        assert_eq!(by_workspace.len(), 2, "exactly two workspace groups");
        assert_eq!(by_workspace[&ws1.id].len(), 2, "ws1 has two stuck repos");
        assert_eq!(by_workspace[&ws2.id].len(), 1, "ws2 has one stuck repo");
    }

    // ─── Finding 2 (legacy): workspace_id collection still correct ──────────

    #[tokio::test]
    async fn resume_running_analyses_queues_workspace_ids() {
        // resume_running_analyses collects distinct workspace_ids from the "running"
        // repo rows to queue relations refreshes. Verify that the workspace_id
        // deduplication logic is correct (independent of spawned task completion).
        let db = mem().await;
        let ws1 = repo::create_workspace(&db, "ws1_f2b").await.unwrap();
        let ws2 = repo::create_workspace(&db, "ws2_f2b").await.unwrap();
        let r1 = repo::add_repo_ref(&db, ws1.id, "r1b", "/tmp/r1b", "main", "", true).await.unwrap();
        let r2 = repo::add_repo_ref(&db, ws1.id, "r2b", "/tmp/r2b", "main", "", true).await.unwrap();
        let r3 = repo::add_repo_ref(&db, ws2.id, "r3b", "/tmp/r3b", "main", "", true).await.unwrap();

        repo::set_analysis_state(&db, r1.id, "running", None).await.unwrap();
        repo::set_analysis_state(&db, r2.id, "running", None).await.unwrap();
        repo::set_analysis_state(&db, r3.id, "running", None).await.unwrap();

        let stuck = repo::repos_with_analysis_state(&db, "running").await.unwrap();
        let mut ws_ids: Vec<i32> = stuck.iter().map(|r| r.workspace_id).collect();
        ws_ids.sort_unstable();
        ws_ids.dedup();
        assert_eq!(ws_ids.len(), 2, "two distinct workspaces collected for relations refresh");
        assert!(ws_ids.contains(&ws1.id), "ws1 included");
        assert!(ws_ids.contains(&ws2.id), "ws2 included");
    }

    #[test]
    fn clear_failure_only_drops_failed() {
        // Clearing a re-added repo's stale failure must NOT disturb an in-flight run.
        let _g = super::test_run_state_guard();
        super::run_state_clear_all_for_test();
        super::run_finish_err(7, "gone".into());
        super::clear_failure(7);
        assert_eq!(super::run_phase(7), "idle", "a failed entry is forgotten");
        super::run_begin(8);
        super::clear_failure(8);
        assert_eq!(super::run_phase(8), "running", "a running run is left alone");
    }

    #[tokio::test]
    async fn manual_canonical_edit_recovers_a_failed_repo() {
        // A failed analysis the user fixes by hand (canonical tier) must stop reading
        // as failed; a summary-only edit that leaves it unclassified does not.
        let _g = super::test_run_state_guard();
        super::run_state_clear_all_for_test();
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true).await.unwrap();

        super::run_finish_err(r.id, "analysis failed".into());
        super::edit_profile(&db, r.id, Some("notes"), None).await.unwrap();
        assert_eq!(super::run_phase(r.id), "failed", "summary-only edit leaves it failed (no tier)");

        // A summary-only edit on a repo that ALREADY has a canonical tier but is
        // `failed` (a stale-refresh failure) must NOT clear it — the user didn't
        // re-classify, the prior tier is just inherited, and the failure is real.
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "old", "[]", "agent", "sha_a")
            .await
            .unwrap();
        super::run_finish_err(r.id, "stale refresh failed".into());
        super::edit_profile(&db, r.id, Some("new notes"), None).await.unwrap();
        assert_eq!(
            super::run_phase(r.id),
            "failed",
            "summary-only edit inheriting a canonical tier does NOT clear a real failure"
        );

        super::edit_profile(&db, r.id, Some("a web app"), Some("frontend")).await.unwrap();
        assert_eq!(super::run_phase(r.id), "idle", "a canonical tier clears the failure");
    }

    #[tokio::test]
    async fn classified_for_requires_canonical_tier_at_the_analyzed_commit() {
        // Success = a canonical tier FOR THE TREE THIS RUN ANALYZED. No profile or a
        // non-canonical tier → not classified. A canonical tier counts only when its
        // profiled_commit matches the commit just analyzed: a moved-HEAD repo whose
        // refresh failed (prior tier kept at the OLD commit) is NOT a success, so the
        // stale tier isn't shown as a fresh `done` (and the re-run can converge).
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true).await.unwrap();
        assert!(!super::classified_for(&db, r.id, "sha_a").await, "no profile → not classified");
        repo::upsert_repo_profile(&db, r.id, "service", "[]", "s", "[]", "agent", "sha_a")
            .await
            .unwrap();
        assert!(!super::classified_for(&db, r.id, "sha_a").await, "non-canonical tier → no");
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "an api", "[]", "agent", "sha_a")
            .await
            .unwrap();
        assert!(super::classified_for(&db, r.id, "sha_a").await, "canonical @ analyzed commit → yes");
        assert!(
            !super::classified_for(&db, r.id, "sha_b").await,
            "canonical but for an OLD commit (HEAD moved) → not a success"
        );
    }

    #[tokio::test]
    async fn missing_checkout_retry_stays_failed() {
        // A retry whose checkout was moved/deleted must keep a visible, retryable
        // failure — not silently drop to idle. profile_repo_agent's cwd guard fires
        // before any agent spawn, so this is exercisable without a real run.
        let _g = super::test_run_state_guard();
        super::run_state_clear_all_for_test();
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "gone", "/nonexistent/weft/checkout/zzz", "main", "", true)
            .await
            .unwrap();
        super::profile_repo_agent(&db, &r).await.unwrap();
        // The in-memory guard is still "failed" — analyze_workspace uses this to skip
        // auto re-runs. No profile row exists (never analyzed), so DB state stays at
        // the default "idle"; the persisted state is irrelevant for the dedupe gate.
        assert_eq!(super::run_phase(r.id), "failed", "missing checkout stays failed in-memory");
    }

    #[tokio::test]
    async fn view_surfaces_failed_analysis_state() {
        // view_of reads from the persisted profile columns, so the failed state must
        // be written to the DB (via set_analysis_state) to appear in list()/graph().
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "s", "[]", "agent", "")
            .await
            .unwrap();
        repo::set_analysis_state(&db, r.id, "failed", Some("codex failed"))
            .await
            .unwrap();
        let views = super::list(&db, ws.id).await.unwrap();
        let v = views.iter().find(|v| v.repo_id == r.id).unwrap();
        assert_eq!(v.analysis_state, "failed");
        assert_eq!(v.analysis_error.as_deref(), Some("codex failed"));
    }

    /// Upsert a minimal agent profile row (tier only) so a repo has a node.
    async fn profile(db: &Db, repo_id: i32, tier: &str) {
        repo::upsert_repo_profile(db, repo_id, tier, "[]", "", "[]", "agent", "")
            .await
            .unwrap();
    }

    /// Drive analysis_state transitions via set_analysis_state and assert that
    /// graph()/view_of reflects the persisted state — not just the in-memory guard.
    #[tokio::test]
    async fn graph_and_view_reflect_persisted_analysis_state() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        // Upsert a profile row so set_analysis_state has a row to update.
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "an api", "[]", "agent", "sha1")
            .await
            .unwrap();

        // running: graph node reads the persisted "running" state.
        repo::set_analysis_state(&db, r.id, "running", None).await.unwrap();
        let g = super::graph(&db, ws.id).await.unwrap();
        let node = g.nodes.iter().find(|n| n.repo_id == r.id).unwrap();
        assert_eq!(node.analysis_state, "running");
        assert_eq!(node.analysis_error, None);

        // failed: graph node reflects the persisted error.
        repo::set_analysis_state(&db, r.id, "failed", Some("timed out")).await.unwrap();
        let g = super::graph(&db, ws.id).await.unwrap();
        let node = g.nodes.iter().find(|n| n.repo_id == r.id).unwrap();
        assert_eq!(node.analysis_state, "failed");
        assert_eq!(node.analysis_error.as_deref(), Some("timed out"));

        // idle: graph node reads "idle" and no error after recovery.
        repo::set_analysis_state(&db, r.id, "idle", None).await.unwrap();
        let g = super::graph(&db, ws.id).await.unwrap();
        let node = g.nodes.iter().find(|n| n.repo_id == r.id).unwrap();
        assert_eq!(node.analysis_state, "idle");
        assert_eq!(node.analysis_error, None);

        // Placeholder (no profile row): view_of returns "idle"/None by default.
        let r2 = repo::add_repo_ref(&db, ws.id, "fresh", "/tmp/fresh", "main", "", true)
            .await
            .unwrap();
        let g = super::graph(&db, ws.id).await.unwrap();
        let node2 = g.nodes.iter().find(|n| n.repo_id == r2.id).unwrap();
        assert_eq!(node2.analysis_state, "idle", "placeholder defaults to idle");
        assert_eq!(node2.analysis_error, None);
    }

    #[tokio::test]
    async fn graph_builds_edges_from_agent_relations() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let api = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        profile(&db, web.id, "frontend").await;
        profile(&db, api.id, "backend").await;
        // The only edges now come from agent relations (no manifest floor).
        let rels = serde_json::to_string(&vec![AgentRelation {
            to: api.id,
            kind: "http".into(),
            via: "GET /orders".into(),
            confidence: 80,
            ..Default::default()
        }])
        .unwrap();
        repo::set_repo_relations(&db, web.id, &rels).await.unwrap();

        let g = graph(&db, ws.id).await.unwrap();
        // Nodes carry the agent tier classification.
        assert!(g.nodes.iter().any(|n| n.repo_id == web.id && n.tier == "frontend" && n.analyzed));
        assert!(g.nodes.iter().any(|n| n.repo_id == api.id && n.tier == "backend"));
        assert_eq!(g.edges.len(), 1, "exactly the one agent edge");
        assert!(
            g.edges.iter().any(|e| e.from == web.id
                && e.to == api.id
                && e.kind == "http"
                && e.source == "agent"
                && e.via == "GET /orders"),
            "agent http edge present"
        );
    }

    #[tokio::test]
    async fn is_fully_profiled_requires_tier_and_summary() {
        // The cross-repo relation pass must include only completed deep profiles:
        // a canonical tier alone (e.g. a user-picked tier on a placeholder whose
        // deep classifier never finished) is NOT enough, because that blank node
        // can let a degraded prompt wipe the other repos' edges.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true)
            .await
            .unwrap();

        // Canonical tier but blank summary (tier-only placeholder) → excluded.
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "user_tier", "")
            .await
            .unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert!(!super::is_fully_profiled(&p), "tier-only placeholder excluded");

        // Non-canonical (legacy) tier with a summary → excluded.
        repo::upsert_repo_profile(&db, r.id, "service", "[]", "s", "[]", "agent", "")
            .await
            .unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert!(!super::is_fully_profiled(&p), "legacy tier excluded");

        // Canonical tier AND a non-blank summary → included.
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "an api", "[]", "agent", "")
            .await
            .unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert!(super::is_fully_profiled(&p), "completed deep profile included");
    }

    #[tokio::test]
    async fn edit_profile_creates_row_for_placeholder() {
        // A repo can be calibrated while it is still an unanalyzed placeholder
        // (no profile row). The edit must upsert one, not error.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true)
            .await
            .unwrap();
        assert!(repo::get_repo_profile(&db, r.id).await.unwrap().is_none());
        super::edit_profile(&db, r.id, Some("a web client"), Some("frontend")).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "frontend");
        assert_eq!(p.summary, "a web client");
        assert_eq!(p.source, "user");
    }

    #[tokio::test]
    async fn edit_profile_manual_tier_clears_persisted_failure() {
        // A manual canonical-tier edit RECOVERS a failed repo. Since the views now
        // read `analysis_state` from the persisted row, clearing only the in-memory
        // map would leave the DB column at "failed" — the edit must persist "idle".
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        repo::set_analysis_state(&db, r.id, "failed", Some("boom")).await.unwrap();

        super::edit_profile(&db, r.id, None, Some("backend")).await.unwrap();

        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend");
        assert_eq!(p.analysis_state, "idle", "persisted failure must be cleared");
        assert_eq!(p.analysis_error, None, "persisted error must be cleared");
    }

    #[tokio::test]
    async fn edit_profile_summary_only_keeps_persisted_failure() {
        // A summary-only edit INHERITS the prior tier without re-classifying, so it
        // must NOT silently clear a real failure (no re-run happened).
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "old", "[]", "agent", "")
            .await
            .unwrap();
        repo::set_analysis_state(&db, r.id, "failed", Some("boom")).await.unwrap();

        super::edit_profile(&db, r.id, Some("new summary"), None).await.unwrap();

        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.analysis_state, "failed", "summary-only edit must not clear failure");
    }

    #[tokio::test]
    async fn edit_profile_invalidates_map_doc() {
        // A manual profile edit changes the map's INVENTORY surface (a tier edit
        // cascades to component tiers; summary changes the narrative) without
        // touching relations, so it must invalidate the workspace map doc — the
        // relation chokepoint (set_repo_relations) doesn't cover this path.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true).await.unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "old", "[]", "agent", "")
            .await
            .unwrap();
        repo::set_repo_map_doc(&db, ws.id, "## old map").await.unwrap();

        super::edit_profile(&db, r.id, None, Some("frontend")).await.unwrap();

        assert!(
            repo::get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "a manual profile edit must invalidate the stale workspace map doc"
        );
    }

    #[tokio::test]
    async fn persist_repo_class_migrates_legacy_user_role() {
        // An upgraded db may carry a legacy role on a user-owned row; the agent
        // pass keeps the user's summary but migrates the invalid tier.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "service", "[]", "mine", "[]", "user", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "agent summary".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend", "legacy 'service' migrated to a real tier");
        assert_eq!(p.summary, "mine", "user-pinned summary preserved");
        // Tier ownership dropped (it's the agent's value now); summary still owned.
        assert_eq!(p.source, "user_summary");
    }

    #[tokio::test]
    async fn persist_repo_class_keeps_valid_user_tier() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "frontend", "[]", "mine", "[]", "user", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "agent".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "frontend", "a valid user-pinned tier is not overwritten");
    }

    #[tokio::test]
    async fn persist_repo_class_fills_empty_user_tier_from_agent() {
        // A user-owned row with an EMPTY tier (e.g. a summary-only edit on a
        // placeholder) adopts the agent's valid tier rather than staying blank;
        // the user's summary is still preserved.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "", "[]", "mine", "[]", "user", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "agent".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend", "empty user tier adopts the agent's classification");
        assert_eq!(p.summary, "mine", "user-pinned summary preserved");
    }

    #[tokio::test]
    async fn persist_repo_class_drops_ownership_for_agent_filled_field() {
        // A user-owned row whose tier was filled by the agent must NOT stay
        // tier-pinned, so a later pass can refresh it.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        // source "user" = both owned, but tier is empty (legacy/placeholder).
        repo::upsert_repo_profile(&db, r.id, "", "[]", "mine", "[]", "user", "")
            .await
            .unwrap();
        let mk = |tier: &str| super::RepoClassWire {
            name: None,
            tier: tier.into(),
            summary: "agent".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, mk("backend"), "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend", "empty tier adopts the agent's");
        assert_eq!(p.source, "user_summary", "tier ownership dropped, summary kept");
        // A second pass now refreshes the (no-longer-owned) tier.
        super::persist_repo_class(&db, &r, mk("frontend"), "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "frontend", "agent-filled tier is refreshable");
        assert_eq!(p.summary, "mine", "user summary still pinned");
    }

    #[tokio::test]
    async fn persist_repo_class_preserves_facts_on_partial_reply() {
        // A partial reply (tier only, no stack/components/summary) must not erase
        // the repo's existing stack/components/summary.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(
            &db, r.id, "backend", r#"["rust"]"#, "old summary",
            r#"[{"name":"api","tier":"backend"}]"#, "agent", "",
        )
        .await
        .unwrap();
        let wire = super::RepoClassWire {
            tier: "backend".into(),
            name: None,
            summary: "".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.stack, r#"["rust"]"#, "prior stack preserved");
        assert!(p.components.contains("api"), "prior components preserved");
        assert_eq!(p.summary, "old summary", "prior summary preserved");
    }

    #[tokio::test]
    async fn persist_repo_class_explicit_empty_clears_facts() {
        // An EXPLICIT empty components/stack (Some([])) clears prior facts — distinct
        // from an omitted field (None), which preserves them.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(
            &db, r.id, "backend", r#"["rust"]"#, "old",
            r#"[{"name":"api","tier":"backend"}]"#, "agent", "",
        )
        .await
        .unwrap();
        let wire = super::RepoClassWire {
            tier: "backend".into(),
            name: None,
            summary: "now monolith".into(),
            stack: Some(vec![]),
            components: Some(vec![]),
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.stack, "[]", "explicit empty stack clears prior");
        assert_eq!(p.components, "[]", "explicit empty components clears prior");
    }

    #[tokio::test]
    async fn persist_repo_class_rejects_blank_summary_classification() {
        // A truncated reply (tier but no summary) on a fresh placeholder with no
        // prior summary is incomplete: leave the placeholder for retry rather than
        // saving a canonical tier that needs_classification would skip forever.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "", "[]", "", "[]", "agent", "")
            .await
            .unwrap(); // eager placeholder, blank summary
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "abc").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "", "incomplete classification not persisted — stays a placeholder");
        assert_eq!(p.summary, "");
    }

    #[tokio::test]
    async fn persist_repo_class_keeps_prior_summary_when_agent_omits() {
        // A truncated reply (tier but no summary) must not blank an existing
        // summary, or needs_classification would skip it forever.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "prior summary", "[]", "agent", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "".into(), // agent omitted it
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.summary, "prior summary", "blank agent summary keeps the prior one");
    }

    #[tokio::test]
    async fn persist_repo_class_fills_blank_user_summary() {
        // A tier-only calibration pins source="user" with a blank summary; the
        // agent's real summary should fill it rather than stay blank forever.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "user", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "agent summary".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend", "user-pinned tier kept");
        assert_eq!(p.summary, "agent summary", "blank user summary filled by the agent");
    }

    #[tokio::test]
    async fn persist_repo_class_rejects_invalid_agent_tier() {
        // A non-canonical agent tier leaves the repo as a placeholder (no row),
        // rather than persisting an analyzed-but-unclassified row that the
        // backfill gate would never retry.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "service".into(), // not one of frontend|backend
            summary: "agent".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        assert!(
            repo::get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "invalid agent tier is not persisted as an analyzed row"
        );
    }

    #[tokio::test]
    async fn persist_repo_class_drops_nameless_components() {
        // A malformed (nameless) sub-component is dropped, but the rest of the
        // classification still persists.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "mono", "/tmp/mono", "main", "", true)
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "monorepo".into(),
            stack: None,
            components: Some(vec![
                Component { name: "api".into(), tier: "backend".into(), ..Default::default() },
                Component { name: "".into(), tier: "frontend".into(), ..Default::default() },
            ]),
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend");
        let comps: Vec<Component> = serde_json::from_str(&p.components).unwrap();
        assert_eq!(comps.len(), 1, "the nameless component is dropped");
        assert_eq!(comps[0].name, "api");
    }

    #[tokio::test]
    async fn edit_profile_summary_only_leaves_tier_and_ownership() {
        // A summary-only edit (tier = None) updates the summary and pins ONLY the
        // summary; the legacy tier is left untouched (and unowned) so it still
        // qualifies for the agent's backfill/migration.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "service", "[]", "old", "[]", "agent", "")
            .await
            .unwrap();
        super::edit_profile(&db, r.id, Some("new summary"), None).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "service", "tier untouched by a summary-only edit");
        assert_eq!(p.summary, "new summary");
        assert_eq!(p.source, "user_summary", "only the summary is pinned");
    }

    #[tokio::test]
    async fn edit_profile_tier_only_does_not_pin_summary() {
        // Calibrating only the tier pins the tier but NOT the (agent) summary, so a
        // later agent pass can still refresh the summary.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "agent sum", "[]", "agent", "")
            .await
            .unwrap();
        super::edit_profile(&db, r.id, None, Some("frontend")).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "frontend", "tier pinned");
        assert_eq!(p.summary, "agent sum", "summary unchanged by a tier-only edit");
        assert_eq!(p.source, "user_tier", "only the tier is pinned");

        // A later agent pass keeps the pinned tier but refreshes the unpinned summary.
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "fresh agent sum".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "frontend", "user-pinned tier survives re-analysis");
        assert_eq!(p.summary, "fresh agent sum", "unpinned summary is refreshed");
    }

    #[tokio::test]
    async fn persist_repo_class_skips_deleted_repo() {
        // A repo removed mid-pass must not get an orphaned profile recreated.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "gone", "/tmp/gone", "main", "", true)
            .await
            .unwrap();
        repo::delete_repo_cascade(&db, r.id).await.unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "s".into(),
            stack: None,
            components: None,
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        assert!(
            repo::get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "no orphaned profile recreated for a deleted repo"
        );
    }

    #[tokio::test]
    async fn edit_profile_rejects_deleted_repo() {
        // A stale edit after the repo is gone must error, not recreate an orphan.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "gone", "/tmp/gone", "main", "", true)
            .await
            .unwrap();
        repo::delete_repo_cascade(&db, r.id).await.unwrap();
        assert!(super::edit_profile(&db, r.id, Some("s"), Some("frontend")).await.is_err());
        assert!(repo::get_repo_profile(&db, r.id).await.unwrap().is_none());
    }

    /// Seed a repo with a profile containing the given components (name, tier pairs).
    async fn seed_repo_with_components(
        db: &Db,
        ws_id: i32,
        repo_tier: &str,
        components: &[(&str, &str)],
    ) -> crate::store::entities::repo_ref::Model {
        let r = repo::add_repo_ref(db, ws_id, "mono", "/tmp/mono", "main", "", true)
            .await
            .unwrap();
        let comps: Vec<crate::profile::Component> = components
            .iter()
            .map(|(name, tier)| crate::profile::Component {
                name: name.to_string(),
                tier: tier.to_string(),
                ..Default::default()
            })
            .collect();
        let comps_json = serde_json::to_string(&comps).unwrap();
        repo::upsert_repo_profile(db, r.id, repo_tier, "[]", "monorepo summary", &comps_json, "agent", "")
            .await
            .unwrap();
        r
    }

    #[tokio::test]
    async fn edit_tier_cascades_to_components() {
        // A canonical-tier edit rewrites every component's tier to match, so the
        // expanded view (grouping by component tier) stays in sync with the overview.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_cascade").await.unwrap();
        let r = seed_repo_with_components(
            &db,
            ws.id,
            "frontend",
            &[("web", "frontend"), ("api", "frontend")],
        )
        .await;
        super::edit_profile(&db, r.id, None, Some("backend")).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        let comps: Vec<crate::profile::Component> = serde_json::from_str(&p.components).unwrap();
        assert!(comps.iter().all(|c| c.tier == "backend"), "all component tiers cascaded to backend");
        assert_eq!(p.role, "backend");
    }

    #[tokio::test]
    async fn edit_summary_only_keeps_component_tiers() {
        // A summary-only edit (tier = None) must leave component tiers unchanged.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_summary_only").await.unwrap();
        let r = seed_repo_with_components(&db, ws.id, "frontend", &[("web", "frontend")]).await;
        super::edit_profile(&db, r.id, Some("just a summary"), None).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        let comps: Vec<crate::profile::Component> = serde_json::from_str(&p.components).unwrap();
        assert_eq!(comps[0].tier, "frontend", "summary-only edit leaves component tier unchanged");
    }

    #[test]
    fn parse_repo_class_rejects_component_object() {
        // A truncated top-level object missing its closing brace lets json_objects
        // scan into the nested component; a component carries `name`, so it must NOT
        // be accepted — including a minimal one with no path/deps. Rejecting leaves
        // the placeholder for retry (the safe failure).
        let with_path = "{\"tier\":\"backend\",\"summary\":\"svc\",\"components\":[\
            {\"name\":\"api\",\"path\":\"packages/api\",\"tier\":\"frontend\",\"summary\":\"web\"}]";
        assert!(super::parse_repo_class(with_path).is_none());
        let minimal = "{\"tier\":\"backend\",\"summary\":\"svc\",\"components\":[\
            {\"name\":\"api\",\"tier\":\"frontend\",\"summary\":\"web\"}]";
        assert!(super::parse_repo_class(minimal).is_none());
        // A well-formed repo reply (no top-level `name`) still parses.
        let ok = "{\"tier\":\"backend\",\"summary\":\"svc\"}";
        assert_eq!(super::parse_repo_class(ok).unwrap().tier, "backend");
    }

    #[test]
    fn parse_curator_output_tolerates_bad_confidence() {
        // A float / word / out-of-range confidence on one row must not reject the
        // whole relations payload.
        let reply = r#"{"relations":[{"from":1,"to":2,"kind":"http","via":"x","confidence":"high"},
            {"from":1,"to":3,"kind":"grpc","via":"y","confidence":0.8},
            {"from":1,"to":4,"kind":"lib","via":"z","confidence":250}]}"#;
        let out = super::parse_curator_output(reply).expect("parsed despite bad confidence");
        assert_eq!(out.relations.len(), 3);
        assert_eq!(out.relations[0].confidence, 0); // "high" → 0
        assert_eq!(out.relations[2].confidence, 100); // 250 clamped
    }

    #[test]
    fn common_ancestor_finds_shared_dir_or_none() {
        assert_eq!(
            super::common_ancestor(&["/home/u/ws/web", "/home/u/ws/api"]),
            Some(std::path::PathBuf::from("/home/u/ws"))
        );
        assert_eq!(
            super::common_ancestor(&["/home/u/web", "/home/u/api", "/home/u/web/sub"]),
            Some(std::path::PathBuf::from("/home/u"))
        );
        // No meaningful shared ancestor (only the root) → None.
        assert_eq!(super::common_ancestor(&["/a/x", "/b/y"]), None);
    }

    #[test]
    fn parse_repo_class_skips_stray_brace() {
        // A stray unmatched `{` in prose before the JSON must not abort parsing.
        let reply = "Looking at fn main() { ... then the entry point.\n\n\
            {\"tier\":\"backend\",\"summary\":\"svc\"}";
        let wire = super::parse_repo_class(reply).expect("found the valid object past the stray brace");
        assert_eq!(wire.tier, "backend");
        assert_eq!(wire.summary, "svc");
    }

    #[test]
    fn parse_repo_class_tolerates_nameless_component() {
        // One component missing `name` must not make the whole reply unparseable.
        let reply = r#"{"tier":"backend","summary":"s","components":[{"name":"api"},{"tier":"frontend"}]}"#;
        let wire = super::parse_repo_class(reply).expect("parsed despite a nameless component");
        assert_eq!(wire.tier, "backend");
        assert_eq!(wire.components.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn graph_returns_placeholder_for_unanalyzed_repo() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "fresh", "/tmp/fresh", "main", "", true)
            .await
            .unwrap();
        // No profile row yet → a placeholder node, not an omission.
        let g = graph(&db, ws.id).await.unwrap();
        let node = g.nodes.iter().find(|n| n.repo_id == r.id).expect("placeholder node");
        assert!(!node.analyzed);
        assert_eq!(node.tier, "");
        assert!(g.edges.is_empty());
    }

    #[test]
    fn parse_curator_output_extracts_json_from_fenced_prose() {
        let reply = "Here is the analysis:\n```json\n{\"relations\":[{\"from\":1,\"to\":2,\
            \"kind\":\"http\",\"via\":\"GET /orders\",\"confidence\":80}]}\n```\nDone.";
        let out = super::parse_curator_output(reply).expect("parsed a relations object");
        assert_eq!(out.relations.len(), 1);
        assert_eq!((out.relations[0].from, out.relations[0].to), (Some(1), Some(2)));
        assert_eq!(out.relations[0].kind, "http");
        assert_eq!(out.relations[0].via, "GET /orders");
        assert_eq!(out.relations[0].confidence, 80);
    }

    #[test]
    fn parse_curator_output_distinguishes_unparseable_from_explicit_empty() {
        // Unparseable / timed-out replies → None (caller must NOT wipe the graph).
        assert!(super::parse_curator_output("no json here").is_none());
        assert!(super::parse_curator_output("").is_none());
        // An object without a `relations` array is treated as unparseable too.
        assert!(super::parse_curator_output(r#"{"notes":"none found"}"#).is_none());
        // An explicit empty relations array → Some with empty relations.
        let explicit = super::parse_curator_output(r#"{"relations":[]}"#);
        assert!(explicit.is_some_and(|o| o.relations.is_empty()));
    }

    #[test]
    fn parse_curator_output_skips_earlier_non_result_objects() {
        // A prose/config object appears BEFORE the real relations object; the
        // earlier brace block must not hide the valid result later in the reply.
        let reply = "I looked at the config `{\"port\": 8080}` and traced the call.\n\n\
            {\"relations\":[{\"from\":1,\"to\":2,\"kind\":\"http\",\"via\":\"GET /x\",\"confidence\":70}]}";
        let out = super::parse_curator_output(reply).expect("found the later relations object");
        assert_eq!(out.relations.len(), 1);
        assert_eq!((out.relations[0].from, out.relations[0].to, out.relations[0].kind.as_str()), (Some(1), Some(2), "http"));
    }

    #[test]
    fn parse_curator_output_reads_rationale_and_repo_map_markdown() {
        // Relations with rationale + a markdown doc in the same JSON object.
        // Use a regular string so \n in the JSON value is a literal backslash-n.
        let reply = "{\"relations\":[{\"from\":1,\"to\":2,\"kind\":\"http\",\"via\":\"GET /orders\",\
            \"confidence\":80,\"rationale\":\"frontend calls the orders API to display the cart\"}],\
            \"repo_map_markdown\":\"## Inventory\\n- web (frontend)\"}";
        let out = super::parse_curator_output(reply).expect("parsed with rationale + markdown");
        assert_eq!(out.relations.len(), 1);
        assert_eq!(
            out.relations[0].rationale,
            "frontend calls the orders API to display the cart"
        );
        // The JSON string "## Inventory\n- web (frontend)" deserializes to a real newline.
        assert_eq!(
            out.repo_map_markdown.as_deref(),
            Some("## Inventory\n- web (frontend)")
        );
    }

    #[test]
    fn parse_curator_output_tolerates_absent_rationale_and_markdown() {
        // An agent that omits rationale and repo_map_markdown must not fail.
        let reply = r#"{"relations":[{"from":1,"to":2,"kind":"lib","via":"@pkg/core","confidence":90}]}"#;
        let out = super::parse_curator_output(reply).expect("parsed despite missing optional fields");
        assert_eq!(out.relations[0].rationale, "");
        assert!(out.repo_map_markdown.is_none(), "absent markdown → None (skip doc update)");
    }

    #[tokio::test]
    async fn persist_relations_groups_filters_and_clears() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let api = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        profile(&db, web.id, "frontend").await;
        profile(&db, api.id, "backend").await;
        let profiled = vec![
            (
                web.clone(),
                repo::get_repo_profile(&db, web.id).await.unwrap().unwrap(),
            ),
            (
                api.clone(),
                repo::get_repo_profile(&db, api.id).await.unwrap().unwrap(),
            ),
        ];
        let mk = |from: i32, to: i32, via: &str| super::CuratorRelation {
            from: Some(from),
            to: Some(to),
            kind: "http".into(),
            via: via.into(),
            confidence: 70,
            rationale: String::new(),
        };
        let rels = vec![
            mk(web.id, api.id, "GET /x"), // kept
            mk(web.id, web.id, "self"),   // dropped: self-edge
            mk(web.id, 999, "ghost-to"),  // dropped: unknown target
            mk(999, api.id, "ghost-from"), // dropped: unknown producer
            // dropped: malformed row missing an endpoint, but the valid one above
            // must still persist.
            super::CuratorRelation {
                from: Some(web.id),
                to: None,
                kind: "http".into(),
                via: "no-target".into(),
                confidence: 50,
                rationale: String::new(),
            },
        ];
        super::persist_relations(&db, &profiled, &rels).await.unwrap();

        let rels_of = |p: repo_profile::Model| {
            serde_json::from_str::<Vec<AgentRelation>>(&p.relations).unwrap()
        };
        let web_rels = rels_of(repo::get_repo_profile(&db, web.id).await.unwrap().unwrap());
        assert_eq!(web_rels.len(), 1);
        assert_eq!(web_rels[0].to, api.id);
        assert_eq!(web_rels[0].kind, "http");
        // A repo the agent didn't mention is cleared to [] (re-run replaces).
        let api_rels = rels_of(repo::get_repo_profile(&db, api.id).await.unwrap().unwrap());
        assert!(api_rels.is_empty());
    }

    #[tokio::test]
    async fn graph_drops_agent_relation_to_unknown_repo() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        profile(&db, web.id, "frontend").await;
        // Points at a repo id not in this workspace → dropped, no edge.
        let rels = serde_json::to_string(&vec![AgentRelation {
            to: 999,
            kind: "http".into(),
            via: "x".into(),
            confidence: 50,
            ..Default::default()
        }])
        .unwrap();
        repo::set_repo_relations(&db, web.id, &rels).await.unwrap();
        assert!(graph(&db, ws.id).await.unwrap().edges.is_empty());
    }

    /// Helper: write temp manifests and register a repo with `local_git_path` pointing there.
    async fn seed_repo_with_manifest(
        db: &Db,
        ws_id: i32,
        name: &str,
        manifest_filename: &str,
        manifest_content: &str,
    ) -> (crate::store::entities::repo_ref::Model, std::path::PathBuf) {
        let tmp = std::env::temp_dir()
            .join(format!("weft_seed_test_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(manifest_filename), manifest_content).unwrap();
        let r = repo::add_repo_ref(db, ws_id, name, tmp.to_str().unwrap(), "main", "", true)
            .await
            .unwrap();
        // Upsert a minimal profile so set_repo_relations has a row to update.
        repo::upsert_repo_profile(db, r.id, "backend", "[]", "summary", "[]", "agent", "")
            .await
            .unwrap();
        (r, tmp)
    }

    #[tokio::test]
    async fn seed_manifest_relations_emits_lib_edge_between_workspace_repos() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_seed").await.unwrap();

        // Repo A: package.json that requires "@acme/lib" (provided by B).
        let (repo_a, tmp_a) = seed_repo_with_manifest(
            &db,
            ws.id,
            "repo-a",
            "package.json",
            r#"{"name":"@acme/app","dependencies":{"@acme/lib":"^1","lodash":"^4"}}"#,
        )
        .await;

        // Repo B: package.json that provides "@acme/lib".
        let (repo_b, tmp_b) = seed_repo_with_manifest(
            &db,
            ws.id,
            "repo-b",
            "package.json",
            r#"{"name":"@acme/lib","dependencies":{}}"#,
        )
        .await;

        super::seed_manifest_relations(&db, ws.id).await.unwrap();

        let rels_a: Vec<AgentRelation> = {
            let p = repo::get_repo_profile(&db, repo_a.id).await.unwrap().unwrap();
            serde_json::from_str(&p.relations).unwrap()
        };
        // A should have a manifest lib edge to B.
        assert!(
            rels_a.iter().any(|r| r.to == repo_b.id
                && r.kind == "lib"
                && r.source == "manifest"
                && r.via == "@acme/lib"
                && !r.rejected),
            "A has a manifest lib edge to B via @acme/lib"
        );
        // "lodash" is external (not in workspace) — no edge for it.
        assert!(
            !rels_a.iter().any(|r| r.via == "lodash"),
            "external dep 'lodash' produces no edge"
        );

        // B has no requires that map to workspace repos → no lib edges.
        let rels_b: Vec<AgentRelation> = {
            let p = repo::get_repo_profile(&db, repo_b.id).await.unwrap().unwrap();
            serde_json::from_str(&p.relations).unwrap()
        };
        assert!(
            rels_b.iter().all(|r| r.source != "manifest"),
            "B has no manifest edges (it has no workspace deps)"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp_a);
        let _ = std::fs::remove_dir_all(&tmp_b);
    }

    #[tokio::test]
    async fn analyze_relations_clears_stale_map_when_below_threshold() {
        // A workspace that previously generated a map but now has < 2 fully-profiled
        // repos (e.g. a repo was deleted) must not keep serving the stale doc.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_stale_map").await.unwrap();
        let _r = repo::add_repo_ref(&db, ws.id, "solo", "/nonexistent/solo", "main", "", true)
            .await
            .unwrap();
        repo::set_repo_map_doc(&db, ws.id, "## Inventory\n- gone (backend)").await.unwrap();

        super::analyze_relations(&db, ws.id).await.unwrap();

        assert!(
            repo::get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "stale map doc must be cleared when the workspace drops below 2 profiled repos"
        );
    }

    #[tokio::test]
    async fn resume_workspace_serializes_under_pass_gate() {
        // resume_workspace must hold the workspace pass-gate for its WHOLE run, so
        // its relation pass can't run concurrently with another pass (and overwrite
        // newer output). Proof: while we hold the gate, the spawned resume is blocked
        // and its map-clear side effect can't happen; once released, it proceeds.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_resume_gate").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "solo", "/nonexistent/solo-resume", "main", "", true)
            .await
            .unwrap();
        repo::set_repo_map_doc(&db, ws.id, "## stale").await.unwrap();

        let gate = super::pass_gate(ws.id);
        let guard = gate.lock.lock().await;

        let db2 = db.clone();
        let handle = tokio::spawn(async move {
            super::resume_workspace(db2, vec![r], ws.id).await;
        });

        // While the gate is held, resume is parked on lock() → stale doc survives.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            repo::get_repo_map_doc(&db, ws.id).await.unwrap().is_some(),
            "resume must block on the pass-gate while it is held"
        );

        drop(guard);
        handle.await.unwrap();

        assert!(
            repo::get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "after the gate is released, resume runs and clears the stale map"
        );
    }

    #[test]
    fn markdown_covers_graph_detects_unprofiled_edges() {
        use std::collections::HashSet;
        let profiled: HashSet<i32> = [1, 2].into_iter().collect();
        // 3 still exists (unprofiled); 99 has been deleted.
        let current: HashSet<i32> = [1, 2, 3].into_iter().collect();
        let edge = |to: i32, rejected: bool| AgentRelation {
            to,
            kind: "lib".into(),
            via: "x".into(),
            confidence: 100,
            source: "manifest".into(),
            rejected,
            ..Default::default()
        };
        // All live edges within the profiled set → markdown covers the graph.
        let within = vec![(1, vec![edge(2, false)]), (2, vec![])];
        assert!(super::markdown_covers_graph(&profiled, &current, &within));

        // A profiled repo has a live edge to an UNPROFILED-but-current repo (3) →
        // not covered (the target node is missing from the analyst's markdown).
        let to_unprofiled = vec![(1, vec![edge(3, false)]), (2, vec![])];
        assert!(!super::markdown_covers_graph(&profiled, &current, &to_unprofiled));

        // An UNPROFILED-but-current repo (3) owns a live edge → not covered (its
        // source node is missing from the markdown).
        let from_unprofiled = vec![(1, vec![]), (3, vec![edge(1, false)])];
        assert!(!super::markdown_covers_graph(&profiled, &current, &from_unprofiled));

        // A REJECTED edge to an unprofiled repo is a tombstone, not a live edge → covered.
        let rejected_only = vec![(1, vec![edge(3, true)]), (2, vec![])];
        assert!(super::markdown_covers_graph(&profiled, &current, &rejected_only));

        // A live edge to a DELETED repo (99 ∉ current) is stale — the graph drops
        // it, so it must NOT block coverage. (Regression: round-9 bug where this
        // refused to republish the map forever after deleting an edge target.)
        let to_deleted = vec![(1, vec![edge(99, false)]), (2, vec![])];
        assert!(super::markdown_covers_graph(&profiled, &current, &to_deleted));
    }

    #[tokio::test]
    async fn seed_manifest_relations_skips_ambiguous_providers() {
        // When two repos provide the SAME manifest name, a first-wins pick would
        // seed a concrete-but-arbitrary edge. The ambiguous name must seed nothing
        // (let the agent decide), while a uniquely-provided name still seeds.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_seed_dup").await.unwrap();

        let (consumer, tc) = seed_repo_with_manifest(
            &db,
            ws.id,
            "consumer",
            "package.json",
            r#"{"name":"@acme/consumer","dependencies":{"@acme/dup":"^1","@acme/uniq":"^1"}}"#,
        )
        .await;
        // Two repos BOTH provide "@acme/dup" → ambiguous.
        let (_dup1, t1) = seed_repo_with_manifest(
            &db, ws.id, "dup1", "package.json", r#"{"name":"@acme/dup","dependencies":{}}"#,
        )
        .await;
        let (_dup2, t2) = seed_repo_with_manifest(
            &db, ws.id, "dup2", "package.json", r#"{"name":"@acme/dup","dependencies":{}}"#,
        )
        .await;
        // One repo uniquely provides "@acme/uniq".
        let (uniq, tu) = seed_repo_with_manifest(
            &db, ws.id, "uniq", "package.json", r#"{"name":"@acme/uniq","dependencies":{}}"#,
        )
        .await;

        super::seed_manifest_relations(&db, ws.id).await.unwrap();

        let rels: Vec<AgentRelation> = {
            let p = repo::get_repo_profile(&db, consumer.id).await.unwrap().unwrap();
            serde_json::from_str(&p.relations).unwrap()
        };
        assert!(
            !rels.iter().any(|r| r.via == "@acme/dup"),
            "ambiguous provider must not seed an arbitrary lib edge: {rels:?}"
        );
        assert!(
            rels.iter()
                .any(|r| r.to == uniq.id && r.via == "@acme/uniq" && r.source == "manifest"),
            "uniquely-provided dep must still seed a lib edge: {rels:?}"
        );

        let _ = std::fs::remove_dir_all(&tc);
        let _ = std::fs::remove_dir_all(&t1);
        let _ = std::fs::remove_dir_all(&t2);
        let _ = std::fs::remove_dir_all(&tu);
    }

    #[tokio::test]
    async fn seed_manifest_relations_preserves_existing_agent_and_user_relations() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_seed2").await.unwrap();

        let (repo_a, tmp_a) = seed_repo_with_manifest(
            &db,
            ws.id,
            "repo-a2",
            "package.json",
            r#"{"name":"@acme/app2","dependencies":{"@acme/lib2":"^1"}}"#,
        )
        .await;
        let (repo_b, tmp_b) = seed_repo_with_manifest(
            &db,
            ws.id,
            "repo-b2",
            "package.json",
            r#"{"name":"@acme/lib2","dependencies":{}}"#,
        )
        .await;

        // Pre-seed repo_a with an existing agent edge (http to B) and a user edge.
        let pre_rels = serde_json::to_string(&vec![
            AgentRelation {
                to: repo_b.id,
                kind: "http".into(),
                via: "GET /health".into(),
                confidence: 70,
                source: "agent".into(),
                rejected: false,
                ..Default::default()
            },
            AgentRelation {
                to: repo_b.id,
                kind: "grpc".into(),
                via: "Svc.Call".into(),
                confidence: 90,
                source: "user".into(),
                rejected: false,
                ..Default::default()
            },
        ])
        .unwrap();
        repo::set_repo_relations(&db, repo_a.id, &pre_rels).await.unwrap();

        super::seed_manifest_relations(&db, ws.id).await.unwrap();

        let rels_a: Vec<AgentRelation> = {
            let p = repo::get_repo_profile(&db, repo_a.id).await.unwrap().unwrap();
            serde_json::from_str(&p.relations).unwrap()
        };
        // Manifest lib edge must be present.
        assert!(
            rels_a.iter().any(|r| r.source == "manifest" && r.kind == "lib"),
            "manifest lib edge added"
        );
        // Prior user edge must survive.
        assert!(
            rels_a.iter().any(|r| r.source == "user" && r.kind == "grpc"),
            "prior user grpc edge preserved"
        );
        // Prior agent http edge must survive (different kind from manifest lib).
        assert!(
            rels_a.iter().any(|r| r.source == "agent" && r.kind == "http"),
            "prior agent http edge preserved"
        );

        let _ = std::fs::remove_dir_all(&tmp_a);
        let _ = std::fs::remove_dir_all(&tmp_b);
    }

    // ─── Finding 3: build_curator_prompt includes manifest edges pre-persistence ─

    #[tokio::test]
    async fn build_curator_prompt_includes_manifest_edges_before_persistence() {
        // On a first analysis, build_curator_prompt must include manifest-derived
        // lib edges in the prompt even before seed_manifest_relations has persisted
        // anything to the relations column (which would be "[]" on first run). This
        // ensures the agent sees the manifest floor as a hint. (Finding 3)
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_f3").await.unwrap();

        // Repo A: package.json requiring "@acme/lib" (provided by B).
        let (repo_a, tmp_a) = seed_repo_with_manifest(
            &db, ws.id, "app-f3",
            "package.json",
            r#"{"name":"app-f3","dependencies":{"@acme/lib":"*"}}"#,
        ).await;
        // Repo B: package.json providing "@acme/lib".
        let (repo_b, tmp_b) = seed_repo_with_manifest(
            &db, ws.id, "lib-f3",
            "package.json",
            r#"{"name":"@acme/lib","version":"1.0.0"}"#,
        ).await;

        // Upsert full profiles so both repos are "fully profiled".
        repo::upsert_repo_profile(&db, repo_a.id, "frontend", "[]", "the app", "[]", "agent", "")
            .await.unwrap();
        repo::upsert_repo_profile(&db, repo_b.id, "backend", "[]", "the lib", "[]", "agent", "")
            .await.unwrap();

        // Do NOT call seed_manifest_relations — the relations column is "[]" for both.
        let pa = repo::get_repo_profile(&db, repo_a.id).await.unwrap().unwrap();
        let pb = repo::get_repo_profile(&db, repo_b.id).await.unwrap().unwrap();
        assert_eq!(pa.relations, "[]", "precondition: relations column is empty");
        assert_eq!(pb.relations, "[]", "precondition: relations column is empty");

        // Build the curator prompt for these two repos.
        let prompt = super::build_curator_prompt(&[(repo_a.clone(), pa), (repo_b.clone(), pb)]);

        // The prompt must mention a manifest lib edge from app-f3 → lib-f3 even
        // though seed_manifest_relations was never called.
        assert!(
            prompt.contains("manifest edges") && prompt.contains(&repo_b.id.to_string()),
            "curator prompt must include manifest edge hint before persistence: {prompt}"
        );

        let _ = std::fs::remove_dir_all(&tmp_a);
        let _ = std::fs::remove_dir_all(&tmp_b);
    }

    #[tokio::test]
    async fn build_curator_prompt_skips_ambiguous_provider_hints() {
        // The prompt's manifest-edge hints must apply the SAME ambiguity filter as
        // seed_manifest_relations: a name provided by 2+ repos seeds no hint (else
        // the agent could be steered into persisting an arbitrary wrong edge), while
        // a uniquely-provided name still produces a hint.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_amb_hint").await.unwrap();

        let (consumer, tc) = seed_repo_with_manifest(
            &db, ws.id, "consumer-h",
            "package.json",
            r#"{"name":"@acme/consumer-h","dependencies":{"@acme/dup":"*","@acme/uniq":"*"}}"#,
        ).await;
        let (_d1, t1) = seed_repo_with_manifest(
            &db, ws.id, "dup1-h", "package.json", r#"{"name":"@acme/dup","version":"1"}"#,
        ).await;
        let (_d2, t2) = seed_repo_with_manifest(
            &db, ws.id, "dup2-h", "package.json", r#"{"name":"@acme/dup","version":"1"}"#,
        ).await;
        let (uniq, tu) = seed_repo_with_manifest(
            &db, ws.id, "uniq-h", "package.json", r#"{"name":"@acme/uniq","version":"1"}"#,
        ).await;

        let mut slice = Vec::new();
        for r in [&consumer, &_d1, &_d2, &uniq] {
            let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
            slice.push((r.clone(), p));
        }
        let prompt = super::build_curator_prompt(&slice);

        assert!(
            !prompt.contains("@acme/dup"),
            "ambiguous provider must not produce a manifest-edge hint: {prompt}"
        );
        assert!(
            prompt.contains("@acme/uniq") && prompt.contains(&uniq.id.to_string()),
            "uniquely-provided dep must still produce a hint: {prompt}"
        );

        let _ = std::fs::remove_dir_all(&tc);
        let _ = std::fs::remove_dir_all(&t1);
        let _ = std::fs::remove_dir_all(&t2);
        let _ = std::fs::remove_dir_all(&tu);
    }

    // ─── parse_repo_class: category/domains/exposed/consumes ───────────────

    #[test]
    fn parse_repo_class_with_category_and_domains() {
        // A reply that includes all four new fields: they are parsed correctly.
        let text = r#"
Some prose here.
{"tier":"backend","summary":"order service","stack":["go"],
 "components":[],"category":"biz","domains":["orders","payments"],
 "exposed":["POST /orders","GET /orders/:id"],"consumes":["auth-svc","product-svc"]}
"#;
        let wire = super::parse_repo_class(text).expect("should parse");
        assert_eq!(wire.tier, "backend");
        assert_eq!(wire.category.as_deref(), Some("biz"));
        assert_eq!(wire.domains.as_deref(), Some(["orders".to_string(), "payments".to_string()].as_slice()));
        assert_eq!(wire.exposed, vec!["POST /orders".to_string(), "GET /orders/:id".to_string()]);
        assert_eq!(wire.consumes, vec!["auth-svc".to_string(), "product-svc".to_string()]);
    }

    #[test]
    fn parse_repo_class_missing_new_fields_defaults() {
        // A reply with only the original fields: category/domains/exposed/consumes
        // default to None / [] and the parse still succeeds.
        let text = r#"{"tier":"frontend","summary":"web app","stack":["react"],"components":[]}"#;
        let wire = super::parse_repo_class(text).expect("should parse even without new fields");
        assert!(wire.category.is_none(), "absent category → None");
        assert!(wire.domains.is_none(), "absent domains → None");
        assert!(wire.exposed.is_empty());
        assert!(wire.consumes.is_empty());
    }

    #[tokio::test]
    async fn persist_repo_class_writes_category_and_domains() {
        // When the agent provides category + domains, they are written to the profile.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_catdom").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "order service".into(),
            stack: None,
            components: None,
            category: Some("biz".into()),
            domains: Some(vec!["orders".into(), "payments".into()]),
            exposed: vec!["POST /orders".into()],
            consumes: vec!["auth-svc".into()],
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.category, "biz");
        let doms: Vec<String> = serde_json::from_str(&p.domains).unwrap();
        assert_eq!(doms, vec!["orders".to_string(), "payments".to_string()]);
    }

    #[tokio::test]
    async fn persist_repo_class_preserves_prior_category_domains_on_omission() {
        // When the agent omits category/domains (None), the prior values are preserved.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_catdom2").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc2", "/tmp/svc2", "main", "", true)
            .await
            .unwrap();
        // Seed an existing profile with category/domains.
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "svc2 summary", "[]", "agent", "")
            .await
            .unwrap();
        repo::set_repo_category_domains(&db, r.id, "core", r#"["auth"]"#).await.unwrap();
        // Omit category/domains entirely (None = absent, not "" or []).
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "updated summary".into(),
            stack: None,
            components: None,
            category: None,
            domains: None,
            exposed: vec![],
            consumes: vec![],
        };
        super::persist_repo_class(&db, &r, wire, "sha2").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.category, "core", "prior category preserved when agent omits it");
        let doms: Vec<String> = serde_json::from_str(&p.domains).unwrap();
        assert_eq!(doms, vec!["auth".to_string()], "prior domains preserved when agent omits them");
    }

    // ─── Finding 4: explicit empty clears; None preserves ──────────────────

    #[tokio::test]
    async fn persist_repo_class_explicit_empty_domains_clears_prior() {
        // An explicit empty domains (Some([])) must clear prior domains;
        // an absent (None) must preserve them.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws_f4").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "svc summary", "[]", "agent", "")
            .await
            .unwrap();
        repo::set_repo_category_domains(&db, r.id, "biz", r#"["orders","payments"]"#).await.unwrap();

        // Pass 1: explicit empty domains clears prior.
        let wire_clear = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "svc summary".into(),
            stack: None,
            components: None,
            category: Some("biz".into()),
            domains: Some(vec![]),    // explicit empty → should clear
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire_clear, "sha1").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        let doms: Vec<String> = serde_json::from_str(&p.domains).unwrap();
        assert!(doms.is_empty(), "explicit empty domains must clear prior domains");

        // Seed category for the next check.
        repo::set_repo_category_domains(&db, r.id, "core", r#"["auth"]"#).await.unwrap();

        // Pass 2: absent domains (None) preserves prior.
        let wire_preserve = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "svc summary".into(),
            stack: None,
            components: None,
            category: None,  // absent → preserve
            domains: None,   // absent → preserve
            ..Default::default()
        };
        super::persist_repo_class(&db, &r, wire_preserve, "sha2").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        let doms: Vec<String> = serde_json::from_str(&p.domains).unwrap();
        assert_eq!(doms, vec!["auth".to_string()], "absent domains must preserve prior");
        assert_eq!(p.category, "core", "absent category must preserve prior");
    }
}
