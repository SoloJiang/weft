//! Curator: repo intake & decomposition (migration spec §5.5). Each analysis
//! is an EPHEMERAL Codex thread (not listed in Desktop) whose single turn is
//! constrained by `outputSchema` — the final assistant message is guaranteed
//! schema-shaped JSON, so weft's tolerant free-text parsing layer
//! (`json_objects`, `lenient_confidence`, fence stripping) is unnecessary.
//!
//! Two passes:
//! - per-repo profile: tier / stack / summary / monorepo components
//! - cross-repo relations: edges + architectural layers + a repo-map doc,
//!   fed with every repo's profile summary as its only context

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use weft_app_server::client as codex;
use weft_app_server::client::ThreadMsg;
use weft_app_server::proto::ChatEvent;

use crate::api_error::ApiError;
use crate::events;
use crate::runtime;
use crate::store::{RelationRow, Store};

/// Per-analysis turn cap — generous; a hung turn fails the profile instead
/// of leaking the task forever.
const ANALYSIS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// JSON Schema for the per-repo profile pass (structured-output strict
/// shape: `additionalProperties: false`, every field required).
pub fn profile_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["tier", "stack", "summary", "components"],
        "properties": {
            "tier": {
                "type": "string",
                "description": "one of: app | lib | service | tool | docs | unknown"
            },
            "stack": {
                "type": "array",
                "items": { "type": "string" },
                "description": "key languages, frameworks, runtimes"
            },
            "summary": {
                "type": "string",
                "description": "2-4 sentences: what it is, entry points, how it is built/run"
            },
            "components": {
                "type": "array",
                "description": "deployable/publishable parts; empty for single-crate repos",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "path", "summary"],
                    "properties": {
                        "name": { "type": "string" },
                        "path": { "type": "string" },
                        "summary": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// JSON Schema for the cross-repo relations/layers pass.
pub fn relations_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["relations", "layers", "repoMapMarkdown"],
        "properties": {
            "relations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["from", "to", "kind", "via", "confidence", "rationale"],
                    "properties": {
                        "from": { "type": "string", "description": "repo NAME that depends" },
                        "to": { "type": "string", "description": "repo NAME depended upon" },
                        "kind": { "type": "string", "description": "cargo | npm | http | grpc | …" },
                        "via": { "type": "string", "description": "manifest/endpoint evidence" },
                        "confidence": { "type": "integer", "description": "0-100" },
                        "rationale": { "type": "string" }
                    }
                }
            },
            "layers": {
                "type": "array",
                "description": "architectural bands, one item per band; rank 0 = foundation",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["label", "rank", "repos"],
                    "properties": {
                        "label": { "type": "string" },
                        "rank": { "type": "integer" },
                        "repos": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "repo names or ids in this band"
                        }
                    }
                }
            },
            "repoMapMarkdown": {
                "type": "string",
                "description": "concise workspace map document"
            }
        }
    })
}

pub fn profile_brief(repo_name: &str) -> String {
    format!(
        "Analyze the repository `{repo_name}` at your working directory (you are \
         read-only: modify nothing).\n\
         Read its manifests, README, and entry points, then produce the profile: \
         tier (app | lib | service | tool | docs | unknown), stack, a 2-4 sentence \
         summary (what it is, entry points, how it is built/run), and — for \
         monorepos — its deployable/publishable components (empty array otherwise).\n\n\
         IMPORTANT: your final message must be ONLY the JSON object matching the \
         requested schema — no prose, no markdown fences, no commentary."
    )
}

pub fn relations_brief(repos: &[(i64, String, String, String)]) -> String {
    let mut lines = String::from(
        "You are analyzing the dependency structure of a multi-repo workspace.\n\
         Repos (name — id — path — profile summary):\n",
    );
    for (id, name, path, summary) in repos {
        lines.push_str(&format!("- `{name}` (id {id}) — {path} — {summary}\n"));
    }
    lines.push_str(
        "\nYou are read-only: inspect any of the listed paths for evidence \
         (manifests, imports, client stubs). Produce: `relations` (an edge \
         from → to when FROM depends on TO; kind e.g. cargo/npm/http/grpc; via = \
         manifest or endpoint evidence; confidence 0-100; short rationale), \
         `layers` (architectural bands with a shared label and rank; rank 0 = \
         foundation), and `repoMapMarkdown` (a concise workspace map). Reference \
         ONLY the repo names listed above; emit no edge you have no evidence for.\n\n\
         IMPORTANT: your final message must be ONLY the JSON object matching the \
         requested schema — no prose, no markdown fences, no commentary.",
    );
    lines
}

/// Every balanced top-level `{...}` substring, in order (ported from weft's
/// curator). Byte-scans the ASCII structural chars — string-literal aware,
/// so braces inside strings don't fool the depth counter; an unbalanced
/// `{` in prose is skipped, not fatal.
///
/// Fallback channel: `outputSchema` SHOULD make the final message raw JSON,
/// but the ChatGPT backend proved non-strict in practice (2026-08-08 smoke:
/// prose + fenced ```json block). The parser therefore takes the LAST
/// object carrying the required keys.
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
            None => i += 1,
        }
    }
    out
}

/// Parse the analysis output: the whole text when it's raw JSON (strict
/// backend), else the last balanced object containing every `required_key`.
fn parse_output(text: &str, required_keys: &[&str]) -> Option<Value> {
    let has_keys = |v: &Value| required_keys.iter().all(|k| !v[k].is_null());
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return has_keys(&v).then_some(v);
    }
    json_objects(text)
        .into_iter()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .filter(has_keys)
        .last()
}

/// Accumulate assistant text until the turn ends. Completed items
/// (TextDone / Assistant) are authoritative over streamed deltas; deltas are
/// tracked PER ITEM (a new item id resets the buffer) so narration never
/// concatenates with the final schema JSON. With `outputSchema` the final
/// message IS the schema JSON. Returns `(text, had_error)`.
async fn collect_final_text(mut rx: UnboundedReceiver<ThreadMsg>) -> (String, bool) {
    let mut current_item: Option<String> = None;
    let mut current = String::new();
    let mut completed: Option<String> = None;
    loop {
        let Some(msg) = rx.recv().await else {
            return (completed.unwrap_or(current), true);
        };
        match msg {
            ThreadMsg::Event(ChatEvent::TextDelta { text, item, .. }) => {
                if item != current_item {
                    current_item = item;
                    current.clear();
                }
                current.push_str(&text);
            }
            ThreadMsg::Event(ChatEvent::TextDone { text: Some(t), .. }) => completed = Some(t),
            ThreadMsg::Event(ChatEvent::Assistant { texts, .. }) => {
                completed = Some(texts.join("\n"));
            }
            ThreadMsg::Event(ChatEvent::TurnEnd { is_error, .. }) => {
                return (completed.unwrap_or(current), is_error);
            }
            _ => {}
        }
    }
}

/// One ephemeral analysis thread: `cwd` + read-only sandbox + never-approve,
/// one schema-constrained turn, returns the final message text. Err carries
/// the human-readable failure (landed into `repo_profile.run_error`).
async fn run_analysis_turn(cwd: &str, brief: &str, schema: Value) -> Result<String, String> {
    let client = codex::client()
        .await
        .map_err(|e| format!("connect app-server: {e:#}"))?;
    let result = client
        .request(
            "thread/start",
            codex::thread_start_params_configured(cwd, "never", "read-only", None, true),
        )
        .await
        .map_err(|e| format!("thread/start: {e:#}"))?;
    let thread_id =
        codex::thread_id_of(&result).ok_or_else(|| "thread/start: no thread.id".to_string())?;
    let rx = client.subscribe(&thread_id).await;
    let turn = client
        .request(
            "turn/start",
            codex::turn_start_params_with_schema(&thread_id, brief, schema),
        )
        .await
        .map_err(|e| format!("turn/start: {e:#}"))?;
    let turn_id = codex::turn_id_of(&turn).ok_or_else(|| "turn/start: no turn.id".to_string())?;
    client.set_active_turn(&thread_id, &turn_id).await;
    let outcome = tokio::time::timeout(ANALYSIS_TIMEOUT, collect_final_text(rx)).await;
    let (text, is_error) = match outcome {
        Err(_) => {
            let _ = client.interrupt(&thread_id, &turn_id).await;
            return Err("analysis timed out".to_string());
        }
        Ok(pair) => pair,
    };
    if is_error {
        return Err("analysis turn ended with error".to_string());
    }
    if text.trim().is_empty() {
        return Err("analysis returned no output".to_string());
    }
    Ok(text)
}

/// Analyze one repo into `repo_profile`. Refuses to overlap a running one.
pub async fn analyze_repo(store: &Store, repo_id: i64) -> anyhow::Result<()> {
    if !runtime::agents_allowed() {
        anyhow::bail!("daemon is not live; refusing to spawn agents");
    }
    let repo = store
        .get_repo(repo_id)
        .await?
        .ok_or(ApiError::not_found("repo", repo_id))?;
    if let Some(p) = store.get_profile(repo_id).await? {
        if p.run_state == "running" {
            return Err(ApiError::conflict(
                "already_running",
                format!("repo {repo_id} already has a running analysis"),
            )
            .into());
        }
    }

    store.profile_mark_running(repo_id, "").await?;
    events::emit(
        "repo.profile",
        json!({ "repoId": repo_id, "runState": "running" }),
    );

    let outcome = run_analysis_turn(&repo.path, &profile_brief(&repo.name), profile_schema()).await;
    let text = match outcome {
        Ok(t) => t,
        Err(msg) => {
            store.profile_fail(repo_id, &msg).await?;
            events::emit(
                "repo.profile",
                json!({ "repoId": repo_id, "runState": "failed" }),
            );
            return Ok(());
        }
    };
    match parse_output(&text, &["tier", "summary"]) {
        Some(v) => {
            let tier = v["tier"].as_str().unwrap_or("unknown");
            let summary = v["summary"].as_str().unwrap_or("");
            let stack = coerce_array(&v["stack"]);
            let components = coerce_array(&v["components"]);
            store
                .profile_complete(repo_id, tier, &stack, summary, &components)
                .await?;
            events::emit(
                "repo.profile",
                json!({ "repoId": repo_id, "runState": "done" }),
            );
        }
        None => {
            let snippet: String = text.chars().take(200).collect();
            store
                .profile_fail(
                    repo_id,
                    &format!("no schema JSON in output; text: {snippet:?}"),
                )
                .await?;
            events::emit(
                "repo.profile",
                json!({ "repoId": repo_id, "runState": "failed" }),
            );
        }
    }
    Ok(())
}

/// Lenient coercion (a weft curator lesson): agents sometimes emit a bare
/// string where the schema wants an array — wrap it; a bare object becomes
/// a one-element array; anything else wrong-typed is dropped.
fn coerce_array(v: &Value) -> String {
    if v.is_array() {
        return serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string());
    }
    if let Some(s) = v.as_str() {
        return serde_json::to_string(&[s]).unwrap_or_else(|_| "[]".to_string());
    }
    if v.is_object() {
        return serde_json::to_string(&[v]).unwrap_or_else(|_| "[]".to_string());
    }
    "[]".to_string()
}

/// Full workspace pass: profile every repo (in parallel — each is its own
/// ephemeral thread), then the cross-repo relations/layers pass.
pub async fn analyze_workspace(store: &Store, workspace_id: i64) -> anyhow::Result<()> {
    if !runtime::agents_allowed() {
        anyhow::bail!("daemon is not live; refusing to spawn agents");
    }
    let repos = store.list_repos(workspace_id).await?;
    if repos.is_empty() {
        anyhow::bail!("workspace {workspace_id} has no repos");
    }
    let mut set = tokio::task::JoinSet::new();
    for repo in &repos {
        let store = store.clone();
        let id = repo.id;
        set.spawn(async move {
            let _ = analyze_repo(&store, id).await;
        });
    }
    while set.join_next().await.is_some() {}
    analyze_relations(store, workspace_id).await
}

/// Intake pipeline: profile only newly registered repos, then refresh the
/// cross-repo map once all of those passes settle. Existing profiles are not
/// needlessly re-run when another repository joins the workspace.
pub async fn analyze_imported_repos(
    store: &Store,
    workspace_id: i64,
    repo_ids: Vec<i64>,
) -> anyhow::Result<()> {
    if !runtime::agents_allowed() {
        anyhow::bail!("daemon is not live; refusing to spawn agents");
    }
    let mut set = tokio::task::JoinSet::new();
    for repo_id in repo_ids {
        let repo = store
            .get_repo(repo_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown repo {repo_id}"))?;
        if repo.workspace_id != workspace_id {
            anyhow::bail!("repo {repo_id} is not in workspace {workspace_id}");
        }
        let store = store.clone();
        set.spawn(async move { analyze_repo(&store, repo_id).await });
    }
    while let Some(result) = set.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => events::emit(
                "repo.profile",
                json!({ "workspaceId": workspace_id, "error": format!("{error:#}") }),
            ),
            Err(error) => events::emit(
                "repo.profile",
                json!({ "workspaceId": workspace_id, "error": format!("analysis task: {error}") }),
            ),
        }
    }
    analyze_relations(store, workspace_id).await
}

/// Cross-repo relations/layers pass alone (repos already profiled) — fed
/// with every done profile's summary as its context.
pub async fn analyze_relations(store: &Store, workspace_id: i64) -> anyhow::Result<()> {
    if !runtime::agents_allowed() {
        anyhow::bail!("daemon is not live; refusing to spawn agents");
    }
    let repos = store.list_repos(workspace_id).await?;
    let mut context: Vec<(i64, String, String, String)> = Vec::new();
    for repo in &repos {
        if let Some(p) = store.get_profile(repo.id).await? {
            if p.run_state == "done" {
                context.push((
                    repo.id,
                    repo.name.clone(),
                    repo.path.clone(),
                    p.summary.clone(),
                ));
            }
        }
    }
    if context.len() < 2 {
        // Nothing meaningful to relate.
        return Ok(());
    }
    let cwd = repos[0].path.clone();
    let outcome = run_analysis_turn(&cwd, &relations_brief(&context), relations_schema()).await;
    let text = match outcome {
        Ok(t) => t,
        Err(msg) => {
            events::emit(
                "repo.relations",
                json!({ "workspaceId": workspace_id, "error": msg }),
            );
            return Ok(());
        }
    };
    let parsed: Value = match parse_output(&text, &["relations"]) {
        Some(v) => v,
        None => {
            events::emit(
                "repo.relations",
                json!({ "workspaceId": workspace_id, "error": "no schema JSON in output" }),
            );
            return Ok(());
        }
    };

    // Agents reference repos by name OR by numeric id (weft's schema used
    // ids; either resolves here).
    let known: std::collections::HashMap<String, i64> = repos
        .iter()
        .flat_map(|r| [(r.name.clone(), r.id), (r.id.to_string(), r.id)])
        .collect();
    let mut relations: Vec<RelationRow> = Vec::new();
    if let Some(items) = parsed["relations"].as_array() {
        for item in items {
            let from = item["from"].as_str().unwrap_or("");
            let to = item["to"].as_str().unwrap_or("");
            if !known.contains_key(from) || !known.contains_key(to) {
                continue;
            }
            let from_id = known[from];
            let to_id = known[to];
            relations.push(RelationRow {
                id: 0,
                workspace_id,
                from_repo: repo_name(&repos, from_id).to_string(),
                to_repo: repo_name(&repos, to_id).to_string(),
                kind: item["kind"].as_str().unwrap_or("").to_string(),
                via: item["via"].as_str().unwrap_or("").to_string(),
                confidence: item["confidence"].as_i64().unwrap_or(0).clamp(0, 100),
                rationale: item["rationale"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    store.replace_relations(workspace_id, &relations).await?;

    let mut layers_seen = 0usize;
    let mut layers_applied = 0usize;
    let mut layers_skipped: Vec<String> = Vec::new();
    if let Some(layers) = parsed["layers"].as_array() {
        layers_seen = layers.len();
        for layer in layers {
            let label = layer["label"].as_str().unwrap_or("");
            let rank = layer["rank"].as_i64().unwrap_or(0);
            // Tolerate both observed agent shapes: the schema's grouped
            // `repos: [...]` and the earlier per-repo `repo: "name"|id`.
            let mut refs: Vec<String> = Vec::new();
            if let Some(group) = layer["repos"].as_array() {
                for r in group {
                    refs.push(json_ref_string(r));
                }
            }
            if refs.is_empty() {
                refs.push(json_ref_string(&layer["repo"]));
            }
            for name in refs {
                let Some(repo_id) = known.get(&name) else {
                    layers_skipped.push(name);
                    continue;
                };
                if store.profile_set_layer(*repo_id, label, rank).await.is_ok() {
                    layers_applied += 1;
                }
            }
        }
    }
    if let Some(doc) = parsed["repoMapMarkdown"].as_str() {
        if !doc.trim().is_empty() {
            store.set_workspace_repo_map(workspace_id, doc).await?;
        }
    }
    events::emit(
        "repo.relations",
        json!({
            "workspaceId": workspace_id,
            "relations": relations.len(),
            "layersSeen": layers_seen,
            "layersApplied": layers_applied,
            "layersSkipped": layers_skipped,
            "layersRaw": parsed["layers"].to_string().chars().take(400).collect::<String>(),
        }),
    );
    Ok(())
}

/// A repo reference as the agent may emit it: string name or numeric id.
fn json_ref_string(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(n) = v.as_i64() {
        return n.to_string();
    }
    String::new()
}

fn repo_name(repos: &[crate::store::RepoRow], id: i64) -> &str {
    repos
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.name.as_str())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_strict_shaped() {
        let p = profile_schema();
        assert_eq!(p["additionalProperties"], false);
        assert_eq!(
            p["required"],
            json!(["tier", "stack", "summary", "components"])
        );
        let r = relations_schema();
        assert_eq!(r["additionalProperties"], false);
        assert_eq!(
            r["required"],
            json!(["relations", "layers", "repoMapMarkdown"])
        );
        // Relations items must carry the full evidence set.
        let item = &r["properties"]["relations"]["items"];
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(
            item["required"],
            json!(["from", "to", "kind", "via", "confidence", "rationale"])
        );
    }

    #[test]
    fn relations_brief_lists_only_known_repos() {
        let brief = relations_brief(&[
            (
                1,
                "api".to_string(),
                "/srv/api".to_string(),
                "backend service".to_string(),
            ),
            (
                2,
                "web".to_string(),
                "/srv/web".to_string(),
                "frontend".to_string(),
            ),
        ]);
        assert!(brief.contains("`api` (id 1) — /srv/api — backend service"));
        assert!(brief.contains("`web` (id 2) — /srv/web — frontend"));
        assert!(brief.contains("ONLY the repo names listed above"));
    }

    fn delta(item: &str, text: &str) -> ThreadMsg {
        ThreadMsg::Event(ChatEvent::TextDelta {
            text: text.to_string(),
            item: Some(item.to_string()),
            agent_thread: None,
        })
    }

    fn turn_end() -> ThreadMsg {
        ThreadMsg::Event(ChatEvent::TurnEnd {
            is_error: false,
            context_tokens: None,
        })
    }

    #[tokio::test]
    async fn collect_takes_last_item_not_concatenation() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(delta("a", "narration ")).expect("send");
        tx.send(delta("a", "more narration")).expect("send");
        tx.send(delta("b", "{\"tier\":")).expect("send");
        tx.send(delta("b", "\"app\"}")).expect("send");
        tx.send(turn_end()).expect("send");
        let (text, is_error) = collect_final_text(rx).await;
        assert!(!is_error);
        assert_eq!(text, "{\"tier\":\"app\"}");
    }

    #[tokio::test]
    async fn collect_prefers_completed_item_text() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(delta("a", "par")).expect("send");
        tx.send(ThreadMsg::Event(ChatEvent::TextDone {
            item: Some("a".to_string()),
            text: Some("{\"tier\":\"lib\"}".to_string()),
            agent_thread: None,
        }))
        .expect("send");
        tx.send(turn_end()).expect("send");
        let (text, is_error) = collect_final_text(rx).await;
        assert!(!is_error);
        assert_eq!(text, "{\"tier\":\"lib\"}");
    }

    #[test]
    fn json_objects_scans_past_prose_and_fences() {
        let text = "some {unbalanced prose\n```json\n{\"a\": {\"b\": 1}}\n```\n\
                    trailing {\"c\": \"brace } inside\"} end";
        let objs = json_objects(text);
        assert_eq!(objs.len(), 2);
        assert!(objs[0].contains("\"a\""));
        assert!(objs[1].contains("brace } inside"));
    }

    #[test]
    fn parse_output_raw_and_fenced() {
        // Strict backend: the whole message is the JSON.
        let raw = r#"{"tier":"app","summary":"s"}"#;
        assert_eq!(
            parse_output(raw, &["tier", "summary"]).expect("raw")["tier"],
            "app"
        );
        // Non-strict backend: prose + fenced JSON — take the LAST object
        // carrying the required keys (earlier prose objects must not hide it).
        let fenced = "Analysis done.\n\n{\"unrelated\": true}\n\n```json\n\
                      {\"tier\": \"lib\", \"summary\": \"real\"}\n```";
        let v = parse_output(fenced, &["tier", "summary"]).expect("fenced");
        assert_eq!(v["tier"], "lib");
        // Missing required keys → None.
        assert!(parse_output("{\"other\": 1}", &["tier"]).is_none());
    }

    #[test]
    fn coerce_array_wraps_agent_sloppiness() {
        assert_eq!(coerce_array(&json!(["a", "b"])), "[\"a\",\"b\"]");
        assert_eq!(coerce_array(&json!("solo")), "[\"solo\"]");
        assert_eq!(coerce_array(&json!({"name": "x"})), "[{\"name\":\"x\"}]");
        assert_eq!(coerce_array(&json!(null)), "[]");
        assert_eq!(coerce_array(&json!(42)), "[]");
    }

    #[test]
    fn json_ref_string_accepts_name_or_id() {
        assert_eq!(json_ref_string(&json!("api")), "api");
        assert_eq!(json_ref_string(&json!(3)), "3");
        assert_eq!(json_ref_string(&json!(null)), "");
    }
}
