//! The deterministic half of the workspace Curator (ARCHITECTURE §4.9, §4.11):
//! profile each repo from its manifests and reconcile the cross-repo dependency
//! graph. No agent here — this is the cheap, always-available floor. The
//! semantic one-liner from an agent curator layers on top later; a user edit
//! (source = "user") always outranks re-inference.

use crate::git;
use crate::profile::{self, Edge, RepoFacts};
use crate::store::entities::{repo_profile, repo_ref};
use crate::store::{repo, Db};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// A profile as the UI sees it: decoded arrays + repo name + live staleness.
#[derive(Clone, Debug, Serialize)]
pub struct ProfileView {
    pub repo_id: i32,
    pub repo_name: String,
    pub role: String,
    pub stack: Vec<String>,
    pub summary: String,
    pub published: Vec<String>,
    pub deps: Vec<String>,
    pub source: String,
    pub profiled_commit: String,
    pub stale: bool,
}

/// The workspace dependency graph: profiled repos + the edges between them.
#[derive(Clone, Debug, Serialize)]
pub struct Graph {
    pub nodes: Vec<ProfileView>,
    pub edges: Vec<Edge>,
}

fn json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn arr(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn facts_of(m: &repo_profile::Model) -> RepoFacts {
    RepoFacts {
        role: m.role.clone(),
        stack: arr(&m.stack),
        summary: m.summary.clone(),
        published: arr(&m.published),
        deps: arr(&m.deps),
    }
}

/// Re-infer a repo's facts from disk and persist. Factual fields
/// (stack/published/deps) always refresh; the opinion fields (summary/role) are
/// preserved when the user has edited them (source = "user").
pub async fn profile_repo(db: &Db, repo: &repo_ref::Model) -> Result<repo_profile::Model> {
    let path = Path::new(&repo.local_git_path);
    let facts = profile::infer_repo_facts(path);
    let commit = git::head_commit(path).unwrap_or_default();

    let prior = repo::get_repo_profile(db, repo.id).await?;
    let user_owned = prior.as_ref().map(|p| p.source == "user").unwrap_or(false);
    let (role, summary, source) = match &prior {
        Some(p) if user_owned => (p.role.clone(), p.summary.clone(), "user"),
        _ => (facts.role.clone(), facts.summary.clone(), "inferred"),
    };

    repo::upsert_repo_profile(
        db,
        repo.id,
        &role,
        &json(&facts.stack),
        &summary,
        &json(&facts.published),
        &json(&facts.deps),
        source,
        &commit,
    )
    .await
}

/// Apply a user edit to the opinion fields; marks the profile user-owned so
/// future re-profiling won't clobber it.
pub async fn edit_profile(
    db: &Db,
    repo_id: i32,
    summary: &str,
    role: &str,
) -> Result<repo_profile::Model> {
    let existing = repo::get_repo_profile(db, repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no profile for repo {repo_id} yet"))?;
    repo::upsert_repo_profile(
        db,
        repo_id,
        role,
        &existing.stack,
        summary,
        &existing.published,
        &existing.deps,
        "user",
        &existing.profiled_commit,
    )
    .await
}

fn view_of(repo: &repo_ref::Model, profile: &repo_profile::Model) -> ProfileView {
    let live = git::head_commit(Path::new(&repo.local_git_path)).ok();
    let stale = match (&live, profile.profiled_commit.as_str()) {
        (Some(_), "") => true,
        (Some(head), at) => head != at,
        (None, _) => false, // can't tell (not a git repo / no commits)
    };
    ProfileView {
        repo_id: repo.id,
        repo_name: repo.name.clone(),
        role: profile.role.clone(),
        stack: arr(&profile.stack),
        summary: profile.summary.clone(),
        published: arr(&profile.published),
        deps: arr(&profile.deps),
        source: profile.source.clone(),
        profiled_commit: profile.profiled_commit.clone(),
        stale,
    }
}

/// All profiled repos in a workspace as the UI sees them (unprofiled repos are
/// omitted; profiling is eager on add, so this is normally every repo).
pub async fn list(db: &Db, workspace_id: i32) -> Result<Vec<ProfileView>> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut out = Vec::new();
    for r in &repos {
        if let Some(p) = repo::get_repo_profile(db, r.id).await? {
            out.push(view_of(r, &p));
        }
    }
    Ok(out)
}

/// The workspace dependency graph: nodes + edges, computed from stored profiles
/// (no disk read). Edges are the deterministic manifest floor merged with the
/// agent curator's inferred relations (service-to-service, infra, …) where
/// present; manifest edges win on a shared (from, to, via) triple.
pub async fn graph(db: &Db, workspace_id: i32) -> Result<Graph> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut nodes = Vec::new();
    let mut facts: Vec<(i32, RepoFacts)> = Vec::new();
    let mut relations: Vec<(i32, Vec<profile::AgentRelation>)> = Vec::new();
    for r in &repos {
        if let Some(p) = repo::get_repo_profile(db, r.id).await? {
            facts.push((r.id, facts_of(&p)));
            // Tolerate malformed/empty JSON: a bad relations blob just means no
            // agent edges for that repo, never a failed graph.
            relations.push((r.id, serde_json::from_str(&p.relations).unwrap_or_default()));
            nodes.push(view_of(r, &p));
        }
    }
    let node_ids: std::collections::HashSet<i32> = nodes.iter().map(|n| n.repo_id).collect();
    let manifest = profile::compute_edges(&facts);
    let agent: Vec<profile::Edge> = relations
        .iter()
        .flat_map(|(id, rels)| profile::agent_edges(*id, rels, &node_ids))
        .collect();
    // A user removal is a `rejected` tombstone. agent_edges already drops the
    // agent edge, but a MANIFEST edge for the same (from, to, kind) is recomputed
    // unconditionally — so apply tombstones to the merged set too, or a removed
    // `lib` edge would reappear in the map and in briefs.
    let tombstoned: std::collections::HashSet<(i32, i32, String)> = relations
        .iter()
        .flat_map(|(id, rels)| {
            rels.iter()
                .filter(|r| r.rejected)
                .map(move |r| (*id, r.to, r.kind.clone()))
        })
        .collect();
    let edges = profile::merge_edges(manifest, agent)
        .into_iter()
        .filter(|e| !tombstoned.contains(&(e.from, e.to, e.kind.clone())))
        .collect();
    Ok(Graph { nodes, edges })
}

// ─────────────────────────── agent curator ───────────────────────────
//
// The semantic layer the deterministic floor (above) only promised: a bounded,
// read-only agent reads the workspace's repos and reports cross-repo RUNTIME /
// infra relations manifests can't see (HTTP, gRPC, queues, shared infra). Its
// findings persist as `repo_profile.relations` and merge into `graph()` as
// edges tagged `source="agent"`.

/// One relation as the curator agent reports it: flat, with an explicit `from`
/// (the deterministic `AgentRelation` is stored per-producer, so `from` is
/// implicit there). Lenient: missing fields default.
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

const CURATOR_SYSTEM_PROMPT: &str = "You are a read-only repository dependency \
analyst. You may read code and configuration, but you must never modify, create, \
or delete files, and never run mutating commands.";

const CURATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Prompt listing every profiled repo (id/name/role/path/publishes/summary) and
/// asking for STRICT JSON cross-repo RUNTIME/infra relations keyed by repo id.
fn build_curator_prompt(repos: &[(repo_ref::Model, repo_profile::Model)]) -> String {
    let mut lines = String::new();
    for (r, p) in repos {
        lines.push_str(&format!(
            "- id={} name={:?} role={} path={:?} publishes={:?}\n  summary: {}\n",
            r.id,
            r.name,
            p.role,
            r.local_git_path,
            arr(&p.published),
            p.summary
        ));
    }
    format!(
        "Map how these repositories in one workspace depend on each other at \
RUNTIME and through shared infrastructure — relationships package manifests do \
NOT capture: HTTP/REST calls, gRPC, message queues, shared databases/infra, and \
cross-repo internal imports.\n\nRepositories:\n{lines}\nRead each repo's code and \
config at its path (READ-ONLY — change nothing). Then, as the LAST thing in your \
reply, output a single JSON object and nothing after it:\n\
{{\"relations\":[{{\"from\":<id>,\"to\":<id>,\"kind\":\"http|grpc|queue|infra|lib\",\
\"via\":\"<short evidence>\",\"confidence\":<0-100>}}]}}\n\
Rules: `from` and `to` MUST be ids from the list above and must differ. Only \
include a relation you have concrete evidence for. `via` is a short label (e.g. \
\"POST /orders\", \"orders-topic\", \"shared postgres\"). If you find none, output \
{{\"relations\":[]}}."
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

/// Run the agent curator over a workspace: infer cross-repo runtime/infra
/// relations the manifest floor can't see, and persist them per producer repo.
/// Best-effort — any failure leaves the existing graph intact. Skipped for a
/// workspace with fewer than two profiled repos (nothing to relate). The agent
/// runs read-only with `cwd` at the first repo and the others' absolute paths in
/// the prompt.
pub async fn analyze_workspace(db: &Db, workspace_id: i32) -> Result<()> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut profiled: Vec<(repo_ref::Model, repo_profile::Model)> = Vec::new();
    for r in &repos {
        if let Some(p) = repo::get_repo_profile(db, r.id).await? {
            // Only analyze repos whose checkout still exists: a missing first repo
            // would make `run_agent_once`'s cwd invalid and fail the whole pass,
            // and the agent can't read a path that isn't there.
            if Path::new(&r.local_git_path).exists() {
                profiled.push((r.clone(), p));
            }
        }
    }
    if profiled.len() < 2 {
        return Ok(());
    }
    let prompt = build_curator_prompt(&profiled);
    // cwd is a live repo (the list is already filtered to existing paths).
    let cwd = Path::new(&profiled[0].0.local_git_path).to_path_buf();
    // Resolve the same effective default coding agent normal threads use, rather
    // than hard-coding claude (which would no-op for codex/opencode users).
    let tool = crate::tools::default_tool(db).await;
    let text = run_agent_once(&tool, &cwd, &prompt).await?;
    let Some(relations) = parse_curator_output(&text) else {
        // A timed-out / unparseable reply: leave the existing graph intact rather
        // than persisting an empty set (which would drop all agent relations).
        return Ok(());
    };
    persist_relations(db, &profiled, &relations).await?;
    // Refresh any open repo map (mirrors calibrate_edges_tool) so edges inferred
    // by the background pass appear without a manual reload.
    if let Some(app) = crate::APP_HANDLE.get() {
        use tauri::Emitter;
        let _ = app.emit("repo-graph-updated", workspace_id);
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

    /// (db, repo_id, role, stack, summary, published, deps, source, commit)
    async fn profile(db: &Db, repo_id: i32, published: &str, deps: &str) {
        repo::upsert_repo_profile(db, repo_id, "service", "[]", "", published, deps, "inferred", "")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn graph_merges_manifest_and_agent_edges() {
        let db = mem().await;
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let web = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
            .await
            .unwrap();
        let api = repo::add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "")
            .await
            .unwrap();
        // Manifest edge: web declares a dep on @acme/api, which api publishes.
        profile(&db, web.id, "[]", r#"["@acme/api"]"#).await;
        profile(&db, api.id, r#"["@acme/api"]"#, "[]").await;
        // Agent edge: web also calls api over HTTP — a relation manifests can't see.
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
        assert!(
            g.edges.iter().any(|e| e.from == web.id
                && e.to == api.id
                && e.kind == "lib"
                && e.source == "manifest"),
            "manifest edge present"
        );
        assert!(
            g.edges.iter().any(|e| e.from == web.id
                && e.to == api.id
                && e.kind == "http"
                && e.source == "agent"
                && e.via == "GET /orders"),
            "agent http edge merged in"
        );
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
        profile(&db, web.id, "[]", "[]").await;
        profile(&db, api.id, "[]", "[]").await;
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
        profile(&db, web.id, "[]", "[]").await;
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
