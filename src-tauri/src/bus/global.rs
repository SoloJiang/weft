//! `weft_global` MCP server (spec §5 / M3-2): a stable, NOT-per-thread tool face
//! exposed to the Concierge engine — so the IM conversation assistant can read workspaces /
//! issues / Needs-you, answer asks on behalf of the user, message a lead, or
//! file a new issue. Pure tool dispatch; the human is still the decision side
//! for `confirm_scope` / `approve_direction` (those go through the desktop,
//! not Concierge — see spec).
//!
//! Wiring mirrors `handle_planner` in `bus::server`:
//!  - HTTP POST → JSON-RPC (`initialize` / `tools/list` / `tools/call`)
//!  - body wrapped in one SSE `event: message`
//!  - tool specs in `global_specs()`; per-tool dispatch in `call_global()`
//!  - failures soft-return via `text_result("error: …")` (no 500s)

use crate::ask::{Answer, AskRegistry};
use crate::bus::BusRegistry;
use crate::store::{repo, Db};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

fn text_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }] })
}

fn json_result(v: Value) -> Value {
    text_result(v.to_string())
}

/// HTTP handler for `POST /global/mcp`. Stateless — each call carries its full
/// JSON-RPC frame; same SSE response shape as the rest of the bus server.
pub async fn handle_global(
    State(db): State<Db>,
    State(asks): State<AskRegistry>,
    State(bus): State<BusRegistry>,
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
            "serverInfo": { "name": "weft_global", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": global_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_global(&db, &asks, &bus, name, &args).await
        }
        _ => json!({}),
    };
    let body = format!(
        "event: message\ndata: {}\n\n",
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    );
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

/// Per-tool dispatch. Errors short-return via text_result so MCP clients see a
/// friendly message instead of a transport failure (mirrors `call_planner`).
pub async fn call_global(
    db: &Db,
    asks: &AskRegistry,
    bus: &BusRegistry,
    name: &str,
    args: &Value,
) -> Value {
    match name {
        "list_workspaces" => match list_workspaces(db).await {
            Ok(v) => json_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "list_issues" => {
            let ws = args
                .get("workspace_id")
                .and_then(|v| v.as_i64())
                .map(|x| x as i32);
            match list_issues(db, ws).await {
                Ok(v) => json_result(v),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "issue_status" => {
            let Some(tid) = args
                .get("thread_id")
                .and_then(|v| v.as_i64())
                .map(|x| x as i32)
            else {
                return text_result("error: thread_id required".into());
            };
            match issue_status(db, asks, tid).await {
                Ok(v) => json_result(v),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "pending_needs_you" => match pending_needs_you(db, asks).await {
            Ok(v) => json_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "answer_permission" => {
            let Some(ask_id) = args.get("ask_id").and_then(|v| v.as_u64()) else {
                return text_result("error: ask_id required".into());
            };
            let verdict = args.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
            let Some(ans) = Answer::parse(verdict) else {
                return text_result(format!(
                    "error: unknown verdict '{verdict}' (use allow/deny/always/full)"
                ));
            };
            if asks.answer(ask_id, ans) {
                text_result(format!("answered ask #{ask_id} as {verdict}"))
            } else {
                text_result(format!("ask #{ask_id} was already answered or expired"))
            }
        }
        "answer_question" => {
            let Some(tid) = args
                .get("thread_id")
                .and_then(|v| v.as_i64())
                .map(|x| x as i32)
            else {
                return text_result("error: thread_id required".into());
            };
            let Some(ask_id) = args.get("ask_id").and_then(|v| v.as_u64()) else {
                return text_result("error: ask_id required".into());
            };
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if bus.answer_ask(tid, ask_id, text) {
                text_result(format!("answered ask #{ask_id} on thread {tid}"))
            } else {
                text_result(format!(
                    "ask #{ask_id} on thread {tid} was already answered or no longer exists"
                ))
            }
        }
        "message_lead" => {
            let Some(tid) = args
                .get("thread_id")
                .and_then(|v| v.as_i64())
                .map(|x| x as i32)
            else {
                return text_result("error: thread_id required".into());
            };
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if text.trim().is_empty() {
                return text_result("error: text required".into());
            }
            match message_lead(db, tid, &text).await {
                Ok(()) => text_result(format!("delivered to lead of thread {tid}")),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "ensure_issue_topic" => {
            let Some(tid) = args
                .get("thread_id")
                .and_then(|v| v.as_i64())
                .map(|x| x as i32)
            else {
                return text_result("error: thread_id required".into());
            };
            let chat_id = args
                .get("chat_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if chat_id.is_empty() {
                return text_result("error: chat_id required".into());
            }
            match ensure_issue_topic(db, tid, chat_id).await {
                Ok(v) => json_result(v),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "create_issue" => {
            let Some(ws) = args
                .get("workspace_id")
                .and_then(|v| v.as_i64())
                .map(|x| x as i32)
            else {
                return text_result("error: workspace_id required".into());
            };
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let kind = args
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("feature")
                .to_string();
            if title.trim().is_empty() {
                return text_result("error: title required".into());
            }
            match create_issue(db, ws, &title, &kind).await {
                Ok(v) => json_result(v),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        _ => text_result(format!("unknown tool: {name}")),
    }
}

// ───────────────────── tool implementations ─────────────────────

async fn list_workspaces(db: &Db) -> anyhow::Result<Value> {
    let mut out = Vec::new();
    for w in repo::list_workspaces(db).await? {
        let count = repo::list_threads(db, w.id)
            .await
            .map(|v| v.len())
            .unwrap_or(0);
        out.push(json!({ "id": w.id, "name": w.name, "thread_count": count }));
    }
    Ok(Value::Array(out))
}

async fn list_issues(db: &Db, ws: Option<i32>) -> anyhow::Result<Value> {
    let workspaces = match ws {
        Some(id) => vec![id],
        None => repo::list_workspaces(db)
            .await?
            .into_iter()
            .map(|w| w.id)
            .collect(),
    };
    let mut out = Vec::new();
    for w in workspaces {
        for t in repo::list_threads(db, w).await? {
            out.push(json!({
                "id": t.id,
                "workspace_id": t.workspace_id,
                "title": t.title,
                "kind": t.kind,
            }));
        }
    }
    Ok(Value::Array(out))
}

async fn issue_status(db: &Db, asks: &AskRegistry, tid: i32) -> anyhow::Result<Value> {
    let t = repo::get_thread(db, tid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {tid} not found"))?;
    let open_asks = asks.open_in(tid).len();
    Ok(json!({
        "thread_id": t.id,
        "title": t.title,
        "kind": t.kind,
        "open_asks_count": open_asks,
    }))
}

async fn pending_needs_you(db: &Db, asks: &AskRegistry) -> anyhow::Result<Value> {
    let mut open = asks.open();
    for a in &mut open {
        if let Ok(Some(t)) = repo::get_thread(db, a.thread).await {
            a.thread_title = t.title;
        }
        if let Ok(id) = a.dir.parse::<i32>() {
            if let Ok(Some(d)) = repo::get_direction(db, id).await {
                a.dir_name = d.name;
            }
        }
    }
    let arr: Vec<Value> = open
        .into_iter()
        .map(|a| {
            json!({
                "ask_id": a.id,
                "thread_id": a.thread,
                "thread_title": a.thread_title,
                "direction": a.dir_name,
                "tool": a.tool,
                "summary": a.summary,
                "ts": a.ts,
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

/// Push a message into the lead engine of `thread_id` from outside (Concierge).
/// Pulls the global `AppHandle` from the `OnceLock` set in `setup()` — by the
/// time an MCP request lands, the Tauri builder is long past that point.
async fn message_lead(db: &Db, thread_id: i32, text: &str) -> anyhow::Result<()> {
    let app = crate::APP_HANDLE
        .get()
        .ok_or_else(|| anyhow::anyhow!("app handle not initialized"))?;
    let eng = crate::lead_chat::commands::lead_engine(app, db, thread_id, "zh").await?;
    crate::lead_chat::engine::send(app, db, &eng, text, Vec::new(), Vec::new()).await
}

async fn create_issue(db: &Db, ws: i32, title: &str, kind: &str) -> anyhow::Result<Value> {
    let tool = crate::tools::default_tool(db).await;
    let t = repo::create_thread(db, ws, title, kind, &tool).await?;
    Ok(json!({
        "id": t.id,
        "workspace_id": t.workspace_id,
        "title": t.title,
        "kind": t.kind,
    }))
}

async fn ensure_issue_topic(db: &Db, thread_id: i32, chat_id: &str) -> anyhow::Result<Value> {
    let before = repo::im_route_of_thread(db, thread_id).await?;
    let settings = crate::im::ImSettings::load(db).await?;
    if !settings.ready() {
        anyhow::bail!("Feishu app credentials are not configured");
    }
    let ch = crate::im::feishu::FeishuChannel::new(&settings.app_id, &settings.app_secret)?;
    crate::im::ensure_issue_topic(db, &ch, thread_id, chat_id, None, "zh").await?;
    let after = repo::im_route_of_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("topic route was not created"))?;
    Ok(json!({
        "thread_id": after.thread_id,
        "chat_id": after.chat_id,
        "im_thread_ref": after.im_thread_ref,
        "created": before.is_none(),
    }))
}

// ───────────────────── tool specs ─────────────────────

pub fn global_specs() -> Value {
    let s = || json!({ "type": "string" });
    let i = || json!({ "type": "integer" });
    let u = || json!({ "type": "integer", "minimum": 0 });
    json!([
        {
            "name": "list_workspaces",
            "description": "List every workspace (id, name, thread_count). Call before answering any question that mentions \"workspaces\" or \"issues\".",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_issues",
            "description": "List issues. Pass workspace_id to scope to one workspace; omit for all.",
            "inputSchema": { "type": "object", "properties": { "workspace_id": i() } }
        },
        {
            "name": "issue_status",
            "description": "Read one issue's title, kind, and how many open permission asks it has.",
            "inputSchema": { "type": "object", "properties": { "thread_id": i() }, "required": ["thread_id"] }
        },
        {
            "name": "pending_needs_you",
            "description": "Every open permission Ask across all workspaces — id, thread, asking direction, tool, summary, ts. Use this when the human asks \"what's waiting on me\".",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "answer_permission",
            "description": "Answer a permission ask on behalf of the human. verdict ∈ allow|deny|always|full. always = remember this exact action for the asking task; full = grant the task full access (skips future asks).",
            "inputSchema": { "type": "object",
                "properties": { "ask_id": u(), "verdict": s() },
                "required": ["ask_id", "verdict"] }
        },
        {
            "name": "answer_question",
            "description": "Reply to an agent's open question (ask_human). The text is delivered into that direction's bus inbox.",
            "inputSchema": { "type": "object",
                "properties": { "thread_id": i(), "ask_id": u(), "text": s() },
                "required": ["thread_id", "ask_id", "text"] }
        },
        {
            "name": "message_lead",
            "description": "Send a message into a thread's lead engine, as if the human typed it in the desktop. Use when the human wants to nudge a specific issue's lead from IM.",
            "inputSchema": { "type": "object",
                "properties": { "thread_id": i(), "text": s() },
                "required": ["thread_id", "text"] }
        },
        {
            "name": "ensure_issue_topic",
            "description": "Ensure an existing issue has a Feishu topic in chat_id. Use only when the user semantically asks to create/open/continue an issue-specific Feishu topic; do not call for ordinary chat.",
            "inputSchema": { "type": "object",
                "properties": { "thread_id": i(), "chat_id": s() },
                "required": ["thread_id", "chat_id"] }
        },
        {
            "name": "create_issue",
            "description": "File a new issue in a workspace. kind ∈ feature|bugfix|refactor|spike (defaults to feature).",
            "inputSchema": { "type": "object",
                "properties": { "workspace_id": i(), "title": s(), "kind": s() },
                "required": ["workspace_id", "title"] }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    async fn mem_db() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn list_workspaces_returns_id_name_and_count() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let w = repo::create_workspace(&db, "alpha").await.unwrap();
        let _t = repo::create_thread(&db, w.id, "first", "feature", "claude")
            .await
            .unwrap();
        let _t2 = repo::create_thread(&db, w.id, "second", "bugfix", "claude")
            .await
            .unwrap();
        let v = call_global(&db, &asks, &bus, "list_workspaces", &json!({})).await;
        let parsed: Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed[0]["name"], "alpha");
        assert_eq!(parsed[0]["thread_count"], 2);
    }

    #[tokio::test]
    async fn list_issues_scopes_to_workspace() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let w1 = repo::create_workspace(&db, "a").await.unwrap();
        let w2 = repo::create_workspace(&db, "b").await.unwrap();
        repo::create_thread(&db, w1.id, "in-a", "feature", "claude")
            .await
            .unwrap();
        repo::create_thread(&db, w2.id, "in-b", "feature", "claude")
            .await
            .unwrap();
        let v = call_global(
            &db,
            &asks,
            &bus,
            "list_issues",
            &json!({ "workspace_id": w1.id }),
        )
        .await;
        let parsed: Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["title"], "in-a");
    }

    #[tokio::test]
    async fn pending_needs_you_lists_open_asks_with_thread_title() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let w = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, w.id, "登录修复", "bugfix", "claude")
            .await
            .unwrap();
        let (id, _rx) = asks.request(t.id, "10", "claude", "Run: npm test", "npm test");
        let v = call_global(&db, &asks, &bus, "pending_needs_you", &json!({})).await;
        let parsed: Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed[0]["ask_id"], id);
        assert_eq!(parsed[0]["thread_id"], t.id);
        assert_eq!(parsed[0]["thread_title"], "登录修复");
        assert_eq!(parsed[0]["summary"], "Run: npm test");
    }

    #[tokio::test]
    async fn answer_permission_resolves_ask() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let (id, rx) = asks.request(1, "10", "claude", "Run: npm test", "npm test");
        let v = call_global(
            &db,
            &asks,
            &bus,
            "answer_permission",
            &json!({ "ask_id": id, "verdict": "allow" }),
        )
        .await;
        assert!(v["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("answered"));
        assert_eq!(rx.await.unwrap(), crate::ask::Decision::Allow);
    }

    #[tokio::test]
    async fn answer_permission_unknown_verdict_soft_errors() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let (id, _rx) = asks.request(1, "10", "claude", "x", "x");
        let v = call_global(
            &db,
            &asks,
            &bus,
            "answer_permission",
            &json!({ "ask_id": id, "verdict": "maybe" }),
        )
        .await;
        let s = v["content"][0]["text"].as_str().unwrap();
        assert!(s.starts_with("error:") && s.contains("maybe"));
    }

    #[tokio::test]
    async fn issue_status_reports_open_ask_count() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let w = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, w.id, "issue", "feature", "claude")
            .await
            .unwrap();
        let _ = asks.request(t.id, "10", "claude", "a", "a");
        let _ = asks.request(t.id, "10", "claude", "b", "b");
        let v = call_global(
            &db,
            &asks,
            &bus,
            "issue_status",
            &json!({ "thread_id": t.id }),
        )
        .await;
        let parsed: Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["open_asks_count"], 2);
        assert_eq!(parsed["title"], "issue");
    }

    #[tokio::test]
    async fn create_issue_persists_thread() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let w = repo::create_workspace(&db, "ws").await.unwrap();
        let v = call_global(
            &db,
            &asks,
            &bus,
            "create_issue",
            &json!({ "workspace_id": w.id, "title": "new feature", "kind": "feature" }),
        )
        .await;
        let parsed: Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["title"], "new feature");
        assert_eq!(parsed["kind"], "feature");
        // confirm it landed in the DB
        let ts = repo::list_threads(&db, w.id).await.unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].title, "new feature");
    }

    #[tokio::test]
    async fn unknown_tool_returns_friendly_message() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let v = call_global(&db, &asks, &bus, "bogus", &json!({})).await;
        assert!(v["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }
}
