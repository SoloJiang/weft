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
use tauri::Manager;

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

const IM_CONTEXT_MUTATING_TOOLS: [&str; 7] = [
    "answer_permission",
    "answer_question",
    "message_lead",
    "create_issue_from_im",
    "ensure_issue_im_topic",
    "ensure_issue_topic",
    "create_issue",
];

/// Per-tool dispatch. Errors short-return via text_result so MCP clients see a
/// friendly message instead of a transport failure (mirrors `call_planner`).
pub async fn call_global(
    db: &Db,
    asks: &AskRegistry,
    bus: &BusRegistry,
    name: &str,
    args: &Value,
) -> Value {
    // Every tool that mutates on behalf of an IM conversation shares the same
    // read lease. Provider/owner/enable changes take the write side through
    // persistence + bridge retirement, so ask answers, DB writes, and the
    // actual lead enqueue are each linearized wholly before or after a revoke.
    let _authority = if IM_CONTEXT_MUTATING_TOOLS.contains(&name) {
        match crate::APP_HANDLE.get() {
            Some(app) => Some(
                app.state::<crate::im::ImBridge>()
                    .authority_read_lease()
                    .await,
            ),
            None => None,
        }
    } else {
        None
    };
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
            match issue_status(db, asks, bus, tid).await {
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
            if let Err(e) = require_active_im_context(db, args).await {
                return text_result(format!("error: {e}"));
            }
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
            if let Err(e) = require_active_im_context(db, args).await {
                return text_result(format!("error: {e}"));
            }
            match answer_durable_question(db, bus, tid, ask_id, text).await {
                Ok(true) => text_result(format!("answered ask #{ask_id} on issue {tid}")),
                Ok(false) => text_result(format!(
                    "ask #{ask_id} on issue {tid} was already answered or no longer exists"
                )),
                Err(error) => text_result(format!("error: {error}")),
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
            match message_lead(db, tid, &text, args).await {
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
            match ensure_issue_topic_from_im(db, tid, chat_id, args).await {
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
            if let Err(e) = require_active_im_context(db, args).await {
                return text_result(format!("error: {e}"));
            }
            // Keep the same context through the final repository boundary;
            // routing preparation above can await long enough for a stale
            // Concierge turn to otherwise outlive its authority.
            match create_issue(db, ws, &title, &kind, Some(args)).await {
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
            .map(|v| {
                v.into_iter()
                    .filter(|t| t.kind != "concierge" && t.kind != "curator")
                    .count()
            })
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
            if t.kind == "concierge" || t.kind == "curator" {
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

async fn issue_status(
    db: &Db,
    asks: &AskRegistry,
    bus: &BusRegistry,
    tid: i32,
) -> anyhow::Result<Value> {
    let t = repo::get_thread(db, tid)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {tid} not found"))?;
    let open_asks = asks.open_in(tid).len();
    let readiness = crate::readiness::collect_with_check_execution(
        db,
        bus,
        tid,
        crate::readiness::CheckExecution::CachedOnly,
    )
    .await?;
    Ok(json!({
        "issue_id": t.id,
        "title": t.title,
        "kind": t.kind,
        "open_asks_count": open_asks,
        "readiness": readiness,
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

/// Resolve a durable free-text question from the global/Concierge tool with
/// the same OCC + persisted-answer path used by desktop and IM. The bus event
/// closes cards/trail; its request-tagged inbox message remains replayable
/// until the asking agent explicitly acknowledges it through `bus_ack`.
async fn answer_durable_question(
    db: &Db,
    bus: &BusRegistry,
    thread_id: i32,
    ask_id: u64,
    text: &str,
) -> anyhow::Result<bool> {
    let Ok(request_id) = i32::try_from(ask_id) else {
        return Ok(false);
    };
    Ok(crate::attention::answer_durable_human_request(
        db,
        bus,
        request_id,
        Some(thread_id),
        None,
        None,
        text,
    )
    .await?
    .is_some())
}

/// Push a message into the lead engine of `thread_id` from outside (Concierge).
/// Pulls the global `AppHandle` from the `OnceLock` set in `setup()` — by the
/// time an MCP request lands, the Tauri builder is long past that point.
async fn message_lead_origin(
    db: &Db,
    thread_id: i32,
    args: &Value,
) -> anyhow::Result<Option<String>> {
    let provider = require_active_im_context(db, args).await?.as_str();
    let reply_to = im_reply_to(args);
    let route = repo::im_route_of_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("issue {thread_id} has no active IM topic"))?;
    if route.channel != provider {
        anyhow::bail!(
            "issue {thread_id} is bound to {}, not the active {provider} conversation",
            route.channel
        );
    }
    if route.channel == "dingtalk" {
        let reply_to = reply_to.ok_or_else(|| {
            anyhow::anyhow!("dingtalk lead delivery requires the current conversation reply target")
        })?;
        return Ok(Some(reply_to.to_string()));
    }
    // Feishu issue routes retain their stable topic/seed fallback. The current
    // Concierge message can be outside that topic and must not become its reply
    // parent merely because both conversations use the same provider.
    Ok(None)
}

async fn message_lead(db: &Db, thread_id: i32, text: &str, args: &Value) -> anyhow::Result<()> {
    let app = crate::APP_HANDLE
        .get()
        .ok_or_else(|| anyhow::anyhow!("app handle not initialized"))?;
    // Resolve the delivery target before starting the engine. DingTalk issue
    // output cannot fall back to a stable topic message, so a no-origin turn
    // would run successfully while silently discarding its response.
    message_lead_origin(db, thread_id, args).await?;
    let eng = crate::lead_chat::commands::lead_engine(app, db, thread_id, im_lang(args)).await?;
    // Revalidate after engine lookup while call_global's authority lease
    // remains held; `engine::send` cannot straddle a reset/switch/disable.
    let origin = message_lead_origin(db, thread_id, args).await?;
    crate::lead_chat::engine::send(app, db, &eng, text, Vec::new(), Vec::new(), origin).await
}

fn im_provider(args: &Value) -> &str {
    args.pointer("/im_context/provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

async fn require_active_im_context(
    db: &Db,
    args: &Value,
) -> anyhow::Result<crate::im::ImProvider> {
    let context_provider = im_provider(args);
    if context_provider.is_empty() {
        anyhow::bail!("current IM provider context is required");
    }
    let context_provider = crate::im::ImProvider::parse(context_provider)?;
    let sender_open_id = im_sender_id(args)
        .ok_or_else(|| anyhow::anyhow!("current IM sender context is required"))?;
    crate::im::ImSettings::require_active_owner(db, context_provider, sender_open_id).await?;
    Ok(context_provider)
}

fn im_lang(args: &Value) -> &str {
    match args.pointer("/im_context/locale").and_then(Value::as_str) {
        Some("en") => "en",
        _ => "zh",
    }
}

/// Whether a topic can be created from the CURRENT conversation — not merely
/// whether the provider supports topics in general. Feishu DMs report
/// `supported: true` but `can_create_from_current_conversation: false`, so gating
/// on this avoids attempting a topic (with the DM chat id) where none can exist.
fn im_can_create_topic_here(args: &Value) -> bool {
    args.pointer("/im_context/capabilities/issue_topic/can_create_from_current_conversation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn im_chat_id(args: &Value) -> Option<&str> {
    args.pointer("/im_context/conversation/chat_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn im_reply_to(args: &Value) -> Option<&str> {
    args.pointer("/im_context/conversation/reply_to")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn im_sender_id(args: &Value) -> Option<&str> {
    args.pointer("/im_context/conversation/sender_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

async fn create_issue_from_im(
    db: &Db,
    ws: i32,
    title: &str,
    kind: &str,
    args: &Value,
) -> anyhow::Result<Value> {
    let active_provider = require_active_im_context(db, args).await?;
    let provider = active_provider.as_str();
    let issue = create_issue(db, ws, title, kind, Some(args)).await?;
    let thread_id = issue["issue_id"].as_i64().unwrap_or_default() as i32;
    let can_create = im_can_create_topic_here(args);
    let open_hint = if active_provider == crate::im::ImProvider::DingTalk {
        format!("/bind {thread_id}")
    } else {
        "provider does not support issue topic in this conversation".to_string()
    };
    let mut im = json!({
        "provider": provider,
        "topic_exists": false,
        "topic_created": false,
        "topic_ref": null,
        "open_hint": open_hint
    });
    if provider == "feishu" && can_create {
        if let Some(chat_id) = im_chat_id(args) {
            match require_active_im_context(db, args).await {
                Ok(_) => match ensure_issue_topic(db, thread_id, chat_id, args).await {
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
                },
                Err(e) => {
                    im = json!({
                        "provider": provider,
                        "topic_exists": false,
                        "topic_created": false,
                        "topic_ref": null,
                        "open_hint": format!("issue created, but the IM provider changed before topic creation: {e}")
                    });
                }
            }
        }
    }
    Ok(json!({ "issue": issue, "im": im }))
}
async fn ensure_issue_im_topic(db: &Db, thread_id: i32, args: &Value) -> anyhow::Result<Value> {
    let active_provider = require_active_im_context(db, args).await?;
    let issue = repo::get_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("issue {thread_id} not found"))?;
    let provider = active_provider.as_str();
    let can_create = im_can_create_topic_here(args);
    let initial_message = args
        .get("initial_message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let open_hint = if provider == "dingtalk" {
        format!("/bind {thread_id}")
    } else {
        "provider does not support issue topic in this conversation".to_string()
    };
    let mut im = json!({
        "provider": provider,
        "topic_exists": false,
        "topic_created": false,
        "topic_ref": null,
        "open_hint": open_hint
    });
    let existing_route = repo::im_route_of_thread(db, thread_id).await?;
    if let Some(route) = existing_route
        .as_ref()
        .filter(|route| route.channel == provider)
    {
        im = json!({
            "provider": route.channel,
            "topic_exists": true,
            "topic_created": false,
            "topic_ref": route.im_thread_ref,
            "chat_id": route.chat_id,
            "open_hint": "已有 issue topic，请进入那里继续讨论"
        });
    } else if provider == "feishu" && can_create {
        if let Some(chat_id) = im_chat_id(args) {
            require_active_im_context(db, args).await?;
            let v = ensure_issue_topic(db, thread_id, chat_id, args).await?;
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
    // Only relay the initial message once a topic actually exists — otherwise the
    // lead's reply has no IM route back to the user, yet we'd report it delivered.
    let has_topic = im
        .get("topic_exists")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let delivered = if has_topic && !initial_message.is_empty() {
        message_lead(db, thread_id, initial_message, args)
            .await
            .is_ok()
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

struct PreparedIssue {
    route: crate::engine_routing::RouteDecision,
    tool: String,
}

async fn prepare_issue(db: &Db) -> PreparedIssue {
    let legacy_tool = crate::tools::default_tool(db).await;
    let route = crate::engine_routing::resolve_for_db(
        db,
        None,
        &legacy_tool,
        crate::engine_routing::RoutingHint::Normal,
    )
    .await;
    let tool = route
        .selected()
        .map(|selected| selected.as_str().to_string())
        .unwrap_or(legacy_tool);
    PreparedIssue { route, tool }
}

async fn persist_prepared_issue(
    db: &Db,
    ws: i32,
    title: &str,
    kind: &str,
    prepared: PreparedIssue,
    im_args: Option<&Value>,
) -> anyhow::Result<Value> {
    let PreparedIssue { route, tool } = prepared;
    // Default-tool and routing resolution above can await. An IM Concierge turn
    // may be retired during those reads, so validate at the final repository
    // boundary as well as at tool entry before creating any durable issue.
    if let Some(args) = im_args {
        require_active_im_context(db, args).await?;
    }
    let t = repo::create_thread(db, ws, title, kind, &tool).await?;
    crate::engine_routing::record_decision(db, t.id, None, None, "new_issue", &route).await;
    Ok(json!({
        "issue_id": t.id,
        "workspace_id": t.workspace_id,
        "title": t.title,
        "kind": t.kind,
    }))
}

async fn create_issue(
    db: &Db,
    ws: i32,
    title: &str,
    kind: &str,
    im_args: Option<&Value>,
) -> anyhow::Result<Value> {
    let prepared = prepare_issue(db).await;
    persist_prepared_issue(db, ws, title, kind, prepared, im_args).await
}

async fn ensure_issue_topic(
    db: &Db,
    thread_id: i32,
    chat_id: &str,
    args: &Value,
) -> anyhow::Result<Value> {
    let before = repo::im_route_of_thread(db, thread_id).await?;
    let created = before.as_ref().map(|route| route.channel.as_str()) != Some("feishu");
    let settings = crate::im::ImSettings::load(db).await?;
    if settings.provider != crate::im::ImProvider::Feishu {
        anyhow::bail!("Feishu is not the active IM provider");
    }
    if !settings.ready() {
        anyhow::bail!("Feishu app credentials are not configured");
    }
    let sender_open_id = im_sender_id(args)
        .ok_or_else(|| anyhow::anyhow!("current IM sender context is required"))?;
    let ch = crate::im::feishu::FeishuChannel::new(&settings.app_id, &settings.app_secret)?;
    crate::im::ensure_issue_topic(
        db,
        &ch,
        thread_id,
        chat_id,
        sender_open_id,
        None,
        "zh",
    )
    .await?;
    let after = repo::im_route_of_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("topic route was not created"))?;
    Ok(json!({
        "issue_id": after.thread_id,
        "chat_id": after.chat_id,
        "topic_ref": after.im_thread_ref,
        "created": created,
    }))
}

async fn ensure_issue_topic_from_im(
    db: &Db,
    thread_id: i32,
    chat_id: &str,
    args: &Value,
) -> anyhow::Result<Value> {
    let provider = require_active_im_context(db, args).await?;
    if provider != crate::im::ImProvider::Feishu {
        anyhow::bail!("the active IM provider cannot create a Feishu topic");
    }
    ensure_issue_topic(db, thread_id, chat_id, args).await
}

// ───────────────────── tool specs ─────────────────────

pub fn global_specs() -> Value {
    let s = || json!({ "type": "string" });
    let i = || json!({ "type": "integer" });
    let u = || json!({ "type": "integer", "minimum": 0 });
    let im_context = || {
        json!({
            "type": "object",
            "properties": {
                "provider": { "type": "string", "enum": ["feishu", "dingtalk"] },
                "locale": { "type": "string", "enum": ["zh", "en"] },
                "conversation": {
                    "type": "object",
                    "properties": {
                        "chat_id": { "type": "string" },
                        "topic_ref": { "type": "string" },
                        "reply_to": { "type": ["string", "null"] },
                        "sender_id": { "type": "string" }
                    },
                    "required": ["chat_id", "topic_ref", "reply_to", "sender_id"]
                },
                "capabilities": {
                    "type": "object",
                    "properties": {
                        "issue_topic": {
                            "type": "object",
                            "properties": {
                                "supported": { "type": "boolean" },
                                "default_on_create_issue": { "type": "boolean" },
                                "can_create_from_current_conversation": { "type": "boolean" },
                                "terminology": {
                                    "type": "object",
                                    "properties": {
                                        "zh": { "type": "string" },
                                        "en": { "type": "string" }
                                    },
                                    "required": ["zh", "en"]
                                }
                            },
                            "required": [
                                "supported",
                                "default_on_create_issue",
                                "can_create_from_current_conversation",
                                "terminology"
                            ]
                        },
                        "reply": {
                            "type": "object",
                            "properties": { "supported": { "type": "boolean" } },
                            "required": ["supported"]
                        },
                        "issue_conversation_binding": {
                            "type": "object",
                            "properties": {
                                "supported": { "type": "boolean" },
                                "can_bind_current_conversation": { "type": "boolean" },
                                "command": { "type": "string" }
                            },
                            "required": ["supported", "can_bind_current_conversation", "command"]
                        }
                    },
                    "required": ["issue_topic", "reply", "issue_conversation_binding"]
                }
            },
            "required": ["provider", "locale", "conversation", "capabilities"]
        })
    };
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
            "description": "Answer a permission ask on behalf of the human from the current IM conversation. verdict ∈ allow|deny|always|full. always = remember this exact action for the asking task; full = grant the task full access (skips future asks). Pass the unchanged current im_context so a retired provider or owner turn cannot mutate the registry.",
            "inputSchema": { "type": "object",
                "properties": { "ask_id": u(), "verdict": s(), "im_context": im_context() },
                "required": ["ask_id", "verdict", "im_context"] }
        },
        {
            "name": "answer_question",
            "description": "Reply to an agent's open question (ask_human) from the current IM conversation. The text is delivered into that task's bus inbox only while that im_context provider and sender are still active.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "ask_id": u(), "text": s(), "im_context": im_context() },
                "required": ["issue_id", "ask_id", "text", "im_context"] }
        },
        {
            "name": "message_lead",
            "description": "Send a message into an issue's lead engine, as if the human typed it in the desktop. Use when the human wants to nudge a specific issue's lead from IM; pass the current im_context so the lead preserves the active locale and reply target.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "text": s(), "im_context": im_context() },
                "required": ["issue_id", "text", "im_context"] }
        },
        {
            "name": "create_issue_from_im",
            "description": "Create a Weft issue from the current IM conversation. If the provider supports issue topics in this conversation, create or bind one by default so the user continues in the issue-specific discussion location.",
            "inputSchema": { "type": "object",
                "properties": { "workspace_id": i(), "title": s(), "kind": s(), "im_context": im_context() },
                "required": ["workspace_id", "title", "kind", "im_context"] }
        },
        {
            "name": "ensure_issue_im_topic",
            "description": "Ensure an existing issue has a provider-native IM topic and guide the user there. Use when the user wants to open, enter, intervene in, or continue an issue from IM. initial_message is optional and should be set only when the user gave concrete text to relay to the lead.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "im_context": im_context(), "initial_message": s() },
                "required": ["issue_id", "im_context"] }
        },
        {
            "name": "ensure_issue_topic",
            "description": "Ensure an existing issue has a Feishu topic in chat_id. Use only when the user semantically asks to create/open/continue an issue-specific Feishu topic; do not call for ordinary chat.",
            "inputSchema": { "type": "object",
                "properties": { "issue_id": i(), "chat_id": s(), "im_context": im_context() },
                "required": ["issue_id", "chat_id", "im_context"] }
        },
        {
            "name": "create_issue",
            "description": "File a new issue in a workspace from the current IM conversation. kind is required and must be chosen explicitly: feature|bugfix|refactor|spike. Pass the unchanged current im_context so a retired provider, owner, or disabled bridge cannot create durable work.",
            "inputSchema": { "type": "object",
                "properties": { "workspace_id": i(), "title": s(), "kind": s(), "im_context": im_context() },
                "required": ["workspace_id", "title", "kind", "im_context"] }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::RiskLevel;
    use crate::store::Db;

    async fn mem_db() -> Db {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, crate::im::K_ALLOW, "owner")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_ENABLED, "1")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ENABLED, "1")
            .await
            .unwrap();
        db
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
        let (id, _rx) = asks.request(
            t.id,
            "10",
            "claude",
            "Run: npm test",
            "npm test",
            RiskLevel::Unknown,
            "npm test",
        );
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
        let (id, rx) = asks.request(
            1,
            "10",
            "claude",
            "Run: npm test",
            "npm test",
            RiskLevel::Unknown,
            "npm test",
        );
        let v = call_global(
            &db,
            &asks,
            &bus,
            "answer_permission",
            &json!({
                "ask_id": id,
                "verdict": "allow",
                "im_context": {
                    "provider": "feishu",
                    "conversation": { "sender_id": "owner" }
                }
            }),
        )
        .await;
        assert!(v["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("answered"));
        assert_eq!(rx.await.unwrap(), crate::ask::Decision::Allow);
    }

    #[tokio::test]
    async fn stale_im_context_cannot_answer_permission_or_human_question() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let (permission_id, _permission_rx) = asks.request(
            1,
            "10",
            "claude",
            "Run tests",
            "cargo test",
            RiskLevel::Unknown,
            "cargo test",
        );
        let human_id = bus.ask_human(1, "10", "Which release channel?");
        let stale_context = json!({
            "provider": "feishu",
            "conversation": { "sender_id": "owner" }
        });
        // Both asks were opened by a Feishu Concierge turn. The user switched
        // to DingTalk before that turn reached either mutating global tool.
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();

        let permission = call_global(
            &db,
            &asks,
            &bus,
            "answer_permission",
            &json!({
                "ask_id": permission_id,
                "verdict": "full",
                "im_context": stale_context.clone()
            }),
        )
        .await;
        let question = call_global(
            &db,
            &asks,
            &bus,
            "answer_question",
            &json!({
                "issue_id": 1,
                "ask_id": human_id,
                "text": "stable",
                "im_context": stale_context
            }),
        )
        .await;

        assert!(permission["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("stale IM context")));
        assert!(question["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("stale IM context")));
        assert!(asks.open().iter().any(|ask| ask.id == permission_id));
        assert!(bus.open_asks(1).iter().any(|ask| ask.id == human_id));
    }

    #[tokio::test]
    async fn owner_reset_invalidates_in_flight_dingtalk_mutations() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let workspace = repo::create_workspace(&db, "alpha").await.unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "former_owner")
            .await
            .unwrap();
        let former_context = json!({
            "provider": "dingtalk",
            "conversation": { "sender_id": "former_owner" }
        });
        let (permission_id, _permission_rx) = asks.request(
            1,
            "10",
            "claude",
            "Run tests",
            "cargo test",
            RiskLevel::Unknown,
            "cargo test",
        );
        let human_id = bus.ask_human(1, "10", "Which release channel?");

        // Model an issue tool already in flight when the owner is reset: its
        // asynchronous preparation completed for the former owner. A different
        // account then binds before the old turn reaches any final mutation.
        let prepared = prepare_issue(&db).await;
        crate::im::reset_owner(&db, crate::im::ImProvider::DingTalk)
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "replacement_owner")
            .await
            .unwrap();

        let permission = call_global(
            &db,
            &asks,
            &bus,
            "answer_permission",
            &json!({
                "ask_id": permission_id,
                "verdict": "full",
                "im_context": former_context.clone()
            }),
        )
        .await;
        let question = call_global(
            &db,
            &asks,
            &bus,
            "answer_question",
            &json!({
                "issue_id": 1,
                "ask_id": human_id,
                "text": "stale answer",
                "im_context": former_context.clone()
            }),
        )
        .await;
        let create_args = json!({ "im_context": former_context });
        let create_error = persist_prepared_issue(
            &db,
            workspace.id,
            "Must not exist",
            "feature",
            prepared,
            Some(&create_args),
        )
        .await
        .unwrap_err();

        for result in [permission, question] {
            assert!(result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("stale IM context")));
        }
        assert!(create_error.to_string().contains("stale IM context"));
        assert!(asks.open().iter().any(|ask| ask.id == permission_id));
        assert!(bus.open_asks(1).iter().any(|ask| ask.id == human_id));
        assert!(repo::list_threads(&db, workspace.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn disabling_active_provider_invalidates_in_flight_im_mutations() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let workspace = repo::create_workspace(&db, "alpha").await.unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "owner")
            .await
            .unwrap();
        let context = json!({
            "provider": "dingtalk",
            "conversation": { "sender_id": "owner" }
        });
        let (permission_id, _permission_rx) = asks.request(
            1,
            "10",
            "claude",
            "Run tests",
            "cargo test",
            RiskLevel::Unknown,
            "cargo test",
        );
        let human_id = bus.ask_human(1, "10", "Which release channel?");

        // Model a tool call whose asynchronous preparation began while remote
        // control was enabled, then the active provider was disabled before
        // its final registry/DB mutation boundary.
        let prepared = prepare_issue(&db).await;
        repo::set_setting(&db, crate::im::K_DINGTALK_ENABLED, "0")
            .await
            .unwrap();

        let permission = call_global(
            &db,
            &asks,
            &bus,
            "answer_permission",
            &json!({
                "ask_id": permission_id,
                "verdict": "full",
                "im_context": context.clone()
            }),
        )
        .await;
        let question = call_global(
            &db,
            &asks,
            &bus,
            "answer_question",
            &json!({
                "issue_id": 1,
                "ask_id": human_id,
                "text": "stale answer",
                "im_context": context.clone()
            }),
        )
        .await;
        let create_error = persist_prepared_issue(
            &db,
            workspace.id,
            "Must not exist",
            "feature",
            prepared,
            Some(&json!({ "im_context": context })),
        )
        .await
        .unwrap_err();

        for result in [permission, question] {
            assert!(result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("provider is disabled")));
        }
        assert!(create_error.to_string().contains("provider is disabled"));
        assert!(asks.open().iter().any(|ask| ask.id == permission_id));
        assert!(bus.open_asks(1).iter().any(|ask| ask.id == human_id));
        assert!(repo::list_threads(&db, workspace.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn answer_permission_unknown_verdict_soft_errors() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let (id, _rx) = asks.request(1, "10", "claude", "x", "x", RiskLevel::Unknown, "x");
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
        let _ = asks.request(t.id, "10", "claude", "a", "a", RiskLevel::Unknown, "a");
        let _ = asks.request(t.id, "10", "claude", "b", "b", RiskLevel::Unknown, "b");
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
    fn mutating_tools_declare_the_complete_im_context() {
        let specs = global_specs();
        for name in IM_CONTEXT_MUTATING_TOOLS {
            let spec = specs
                .as_array()
                .unwrap()
                .iter()
                .find(|spec| spec["name"] == name)
                .unwrap();
            assert!(spec["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|required| required == "im_context"));
            let context = &spec["inputSchema"]["properties"]["im_context"];
            assert_eq!(
                context["required"],
                json!(["provider", "locale", "conversation", "capabilities"]),
                "{name} must preserve the complete framed context"
            );
            assert_eq!(
                context["properties"]["conversation"]["required"],
                json!(["chat_id", "topic_ref", "reply_to", "sender_id"]),
                "{name} must preserve every conversation delivery field"
            );
            assert_eq!(
                context["properties"]["capabilities"]["required"],
                json!(["issue_topic", "reply", "issue_conversation_binding"]),
                "{name} must preserve provider capabilities"
            );
            assert!(context["properties"]["capabilities"]["properties"]["issue_topic"]
                ["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|required| required == "can_create_from_current_conversation"));
        }
    }

    #[test]
    fn im_context_schema_covers_every_field_in_the_framed_bus_context() {
        fn assert_declared(schema: &Value, value: &Value, path: &str) {
            let Some(object) = value.as_object() else {
                return;
            };
            let properties = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("schema properties missing at {path}"));
            let required = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("schema required missing at {path}"));
            for (key, child) in object {
                let child_schema = properties
                    .get(key)
                    .unwrap_or_else(|| panic!("framed field {path}.{key} is undeclared"));
                assert!(
                    required.iter().any(|required| required == key),
                    "framed field {path}.{key} is optional in the tool schema"
                );
                assert_declared(child_schema, child, &format!("{path}.{key}"));
            }
        }

        let frame = crate::im::format_im_user_message(
            "ou_owner",
            "oc_chat",
            "chat:oc_chat",
            Some("om_reply"),
            "continue",
            "en",
            &crate::im::feishu_provider_capabilities(true),
        );
        let context_json = frame
            .strip_prefix("<weft:im_context>")
            .and_then(|rest| rest.split_once("</weft:im_context>"))
            .map(|(context, _)| context)
            .expect("framed IM context");
        let context: Value = serde_json::from_str(context_json).unwrap();
        let specs = global_specs();
        let schema = &specs
            .as_array()
            .unwrap()
            .iter()
            .find(|spec| spec["name"] == "message_lead")
            .unwrap()["inputSchema"]["properties"]["im_context"];

        assert_declared(schema, &context, "im_context");
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
    async fn create_issue_from_dingtalk_returns_bind_hint() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "owner")
            .await
            .unwrap();
        let args = json!({
            "workspace_id": ws.id,
            "title": "New task",
            "kind": "feature",
            "im_context": {
                "provider": "dingtalk",
                "conversation": { "chat_id": "c", "sender_id": "owner" },
                "capabilities": { "issue_topic": { "supported": false } }
            }
        });

        let result = call_global(&db, &asks, &bus, "create_issue_from_im", &args).await;
        let parsed: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        let issue_id = parsed["issue"]["issue_id"].as_i64().unwrap();
        assert_eq!(parsed["issue"]["title"], "New task");
        assert_eq!(parsed["im"]["topic_created"], false);
        assert_eq!(parsed["im"]["open_hint"], format!("/bind {issue_id}"));
    }

    #[tokio::test]
    async fn provider_switch_after_issue_preparation_prevents_database_creation() {
        let db = mem_db().await;
        let workspace = repo::create_workspace(&db, "alpha").await.unwrap();
        let stale_args = json!({
            "im_context": {
                "provider": "feishu",
                "conversation": { "sender_id": "owner" }
            }
        });

        // Model the exact await window in create_issue_from_im: tool/routing
        // preparation completed under Feishu, then the provider changed before
        // the final repo::create_thread boundary.
        let prepared = prepare_issue(&db).await;
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        let error = persist_prepared_issue(
            &db,
            workspace.id,
            "Must not exist",
            "feature",
            prepared,
            Some(&stale_args),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("stale IM context"));
        assert!(repo::list_threads(&db, workspace.id)
            .await
            .unwrap()
            .is_empty());
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
                "conversation": { "chat_id": "oc_chat", "sender_id": "owner" },
                "capabilities": { "issue_topic": { "supported": true } }
            }
        });

        let result = call_global(&db, &asks, &bus, "ensure_issue_im_topic", &args).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("om_root"));
        assert!(text.contains("\"topic_created\":false"));
    }

    #[tokio::test]
    async fn ensure_issue_im_topic_rejects_an_inactive_provider_route() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "owner")
            .await
            .unwrap();
        repo::bind_im_route(&db, issue.id, "feishu", "oc_old", "omt_old")
            .await
            .unwrap();
        let args = json!({
            "issue_id": issue.id,
            "initial_message": "继续推进",
            "im_context": {
                "provider": "dingtalk",
                "locale": "zh",
                "conversation": { "chat_id": "cid_current", "sender_id": "owner" },
                "capabilities": {
                    "issue_topic": { "supported": true, "can_create_from_current_conversation": false }
                }
            }
        });

        let result = call_global(&db, &asks, &bus, "ensure_issue_im_topic", &args).await;
        let parsed: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["im"]["provider"], "dingtalk");
        assert_eq!(parsed["im"]["topic_exists"], false);
        assert_eq!(parsed["lead_message_delivered"], false);
        assert!(parsed["im"]["open_hint"]
            .as_str()
            .is_some_and(|hint| hint.contains(&format!("/bind {}", issue.id))));
    }

    #[tokio::test]
    async fn stale_feishu_tool_call_cannot_replace_route_after_provider_switch() {
        let db = mem_db().await;
        let asks = AskRegistry::new();
        let bus = BusRegistry::new();
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude")
            .await
            .unwrap();
        repo::bind_im_route(
            &db,
            issue.id,
            "dingtalk",
            "cid_current",
            "ding-thread-current",
        )
        .await
        .unwrap();
        let stale_args = json!({
            "issue_id": issue.id,
            "im_context": {
                "provider": "feishu",
                "conversation": { "chat_id": "oc_stale", "sender_id": "owner" },
                "capabilities": {
                    "issue_topic": { "supported": true, "can_create_from_current_conversation": true }
                }
            }
        });
        // The turn captured Feishu above, then the user switched providers
        // before its global-tool call reached this process.
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "owner")
            .await
            .unwrap();

        let result = call_global(&db, &asks, &bus, "ensure_issue_im_topic", &stale_args).await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("stale IM context"));
        let route = repo::im_route_of_thread(&db, issue.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.channel, "dingtalk");
        assert_eq!(route.im_thread_ref, "ding-thread-current");
    }

    #[tokio::test]
    async fn message_lead_origin_uses_the_current_dingtalk_reply_target() {
        let db = mem_db().await;
        let ws = repo::create_workspace(&db, "alpha").await.unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();
        repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "owner")
            .await
            .unwrap();
        let issue = repo::create_thread(&db, ws.id, "Existing", "feature", "claude")
            .await
            .unwrap();
        repo::bind_im_route(
            &db,
            issue.id,
            "dingtalk",
            "cid_current",
            "ding-thread-current",
        )
        .await
        .unwrap();
        let args = json!({
            "im_context": {
                "provider": "dingtalk",
                "conversation": { "reply_to": "msg-current", "sender_id": "owner" }
            }
        });

        let origin = message_lead_origin(&db, issue.id, &args).await.unwrap();
        assert_eq!(origin.as_deref(), Some("msg-current"));

        let missing = json!({
            "im_context": {
                "provider": "dingtalk",
                "conversation": { "reply_to": "", "sender_id": "owner" }
            }
        });
        assert!(message_lead_origin(&db, issue.id, &missing).await.is_err());
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
            &json!({
                "workspace_id": w.id,
                "title": "new feature",
                "kind": "feature",
                "im_context": {
                    "provider": "feishu",
                    "conversation": { "sender_id": "owner" }
                }
            }),
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
    async fn generic_create_issue_rejects_every_revoked_im_context() {
        for revocation in ["provider", "owner", "disabled"] {
            let db = mem_db().await;
            let asks = AskRegistry::new();
            let bus = BusRegistry::new();
            let workspace = repo::create_workspace(&db, revocation).await.unwrap();
            repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
                .await
                .unwrap();
            repo::set_setting(&db, crate::im::K_DINGTALK_ALLOW, "former_owner")
                .await
                .unwrap();

            match revocation {
                "provider" => repo::set_setting(&db, crate::im::K_PROVIDER, "feishu")
                    .await
                    .unwrap(),
                "owner" => repo::set_setting(
                    &db,
                    crate::im::K_DINGTALK_ALLOW,
                    "replacement_owner",
                )
                .await
                .unwrap(),
                "disabled" => {
                    repo::set_setting(&db, crate::im::K_DINGTALK_ENABLED, "0")
                        .await
                        .unwrap()
                }
                _ => unreachable!(),
            }

            let result = call_global(
                &db,
                &asks,
                &bus,
                "create_issue",
                &json!({
                    "workspace_id": workspace.id,
                    "title": "Must not exist",
                    "kind": "feature",
                    "im_context": {
                        "provider": "dingtalk",
                        "conversation": { "sender_id": "former_owner" }
                    }
                }),
            )
            .await;

            assert!(
                result["content"][0]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("stale IM context")),
                "{revocation} revocation must reject legacy create_issue: {result}"
            );
            assert!(repo::list_threads(&db, workspace.id)
                .await
                .unwrap()
                .is_empty());
        }
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
