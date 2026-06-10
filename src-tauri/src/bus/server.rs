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
        .route("/planner/:thread/mcp", post(handle_planner).get(get_not_allowed))
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
    Json(req): Json<Value>,
) -> Response {
    let tool = q.get("tool").map(|s| s.as_str()).unwrap_or("claude");
    let tool_name = req.get("tool_name").and_then(|v| v.as_str()).unwrap_or("tool");
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
    let s = |k: &str| input.and_then(|v| v.get(k)).and_then(|v| v.as_str()).map(|s| s.to_string());
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
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
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
            let name = req.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
            let args = req.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));
            call_planner(&db, thread, name, &args).await
        }
        _ => json!({}),
    };
    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn call_planner(db: &Db, thread: i32, name: &str, args: &Value) -> Value {
    match name {
        "get_repo_map" => match repo_map_json(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "get_task" => match crate::store::repo::get_thread(db, thread).await {
            Ok(Some(t)) => text_result(
                json!({ "title": t.title, "type": t.kind }).to_string(),
            ),
            Ok(None) => text_result("error: thread not found".into()),
            Err(e) => text_result(format!("error: {e}")),
        },
        "propose_directions" => {
            let proposal: crate::planner::Proposal =
                serde_json::from_value(args.clone()).unwrap_or_default();
            let n = proposal.directions.len();
            match crate::planner::save_proposal(db, thread, &proposal).await {
                Ok(()) => {
                    // Anchor the proposal in the chat timeline at the moment it
                    // happened — the console renders it as an interactive card.
                    let content = serde_json::json!({
                        "rationale": proposal.rationale,
                        "count": n,
                    })
                    .to_string();
                    let turn = crate::store::repo::next_turn_id(db, thread).await.unwrap_or(1) - 1;
                    if let Ok(m) = crate::store::repo::insert_lead_message(
                        db, thread, None, turn.max(1), "system", "proposal", &content, "complete",
                    )
                    .await
                    {
                        if let Some(app) = crate::APP_HANDLE.get() {
                            use tauri::Emitter;
                            let _ = app.emit(
                                crate::lead_chat::engine::EVENT,
                                crate::lead_chat::engine::Push::Message { thread_id: thread, message: m },
                            );
                        }
                    }
                    text_result(format!(
                        "proposed {n} direction(s); the human will review and confirm in weft"
                    ))
                }
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        _ => text_result(format!("unknown tool: {name}")),
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
            "name": "propose_directions",
            "description": "Propose how to split this task into directions. Each direction targets EXACTLY ONE repo it will modify (by name, from the repo map) and MUST include a `reason` explaining why that repo must change. Reads are free — an agent may read any repo without declaring it, so never list reads. To modify N repos, propose N directions. The human reviews each as a Needs-you card and approves before any worktree is created.",
            "inputSchema": { "type": "object", "properties": {
                "rationale": str_prop(),
                "directions": { "type": "array", "items": { "type": "object", "properties": {
                    "name": str_prop(),
                    "tool": str_prop(),
                    "repo": str_prop(),
                    "reason": str_prop(),
                    "mandate": { "type": "string", "enum": ["plan+impl", "impl-only"],
                        "description": "Granularity of the role: plan+impl (default) — the worker plans its own direction first, then builds; impl-only — the direction is small/fully specified, the worker builds straight away. Do NOT write the direction's implementation plan yourself; that is the worker's job." }
                }, "required": ["name", "tool", "repo", "reason"] } }
            }, "required": ["directions"] }
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
