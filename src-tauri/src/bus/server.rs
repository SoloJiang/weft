//! MCP-over-HTTP for the thread bus. Stateless: each POST yields one SSE
//! `event: message` carrying the JSON-RPC response. Identity is derived from
//! the URL path, never agent input — so an agent can't spoof `from` via tool
//! arguments. This does NOT stop a local process that forges the URL path
//! itself (no auth; an accepted local-first tradeoff).

use crate::bus::BusRegistry;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

pub fn router(reg: BusRegistry) -> Router {
    Router::new()
        .route("/bus/:thread/:dir/mcp", post(handle).get(get_not_allowed))
        .route("/health", get(|| async { "ok" }))
        .with_state(reg)
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
            call_tool(&reg, thread, &dir, name, &args)
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
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
        }
    ])
}

/// Bind an ephemeral port and serve the router; returns the bound base URL.
pub async fn serve(reg: BusRegistry) -> std::io::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://127.0.0.1:{}", addr.port());
    let app = router(reg);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((base, handle))
}
