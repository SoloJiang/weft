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

/// A profile as the UI sees it: decoded fields + repo name + live staleness.
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
    pub stale: bool,
    pub analyzed: bool,
    pub components: Vec<Component>,
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

/// Apply a user edit to the opinion fields (one-line summary + tier); marks the
/// profile user-owned so future re-analysis won't clobber it.
pub async fn edit_profile(
    db: &Db,
    repo_id: i32,
    summary: &str,
    tier: &str,
) -> Result<repo_profile::Model> {
    // A repo can be edited while it is still an "analyzing" placeholder with no
    // profile row yet (the agent-only pipeline creates rows lazily). Upsert one
    // rather than erroring, so the human's calibration always persists; factual
    // fields default empty until the agent fills them.
    let existing = repo::get_repo_profile(db, repo_id).await?;
    let (stack, components, commit) = match &existing {
        Some(p) => (p.stack.clone(), p.components.clone(), p.profiled_commit.clone()),
        None => ("[]".to_string(), "[]".to_string(), String::new()),
    };
    // Tolerate any tier string the UI sends; "" clears the classification.
    let tier = profile::normalize_tier(tier).unwrap_or_default();
    repo::upsert_repo_profile(db, repo_id, &tier, &stack, summary, &components, "user", &commit).await
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
            stale: false,
            analyzed: false,
            components: Vec::new(),
        };
    };
    let live = git::head_commit(Path::new(&repo.local_git_path)).ok();
    let stale = match (&live, p.profiled_commit.as_str()) {
        (Some(_), "") => true,
        (Some(head), at) => head != at,
        (None, _) => false, // can't tell (not a git repo / no commits)
    };
    ProfileView {
        repo_id: repo.id,
        repo_name: repo.name.clone(),
        tier: p.role.clone(),
        stack: arr(&p.stack),
        summary: p.summary.clone(),
        source: p.source.clone(),
        profiled_commit: p.profiled_commit.clone(),
        stale,
        analyzed: true,
        components: comps(&p.components),
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
    Ok(Graph { nodes, edges })
}

// ─────────────────────────── agent curator ───────────────────────────
//
// The whole curator: a bounded, read-only agent classifies each repo's tier and
// surfaces monorepo sub-components (per-repo deep pass), then reports cross-repo
// relations (HTTP, gRPC, queues, shared infra, and declared libs). Findings
// persist on `repo_profile` and rebuild into `graph()`'s nodes + edges.

/// One relation as the curator agent reports it: flat, with an explicit `from`
/// (the stored `AgentRelation` is per-producer, so `from` is implicit there).
/// Lenient: missing fields default.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CuratorRelation {
    pub from: i32,
    pub to: i32,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub via: String,
    #[serde(default)]
    pub confidence: u8,
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
            None => break, // unbalanced tail — nothing more to find
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
/// left intact rather than blanked.
#[derive(Debug, serde::Deserialize)]
struct RepoClassWire {
    tier: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    stack: Vec<String>,
    #[serde(default)]
    components: Vec<Component>,
}

/// Extract the per-repo classification from the agent's free-form reply, same
/// tolerance as `parse_curator_output`: scan every balanced object, take the LAST
/// that carries a `tier`. `None` for a timed-out/malformed reply.
fn parse_repo_class(text: &str) -> Option<RepoClassWire> {
    json_objects(text)
        .into_iter()
        .rev()
        .find_map(|obj| serde_json::from_str::<RepoClassWire>(obj).ok())
}

const CURATOR_SYSTEM_PROMPT: &str = "You are a read-only repository analyst. You \
may read code and configuration as deeply as you need, but you must never modify, \
create, or delete files, and never run mutating commands.";

const CURATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

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
        "Map how these repositories in one workspace depend on each other — both at \
RUNTIME / through shared infrastructure (HTTP/REST, gRPC, message queues, shared \
databases/infra) AND through declared library/package dependencies and cross-repo \
internal imports.\n\n{TIER_GUIDE}\n\nRepositories:\n{lines}\nRead each repo's code \
and config at its path (READ-ONLY — change nothing). Then, as the LAST thing in \
your reply, output a single JSON object and nothing after it:\n\
{{\"relations\":[{{\"from\":<id>,\"to\":<id>,\"kind\":\"http|grpc|queue|infra|lib\",\
\"via\":\"<short evidence>\",\"confidence\":<0-100>}}]}}\n\
Rules: `from` and `to` MUST be ids from the list above and must differ. Use kind \
`lib` for a declared package/module dependency, the runtime kinds otherwise. Only \
include a relation you have concrete evidence for. `via` is a short label (e.g. \
\"POST /orders\", \"orders-topic\", \"shared postgres\", \"@acme/api-client\"). If \
you find none, output {{\"relations\":[]}}."
    )
}

/// Run the resolved coding agent once over `cwd`, read-only, feeding `prompt`,
/// and return its final assistant text. Reuses the per-tool adapter for argv +
/// line parsing (claude reads stdin as a stream-json envelope; per-turn tools
/// carry the message on argv). Best-effort and bounded by `CURATOR_TIMEOUT`: a
/// timeout or early exit returns whatever text was collected. We never write
/// files, persist a session, or emit UI events.
async fn run_agent_once(tool: &str, cwd: &Path, prompt: &str) -> Result<String> {
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
        "codex" => vec!["--sandbox".into(), "read-only".into()],
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
                ChatEvent::Assistant { texts: t, .. } => texts.extend(t),
                ChatEvent::TextDelta { text } => deltas.push_str(&text),
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
        if rel.from == rel.to || !ids.contains(&rel.from) || !ids.contains(&rel.to) {
            continue;
        }
        // Normalize the agent's kind to the canonical lowercase set; drop anything
        // unrecognized so the graph can't carry an edge calibrate_edges can't match.
        let Some(kind) = crate::profile::normalize_relation_kind(&rel.kind) else {
            continue;
        };
        by_from
            .entry(rel.from)
            .or_default()
            .push(crate::profile::AgentRelation {
                to: rel.to,
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
pub fn try_claim_backfill(workspace_id: i32) -> bool {
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

/// Persist one repo's deep-pass classification: tier + stack + summary +
/// components. Factual fields (stack/components) always refresh; the opinion
/// fields (tier/summary) are preserved when the user has pinned them
/// (source = "user"). Component tiers are normalized to the canonical set.
async fn persist_repo_class(db: &Db, repo: &repo_ref::Model, wire: RepoClassWire) -> Result<()> {
    let prior = repo::get_repo_profile(db, repo.id).await?;
    let user_owned = prior.as_ref().map(|p| p.source == "user").unwrap_or(false);

    // Drop nameless components (a malformed sub-object) but keep the rest, so one
    // bad entry never discards the whole classification; normalize their tiers.
    let components: Vec<Component> = wire
        .components
        .into_iter()
        .filter(|c| !c.name.trim().is_empty())
        .map(|mut c| {
            c.tier = profile::normalize_tier(&c.tier).unwrap_or_default();
            c
        })
        .collect();
    let comps_json = serde_json::to_string(&components).unwrap_or_else(|_| "[]".into());
    let stack_json = json_strs(&wire.stack);
    let commit = git::head_commit(Path::new(&repo.local_git_path)).unwrap_or_default();

    let agent_tier = profile::normalize_tier(&wire.tier).unwrap_or_default();
    // Keep a user-pinned summary/tier; only an upgraded db's non-empty LEGACY role
    // (service/app/…) migrates to the agent's tier. A valid tier and an
    // intentional empty ("Other") choice are both preserved. `filter` avoids an
    // `expect` here (production paths must not panic).
    let (tier, summary, source) = match prior.as_ref().filter(|_| user_owned) {
        Some(p) => {
            let tier = if p.role.is_empty() {
                String::new()
            } else {
                profile::normalize_tier(&p.role).unwrap_or(agent_tier)
            };
            (tier, p.summary.clone(), "user")
        }
        None => (agent_tier, wire.summary, "agent"),
    };

    repo::upsert_repo_profile(
        db, repo.id, &tier, &stack_json, &summary, &comps_json, source, &commit,
    )
    .await?;
    Ok(())
}

/// Deep, read-only per-repo pass: run the agent with cwd AT the repo, parse its
/// classification, and persist it. Best-effort — a timed-out/unparseable reply
/// leaves the prior profile (or placeholder) intact. No-op if the checkout is
/// gone (the agent can't read a path that isn't there).
pub async fn profile_repo_agent(db: &Db, repo: &repo_ref::Model) -> Result<()> {
    let cwd = Path::new(&repo.local_git_path);
    if !cwd.exists() {
        return Ok(());
    }
    let tool = crate::tools::default_tool(db).await;
    let prompt = build_repo_class_prompt(&repo.name);
    let text = run_agent_once(&tool, cwd, &prompt).await?;
    if let Some(wire) = parse_repo_class(&text) {
        persist_repo_class(db, repo, wire).await?;
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

    // Stage 1: deep per-repo classification, progressive.
    for r in &existing {
        let _ = profile_repo_agent(db, r).await;
        emit_graph_updated(workspace_id);
    }

    // Stage 2: cross-repo relations — needs at least two repos to relate. Reload
    // the (now classified) profiles so the listing carries tiers/summaries.
    if existing.len() >= 2 {
        let mut profiled: Vec<(repo_ref::Model, repo_profile::Model)> = Vec::new();
        for r in &existing {
            if let Some(p) = repo::get_repo_profile(db, r.id).await? {
                profiled.push((r.clone(), p));
            }
        }
        if profiled.len() >= 2 {
            let prompt = build_curator_prompt(&profiled);
            let cwd = Path::new(&profiled[0].0.local_git_path).to_path_buf();
            let tool = crate::tools::default_tool(db).await;
            // A timed-out / unparseable reply leaves existing relations intact
            // (never persists an empty set, which would drop all agent edges).
            if let Ok(text) = run_agent_once(&tool, &cwd, &prompt).await {
                if let Some(relations) = parse_curator_output(&text) {
                    persist_relations(db, &profiled, &relations).await?;
                }
            }
        }
        emit_graph_updated(workspace_id);
    }
    Ok(())
}

/// Workspaces with an analysis pass currently running, so a batch add (which
/// registers repos one-by-one) coalesces into a single workspace pass instead
/// of spawning one agent per repo.
fn analyzing() -> &'static std::sync::Mutex<std::collections::HashSet<i32>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i32>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Workspaces for which an add/analyze request arrived WHILE a pass was already
/// running. The running pass captured its repo list before that request, so it
/// reruns once when it finishes to pick up the newly-added repos.
fn pending_reanalyze() -> &'static std::sync::Mutex<std::collections::HashSet<i32>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i32>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Clears a workspace's in-flight marker on drop, so the slot is freed even if
/// the analysis pass panics (otherwise the workspace would be wedged — its
/// marker stuck and every future pass a no-op).
struct InFlightGuard(i32);
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        analyzing()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// `analyze_workspace`, but a no-op if a pass for this workspace is already in
/// flight. Best-effort and self-clearing (via the Drop guard, even on panic);
/// intended to be spawned fire-and-forget after a repo is added so the agent
/// graph refreshes without blocking the add.
pub async fn analyze_workspace_coalesced(db: &Db, workspace_id: i32) {
    {
        let mut g = analyzing().lock().unwrap_or_else(|e| e.into_inner());
        if !g.insert(workspace_id) {
            // A pass is already running; remember that a newer one was requested
            // so the running pass reruns once and picks up repos added since.
            pending_reanalyze()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(workspace_id);
            return;
        }
    }
    let _guard = InFlightGuard(workspace_id);
    loop {
        let _ = analyze_workspace(db, workspace_id).await;
        // Rerun once if an add/analyze landed mid-pass (its repos weren't in scope).
        let again = pending_reanalyze()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&workspace_id);
        if !again {
            break;
        }
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
    async fn edit_profile_creates_row_for_placeholder() {
        // A repo can be calibrated while it is still an unanalyzed placeholder
        // (no profile row). The edit must upsert one, not error.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "")
            .await
            .unwrap();
        assert!(repo::get_repo_profile(&db, r.id).await.unwrap().is_none());
        super::edit_profile(&db, r.id, "a web client", "frontend").await.unwrap();
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
            tier: "backend".into(),
            summary: "agent summary".into(),
            stack: vec![],
            components: vec![],
        };
        super::persist_repo_class(&db, &r, wire).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend", "legacy 'service' migrated to a real tier");
        assert_eq!(p.summary, "mine", "user-pinned summary preserved");
        assert_eq!(p.source, "user");
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
            tier: "backend".into(),
            summary: "agent".into(),
            stack: vec![],
            components: vec![],
        };
        super::persist_repo_class(&db, &r, wire).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "gateway", "a valid user-pinned tier is not overwritten");
    }

    #[tokio::test]
    async fn persist_repo_class_preserves_user_cleared_other() {
        // A user who corrects a repo to "Other" (empty tier) keeps that choice
        // across re-analysis; only non-empty legacy values migrate.
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "x", "/tmp/x", "main", "")
            .await
            .unwrap();
        repo::upsert_repo_profile(&db, r.id, "", "[]", "mine", "[]", "user", "")
            .await
            .unwrap();
        let wire = super::RepoClassWire {
            tier: "backend".into(),
            summary: "agent".into(),
            stack: vec![],
            components: vec![],
        };
        super::persist_repo_class(&db, &r, wire).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "", "user-cleared 'Other' tier is preserved, not migrated");
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
            tier: "backend".into(),
            summary: "monorepo".into(),
            stack: vec![],
            components: vec![
                Component { name: "api".into(), tier: "backend".into(), ..Default::default() },
                Component { name: "".into(), tier: "frontend".into(), ..Default::default() },
            ],
        };
        super::persist_repo_class(&db, &r, wire).await.unwrap();
        let p = repo::get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.role, "backend");
        let comps: Vec<Component> = serde_json::from_str(&p.components).unwrap();
        assert_eq!(comps.len(), 1, "the nameless component is dropped");
        assert_eq!(comps[0].name, "api");
    }

    #[test]
    fn parse_repo_class_tolerates_nameless_component() {
        // One component missing `name` must not make the whole reply unparseable.
        let reply = r#"{"tier":"backend","summary":"s","components":[{"name":"api"},{"tier":"frontend"}]}"#;
        let wire = super::parse_repo_class(reply).expect("parsed despite a nameless component");
        assert_eq!(wire.tier, "backend");
        assert_eq!(wire.components.len(), 2);
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
        assert_eq!((rels[0].from, rels[0].to), (1, 2));
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
        assert_eq!((rels[0].from, rels[0].to, rels[0].kind.as_str()), (1, 2, "http"));
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
        let mk = |from, to, via: &str| super::CuratorRelation {
            from,
            to,
            kind: "http".into(),
            via: via.into(),
            confidence: 70,
        };
        let rels = vec![
            mk(web.id, api.id, "GET /x"), // kept
            mk(web.id, web.id, "self"),   // dropped: self-edge
            mk(web.id, 999, "ghost-to"),  // dropped: unknown target
            mk(999, api.id, "ghost-from"), // dropped: unknown producer
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
