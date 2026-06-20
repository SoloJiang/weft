//! The workspace Curator (ARCHITECTURE §4.9, §4.11), now a PURE AGENT pipeline.
//! There is no deterministic manifest engine: a bounded, read-only coding agent
//! reads each repo deeply, classifies its tier (frontend / gateway / backend),
//! summarizes it, surfaces monorepo sub-components, and reports the cross-repo
//! relations it sees. Findings persist on `repo_profile`; the graph is rebuilt
//! from them. A user edit (source = "user") always outranks re-analysis.

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
    /// "frontend" | "gateway" | "backend" | "" (unclassified / analyzing).
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
    let source = combine_source(
        owns_summary(prior_source) || summary.is_some(),
        owns_tier(prior_source) || tier.is_some(),
    );
    repo::upsert_repo_profile(db, repo_id, &new_tier, &stack, &new_summary, &components, source, &commit)
        .await
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
            analysis_state: run_phase(repo.id).to_string(),
            analysis_error: run_error(repo.id),
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
        analysis_state: run_phase(repo.id).to_string(),
        analysis_error: run_error(repo.id),
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
            analyze_workspace_coalesced(&db, workspace_id).await;
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

/// Extract the curator agent's relations from its free-form reply (tolerant of
/// markdown fences / surrounding prose). Scans EVERY balanced object and returns
/// the LAST that deserializes as a relations payload — the prompt asks for the
/// JSON as the final thing, and an earlier prose/config `{...}` must not hide it.
/// `None` when no object has a `relations` array (timed-out/malformed reply) so
/// the caller leaves the graph intact; `Some([])` is an explicit "no relations".
pub fn parse_curator_output(text: &str) -> Option<Vec<CuratorRelation>> {
    json_objects(text)
        .into_iter()
        .rev()
        .find_map(|obj| serde_json::from_str::<CuratorWire>(obj).ok())
        .map(|w| w.relations)
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
#[derive(Debug, serde::Deserialize)]
struct RepoClassWire {
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

/// The shared definition of the three architectural tiers, embedded in both the
/// per-repo classification prompt and the cross-repo relations prompt so the
/// agent applies one consistent taxonomy.
const TIER_GUIDE: &str = "Tiers:\n\
- frontend: user-facing client — web SPA/MPA, mobile, desktop UI, static site.\n\
- gateway: the MIDDLE layer between frontend and backend — API gateway, BFF \
(backend-for-frontend), aggregator, edge service, reverse proxy, GraphQL gateway. \
It mostly orchestrates/forwards to other services rather than owning core domain \
data.\n\
- backend: a service that owns domain logic and/or data — REST/gRPC services, \
workers, batch jobs, databases-of-record, shared libraries that back them.";

/// Per-repo DEEP classification prompt. The agent runs with cwd AT this repo and
/// is told to read widely (subdirectories, monorepo packages) before emitting one
/// strict-JSON object: the repo's tier + one-line summary + stack, plus any
/// monorepo sub-components for the map's "expanded" view.
fn build_repo_class_prompt(repo_name: &str) -> String {
    format!(
        "Analyze the repository at the current working directory (name: {repo_name}) \
DEEPLY and READ-ONLY. Do NOT stop at the top-level manifest: read the source \
layout, entry points, configs, and — if this is a monorepo — its packages/apps/\
services subdirectories, so your classification reflects what the code actually \
does.\n\n{TIER_GUIDE}\n\nClassify the repository into exactly one top-level tier. \
If the repo is a monorepo containing two or more deployable/publishable internal \
packages or services, list each as a component with its own tier; a single-purpose \
repo has no components.\n\nAs the LAST thing in your reply, output a single JSON \
object and nothing after it:\n\
{{\"tier\":\"frontend|gateway|backend\",\"summary\":\"<one line; name the key \
internal modules if it is a monorepo>\",\"stack\":[\"<language/framework tags>\"],\
\"components\":[{{\"name\":\"<package/service>\",\"path\":\"<relative path>\",\
\"tier\":\"frontend|gateway|backend\",\"summary\":\"<one line>\",\
\"deps\":[\"<sibling component name it depends on>\"]}}]}}\n\
Rules: pick the single tier that best fits the repo as a whole. `components` is \
[] unless this is a monorepo with 2+ internal packages/services. `deps` lists only \
SIBLING components in THIS repo. Keep summaries to one line."
    )
}

/// Cross-repo relations prompt: lists every classified repo (id/name/tier/path/
/// summary) and asks for STRICT JSON relations keyed by repo id — runtime, infra,
/// AND declared library dependencies (there is no manifest floor, so the agent
/// reports `lib` edges too).
fn build_curator_prompt(repos: &[(repo_ref::Model, repo_profile::Model)]) -> String {
    let mut lines = String::new();
    for (r, p) in repos {
        let tier = if p.role.is_empty() { "unknown" } else { p.role.as_str() };
        lines.push_str(&format!(
            "- id={} name={:?} tier={} path={:?}\n  summary: {}\n",
            r.id, r.name, tier, r.local_git_path, p.summary
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
a frontend almost always depends on the gateway/backend that serves its data, and a \
gateway on the backends it aggregates — assert such an edge whenever the code \
supports it, even with no shared package.\n\n\
{TIER_GUIDE}\n\nRepositories:\n{lines}\nRead each repo's code and config at its \
path (READ-ONLY — change nothing). Then, as the LAST thing in your reply, output a \
single JSON object and nothing after it:\n\
{{\"relations\":[{{\"from\":<id>,\"to\":<id>,\"kind\":\"http|grpc|queue|infra|lib\",\
\"via\":\"<short evidence>\",\"confidence\":<0-100>}}]}}\n\
Rules: `from` and `to` MUST be ids from the list above and must differ. Use kind \
`lib` for a declared package/module dependency, the runtime kinds otherwise. \
Include a relation when you have concrete evidence — for a runtime edge that means a \
matching endpoint / topic / host / contract across the two repos, NOT necessarily a \
shared import. `via` is a short label (e.g. \"POST /orders\", \"orders-topic\", \
\"shared postgres\", \"@acme/api-client\"). If you find none, output \
{{\"relations\":[]}}."
    )
}

/// A streaming chunk from a curator agent run, forwarded to the caller's sink so
/// the analysis process can stream into the UI.
enum AnalysisEvent<'a> {
    /// A piece of assistant text — a token delta on the app-server transport, or
    /// the full message at completion on the exec transport.
    Delta(&'a str),
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
    use crate::lead_chat::proto::ChatEvent;
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
        let mut texts: Vec<String> = Vec::new();
        let mut deltas = String::new();
        let mut turn_failed = false;
        let mut saw_turn_end = false;
        let collect = async {
            while let Some(msg) = rx.recv().await {
                match msg {
                    ThreadMsg::Event(ChatEvent::TextDelta { text }) => {
                        on_event(AnalysisEvent::Delta(&text));
                        deltas.push_str(&text);
                    }
                    ThreadMsg::Event(ChatEvent::Assistant { texts: t, .. }) => texts.extend(t),
                    ThreadMsg::Event(ChatEvent::TurnEnd { is_error, .. }) => {
                        saw_turn_end = true;
                        turn_failed = is_error;
                        break;
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
        let text = if texts.is_empty() { deltas } else { texts.join("\n") };
        // Accept the reply ONLY on a clean, non-error TurnEnd with text. A timeout
        // (collect cancelled) OR a mid-turn channel close — the child crashed or
        // disconnected after partial text, so `rx.recv()` hits EOF and the loop
        // exits WITHOUT a TurnEnd — both leave `saw_turn_end` false. Treat either as
        // a transport failure (`Err`) so `run_streaming_agent` falls back to exec,
        // instead of returning partial/empty output as if the app-server succeeded.
        if turn_failed || !saw_turn_end || text.trim().is_empty() {
            anyhow::bail!(
                "codex app-server turn did not complete cleanly (error={turn_failed}, sawTurnEnd={saw_turn_end})"
            );
        }
        Ok::<String, anyhow::Error>(text)
    }
    .await;
    client.shutdown().await;
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
    use crate::lead_chat::proto::ChatEvent;
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
    let mut texts: Vec<String> = Vec::new();
    let mut deltas = String::new();
    let collect = async {
        while let Some(line) = reader.next_line().await? {
            match adapter.parse_line(&line) {
                ChatEvent::Assistant { texts: t, .. } => {
                    for s in &t {
                        on_event(AnalysisEvent::Delta(s));
                    }
                    texts.extend(t);
                }
                ChatEvent::TextDelta { text } => {
                    on_event(AnalysisEvent::Delta(&text));
                    deltas.push_str(&text);
                }
                ChatEvent::TurnEnd { .. } => break,
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let _ = tokio::time::timeout(CURATOR_TIMEOUT, collect).await;
    // Close stdout so a child blocked on a full pipe unblocks, then SIGKILL and
    // reap (kill().await waits for exit — avoids a zombie on timeout). On a clean
    // EOF the child has already exited and this is a no-op.
    drop(reader);
    let _ = child.kill().await;
    Ok(if texts.is_empty() {
        deltas
    } else {
        texts.join("\n")
    })
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
        let merged = crate::profile::merge_relations(&existing, &fresh);
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
    phase: &'static str,
    error: Option<String>,
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
    g.insert(id, RunInfo { phase: "running", error: None });
    true
}

/// Analysis finished successfully: the persisted canonical tier now drives the
/// `analyzed` state, so the transient run entry is dropped (→ "idle").
fn run_finish_ok(id: i32) {
    run_lock().remove(&id);
}

/// Analysis failed/timed out: keep a visible `failed` entry with the error, so
/// the detail panel shows it + a manual retry instead of a silent spinner.
fn run_finish_err(id: i32, error: String) {
    run_lock().insert(id, RunInfo { phase: "failed", error: Some(error) });
}

fn run_phase(id: i32) -> &'static str {
    run_lock().get(&id).map(|r| r.phase).unwrap_or("idle")
}

fn run_error(id: i32) -> Option<String> {
    run_lock().get(&id).and_then(|r| r.error.clone())
}

/// Clear the `failed` run-state for every repo in a workspace, so an EXPLICIT
/// user re-run ("Analyze deps") isn't suppressed by the background anti-storm
/// skip that ignores `failed` repos. Returns to idle → eligible for re-analysis.
pub async fn clear_failed_states(db: &Db, workspace_id: i32) {
    let Ok(repos) = repo::list_repos(db, workspace_id).await else {
        return;
    };
    for r in repos {
        if run_phase(r.id) == "failed" {
            run_finish_ok(r.id);
        }
    }
}

/// Whether a repo currently carries a usable (canonical) tier classification.
/// This is the success criterion for an analysis run: the repo is classified iff
/// it has a canonical tier — one just written this run, or a prior one preserved
/// because the agent's reply was unusable. A fresh placeholder left without a
/// canonical tier means the run failed (whether the reply was unparseable, or
/// parseable but dropped by persist_repo_class's no-op rules).
async fn classified_now(db: &Db, repo_id: i32) -> bool {
    repo::get_repo_profile(db, repo_id)
        .await
        .ok()
        .flatten()
        .map(|p| profile::normalize_tier(&p.role).is_some())
        .unwrap_or(false)
}

#[cfg(test)]
fn run_state_clear_all_for_test() {
    run_lock().clear();
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
        // lose its retryable error. Mark a visible, retryable failure with a clear
        // reason instead. (Background passes pre-filter missing checkouts, so this
        // only fires on an explicit reprofile_repo retry.)
        const GONE: &str = "repository checkout not found on disk";
        run_finish_err(repo.id, GONE.into());
        emit_repo_analysis(repo.workspace_id, repo.id, "failed", None, Some(GONE));
        return Ok(());
    }
    // Dedupe: a manual reprofile racing the background pass must not double-spawn
    // the agent for the same repo.
    if !run_begin(repo.id) {
        return Ok(());
    }
    let (ws, rid) = (repo.workspace_id, repo.id);
    emit_repo_analysis(ws, rid, "started", None, None);
    // Also refresh the graph so an UNSELECTED card flips to `running` immediately:
    // the per-repo `started` stream is only observed for the selected repo, and
    // `repo-graph-updated` otherwise fires only when the (minutes-long) run ends —
    // without this, a reprofiled-but-unselected card sits stale for the whole run.
    emit_graph_updated(ws);
    let tool = crate::tools::default_tool(db).await;
    let prompt = build_repo_class_prompt(&repo.name);
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
                    // Success iff the repo now carries a usable (canonical) tier —
                    // freshly written this run, or a prior one preserved when the
                    // reply was unusable. A placeholder left unclassified is a real
                    // failure (visible + retryable), covering BOTH the unparseable
                    // and the parseable-but-no-op cases uniformly.
                    if classified_now(db, repo.id).await {
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
            emit_repo_analysis(ws, rid, "done", None, None);
        }
        Err(e) => {
            // Surface the failure (visible + retryable) instead of silently leaving
            // the placeholder to read as a perpetual "analyzing".
            let msg = e.to_string();
            run_finish_err(rid, msg.clone());
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
pub async fn analyze_workspace(db: &Db, workspace_id: i32) -> Result<()> {
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
        // A FAILED repo is not auto-retried: this auto-pass runs on every graph()
        // read, so retrying here would storm a persistently-failing repo. It keeps
        // its visible `failed` state until the user hits 重新分析 (reprofile_repo,
        // which clears the failed entry and forces a fresh run).
        if run_phase(r.id) == "failed" {
            continue;
        }
        if !needs_classification(db, r).await {
            continue;
        }
        let _ = profile_repo_agent(db, r).await;
        emit_graph_updated(workspace_id);
    }

    // Stage 2: cross-repo relations — needs at least two repos to relate.
    analyze_relations(db, workspace_id).await
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
    if let Ok(text) = run_streaming_agent(&tool, &cwd, &prompt, &mut |_| {}).await {
        if let Some(relations) = parse_curator_output(&text) {
            persist_relations(db, &profiled, &relations).await?;
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

/// Per-workspace serialization for analysis passes: an async lock (two passes for
/// one workspace never overlap) plus a `dirty` flag so a request that lands while
/// a pass is running coalesces into one rerun instead of a parallel pass.
#[derive(Clone)]
struct PassGate {
    lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
pub async fn analyze_workspace_coalesced(db: &Db, workspace_id: i32) {
    use std::sync::atomic::Ordering;
    let gate = pass_gate(workspace_id);
    gate.dirty.store(true, Ordering::SeqCst);
    let _g = gate.lock.lock().await;
    // Drain: run until no new request landed during the previous run. The holder
    // covers waiters' requests, so once it exits they acquire the lock, find
    // `dirty` already cleared, and return immediately — having awaited completion.
    while gate.dirty.swap(false, Ordering::SeqCst) {
        let _ = analyze_workspace(db, workspace_id).await;
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
    fn run_state_transitions_and_dedupe() {
        super::run_state_clear_all_for_test();
        assert!(super::run_begin(101), "first begin starts it");
        assert!(!super::run_begin(101), "second begin deduped while running");
        assert_eq!(super::run_phase(101), "running");
        super::run_finish_err(101, "boom".into());
        assert_eq!(super::run_phase(101), "failed");
        assert_eq!(super::run_error(101).as_deref(), Some("boom"));
        assert!(super::run_begin(101), "a failed repo can be restarted");
        super::run_finish_ok(101);
        assert_eq!(super::run_phase(101), "idle");
        assert_eq!(super::run_error(101), None);
    }

    #[tokio::test]
    async fn clear_failed_states_clears_only_failed() {
        // An explicit "Analyze deps" clears failed repos so the background
        // anti-storm skip doesn't suppress the retry; a running repo is untouched.
        super::run_state_clear_all_for_test();
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let a = repo::add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "").await.unwrap();
        let b = repo::add_repo_ref(&db, ws.id, "b", "/tmp/b", "main", "").await.unwrap();
        super::run_finish_err(a.id, "boom".into());
        super::run_begin(b.id);
        super::clear_failed_states(&db, ws.id).await;
        assert_eq!(super::run_phase(a.id), "idle", "failed → cleared, eligible for retry");
        assert_eq!(super::run_phase(b.id), "running", "a running repo is left alone");
    }

    #[tokio::test]
    async fn classified_now_requires_canonical_tier() {
        // The success criterion for a run: a repo counts as classified ONLY with a
        // canonical tier. No profile, a non-canonical/legacy tier, or a tier-less
        // placeholder all read as "not classified" → a fresh run that lands there
        // is a failure (covers both the unparseable and parseable-but-no-op replies),
        // while a re-analysis preserving a prior canonical tier stays a success.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "").await.unwrap();
        assert!(!super::classified_now(&db, r.id).await, "no profile → not classified");
        repo::upsert_repo_profile(&db, r.id, "service", "[]", "s", "[]", "agent", "")
            .await
            .unwrap();
        assert!(!super::classified_now(&db, r.id).await, "non-canonical tier → not classified");
        repo::upsert_repo_profile(&db, r.id, "backend", "[]", "an api", "[]", "agent", "")
            .await
            .unwrap();
        assert!(super::classified_now(&db, r.id).await, "canonical tier → classified");
    }

    #[tokio::test]
    async fn missing_checkout_retry_stays_failed() {
        // A retry whose checkout was moved/deleted must keep a visible, retryable
        // failure — not silently drop to idle. profile_repo_agent's cwd guard fires
        // before any agent spawn, so this is exercisable without a real run.
        super::run_state_clear_all_for_test();
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "gone", "/nonexistent/weft/checkout/zzz", "main", "")
            .await
            .unwrap();
        super::profile_repo_agent(&db, &r).await.unwrap();
        assert_eq!(super::run_phase(r.id), "failed", "missing checkout stays failed");
        assert!(super::run_error(r.id).is_some(), "with a reason the UI can show");
    }

    #[tokio::test]
    async fn view_surfaces_failed_analysis_state() {
        super::run_state_clear_all_for_test();
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "")
            .await
            .unwrap();
        super::run_finish_err(r.id, "codex failed".into());
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

    #[tokio::test]
    async fn graph_builds_edges_from_agent_relations() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
            .await
            .unwrap();
        let api = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "")
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
    async fn persist_repo_class_migrates_legacy_user_role() {
        // An upgraded db may carry a legacy role on a user-owned row; the agent
        // pass keeps the user's summary but migrates the invalid tier.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "gateway", "[]", "mine", "[]", "user", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "agent".into(),
            stack: None,
            components: None,
        };
        super::persist_repo_class(&db, &r, wire, "").await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "gateway", "a valid user-pinned tier is not overwritten");
    }

    #[tokio::test]
    async fn persist_repo_class_fills_empty_user_tier_from_agent() {
        // A user-owned row with an EMPTY tier (e.g. a summary-only edit on a
        // placeholder) adopts the agent's valid tier rather than staying blank;
        // the user's summary is still preserved.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "service".into(), // not one of frontend|gateway|backend
            summary: "agent".into(),
            stack: None,
            components: None,
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
        let r = repo::add_repo_ref(&db, ws.id, "mono", "/tmp/mono", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
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
        let r = repo::add_repo_ref(&db, ws.id, "gone", "/tmp/gone", "main", "")
            .await
            .unwrap();
        repo::delete_repo_cascade(&db, r.id).await.unwrap();
        let wire = super::RepoClassWire {
            name: None,
            tier: "backend".into(),
            summary: "s".into(),
            stack: None,
            components: None,
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
        let r = repo::add_repo_ref(&db, ws.id, "gone", "/tmp/gone", "main", "")
            .await
            .unwrap();
        repo::delete_repo_cascade(&db, r.id).await.unwrap();
        assert!(super::edit_profile(&db, r.id, Some("s"), Some("frontend")).await.is_err());
        assert!(repo::get_repo_profile(&db, r.id).await.unwrap().is_none());
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
        let rels = super::parse_curator_output(reply).expect("parsed despite bad confidence");
        assert_eq!(rels.len(), 3);
        assert_eq!(rels[0].confidence, 0); // "high" → 0
        assert_eq!(rels[2].confidence, 100); // 250 clamped
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
        let r = repo::add_repo_ref(&db, ws.id, "fresh", "/tmp/fresh", "main", "")
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
        let rels = super::parse_curator_output(reply).expect("parsed a relations object");
        assert_eq!(rels.len(), 1);
        assert_eq!((rels[0].from, rels[0].to), (Some(1), Some(2)));
        assert_eq!(rels[0].kind, "http");
        assert_eq!(rels[0].via, "GET /orders");
        assert_eq!(rels[0].confidence, 80);
    }

    #[test]
    fn parse_curator_output_distinguishes_unparseable_from_explicit_empty() {
        // Unparseable / timed-out replies → None (caller must NOT wipe the graph).
        assert!(super::parse_curator_output("no json here").is_none());
        assert!(super::parse_curator_output("").is_none());
        // An object without a `relations` array is treated as unparseable too.
        assert!(super::parse_curator_output(r#"{"notes":"none found"}"#).is_none());
        // An explicit empty relations array → Some([]) (a real "found nothing").
        let explicit = super::parse_curator_output(r#"{"relations":[]}"#);
        assert!(explicit.is_some_and(|v| v.is_empty()));
    }

    #[test]
    fn parse_curator_output_skips_earlier_non_result_objects() {
        // A prose/config object appears BEFORE the real relations object; the
        // earlier brace block must not hide the valid result later in the reply.
        let reply = "I looked at the config `{\"port\": 8080}` and traced the call.\n\n\
            {\"relations\":[{\"from\":1,\"to\":2,\"kind\":\"http\",\"via\":\"GET /x\",\"confidence\":70}]}";
        let rels = super::parse_curator_output(reply).expect("found the later relations object");
        assert_eq!(rels.len(), 1);
        assert_eq!((rels[0].from, rels[0].to, rels[0].kind.as_str()), (Some(1), Some(2), "http"));
    }

    #[tokio::test]
    async fn persist_relations_groups_filters_and_clears() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
            .await
            .unwrap();
        let api = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "")
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
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
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
}
