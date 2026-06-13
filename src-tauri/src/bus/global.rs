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

fn issue_id_arg(args: &Value) -> Option<i32> {
    args.get("issue_id")
        .or_else(|| args.get("thread_id"))
        .and_then(|v| v.as_i64())
        .map(|x| x as i32)
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
            let Some(tid) = issue_id_arg(args) else {
                return text_result("error: issue_id required".into());
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
            let Some(tid) = issue_id_arg(args) else {
                return text_result("error: issue_id required".into());
            };
            let Some(ask_id) = args.get("ask_id").and_then(|v| v.as_u64()) else {
                return text_result("error: ask_id required".into());
            };
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if bus.answer_ask(tid, ask_id, text) {
                text_result(format!("answered ask #{ask_id} on issue {tid}"))
            } else {
                text_result(format!(
                    "ask #{ask_id} on issue {tid} was already answered or no longer exists"
                ))
            }
        }
        "message_lead" => {
            let Some(tid) = issue_id_arg(args) else {
                return text_result("error: issue_id required".into());
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
                Ok(()) => text_result(format!("delivered to lead of issue {tid}")),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "create_issue_from_im" => {
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
                .trim();
            let kind = args
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if title.is_empty() {
                return text_result("error: title required".into());
            }
            if kind.is_empty() {
                return text_result("error: kind required".into());
            }
            match create_issue_from_im(db, ws, title, kind, args).await {
                Ok(v) => json_result(v),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "ensure_issue_im_topic" => {
            let Some(tid) = issue_id_arg(args) else {
                return text_result("error: issue_id required".into());
            };
            match ensure_issue_im_topic(db, tid, args).await {
                Ok(v) => json_result(v),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "ensure_issue_topic" => {
            let Some(tid) = issue_id_arg(args) else {
                return text_result("error: issue_id required".into());
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
                .unwrap_or("")
                .to_string();
            if title.trim().is_empty() {
                return text_result("error: title required".into());
            }
            if kind.trim().is_empty() {
                return text_result("error: kind required".into());
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

async fn internal_concierge_workspace_id(db: &Db) -> anyhow::Result<Option<i32>> {
    Ok(repo::get_setting(db, repo::K_CONCIERGE_WORKSPACE)
        .await?
        .and_then(|s| s.parse::<i32>().ok()))
}

async fn list_workspaces(db: &Db) -> anyhow::Result<Value> {
    let hidden = internal_concierge_workspace_id(db).await?;
    let mut out = Vec::new();
    for w in repo::list_workspaces(db).await? {
        if Some(w.id) == hidden {
            continue;
        }
        let count = repo::list_threads(db, w.id)
            .await
            .map(|v| v.into_iter().filter(|t| t.kind != "concierge").count())
            .unwrap_or(0);
        out.push(json!({ "id": w.id, "name": w.name, "issue_count": count }));
    }
    Ok(Value::Array(out))
}

async fn list_issues(db: &Db, ws: Option<i32>) -> anyhow::Result<Value> {
    let hidden = internal_concierge_workspace_id(db).await?;
    let workspaces = match ws {
        Some(id) if Some(id) != hidden => vec![id],
        Some(_) => Vec::new(),
        None => repo::list_workspaces(db)
            .await?
            .into_iter()
            .filter(|w| Some(w.id) != hidden)
            .map(|w| w.id)
            .collect(),
    };
    let mut out = Vec::new();
    for w in workspaces {
        for t in repo::list_threads(db, w).await? {
            if t.kind == "concierge" {
                continue;
            }
            out.push(json!({
                "issue_id": t.id,
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
        "issue_id": t.id,
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
                "issue_id": a.thread,
                "issue_title": a.thread_title,
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
    crate::lead_chat::engine::send(app, db, &eng, text, Vec::new(), Vec::new(), None).await
}

fn im_provider(args: &Value) -> &str {
    args.pointer("/im_context/provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn im_issue_topic_supported(args: &Value) -> bool {
    args.pointer("/im_context/capabilities/issue_topic/supported")
        .or_else(|| args.pointer("/im_context/capabilities/issue_thread/supported"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn im_chat_id(args: &Value) -> Option<&str> {
    args.pointer("/im_context/conversation/chat_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

async fn create_issue_from_im(
    db: &Db,
    ws: i32,
    title: &str,
    kind: &str,
    args: &Value,
) -> anyhow::Result<Value> {
    let issue = create_issue(db, ws, title, kind).await?;
    let thread_id = issue["issue_id"].as_i64().unwrap_or_default() as i32;
    let provider = im_provider(args);
    let supported = im_issue_topic_supported(args);
    let mut im = json!({
        "provider": provider,
        "topic_exists": false,
        "topic_created": false,
        "topic_ref": null,
        "open_hint": "provider does not support issue topic in this conversation"
    });
    if provider == "feishu" && supported {
        if let Some(chat_id) = im_chat_id(args) {
            match ensure_issue_topic(db, thread_id, chat_id).await {
                Ok(v) => {
                    im = json!({
                        "provider": provider,
                        "topic_exists": true,
                        "topic_created": v.get("created").and_then(|x| x.as_bool()).unwrap_or(false),
                        "topic_ref": v.get("topic_ref").cloned().unwrap_or(Value::Null),
                        "chat_id": v.get("chat_id").cloned().unwrap_or(Value::Null),
                        "open_hint": "已创建或复用飞书 topic，请进入该 topic 继续讨论"
                    });
                }
                Err(e) => {
                    im = json!({
                        "provider": provider,
                        "topic_exists": false,
                        "topic_created": false,
                        "topic_ref": null,
                        "open_hint": format!("issue created, but IM topic was not created: {e}")
                    });
                }
            }
        }
    }
    Ok(json!({ "issue": issue, "im": im }))
}
async fn ensure_issue_im_topic(db: &Db, thread_id: i32, args: &Value) -> anyhow::Result<Value> {
    let issue = repo::get_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("issue {thread_id} not found"))?;
    let provider = im_provider(args);
    let supported = im_issue_topic_supported(args);
    let initial_message = args
        .get("initial_message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let mut im = json!({
        "provider": provider,
        "topic_exists": false,
        "topic_created": false,
        "topic_ref": null,
        "open_hint": "provider does not support issue topic in this conversation"
    });
    if let Some(route) = repo::im_route_of_thread(db, thread_id).await? {
        im = json!({
            "provider": route.channel,
            "topic_exists": true,
            "topic_created": false,
            "topic_ref": route.im_thread_ref,
            "chat_id": route.chat_id,
            "open_hint": "已有 issue topic，请进入那里继续讨论"
        });
    } else if provider == "feishu" && supported {
        if let Some(chat_id) = im_chat_id(args) {
            let v = ensure_issue_topic(db, thread_id, chat_id).await?;
            im = json!({
                "provider": provider,
                "topic_exists": true,
                "topic_created": v.get("created").and_then(|x| x.as_bool()).unwrap_or(false),
                "topic_ref": v.get("topic_ref").cloned().unwrap_or(Value::Null),
                "chat_id": v.get("chat_id").cloned().unwrap_or(Value::Null),
                "open_hint": "已创建或复用飞书 topic，请进入该 topic 继续讨论"
            });
        }
    }
    let delivered = if !initial_message.is_empty() {
        message_lead(db, thread_id, initial_message).await.is_ok()
    } else {
        false
    };
    Ok(json!({
        "issue": {
            "issue_id": issue.id,
            "workspace_id": issue.workspace_id,
            "title": issue.title,
            "kind": issue.kind
        },
        "im": im,
        "lead_message_delivered": delivered
    }))
}

async fn create_issue(db: &Db, ws: i32, title: &str, kind: &str) -> anyhow::Result<Value> {
    let tool = crate::tools::default_tool(db).await;
    let t = repo::create_thread(db, ws, title, kind, &tool).await?;
    Ok(json!({
        "issue_id": t.id,
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
        "issue_id": after.thread_id,
        "chat_id": after.chat_id,
        "topic_ref": after.im_thread_ref,
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
            "description": "List every user workspace (id, name, issue_count). Internal IM Concierge storage is hidden. Call before answering any question that mentions \"workspaces\" or \"issues\".",
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
            "inputSchema": { "type": "object", "properties": { "issue_id": i() }, "required": ["issue_id"] }
        },
        {
            "name": "pending_needs_you",
            "description": "Every open permission Ask across all workspaces — id, issue, asking direction, tool, summary, ts. Use this when the human asks \"what's waiting on me\".",
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
                "properties": { "issue_id": i(), "ask_id": u(), "text": s() },
                "required": ["issue_id", "ask_id", "text"] }
        },
        {
            "name": "message_lead",
            "description": "Send a message into an issue's lead engine, as if the human typed it in the desktop. Use when the human wants to nudge a specific issue's lead from IM.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "text": s() },
                "required": ["issue_id", "text"] }
        },
        {
            "name": "create_issue_from_im",
            "description": "Create a Weft issue from the current IM conversation. If the provider supports issue topics in this conversation, create or bind one by default so the user continues in the issue-specific discussion location.",
            "inputSchema": { "type": "object",
                "properties": { "workspace_id": i(), "title": s(), "kind": s(), "im_context": { "type": "object" } },
                "required": ["workspace_id", "title", "kind", "im_context"] }
        },
        {
            "name": "ensure_issue_im_topic",
            "description": "Ensure an existing issue has a provider-native IM topic and guide the user there. Use when the user wants to open, enter, intervene in, or continue an issue from IM. initial_message is optional and should be set only when the user gave concrete text to relay to the lead.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "im_context": { "type": "object" }, "initial_message": s() },
                "required": ["issue_id", "im_context"] }
        },
        {
            "name": "ensure_issue_topic",
            "description": "Ensure an existing issue has a Feishu topic in chat_id. Use only when the user semantically asks to create/open/continue an issue-specific Feishu topic; do not call for ordinary chat.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "chat_id": s() },
                "required": ["issue_id", "chat_id"] }
        },
        {
            "name": "create_issue",
            "description": "File a new issue in a workspace. kind is required and must be chosen explicitly: feature|bugfix|refactor|spike.",
            "inputSchema": { "type": "object",
                "properties": { "workspace_id": i(), "title": s(), "kind": s() },
                "required": ["workspace_id", "title", "kind"] }
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
        assert_eq!(parsed[0]["issue_count"], 2);
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
        assert_eq!(parsed[0]["issue_id"], t.id);
        assert_eq!(parsed[0]["issue_title"], "登录修复");
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

    #[test]
    fn global_specs_include_im_aware_issue_tools() {
        let specs = global_specs();
        let names: Vec<String> = specs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect();
        assert!(names.contains(&"create_issue_from_im".to_string()));
        assert!(names.contains(&"ensure_issue_im_topic".to_string()));
    }

    #[test]
    fn global_specs_expose_issue_id_not_thread_id() {
        let specs = global_specs();
        let text = specs.to_string();
        assert!(text.contains("issue_id"));
        assert!(!text.contains("thread_id"));
    }

    #[tokio::test]
    async fn list_workspaces_hides_internal_concierge_workspace() {
        let db = mem_db().await;
        let visible = repo::create_workspace(&db, "Product").await.unwrap();
        let hidden = repo::create_workspace(&db, "Concierge").await.unwrap();
        repo::set_setting(&db, repo::K_CONCIERGE_WORKSPACE, &hidden.id.to_string())
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(
            call_global(
                &db,
                &AskRegistry::new(),
                &BusRegistry::new(),
                "list_workspaces",
                &json!({}),
            )
            .await["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();

        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["id"], visible.id);
    }

    #[tokio::test]
    async fn create_issue_from_im_without_thread_support_creates_issue_only() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        let args = json!({
            "workspace_id": ws.id,
            "title": "New task",
            "kind": "feature",
            "im_context": {
                "provider": "none",
                "conversation": { "chat_id": "c" },
                "capabilities": { "issue_topic": { "supported": false } }
            }
        });

        let result = call_global(&db, &asks, &bus, "create_issue_from_im", &args).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("New task"));
        assert!(text.contains("topic_created"));
        assert!(text.contains("false"));
    }

    #[tokio::test]
    async fn ensure_issue_im_topic_reuses_existing_route() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude")
            .await
            .unwrap();
        repo::bind_im_route(&db, issue.id, "feishu", "oc_chat", "om_root")
            .await
            .unwrap();
        let args = json!({
            "issue_id": issue.id,
            "im_context": {
                "provider": "feishu",
                "conversation": { "chat_id": "oc_chat" },
                "capabilities": { "issue_topic": { "supported": true } }
            }
        });

        let result = call_global(&db, &asks, &bus, "ensure_issue_im_topic", &args).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("om_root"));
        assert!(text.contains("\"topic_created\":false"));
    }

    #[tokio::test]
    async fn read_only_global_queries_do_not_create_im_routes() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude")
            .await
            .unwrap();

        let _ = call_global(
            &db,
            &asks,
            &bus,
            "list_issues",
            &json!({ "workspace_id": ws.id }),
        )
        .await;
        let _ = call_global(
            &db,
            &asks,
            &bus,
            "issue_status",
            &json!({ "thread_id": issue.id }),
        )
        .await;

        let route = repo::im_route_of_thread(&db, issue.id).await.unwrap();
        assert!(route.is_none());
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
    async fn create_issue_requires_kind() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let w = repo::create_workspace(&db, "ws").await.unwrap();
        let v = call_global(
            &db,
            &asks,
            &bus,
            "create_issue",
            &json!({ "workspace_id": w.id, "title": "new feature" }),
        )
        .await;
        let s = v["content"][0]["text"].as_str().unwrap();
        assert!(s.contains("kind required"));
        assert!(repo::list_threads(&db, w.id).await.unwrap().is_empty());
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
