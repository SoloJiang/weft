//! MCP-over-HTTP for the thread bus. Stateless: each POST yields one SSE
//! `event: message` carrying the JSON-RPC response. Identity is derived from
//! the URL path, never agent input — so an agent can't spoof `from` via tool
//! arguments. This does NOT stop a local process that forges the URL path
//! itself (no auth; an accepted local-first tradeoff).

use crate::ask::{AskRegistry, Decision};
use crate::bus::BusRegistry;
use crate::store::Db;
use axum::{
    extract::{FromRef, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// Shared state for the local server: the in-memory thread bus, the DB (the
/// planner reads the repo map and writes proposals), and the Ask registry (the
/// permission Ask Bridge).
#[derive(Clone)]
pub struct ServerState {
    pub bus: BusRegistry,
    pub db: Db,
    pub asks: AskRegistry,
}

impl FromRef<ServerState> for BusRegistry {
    fn from_ref(s: &ServerState) -> BusRegistry {
        s.bus.clone()
    }
}
impl FromRef<ServerState> for Db {
    fn from_ref(s: &ServerState) -> Db {
        s.db.clone()
    }
}
impl FromRef<ServerState> for AskRegistry {
    fn from_ref(s: &ServerState) -> AskRegistry {
        s.asks.clone()
    }
}

pub fn router(bus: BusRegistry, db: Db, asks: AskRegistry) -> Router {
    Router::new()
        .route("/bus/:thread/:dir/mcp", post(handle).get(get_not_allowed))
        .route(
            "/planner/:thread/mcp",
            post(handle_planner).get(get_not_allowed),
        )
        .route(
            "/curator/:thread/mcp",
            post(handle_curator).get(get_not_allowed),
        )
        .route(
            "/global/mcp",
            post(crate::bus::global::handle_global).get(get_not_allowed),
        )
        .route("/ask/:thread/:dir", post(handle_ask).get(get_not_allowed))
        .route("/health", get(|| async { "ok" }))
        .with_state(ServerState { bus, db, asks })
}

/// How long weft holds a permission Ask before letting the tool fall back to its
/// own prompt. Kept under the hook's own timeout so the fallback is clean.
// Hold the tool call until the human answers in Needs-you. Long by design
// (automation-first): a permission decision is the human's to make, so we wait
// rather than time out into the tool's own hidden TUI prompt. Falls back only if
// truly abandoned. Kept just under the hook/curl ceilings in inject.rs.
const ASK_WAIT: Duration = Duration::from_secs(3600);

/// The Ask Bridge endpoint. A tool's permission hook POSTs its PreToolUse-style
/// payload here and BLOCKS until the human answers in weft (→ allow/deny) or the
/// wait elapses (→ empty body, so the tool runs its own prompt — never a
/// silent stall). Identity (thread/dir) comes from the URL path, not the body.
async fn handle_ask(
    Path((thread, dir)): Path<(i32, String)>,
    Query(q): Query<HashMap<String, String>>,
    State(asks): State<AskRegistry>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let tool = q.get("tool").map(|s| s.as_str()).unwrap_or("claude");
    let tool_name = req
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("tool");

    // weft's OWN injected MCP tools are never permission-gated: the human
    // governs them through weft's surfaces (Needs-you, the board, the
    // direction-confirm flow), so a per-call prompt to read the task or post
    // to the bus is pure interruption. Short-circuit before summarizing — but
    // ONLY when weft actually injected this server for THIS session. A repo/user
    // MCP server that reused a weft server name (e.g. its own `weft_planner`) in
    // a session where weft never injected it must still surface the card.
    if is_weft_internal_tool(tool_name) {
        if let Some((server, _)) = split_internal_tool(tool_name) {
            if session_injected(&db, thread, &dir, server).await {
                return hook_decision("allow", "weft-internal tool (auto-approved)");
            }
        }
    }

    let (summary, detail) = summarize(tool_name, req.get("tool_input"));

    // A standing rule (full access / always-allow) decides without surfacing.
    if asks.auto_decision(thread, &dir, &summary) == Some(Decision::Allow) {
        return hook_decision("allow", "Auto-approved by a weft rule");
    }

    let (id, rx) = asks.request(thread, &dir, tool, &summary, &detail);

    match tokio::time::timeout(ASK_WAIT, rx).await {
        Ok(Ok(decision)) => {
            let (d, reason) = match decision {
                Decision::Allow => ("allow", "Approved in weft"),
                Decision::Deny => ("deny", "Denied in weft"),
            };
            hook_decision(d, reason)
        }
        // timed out or dropped → drop the card, return no decision: the tool
        // falls back to its native prompt rather than hanging.
        _ => {
            asks.cancel(id);
            Json(json!({})).into_response()
        }
    }
}

/// The EXACT `(server, tool)` pairs the Ask Bridge auto-approves. This is a
/// closed allowlist, deliberately not a `weft_*` server-name match:
///
/// - Provenance: a user/repo MCP server that happens to share a weft server name
///   only bypasses the card for these exact known calls; any OTHER tool it
///   exposes (e.g. `mcp__weft_planner__deploy`) still surfaces the Needs-you card.
/// - `weft_global`'s `answer_permission` is EXCLUDED on purpose: auto-approving
///   it would let an agent answer an existing Bash/file permission ask as
///   allow/always/full and erase the very boundary this bridge guards, so it
///   stays gated even though weft injected the server.
///
/// The included writes (bus posts, task status, proposals, edge calibration) are
/// governed by weft's own surfaces (Needs-you, the board, the direction-confirm
/// flow), so a per-call prompt for them is pure interruption.
const AUTO_APPROVED_INTERNAL_TOOLS: &[(&str, &str)] = &[
    // weft_bus — thread bus: reads, weft-governed posts, and ask_human (which
    // itself surfaces to the human).
    ("weft_bus", "bus_post"),
    ("weft_bus", "bus_broadcast"),
    ("weft_bus", "bus_inbox"),
    ("weft_bus", "ask_human"),
    ("weft_bus", "thread_state_get"),
    ("weft_bus", "thread_state_set"),
    ("weft_bus", "announce_interface_change"),
    ("weft_bus", "set_task_status"),
    // weft_planner — lead read-only planning; proposals are confirmed by the
    // human downstream in the direction-confirm flow.
    ("weft_planner", "get_task"),
    ("weft_planner", "get_repo_map"),
    ("weft_planner", "get_test_cases"),
    ("weft_planner", "propose_directions"),
    ("weft_planner", "cancel_directions"),
    // weft_curator — repo-map calibration in the curator chat.
    ("weft_curator", "get_repo_map"),
    ("weft_curator", "calibrate_edges"),
    ("weft_curator", "reanalyze"),
    ("weft_curator", "set_classification"),
    // weft_global — concierge/IM helper. answer_permission is intentionally absent.
    ("weft_global", "answer_question"),
    ("weft_global", "create_issue"),
    ("weft_global", "create_issue_from_im"),
    ("weft_global", "ensure_issue_topic"),
    ("weft_global", "ensure_issue_im_topic"),
    ("weft_global", "issue_status"),
    ("weft_global", "list_issues"),
    ("weft_global", "list_workspaces"),
    ("weft_global", "message_lead"),
    ("weft_global", "pending_needs_you"),
];

/// Extract `(server, tool)` from an agent-reported MCP tool name. ONLY the
/// claude-style `mcp__<server>__<tool>` form is recognized — its `__` delimiter is
/// unambiguous. opencode flattens MCP names as `<server>_<tool>` with a single
/// `_`, and weft server names contain `_`, so a project tool such as server `weft`
/// + tool `bus_thread_state_set` is indistinguishable from `weft_bus` +
/// `thread_state_set`; parsing that by prefix could auto-approve a foreign tool.
/// So opencode-form names are NOT matched here — they surface the Needs-you card
/// (fail-safe). Auto-approving opencode internals would need an unambiguous
/// server-identity signal, which the flat tool name alone doesn't carry.
fn split_internal_tool(tool_name: &str) -> Option<(&str, &str)> {
    tool_name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
}

/// A tool weft itself injected AND that is safe to auto-approve. The Ask Bridge
/// skips the permission card for these; everything else (including a weft server
/// name paired with an unknown tool, and `answer_permission`) surfaces normally.
fn is_weft_internal_tool(tool_name: &str) -> bool {
    split_internal_tool(tool_name)
        .is_some_and(|pair| AUTO_APPROVED_INTERNAL_TOOLS.contains(&pair))
}

/// The weft servers a lead-family session injects, by thread `kind`. MIRRORS the
/// injection branch in `lead_chat::commands::start_lead` — the one place that
/// decides what each session gets:
///   concierge → weft_global (never the per-thread bus)
///   curator   → weft_curator + weft_bus
///   per-issue lead (any other kind) → weft_planner + weft_bus
fn session_servers_for_kind(kind: &str) -> &'static [&'static str] {
    match kind {
        "concierge" => &["weft_global"],
        "curator" => &["weft_curator", "weft_bus"],
        _ => &["weft_planner", "weft_bus"],
    }
}

/// Whether weft injected `server` for the session identified by (thread, dir).
/// A worker lane (its ask `dir` is a direction id, not `LEAD`) injects only the
/// bus. The lead family keys off the thread kind and FAILS CLOSED: if the thread
/// can't be resolved — a deleted thread, a DB error, an engine/hook outliving its
/// thread — nothing is auto-approved and the tool surfaces the Needs-you card.
async fn session_injected(db: &Db, thread: i32, dir: &str, server: &str) -> bool {
    if dir != crate::bus::LEAD {
        // Worker lane: only the bus, and only when `dir` is a REAL direction of
        // this thread. Fail closed for a stale/deleted direction or a forged
        // route (an engine/hook outliving its direction, a leftover .weft-ask).
        if server != "weft_bus" {
            return false;
        }
        let Ok(direction_id) = dir.parse::<i32>() else {
            return false;
        };
        return matches!(
            crate::store::repo::get_direction(db, direction_id).await,
            Ok(Some(d)) if d.thread_id == thread
        );
    }
    match crate::store::repo::get_thread(db, thread).await {
        Ok(Some(t)) => session_servers_for_kind(&t.kind).contains(&server),
        _ => false,
    }
}

/// The PreToolUse hook response carrying a permission decision.
fn hook_decision(decision: &str, reason: &str) -> Response {
    Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason
        }
    }))
    .into_response()
}

/// A short human label + raw detail for a tool action. Tool-agnostic across
/// claude (Bash / file_path) and opencode (bash / filePath, lowercase names):
/// a command reads as "Run: …", a file op as "<tool> <file>".
fn summarize(tool_name: &str, input: Option<&Value>) -> (String, String) {
    let s = |k: &str| {
        input
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    if let Some(cmd) = s("command") {
        let first = cmd.lines().next().unwrap_or("").to_string();
        return (format!("Run: {first}"), cmd);
    }
    if let Some(f) = s("file_path").or_else(|| s("filePath")) {
        return (format!("{tool_name} {f}"), f);
    }
    let detail = input.map(|v| v.to_string()).unwrap_or_default();
    (tool_name.to_string(), detail)
}

async fn get_not_allowed() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

/// One SSE event carrying `value`.
fn sse(value: Value) -> Response {
    let body = format!("event: message\ndata: {}\n\n", value);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

// `thread`/`dir` come from the URL path, so an agent can't spoof its identity
// via tool arguments; it does NOT defend against a local process forging the
// path (no auth — local-first tradeoff).
async fn handle(
    Path((thread, dir)): Path<(i32, String)>,
    State(reg): State<BusRegistry>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    // Notifications (no id) get a bare 202.
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    reg.join(thread, &dir);

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_bus", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": tool_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // set_task_status writes the DB (the task is `dir`); the rest are
            // in-memory bus ops.
            if name == "set_task_status" {
                let status = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
                set_task_status_tool(&db, &dir, status).await
            } else {
                call_tool(&reg, thread, &dir, name, &args)
            }
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Bus tool: the agent sets its own task's lifecycle status. `dir` is the
/// direction id from the URL path, so the agent can't move another task.
async fn set_task_status_tool(db: &Db, dir: &str, status: &str) -> Value {
    let allowed = ["queued", "planning", "working", "review", "done"];
    if !allowed.contains(&status) {
        return text_result(format!(
            "invalid status '{status}'; use one of: queued, planning, working, review, done"
        ));
    }
    match dir.parse::<i32>() {
        Ok(id) => match crate::store::repo::set_direction_status(db, id, status).await {
            Ok(()) => text_result(format!("status set to {status}")),
            Err(e) => text_result(format!("error: {e}")),
        },
        Err(_) => text_result("this session has no task to update".into()),
    }
}

fn text_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }] })
}

fn call_tool(reg: &BusRegistry, thread: i32, me: &str, name: &str, args: &Value) -> Value {
    let s = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    match name {
        "bus_post" => {
            reg.post(thread, me, &s("to"), &s("text"), "message");
            text_result(format!("posted to {}", s("to")))
        }
        "bus_broadcast" => {
            reg.broadcast(thread, me, &s("text"), "message");
            text_result("broadcast sent".into())
        }
        "announce_interface_change" => {
            reg.broadcast(thread, me, &s("summary"), "interface");
            text_result("interface change announced".into())
        }
        "bus_inbox" => {
            let msgs = reg.inbox(thread, me);
            text_result(serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".into()))
        }
        "ask_human" => {
            let id = reg.ask_human(thread, me, &s("text"));
            text_result(format!(
                "asked the human (ask #{id}); their answer will arrive in your bus_inbox — keep working and check it"
            ))
        }
        "thread_state_get" => text_result(reg.state_get(thread).to_string()),
        "thread_state_set" => {
            let patch = args.get("patch").cloned().unwrap_or_else(|| json!({}));
            reg.state_set(thread, patch);
            text_result("state updated".into())
        }
        _ => text_result(format!("unknown tool: {name}")),
    }
}

// ---- planner MCP (lead-only, per thread) ----

async fn handle_planner(
    Path(thread): Path<i32>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_planner", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": planner_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_planner(&db, thread, name, &args).await
        }
        _ => json!({}),
    };
    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// MCP for the workspace curator chat: read the dependency graph and apply human
/// calibrations to it. Mirrors `handle_planner`; identity (the curator thread)
/// comes from the URL path.
async fn handle_curator(
    Path(thread): Path<i32>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_curator", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": curator_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_curator(&db, thread, name, &args).await
        }
        _ => json!({}),
    };
    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn curator_specs() -> Value {
    json!([
        {
            "name": "get_repo_map",
            "description": "Read the workspace repos and their current dependency edges (ids, tier, category/role, summaries, components, path) plus each repo's analysis_state (\"failed\" repos carry an analysis_error — automatic passes couldn't classify them; tell the human if they ask why a repo is unclassified). Use the ids when calling calibrate_edges or set_classification.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "calibrate_edges",
            "description": "Add or remove ONE cross-repo dependency edge after inspecting the code (READ-ONLY — never modify files). `from`/`to` are repo ids from get_repo_map and MUST differ. `kind` ∈ http|grpc|queue|infra|lib. `action` ∈ add|remove. `via` is a short evidence label (e.g. \"POST /orders\"). Human-set edges are pinned and survive automatic re-analysis; removals are remembered so the agent won't re-add them.",
            "inputSchema": { "type": "object", "properties": {
                "from": { "type": "integer" },
                "to": { "type": "integer" },
                "kind": { "type": "string" },
                "via": { "type": "string" },
                "action": { "type": "string", "enum": ["add", "remove"] }
            }, "required": ["from", "to", "kind", "action"] }
        },
        {
            "name": "reanalyze",
            "description": "Run a fresh dependency-analysis pass over the WHOLE workspace: re-classify each repo (tier/stack/summary) and re-infer cross-repo runtime/infra edges, then regenerate the map. Call this when the human asks to re-analyze / regenerate the repo map (e.g. after repos changed). Takes no arguments; returns when the pass completes, with the resulting repo/edge counts. Human-pinned edges survive the pass.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "set_classification",
            "description": "Fix ONE repo's architectural classification when get_repo_map shows it wrong (e.g. a frontend/local SDK mislabeled as a backend service). `repo` is the repo id from get_repo_map. `tier` ∈ frontend|backend. `category` is the role within that tier (free text, e.g. gateway|biz|core|common|idl|support for backend; app|sdk|web for frontend). Human/curator-set classification is PINNED and survives automatic re-analysis.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "integer" },
                "tier": { "type": "string", "enum": ["frontend", "backend"] },
                "category": { "type": "string" }
            }, "required": ["repo", "tier"] }
        }
    ])
}

async fn call_curator(db: &Db, thread: i32, name: &str, args: &Value) -> Value {
    match name {
        "get_repo_map" => match curator_map_json(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "calibrate_edges" => calibrate_edges_tool(db, thread, args).await,
        "set_classification" => set_classification_tool(db, thread, args).await,
        "reanalyze" => match reanalyze_tool(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        _ => text_result(format!("unknown tool {name}")),
    }
}

/// Run a full workspace analysis pass for the curator's workspace and return a
/// summary. Awaited inline (NOT detached) so the agent's turn stays busy for the
/// pass's whole duration — subsequent user messages queue, and the UI's `analyzing`
/// flag (derived from the lead turn) accurately tracks it. Clicking 中止 flips a
/// cancel flag the pass checks at safe points (between repos, before the relation
/// pass) and returns cleanly — see the cooperative-cancellation note in curator.rs.
async fn reanalyze_tool(db: &Db, thread: i32) -> anyhow::Result<String> {
    let t = crate::store::repo::get_thread(db, thread)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    if t.kind != "curator" {
        anyhow::bail!("reanalyze is only available in the curator chat");
    }
    let ws_id = t.workspace_id;
    // Register the cancel token BEFORE the (multi-repo, disk-stat) checkout preflight,
    // so a Stop during it trips THIS call's token instead of being dropped. Everything
    // after registration runs through the helper so the token is always unregistered.
    let cancel = crate::curator::register_analysis_cancel(thread);
    let out = reanalyze_after_register(db, ws_id, &cancel).await;
    crate::curator::unregister_analysis_cancel(thread, &cancel);
    out
}

/// The post-registration body of `reanalyze_tool` (checkout preflight + the cancellable
/// forced pass + the summary), split out so `reanalyze_tool` always unregisters the
/// cancel token afterwards.
async fn reanalyze_after_register(
    db: &Db,
    ws_id: i32,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<String> {
    // Every tracked repo's checkout gone → the pass filters them all out and would
    // analyze nothing, leaving the stale graph to read as a clean "complete". Tell
    // the human instead (matches the behavior of the removed analyze command).
    let repos = crate::store::repo::list_repos(db, ws_id).await?;
    if !repos.is_empty()
        && !repos
            .iter()
            .any(|r| std::path::Path::new(&r.local_git_path).exists())
    {
        return Ok("Could not re-analyze: every repository's checkout is missing on disk \
                   (moved or deleted). Restore the repos and try again."
            .to_string());
    }
    // Clicking 中止 interrupts the lead turn and calls `cancel_analysis(thread)`, which
    // trips the token; `reanalyze_workspace` checks it at safe points (between repos,
    // before the relation pass) and returns whether it ACTUALLY bailed — so a Stop that
    // lands after the pass already finished its relation write reports "complete", not
    // "cancelled". The pass runs directly under the gate lock (not via the coalescing
    // flags), so the cancel can't drop unrelated requests. Worker-chat "stop = stop".
    if crate::curator::reanalyze_workspace(db, ws_id, cancel).await {
        return Ok("Re-analysis cancelled.".to_string());
    }
    let g = crate::curator::graph(db, ws_id).await?;
    // Surface repos the pass left unclassified so the human sees them in the chat (the
    // map node renders them as plain "未分析"). Shared with the direct button command via
    // `unanalyzed_repo_names` (failed classification OR missing checkout).
    let unanalyzed = crate::curator::unanalyzed_repo_names(db, ws_id).await;
    let mut msg = format!(
        "Re-analysis complete: {} repos, {} dependency links. The repo map has been refreshed.",
        g.nodes.len(),
        g.edges.len()
    );
    if !unanalyzed.is_empty() {
        msg.push_str(&format!(
            " Note: {} repo(s) could not be analyzed and stayed unclassified (classifier \
             error or missing checkout): {}. Tell the human, who can re-run the analysis.",
            unanalyzed.len(),
            unanalyzed.join(", ")
        ));
    }
    Ok(msg)
}

/// Like `repo_map_json`, but every node carries its full `local_git_path` — the
/// curator agent must read each repo to find evidence, and the system-prompt
/// repo list is capped/truncated, so paths can't be sourced from there alone.
async fn curator_map_json(db: &Db, thread: i32) -> anyhow::Result<String> {
    let t = crate::store::repo::get_thread(db, thread)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let g = crate::curator::graph(db, t.workspace_id).await?;
    let path_of: std::collections::HashMap<i32, String> =
        crate::store::repo::list_repos(db, t.workspace_id)
            .await?
            .into_iter()
            .map(|r| (r.id, r.local_git_path))
            .collect();
    let nodes: Vec<Value> = g
        .nodes
        .iter()
        .map(|n| {
            json!({
                "repo_id": n.repo_id,
                "repo_name": n.repo_name,
                "tier": n.tier,
                "category": n.category,
                "stack": n.stack,
                "summary": n.summary,
                "components": n.components,
                "path": path_of.get(&n.repo_id).cloned().unwrap_or_default(),
                // Per-repo analysis status, so the agent can tell the human which repos
                // an automatic (add/backfill/resume) pass failed to analyze — those
                // never flow through reanalyze's chat summary and the map node just
                // shows them as unclassified. "failed" carries an error; "" / "idle"
                // is normal.
                "analysis_state": n.analysis_state,
                "analysis_error": n.analysis_error,
            })
        })
        .collect();
    Ok(json!({ "nodes": nodes, "edges": g.edges }).to_string())
}

/// Apply one human calibration: validate ids, write a user-sourced relation (or
/// removal tombstone), then emit `repo-graph-updated` so the repo map refreshes.
async fn calibrate_edges_tool(db: &Db, thread: i32, args: &Value) -> Value {
    // i32::try_from (not a lossy `as i32`): a huge id like 4294967297 must NOT
    // wrap to a valid repo id and slip past the workspace membership check.
    let from = args.get("from").and_then(|v| v.as_i64()).and_then(|n| i32::try_from(n).ok());
    let to = args.get("to").and_then(|v| v.as_i64()).and_then(|n| i32::try_from(n).ok());
    let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let via = args.get("via").and_then(|v| v.as_str()).unwrap_or("");
    let (Some(from), Some(to)) = (from, to) else {
        return text_result("from and to must be valid repo ids from get_repo_map".into());
    };
    if from == to {
        return text_result("from and to must be different repos".into());
    }
    // Validate kind against the allowed set: relations are keyed by (to, kind), so
    // a misspelling like "HTTP" would silently fail to match the visible edge.
    if !crate::profile::RELATION_KINDS.contains(&kind) {
        return text_result("kind must be one of: http, grpc, queue, infra, lib".into());
    }
    // Action is REQUIRED and must be add|remove. The store treats anything but
    // "remove" as an add, so a missing/misspelled action must be rejected here
    // rather than silently pinning the opposite of what the caller intended.
    let action = match args.get("action").and_then(|v| v.as_str()) {
        Some(a @ ("add" | "remove")) => a,
        _ => return text_result("action is required and must be \"add\" or \"remove\"".into()),
    };
    // Validate the ids belong to THIS curator's workspace, so a stale/hallucinated
    // id can't pin or remove relations on an unrelated workspace's repo.
    let Ok(Some(t)) = crate::store::repo::get_thread(db, thread).await else {
        return text_result("curator thread not found".into());
    };
    // Only the hidden curator thread may calibrate — reject a direct call to this
    // route with a normal feature thread id (it would bypass the chat boundary).
    if t.kind != "curator" {
        return text_result("calibrate_edges is only available in the curator chat".into());
    }
    let ws_ids: std::collections::HashSet<i32> = crate::store::repo::list_repos(db, t.workspace_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.id)
        .collect();
    if !ws_ids.contains(&from) || !ws_ids.contains(&to) {
        return text_result(
            "from/to must be repo ids in this workspace (use get_repo_map)".into(),
        );
    }
    match crate::store::repo::calibrate_repo_relation(db, from, to, kind, via, action).await {
        Ok(()) => {
            // Live-refresh the repo map for this curator thread's workspace.
            if let Some(app) = crate::APP_HANDLE.get() {
                use tauri::Emitter;
                let _ = app.emit("repo-graph-updated", t.workspace_id);
            }
            text_result(format!(
                "{action} {kind} edge {from}->{to} (pinned to your calibration)"
            ))
        }
        Err(e) => text_result(format!("error: {e}")),
    }
}

/// Pin ONE repo's tier/role classification (e.g. mark a frontend/local SDK that was
/// mislabeled as a backend service). Validates the repo belongs to THIS curator's
/// workspace and the tier is canonical, then writes via `curator::edit_profile`
/// (which pins ownership so the classification survives re-analysis) and emits
/// `repo-graph-updated`.
async fn set_classification_tool(db: &Db, thread: i32, args: &Value) -> Value {
    // i32::try_from (not lossy `as i32`): a huge id must NOT wrap to a valid repo id
    // and slip past the workspace membership check.
    let repo = args.get("repo").and_then(|v| v.as_i64()).and_then(|n| i32::try_from(n).ok());
    let Some(repo) = repo else {
        return text_result("repo must be a valid repo id from get_repo_map".into());
    };
    // Tier is REQUIRED and must canonicalize to frontend|backend: a misspelling would
    // otherwise clear the tier (or store a legacy value) and the node would read as
    // unclassified.
    let tier = args.get("tier").and_then(|v| v.as_str()).unwrap_or("");
    let Some(tier) = crate::profile::normalize_tier(tier) else {
        return text_result("tier is required and must be \"frontend\" or \"backend\"".into());
    };
    // Category (role within the tier) is optional free text; blank → leave it to a
    // later pass (don't pin an empty role).
    let category = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let Ok(Some(t)) = crate::store::repo::get_thread(db, thread).await else {
        return text_result("curator thread not found".into());
    };
    // Only the hidden curator thread may classify — reject a direct call with a normal
    // feature thread id (it would bypass the chat boundary).
    if t.kind != "curator" {
        return text_result("set_classification is only available in the curator chat".into());
    }
    let in_ws = crate::store::repo::list_repos(db, t.workspace_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .any(|r| r.id == repo);
    if !in_ws {
        return text_result("repo must be a repo id in this workspace (use get_repo_map)".into());
    }
    match crate::curator::edit_profile(db, repo, None, Some(tier.as_str()), category).await {
        Ok(_) => {
            if let Some(app) = crate::APP_HANDLE.get() {
                use tauri::Emitter;
                let _ = app.emit("repo-graph-updated", t.workspace_id);
            }
            let role = category.map(|c| format!("/{c}")).unwrap_or_default();
            text_result(format!(
                "repo {repo} classified as {tier}{role} (pinned — survives re-analysis)"
            ))
        }
        Err(e) => text_result(format!("error: {e}")),
    }
}

async fn call_planner(db: &Db, thread: i32, name: &str, args: &Value) -> Value {
    match name {
        "get_repo_map" => match repo_map_json(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "get_task" => match crate::store::repo::get_thread(db, thread).await {
            Ok(Some(t)) => text_result(json!({ "title": t.title, "type": t.kind }).to_string()),
            Ok(None) => text_result("error: thread not found".into()),
            Err(e) => text_result(format!("error: {e}")),
        },
        "get_test_cases" => match crate::store::repo::get_test_plan(db, thread).await {
            Ok(Some(p)) => text_result(p.content),
            Ok(None) => text_result("no test cases derived for this issue yet".into()),
            Err(e) => text_result(format!("error: {e}")),
        },
        "propose_directions" => {
            // `directions` must be a PRESENT, non-empty array. A missing / empty / malformed
            // payload is NOT a cancel: return an error so the lead retries (cancellation goes
            // through cancel_directions) rather than silently clearing the pending plan. Note
            // `Proposal.directions` is `#[serde(default)]`, so a missing/misspelled key would
            // otherwise deserialize to an empty list — hence the explicit presence check.
            let has_directions = args
                .get("directions")
                .and_then(|d| d.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if !has_directions {
                return text_result(
                    "error: propose_directions requires a non-empty `directions` array; \
                     to withdraw pending directions, call cancel_directions"
                        .into(),
                );
            }
            let proposal: crate::planner::Proposal = match serde_json::from_value(args.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return text_result(format!("error: invalid propose_directions payload: {e}"))
                }
            };
            let n = proposal.directions.len();
            match crate::planner::save_proposal(db, thread, &proposal).await {
                Ok(()) => {
                    // Anchor the proposal in the chat timeline at the moment it
                    // happened — the console renders it as an interactive card.
                    emit_proposal_row(db, thread, &proposal.rationale, n).await;
                    text_result(format!(
                        "proposed {n} direction(s); the human will review and confirm in weft"
                    ))
                }
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "cancel_directions" => {
            let rationale = args
                .get("rationale")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            withdraw_and_emit(db, thread, &rationale).await
        }
        _ => text_result(format!("unknown tool: {name}")),
    }
}

/// Insert + emit a proposal-shaped timeline row (kind:"proposal", content
/// {rationale, count}). The console renders count>0 as the interactive review card
/// and count==0 as the settled "已撤回" line. Shared by propose + withdraw.
async fn emit_proposal_row(db: &Db, thread: i32, rationale: &str, count: usize) {
    let content = serde_json::json!({ "rationale": rationale, "count": count }).to_string();
    let turn = crate::store::repo::next_turn_id(db, thread).await.unwrap_or(1) - 1;
    if let Ok(m) = crate::store::repo::insert_lead_message(
        db,
        thread,
        None,
        turn.max(1),
        "system",
        "proposal",
        &content,
        "complete",
    )
    .await
    {
        if let Some(app) = crate::APP_HANDLE.get() {
            use tauri::Emitter;
            let _ = app.emit(
                crate::lead_chat::engine::EVENT,
                crate::lead_chat::engine::Push::Message {
                    thread_id: thread,
                    message: m,
                },
            );
        }
    }
}

/// Withdraw the pending proposal (the `cancel_directions` tool). Only records the count-0
/// ("已撤回") row when a pending proposal was ACTUALLY cleared — a no-op cancel (already
/// confirmed, a lane approved, or nothing proposed) must not leave a misleading withdrawn
/// row over live work.
async fn withdraw_and_emit(db: &Db, thread: i32, rationale: &str) -> Value {
    match crate::planner::withdraw_proposal(db, thread, rationale).await {
        Ok(true) => {
            emit_proposal_row(db, thread, rationale, 0).await;
            text_result("withdrew pending directions".into())
        }
        Ok(false) => text_result(
            "nothing to withdraw: no pending proposal (it may be confirmed or already dispatched)"
                .into(),
        ),
        Err(e) => text_result(format!("error: {e}")),
    }
}

async fn repo_map_json(db: &Db, thread: i32) -> anyhow::Result<String> {
    let t = crate::store::repo::get_thread(db, thread)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let g = crate::curator::graph(db, t.workspace_id).await?;
    Ok(serde_json::to_string(&g)?)
}

fn planner_specs() -> Value {
    let str_prop = || json!({ "type": "string" });
    json!([
        {
            "name": "get_task",
            "description": "Read this thread's Task: its title and type (feature|bugfix|refactor|spike).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_repo_map",
            "description": "Read the workspace repo map: each repo's role/stack/summary/published+declared packages, plus the cross-repo dependency edges. Use it to decide which repos a task must touch and in what order.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_test_cases",
            "description": "Read the issue's current test-case document (markdown tree), if one exists. The human can edit it in weft, so read it back before shaping or revising the technical approach when you have not seen an update recently.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "propose_directions",
            "description": "Propose how to split this task into directions. Each direction targets EXACTLY ONE repo it will modify (by name, from the repo map) and MUST include a `reason` explaining why that repo must change. Reads are free — an agent may read any repo without declaring it, so never list reads. To modify N repos, propose N directions. The human reviews each as a Needs-you card and approves before any worktree is created.",
            "inputSchema": { "type": "object", "properties": {
                "rationale": str_prop(),
                "directions": { "type": "array", "items": { "type": "object", "properties": {
                    "name": str_prop(),
                    "repo": str_prop(),
                    "reason": str_prop(),
                    "mandate": { "type": "string", "enum": ["plan+impl", "impl-only"],
                        "description": "Granularity of the role: plan+impl (default) — the worker plans its own direction first, then builds; impl-only — the direction is small/fully specified, the worker builds straight away. Do NOT write the direction's implementation plan yourself; that is the worker's job." },
                    "base_branch": { "type": "string",
                        "description": "Branch in the target repo to branch the new work OFF. Leave empty to use the repo's default branch (main/master). Set it only when the repo merges into a non-default branch (develop/staging/a release branch)." }
                }, "required": ["name", "repo", "reason"] } }
            }, "required": ["directions"] }
        },
        {
            "name": "cancel_directions",
            "description": "Withdraw the pending proposed directions. Use when the human says to hold off / cancel, or the write boundary is no longer settled — it clears the current proposal so nothing is dispatched and the review card collapses. Provide a short rationale. Do NOT call propose_directions with an empty directions list to cancel; use this.",
            "inputSchema": { "type": "object", "properties": {
                "rationale": str_prop()
            }, "required": ["rationale"] }
        }
    ])
}

fn tool_specs() -> Value {
    let str_prop = || json!({ "type": "string" });
    json!([
        {
            "name": "bus_post",
            "description": "Post a message to another direction's inbox in this thread.",
            "inputSchema": { "type": "object",
                "properties": { "to": str_prop(), "text": str_prop() },
                "required": ["to", "text"] }
        },
        {
            "name": "bus_broadcast",
            "description": "Send a message to every other direction in this thread.",
            "inputSchema": { "type": "object",
                "properties": { "text": str_prop() }, "required": ["text"] }
        },
        {
            "name": "bus_inbox",
            "description": "Read and clear your unread messages from other directions.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "ask_human",
            "description": "Ask the human operator a question that only they can decide (a judgment call, a missing requirement, an approval). Surfaces in weft's Needs-you inbox; their answer returns via bus_inbox. Non-blocking — keep working and check your inbox.",
            "inputSchema": { "type": "object",
                "properties": { "text": str_prop() }, "required": ["text"] }
        },
        {
            "name": "thread_state_get",
            "description": "Read the shared thread state (a JSON object).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "thread_state_set",
            "description": "Shallow-merge a patch object into the shared thread state.",
            "inputSchema": { "type": "object",
                "properties": { "patch": { "type": "object" } }, "required": ["patch"] }
        },
        {
            "name": "announce_interface_change",
            "description": "Broadcast a contract/interface change to the other directions.",
            "inputSchema": { "type": "object",
                "properties": { "summary": str_prop() }, "required": ["summary"] }
        },
        {
            "name": "set_task_status",
            "description": "Move your task on the board as work really progresses: queued (not started), planning (working out this direction's plan), working (actively building), review (done coding, awaiting the human's look), done (delivered/accepted). Reversible — set it back to working if the human asks for changes. Use this to keep the human's board honest instead of leaving it to guesswork.",
            "inputSchema": { "type": "object",
                "properties": { "status": str_prop() }, "required": ["status"] }
        }
    ])
}

/// Bind an ephemeral port and serve the router; returns the bound base URL.
pub async fn serve(
    bus: BusRegistry,
    db: Db,
    asks: AskRegistry,
) -> std::io::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://127.0.0.1:{}", addr.port());
    let app = router(bus, db, asks);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((base, handle))
}

#[cfg(test)]
mod tests {
    use super::{is_weft_internal_tool, session_servers_for_kind};

    #[test]
    fn session_servers_mirror_injection_policy() {
        // Lead family keys off the thread kind (worker lanes are handled in
        // session_injected — bus only — and a failed lookup fails closed there).
        assert_eq!(session_servers_for_kind("concierge"), &["weft_global"]);
        assert_eq!(
            session_servers_for_kind("curator"),
            &["weft_curator", "weft_bus"]
        );
        assert_eq!(
            session_servers_for_kind("issue"),
            &["weft_planner", "weft_bus"]
        );
        // An unknown but real kind maps to the common per-issue lead set.
        assert_eq!(
            session_servers_for_kind("something-new"),
            &["weft_planner", "weft_bus"]
        );
    }

    #[test]
    fn provenance_blocks_uninjected_servers() {
        // Concierge injects only weft_global — not the per-thread bus.
        assert!(!session_servers_for_kind("concierge").contains(&"weft_bus"));
        assert!(!session_servers_for_kind("concierge").contains(&"weft_planner"));
        // Curator has no planner.
        assert!(!session_servers_for_kind("curator").contains(&"weft_planner"));
        // A per-issue lead has no global / curator.
        assert!(!session_servers_for_kind("issue").contains(&"weft_global"));
        assert!(!session_servers_for_kind("issue").contains(&"weft_curator"));
    }

    #[test]
    fn weft_internal_known_tools_auto_allow() {
        // weft's own injected tools (unambiguous claude naming) → auto-allow.
        assert!(is_weft_internal_tool("mcp__weft_planner__get_test_cases"));
        assert!(is_weft_internal_tool("mcp__weft_planner__get_task"));
        assert!(is_weft_internal_tool("mcp__weft_planner__propose_directions"));
        assert!(is_weft_internal_tool("mcp__weft_bus__bus_inbox"));
        assert!(is_weft_internal_tool("mcp__weft_bus__set_task_status"));
        assert!(is_weft_internal_tool("mcp__weft_curator__get_repo_map"));
        assert!(is_weft_internal_tool("mcp__weft_global__list_workspaces"));
    }

    #[test]
    fn opencode_flat_names_surface() {
        // opencode's `<server>_<tool>` form is ambiguous (weft names contain `_`),
        // so it is NOT parsed as internal — those calls surface the card. A project
        // server `weft` + tool `bus_thread_state_set` must not read as weft_bus.
        assert!(!is_weft_internal_tool("weft_bus_bus_post"));
        assert!(!is_weft_internal_tool("weft_planner_get_task"));
        assert!(!is_weft_internal_tool("weft_bus_thread_state_set"));
    }

    #[test]
    fn permission_answering_tool_still_surfaces() {
        // answer_permission would let an agent erase a pending permission ask —
        // it must NOT be auto-approved even though weft injects weft_global.
        assert!(!is_weft_internal_tool("mcp__weft_global__answer_permission"));
    }

    #[test]
    fn unknown_tool_on_weft_server_surfaces() {
        // Provenance: sharing a weft server name is not enough — an unknown tool
        // (e.g. from a user/repo MCP server that reused the name) still surfaces.
        assert!(!is_weft_internal_tool("mcp__weft_planner__deploy"));
        assert!(!is_weft_internal_tool("mcp__weft_bus__rm_rf"));
        assert!(!is_weft_internal_tool("weft_planner_deploy"));
    }

    #[test]
    fn foreign_and_malformed_names_surface() {
        // A user's own `weft_*`-named server (different server name) must surface.
        assert!(!is_weft_internal_tool("mcp__weft_analytics__query"));
        assert!(!is_weft_internal_tool("mcp__weftly__do"));
        // Non-MCP tools (Bash, file ops) and third-party MCP surface as before.
        assert!(!is_weft_internal_tool("Bash"));
        assert!(!is_weft_internal_tool("mcp__github__create_pr"));
        // Malformed names never match.
        assert!(!is_weft_internal_tool("mcp__weft_bus"));
        assert!(!is_weft_internal_tool("weft_bus__x"));
    }
}
