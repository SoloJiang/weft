//! Codex `app-server` protocol and runtime client used by the weft-codex
//! orchestrator and curator.
//!
//! It encodes client→server requests, classifies incoming lines, maps server
//! notifications to [`ChatEvent`], multiplexes subscriptions by thread id, and
//! rejects every unexpected blocking request because the product has no
//! approval or elicitation surface.
//!
//! Wire format (verified against openai/codex main, app-server-protocol):
//! codex uses a JSON-RPC-LIKE envelope with NO `"jsonrpc":"2.0"` field. Messages
//! are distinguished structurally:
//!   - request   (either direction): has `method` AND `id`            -> needs a response
//!   - notification (server→client): has `method`, NO `id`
//!   - response  (to our request):   has `id` AND `result`
//!   - error     (to our request):   has `id` AND `error{code,message}`
//! `id` (RequestId) is untagged: a JSON string or integer. We send integer ids.
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::proto::ChatEvent;

/// Encode a client→server request line (newline-terminated). `params` is sent
/// verbatim; all our requests carry params.
pub fn encode_request(id: i64, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        json!({ "id": id, "method": method, "params": params })
    )
}

/// Encode a client→server notification (no id), e.g. the `initialized` handshake.
pub fn encode_notification(method: &str, params: Option<Value>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("method".into(), Value::String(method.to_string()));
    if let Some(p) = params {
        obj.insert("params".into(), p);
    }
    format!("{}\n", Value::Object(obj))
}

/// Encode our reply to a server-initiated request (echo its `id` verbatim — it
/// may be a string or integer). Used for approval responses (Stage 2).
pub fn encode_response(id: &Value, result: Value) -> String {
    format!("{}\n", json!({ "id": id, "result": result }))
}

/// Encode an error reply to a server-initiated request — our "can't satisfy this"
/// answer to a blocking request we don't support, so the turn doesn't hang.
pub fn encode_error_response(id: &Value, code: i64, message: &str) -> String {
    format!(
        "{}\n",
        json!({ "id": id, "error": { "code": code, "message": message } })
    )
}

// ── the core request builders (params shapes verified against v2 source) ──

/// `initialize` params. capabilities.experimentalApi=false — the core
/// thread/turn methods are non-experimental and need no opt-in.
pub fn initialize_params(client_name: &str, client_version: &str) -> Value {
    json!({
        "clientInfo": { "name": client_name, "version": client_version },
        "capabilities": { "experimentalApi": false }
    })
}

pub fn thread_start_params(cwd: &str) -> Value {
    json!({ "cwd": cwd })
}

/// Provenance marker every weft-codex thread carries. Verified 2026-08-08
/// (docs/spike-app-server/probe_thread_source.py): it lands verbatim in the
/// shared state db as `threads.thread_source`, while `threads.source` stays
/// "vscode" — codex 0.145.0 has NO `--session-source` flag (that only exists
/// in newer codex-rs git), so threadSource is the only honest provenance
/// lever. Lets tooling tell orchestrated threads apart from genuinely
/// human-opened vscode/Desktop ones.
pub const THREAD_SOURCE: &str = "weft-codex";

/// thread/start with the full launch config weft-codex uses for orchestrated
/// threads: explicit approval policy + sandbox, our provenance marker, and
/// an optional per-thread `weft-bus` MCP server. Wire shape spike-verified
/// 2026-08-08 (`config.mcp_servers.<name>.url`; see docs/spike-app-server).
pub fn thread_start_params_configured(
    cwd: &str,
    approval_policy: &str,
    sandbox: &str,
    bus_mcp_url: Option<&str>,
    ephemeral: bool,
) -> Value {
    let mut params = json!({
        "cwd": cwd,
        "approvalPolicy": approval_policy,
        "sandbox": sandbox,
        "threadSource": THREAD_SOURCE,
    });
    if let Some(url) = bus_mcp_url {
        params["config"] = json!({ "mcp_servers": { "weft-bus": { "url": url } } });
    }
    if ephemeral {
        params["ephemeral"] = json!(true);
    }
    params
}

pub fn thread_resume_params(thread_id: &str) -> Value {
    json!({ "threadId": thread_id })
}

/// thread/read: fetch persisted thread metadata without loading full turns.
/// `includeTurns=false` is intentional for sidebar location resolution: fork
/// ancestry and the display name live on the thread envelope itself.
pub fn thread_read_params(thread_id: &str) -> Value {
    json!({ "threadId": thread_id, "includeTurns": false })
}

/// thread/fork: fork the thread, omitting turns after `last_turn_id` from the
/// fork — the official fork-at-point. None = full-history fork (the key is
/// omitted, not null), which a rewind never wants: "back to before the first
/// message" starts a fresh thread instead.
pub fn thread_fork_params(thread_id: &str, last_turn_id: Option<&str>) -> Value {
    match last_turn_id {
        Some(t) => json!({ "threadId": thread_id, "lastTurnId": t }),
        None => json!({ "threadId": thread_id }),
    }
}

/// turn/start: `input` is a Vec<UserInput>; a plain message is the `text` variant
/// (serde tag "type" = "text"). NOT a single object, NOT "input_text".
pub fn turn_start_params(thread_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "input": [ { "type": "text", "text": text } ]
    })
}

/// turn/start with `outputSchema` (JSON Schema constraining the final
/// assistant message) — the curator's structured-output channel, replacing
/// weft's tolerant free-text parsing (`parse_curator_output` et al). Wire
/// name verified in codex-rs `app-server-protocol` v2 turn.rs
/// (`output_schema` → camelCase `outputSchema`).
pub fn turn_start_params_with_schema(thread_id: &str, text: &str, schema: Value) -> Value {
    json!({
        "threadId": thread_id,
        "input": [ { "type": "text", "text": text } ],
        "outputSchema": schema,
    })
}

/// turn/start with one `localImage` input item per path, appended after the
/// text item — the app-server counterpart to exec's plain-text image-path
/// listing (engine.rs's per-turn image spill only lists paths in the message
/// body for exec; the app-server transport ALSO hands them over as proper
/// input items here, in addition to that same text listing).
///
/// Wire shape verified live against source: openai/codex
/// `codex-rs/app-server-protocol/src/protocol/v2/turn.rs`, `UserInput` enum —
/// `#[serde(tag = "type", rename_all = "camelCase")]` with a
/// `LocalImage { detail: Option<ImageDetail>, path: PathBuf }` variant. The tag
/// transform turns the variant name into `"localImage"`; `path` serializes as
/// a plain string. `detail` carries `#[serde(default)]` with NO
/// `skip_serializing_if`, so the server tolerates it being entirely absent on
/// deserialize (missing → `None`) — omitted here, matching this function's own
/// sibling above, which likewise omits `text`'s equally-defaulted
/// `textElements` field rather than emit a value the server will supply itself.
pub fn turn_start_params_with_images(thread_id: &str, text: &str, image_paths: &[String]) -> Value {
    let mut input = vec![json!({ "type": "text", "text": text })];
    for p in image_paths {
        input.push(json!({ "type": "localImage", "path": p }));
    }
    json!({ "threadId": thread_id, "input": input })
}

/// turn/interrupt requires BOTH threadId and turnId (turnId is load-bearing —
/// omitting it fails to deserialize server-side).
pub fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
}

/// turn/steer params: redirect an IN-FLIGHT turn. `expectedTurnId` is a
/// precondition — the server rejects the steer when the active turn doesn't
/// match. (Spike-verified 2026-08-08: `turn/start` while busy returns success
/// but the turn never runs — steer is the only reliable mid-turn injection.)
pub fn turn_steer_params(thread_id: &str, expected_turn_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "expectedTurnId": expected_turn_id,
        "input": [{ "type": "text", "text": text }],
    })
}

/// A classified incoming line from the app-server's stdout.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Reply to one of our requests — correlate by `id`.
    Response { id: i64, result: Value },
    /// Error reply to one of our requests.
    Error { id: i64, code: i64, message: String },
    /// Server→client notification (streaming events, hook/skills updates).
    Notification { method: String, params: Value },
    /// Server→client request (approvals) — must be answered, echoing `id`.
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// Unparseable / unrecognised — ignored.
    Other,
}

/// Classify one stdout line. Order matters: a `method` present means it's a
/// request (with id) or notification (no id); otherwise it's our response/error.
pub fn classify(line: &str) -> Incoming {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Incoming::Other;
    };
    if let Some(method) = v.get("method").and_then(|m| m.as_str()).map(String::from) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        return match v.get("id") {
            Some(id) => Incoming::ServerRequest {
                id: id.clone(),
                method,
                params,
            },
            None => Incoming::Notification { method, params },
        };
    }
    let Some(id) = v.get("id").and_then(Value::as_i64) else {
        return Incoming::Other;
    };
    if let Some(result) = v.get("result") {
        return Incoming::Response {
            id,
            result: result.clone(),
        };
    }
    if let Some(err) = v.get("error") {
        return Incoming::Error {
            id,
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        };
    }
    Incoming::Other
}

/// Extract `result.thread.id` from a thread/start (or resume) response.
pub fn thread_id_of(result: &Value) -> Option<String> {
    result["thread"]["id"].as_str().map(String::from)
}

/// The small subset of Codex thread metadata needed to relate a native fork
/// back to the logical Weft chat it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    pub id: String,
    pub forked_from_id: Option<String>,
    pub name: Option<String>,
    pub preview: String,
}

pub fn thread_info_of(result: &Value) -> Option<ThreadInfo> {
    let thread = result.get("thread")?;
    let id = thread.get("id")?.as_str()?.to_string();
    let forked_from_id = thread
        .get("forkedFromId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(String::from);
    let name = thread
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);
    let preview = thread
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(ThreadInfo {
        id,
        forked_from_id,
        name,
        preview,
    })
}

/// Extract `result.turn.id` from a turn/start response.
pub fn turn_id_of(result: &Value) -> Option<String> {
    result["turn"]["id"].as_str().map(String::from)
}

/// Whether a server→client request is an unexpected approval ask. Orchestrated
/// threads use `approvalPolicy=never`; any such request is declined in-client.
pub fn is_approval_request(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    )
}

/// The in-protocol decline result for every known blocking request, or `None`
/// when an unknown method should receive a JSON-RPC error instead.
pub fn decline_response(method: &str) -> Option<Value> {
    if method == "item/permissions/requestApproval" {
        Some(json!({ "permissions": {} }))
    } else if is_approval_request(method) {
        Some(json!({ "decision": "decline" }))
    } else if is_elicitation_request(method) {
        Some(json!({ "action": "decline" }))
    } else if method == "item/tool/requestUserInput" {
        Some(json!({ "answers": {} }))
    } else {
        None
    }
}

/// A blocking MCP elicitation (`mcpServer/elicitation/request`): a configured MCP
/// server asking the user for STRUCTURED input. Weft has no UI to collect that
/// content, so it's declined (`{action:"decline"}`) rather than routed to a
/// yes/no Ask Bridge that can't supply the required `content`.
pub fn is_elicitation_request(method: &str) -> bool {
    method.ends_with("/elicitation/request")
}

/// Map a server notification to the engine's `ChatEvent`. Tool items (camelCase:
/// commandExecution/fileChange/mcpToolCall …) become a running tool row on
/// `item/started` and its result on `item/completed`; agent text streams via
/// deltas; `thread/tokenUsage/updated` carries the current-context usage.
///
/// Every notification's `threadId` is read here and carried RAW into
/// `ChatEvent::agent_thread` — the conversation/agent that produced this event,
/// main narration and every collab sub-agent's own activity alike (app-server
/// forwards a spawned sub-agent's events over this SAME connection, per-thread
/// listener, tagged with ITS OWN thread id — see `read_loop`'s demux). This
/// function does NOT know which thread is "ours": the engine (`codex_consumer`,
/// which alone holds the session's own thread id) compares it and normalizes to
/// "mainline" (None) vs "a specific sub-agent" (Some) before a row is ever built.
pub fn notification_to_event(method: &str, params: &Value) -> Option<ChatEvent> {
    use crate::proto::ChatEvent;
    let item = &params["item"];
    let thread = params["threadId"].as_str().map(String::from);
    match method {
        "item/agentMessage/delta" => {
            params["delta"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| ChatEvent::TextDelta {
                    text: s.to_string(),
                    // Key the delta to its item: parallel streams (collab
                    // sub-agents + the main narration) each get their own row
                    // instead of char-interleaving into one shared bubble.
                    item: params["itemId"].as_str().map(String::from),
                    agent_thread: thread,
                })
        }
        "item/started" => match item["type"].as_str() {
            // Transient error items (e.g. codex's own reconnect banners) go to
            // the anonymous slot — never merged into an agentMessage row, and
            // never attributed to a sub-agent branch either.
            Some("error") => Some(ChatEvent::TextDelta {
                text: crate::proto::error_text_from_item(item),
                item: None,
                agent_thread: None,
            }),
            // The tool-call item types (verified against the 0.139.0 ThreadItem
            // union): exec/edit/MCP plus subagent (collabAgentToolCall) and custom
            // (dynamicToolCall) calls. Content/lifecycle items (agentMessage,
            // reasoning, plan, webSearch, …) are ignored so they don't open rows.
            Some(
                "commandExecution" | "fileChange" | "mcpToolCall" | "collabAgentToolCall"
                | "dynamicToolCall",
            ) => Some(ChatEvent::Assistant {
                texts: vec![],
                tools: vec![appserver_tool_call(item)],
                uuid: None,
                agent_thread: thread,
            }),
            _ => None,
        },
        // Tool items deliver their result here (merged into the running row by id);
        // agentMessage's completion finalizes the streamed text row.
        "item/completed" => match item["type"].as_str() {
            Some(
                "commandExecution" | "fileChange" | "mcpToolCall" | "collabAgentToolCall"
                | "dynamicToolCall",
            ) => Some(ChatEvent::ToolResults {
                items: vec![appserver_tool_result(item)],
            }),
            // agentMessage already streamed via deltas; its *completion* finalizes
            // THAT item's row — the carried text is the authoritative full body
            // (heals any dropped/duped frames), keyed by item id so parallel
            // streams close their own rows.
            Some("agentMessage") => Some(ChatEvent::TextDone {
                item: item["id"].as_str().map(String::from),
                text: item["text"].as_str().map(String::from),
                agent_thread: thread,
            }),
            // userMessage / reasoning carry no display payload; an error item's
            // text was already surfaced by its item/started.
            Some("userMessage" | "reasoning" | "error") | None => None,
            // Other content items (/plan, /review …) don't stream via agentMessage
            // deltas — land their text as a standalone completed row.
            Some(_) => {
                crate::proto::codex_content_item_text(item).map(|text| {
                    ChatEvent::TextDone {
                        item: item["id"].as_str().map(String::from),
                        text: Some(text),
                        agent_thread: thread,
                    }
                })
            }
        },
        // Top-level failure (auth / usage-limit / context-window …): surface the
        // message so the turn doesn't end blank, then turn/completed marks it error.
        "error" => {
            let text = crate::proto::humanize_error_text(
                params["message"]
                    .as_str()
                    .or_else(|| params["error"]["message"].as_str())
                    .or_else(|| params["error"].as_str())
                    .unwrap_or("Codex reported an error."),
            );
            (!text.is_empty()).then(|| ChatEvent::TextDelta {
                text,
                item: None,
                agent_thread: None,
            })
        }
        "thread/tokenUsage/updated" => {
            let tu = &params["tokenUsage"];
            tu["last"]["inputTokens"]
                .as_u64()
                .map(|ct| ChatEvent::Usage {
                    context_tokens: ct,
                    window: tu["modelContextWindow"].as_u64(),
                })
        }
        "turn/completed" => Some(ChatEvent::TurnEnd {
            is_error: params["turn"]["status"].as_str() != Some("completed"),
            context_tokens: None, // 准确上下文走 thread/tokenUsage/updated
        }),
        _ => None,
    }
}

/// Error text carried on a failed `turn/completed` (auth / quota / context), if
/// any — surfaced before the TurnEnd so the row shows the real cause instead of a
/// generic `error_before_output` when the failure is reported only here.
pub fn turn_error_text(params: &Value) -> Option<String> {
    let t = &params["turn"];
    let s = t["error"]["message"]
        .as_str()
        .or_else(|| t["error"].as_str())
        .unwrap_or("")
        .trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// True only for a machine-readable quota cause attached to this completed
/// turn. Do not inspect display text: generic transport, auth, or tool errors
/// may contain similar words but must not create a false attention signal.
pub fn turn_reports_quota_exceeded(params: &Value) -> bool {
    let error = &params["turn"]["error"];
    let mut codes = [
        error["code"].as_str(),
        error["type"].as_str(),
        error["kind"].as_str(),
        error["details"]["code"].as_str(),
    ]
    .into_iter()
    .flatten();
    codes.any(|code| {
        matches!(
            code.trim().to_ascii_lowercase().as_str(),
            "rate_limit_exceeded" | "rate_limit_reached" | "quota_exceeded" | "usage_limit_exceeded"
        )
    })
}

/// Running `ToolCall` from an app-server `item.started` tool item.
fn appserver_tool_call(item: &Value) -> crate::proto::ToolCall {
    crate::proto::ToolCall {
        id: item["id"].as_str().unwrap_or_default().to_string(),
        name: item["type"].as_str().unwrap_or("tool").to_string(),
        input: appserver_tool_input(item),
        summary: appserver_tool_summary(item),
        output: None,
        is_error: false,
        collab_threads: appserver_collab_threads(item),
        // A call's own start never carries a result yet — see
        // `proto::ToolCall::images`.
        images: Vec::new(),
    }
}

/// A `collabAgentToolCall` item's known sub-agent thread ids:
/// `receiverThreadIds` — "the newly spawned agent" for a spawn call, or the
/// existing target agent for a send/wait call. Empty for a spawn's own
/// `item/started` (the child doesn't exist yet) and for every non-collab item —
/// `receiverThreadIds` simply isn't present on those, so the field reads empty
/// rather than erroring. The frontend uses whichever row FIRST reveals a given
/// thread id as that sub-agent's branch anchor.
fn appserver_collab_threads(item: &Value) -> Vec<String> {
    item["receiverThreadIds"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Result of an app-server `item.completed` tool item, keyed by item id.
///
/// issue #99, timing note: a `spawnAgent` collab call's `receiverThreadIds` is
/// EMPTY on its `item/started` (the child thread doesn't exist yet) and only
/// becomes known on THIS event, `item/completed` — captured here via
/// `appserver_collab_threads` for exactly that reason. A `sendInput`/`wait`
/// call already knows its target at `item/started` (captured by
/// `appserver_tool_call` instead), so this is a re-affirmation for those, not
/// their only source.
fn appserver_tool_result(item: &Value) -> crate::proto::ToolResultItem {
    crate::proto::ToolResultItem {
        id: item["id"].as_str().unwrap_or_default().to_string(),
        output: appserver_tool_output(item),
        is_error: appserver_tool_is_error(item),
        collab_threads: appserver_collab_threads(item),
        // app-server's item.completed inbound image content isn't parsed yet.
        images: Vec::new(),
    }
}

fn appserver_tool_input(item: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for k in ["command", "cwd", "changes", "server", "tool", "arguments"] {
        if let Some(v) = item.get(k) {
            if !v.is_null() {
                obj.insert(k.to_string(), v.clone());
            }
        }
    }
    // A big MCP `arguments` or `changes` payload
    // would otherwise bloat the persisted row + its push even though output is
    // capped. Small inputs pass through unchanged (UI still renders the object).
    crate::proto::cap_input(Value::Object(obj))
}

fn appserver_tool_summary(item: &Value) -> String {
    let s = item["command"]
        .as_str()
        .or_else(|| item["tool"].as_str())
        .or_else(|| item["changes"][0]["path"].as_str())
        .unwrap_or_default();
    s.chars().take(120).collect()
}

/// commandExecution → `aggregatedOutput`; fileChange → the per-change diffs;
/// mcpToolCall → result/output.
fn appserver_tool_output(item: &Value) -> String {
    if let Some(s) = item["aggregatedOutput"].as_str() {
        return cap_out(s);
    }
    if let Some(changes) = item["changes"].as_array() {
        let diff = changes
            .iter()
            .filter_map(|c| c["diff"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !diff.is_empty() {
            return cap_out(&diff);
        }
    }
    // mcpToolCall result / generic output: a plain string, an MCP result object
    // (`{content:[{text}]}` — what weft's bus/planner tools return), or some other
    // JSON value. Render the text where possible, else serialize so the expanded
    // row isn't blank.
    for key in ["output", "result", "error"] {
        let v = &item[key];
        if v.is_null() {
            continue;
        }
        if let Some(s) = v.as_str() {
            return cap_out(s);
        }
        if let Some(content) = v["content"].as_array() {
            let text = content
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return cap_out(&text);
            }
        }
        return cap_out(&v.to_string());
    }
    String::new()
}

fn appserver_tool_is_error(item: &Value) -> bool {
    // A declined/canceled approval completes the item without running it — not a
    // success. Check status first; otherwise a non-zero exit code is an error.
    if matches!(
        item["status"].as_str(),
        Some("failed" | "error" | "declined" | "canceled" | "cancelled")
    ) {
        return true;
    }
    item["exitCode"].as_i64().is_some_and(|c| c != 0)
}

fn cap_out(s: &str) -> String {
    const MAX: usize = 16_000;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX).collect();
    out.push_str("\n… (truncated)");
    out
}

// ─────────────────────────── runtime client ────────────────────────────────

/// What the demux delivers to a session subscribed on a thread_id.
#[derive(Debug)]
pub enum ThreadMsg {
    /// A streaming event for the session's timeline.
    Event(ChatEvent),
    /// A structured quota-exceeded cause attached to the active turn. It is
    /// delivered before that turn's `TurnEnd`, allowing the engine to make a
    /// failover decision at the safe boundary only.
    QuotaExceeded,
    /// A liveness ping (e.g. command output-delta while a long command runs) that
    /// carries no timeline change — the consumer uses it only to refresh the
    /// runaway-guard's last-activity clock so a busy command isn't idle-killed.
    Heartbeat,
    /// The server started a turn on this thread (`turn/started`). The consumer
    /// compares `turn_id` against its own active-turn bookkeeping: a mismatch
    /// means someone ELSE is driving (human takeover in Desktop) — mid-turn
    /// injection must then stand down (turn/start would be silently dropped).
    TurnStarted { turn_id: String },
}

struct Inner {
    /// Channel to the dedicated stdin writer task. Holds no `ChildStdin` directly,
    /// so async writes never need the state lock.
    // Each entry is (bytes, optional flush-ack): the writer task acks AFTER
    // write_all + flush succeed, so request() can arm its reply timeout only once
    // the bytes actually reached the child's stdin (see `request`).
    write_tx: mpsc::UnboundedSender<(Vec<u8>, Option<oneshot::Sender<()>>)>,
    next_id: i64,
    /// our request id → awaiting caller (Ok(result) / Err(message)).
    pending: HashMap<i64, oneshot::Sender<Result<Value, String>>>,
    /// thread_id → that session's event sink.
    threads: HashMap<String, mpsc::UnboundedSender<ThreadMsg>>,
    /// thread_id → the in-flight turn id (needed by turn/interrupt).
    active_turn: HashMap<String, String>,
    _child: tokio::process::Child,
    /// Registration for the spawned codex app-server child, kept ALONGSIDE `_child`
    /// so the registry entry lives exactly as long as the process. T1 only registers
    /// here; the tree-aware reclaim (`proc_registry::reap`) is wired into the teardown
    /// paths (shutdown_and_reap / stop_quiet …) by T2. Dropping it just deregisters.
    _reg: crate::proc_registry::Registration,
}

/// Handle to the single global `codex app-server` connection.
#[derive(Clone)]
pub struct Client(Arc<Mutex<Option<Inner>>>);

/// The global client handle (connect lazily via [`client`]).
fn cell() -> Client {
    static C: OnceLock<Client> = OnceLock::new();
    C.get_or_init(|| Client(Arc::new(Mutex::new(None)))).clone()
}

/// Get the global client, spawning + handshaking on first use (or after the
/// previous connection died).
pub async fn client() -> anyhow::Result<Client> {
    let c = cell();
    if c.0.lock().await.is_some() {
        return Ok(c);
    }
    c.connect().await?;
    Ok(c)
}

/// Shut down the global client. Call this after a probe `timeout()` cancels a
/// `client()` mid-handshake: the dropped future may have left `spawn_inner`'s
/// `Inner` half-initialized, and the next `client()` would reuse that broken
/// connection — shutting it down forces a clean reconnect instead.
pub async fn shutdown_global() {
    cell().shutdown_and_reap().await;
}

impl Client {
    /// Spawn + handshake a fresh `codex app-server`, injecting `extra_args` (a
    /// session's `-c mcp_servers...` bus flags) and running in `cwd`. Each session
    /// gets its OWN process so its per-thread MCP config is isolated — app-server
    /// MCP is app-scoped, so one shared connection couldn't carry per-thread bus URLs.
    pub async fn connect_session(
        program: &str,
        extra_args: &[String],
        extra_env: &[(String, String)],
        cwd: &std::path::Path,
        owner: crate::proc_registry::Owner,
    ) -> anyhow::Result<Client> {
        let client = Client(Arc::new(Mutex::new(None)));
        client
            .spawn_inner(program, extra_args, extra_env, Some(cwd), owner)
            .await?;
        Ok(client)
    }

    /// Whether the connection is still alive (read_loop clears the inner on EOF).
    pub async fn is_alive(&self) -> bool {
        self.0.lock().await.is_some()
    }

    /// Same underlying connection? Lets a consumer tell a genuine disconnect (still
    /// the engine's active client → run the disconnect cleanup) from an intentional
    /// teardown/replace (client taken or swapped → skip cleanup, don't clobber the
    /// exec fallback turn).
    pub fn ptr_eq(&self, other: &Client) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// A handle with no connection behind it, for tests that only need
    /// `codex_client: Some(..)` to stand for "an app-server connection is
    /// live" — engine teardown decisions branch on the field's presence, not
    /// on the connection's state.
    #[cfg(test)]
    pub fn disconnected_for_test() -> Client {
        Client(Arc::new(Mutex::new(None)))
    }

    /// Decline a blocking server request so the turn cannot hang, using the
    /// method-specific shape or a JSON-RPC error for unknown methods.
    async fn decline_server_request(&self, id: &Value, method: &str) {
        match decline_response(method) {
            Some(result) => {
                let _ = self.reply_result(id, result).await;
            }
            None => {
                let g = self.0.lock().await;
                if let Some(inner) = g.as_ref() {
                    let line = encode_error_response(id, -32601, "unsupported request");
                    let _ = inner.write_tx.send((line.into_bytes(), None));
                }
            }
        }
    }

    /// Kill the connection: drops the child (kill_on_drop) and closes the thread
    /// sinks, so the per-session consumer task exits.
    pub async fn shutdown(&self) {
        *self.0.lock().await = None;
    }

    /// Like [`shutdown`](Self::shutdown) but also REAPS the child — kill + await —
    /// before returning. A caller that spawns many short-lived per-session
    /// connections (the curator's per-repo + relation scans) would otherwise pile
    /// up unreaped `codex app-server` children on tokio's best-effort reaper, since
    /// plain `shutdown` only drops the handle (kill_on_drop, no await).
    pub async fn shutdown_and_reap(&self) {
        if let Some(mut inner) = self.0.lock().await.take() {
            // SIGKILL + wait → the child is reaped synchronously here; closing
            // stdin/stdout also drops the per-session consumer task.
            let _ = inner._child.kill().await;
        }
    }

    /// Teardown used when the stdin writer fails: notify every pending caller
    /// with an error, then kill and reap the child so the connection is not
    /// left half-alive stalling until the request timeout.
    async fn fail_pending_and_reap(&self, message: &str) {
        if let Some(mut inner) = self.0.lock().await.take() {
            for (_, tx) in inner.pending.drain() {
                let _ = tx.send(Err(message.to_string()));
            }
            let _ = inner._child.kill().await;
        }
    }

    async fn connect(&self) -> anyhow::Result<()> {
        self.spawn_inner(
            &crate::command::codex_command(),
            &[],
            &[],
            None,
            crate::proc_registry::Owner::global_app_server(),
        )
        .await
    }

    async fn spawn_inner(
        &self,
        program: &str,
        extra_args: &[String],
        extra_env: &[(String, String)],
        cwd: Option<&std::path::Path>,
        owner: crate::proc_registry::Owner,
    ) -> anyhow::Result<()> {
        let mut g = self.0.lock().await;
        if g.is_some() {
            return Ok(());
        }
        let mut command = Command::new(program);
        command.arg("app-server").arg("--stdio").args(extra_args);
        if let Some(c) = cwd {
            command.current_dir(c);
        }
        // Resolve nvm/fnm/volta CLIs from a GUI launch's minimal PATH without
        // mutating the global env (see detect::tool_path).
        command.env("PATH", crate::detect::tool_path());
        // injection-supplied env —
        // the codex computer-use bearer rides the child's environment (readable
        // only by its own uid), never `-c` argv (world-readable via ps). See
        // `bus::inject::Injection::env`.
        command.envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // T1: own process group + instance/owner marker BEFORE spawn, then register
        // the child ALONGSIDE it (Inner._reg). Reclaim stays in the teardown paths
        // (T2), not here.
        let configured = crate::proc_registry::configure(&mut command, owner);
        let mut child = command.spawn()?;
        let reg = configured.register(&child);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;

        let (write_tx, mut write_rx) =
            mpsc::unbounded_channel::<(Vec<u8>, Option<oneshot::Sender<()>>)>();
        let client_for_writer = self.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            let fail = || async {
                client_for_writer
                    .fail_pending_and_reap("codex app-server stdin writer failed")
                    .await;
            };
            while let Some((bytes, ack)) = write_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() {
                    fail().await;
                    break;
                }
                if stdin.flush().await.is_err() {
                    fail().await;
                    break;
                }
                // Ack only after write_all + flush both succeeded; a failed write
                // drops the ack sender, which request() observes as an error.
                if let Some(a) = ack {
                    let _ = a.send(());
                }
            }
        });

        *g = Some(Inner {
            write_tx,
            next_id: 1,
            pending: HashMap::new(),
            threads: HashMap::new(),
            active_turn: HashMap::new(),
            _child: child,
            _reg: reg,
        });
        drop(g);

        let me = self.clone();
        tokio::spawn(async move {
            me.read_loop(stdout).await;
        });

        // Handshake: initialize (await), then the `initialized` notification. If it
        // wedges/errors (auth/network/version), tear the half-open client down so a
        // retry doesn't leak app-server / MCP processes while the turn falls back to
        // exec. Kill AND REAP the child here (not plain `shutdown`, which only drops
        // the handle): the curator probes app-server once per repo, so repeated
        // handshake failures — e.g. an old Codex binary — would otherwise pile up
        // unreaped children.
        let handshake = async {
            self.request(
                "initialize",
                initialize_params("weft", env!("CARGO_PKG_VERSION")),
            )
            .await?;
            self.notify("initialized", None).await
        }
        .await;
        if let Err(e) = handshake {
            self.shutdown_and_reap().await;
            return Err(e);
        }
        Ok(())
    }

    /// Demux the server's stdout for the connection's lifetime: correlate replies
    /// by id, route notifications + approval requests to the owning thread.
    async fn read_loop(&self, stdout: tokio::process::ChildStdout) {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match classify(&line) {
                Incoming::Response { id, result } => self.resolve(id, Ok(result)).await,
                Incoming::Error { id, message, .. } => self.resolve(id, Err(message)).await,
                Incoming::Notification { method, params } => {
                    let tid = params["threadId"].as_str().map(String::from);
                    if let Some(ev) = notification_to_event(&method, &params) {
                        // A failed turn may carry its only error message on
                        // turn/completed — surface it as text before the TurnEnd so
                        // the row shows the real cause, not error_before_output.
                        if method == "turn/completed" {
                            if turn_reports_quota_exceeded(&params) {
                                self.route_resolved(tid.as_deref(), ThreadMsg::QuotaExceeded)
                                    .await;
                            }
                            if let Some(text) = turn_error_text(&params) {
                                self.route_resolved(
                                    tid.as_deref(),
                                    ThreadMsg::Event(
                                        crate::proto::ChatEvent::TextDelta {
                                            text,
                                            item: None,
                                            // A sub-agent's OWN turn can fail too — tag
                                            // it like any other event on this thread
                                            //  so the error lands in its
                                            // branch, not as spurious mainline text.
                                            agent_thread: tid.clone(),
                                        },
                                    ),
                                )
                                .await;
                            }
                        }
                        self.route_resolved(tid.as_deref(), ThreadMsg::Event(ev))
                            .await;
                    } else if method.ends_with("/outputDelta") {
                        // A long command is still producing output; forward its
                        // activity telemetry.
                        self.route_resolved(tid.as_deref(), ThreadMsg::Heartbeat)
                            .await;
                    } else if method == "turn/started" {
                        // TurnStartedNotification { threadId, turn: { id, .. } }.
                        // Routed so the orchestrator can tell OUR turns from
                        // foreign (human-takeover) turns — the silent-drop
                        // avoidance depends on it.
                        let turn_id = params["turn"]["id"].as_str().unwrap_or("").to_string();
                        self.route_resolved(tid.as_deref(), ThreadMsg::TurnStarted { turn_id })
                            .await;
                    }
                }
                Incoming::ServerRequest { id, method, .. } => {
                    // No approval or elicitation UI exists. Every blocking request
                    // is declined inside the protocol client so no higher layer can
                    // accidentally add an allow path.
                    if is_approval_request(&method) {
                        eprintln!("[weftd] declined unexpected Codex approval request: {method}");
                    }
                    self.decline_server_request(&id, &method).await;
                }
                Incoming::Other => {}
            }
        }
        // EOF/crash → drop the connection so the next use reconnects + re-resumes.
        *self.0.lock().await = None;
    }

    async fn resolve(&self, id: i64, res: Result<Value, String>) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            if let Some(tx) = inner.pending.remove(&id) {
                let _ = tx.send(res);
            }
        }
    }

    /// Route to `tid` when present (and subscribed), else — each connection is
    /// per-session, so it owns a single thread — fall back to that sole thread.
    /// This keeps thread-less notifications (the id sometimes lives only inside
    /// `turn`/`item`, not at the top level) reaching the consumer.
    async fn route_resolved(&self, tid: Option<&str>, msg: ThreadMsg) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            let key: Option<String> = tid
                .filter(|t| inner.threads.contains_key(*t))
                .map(String::from)
                .or_else(|| {
                    (inner.threads.len() == 1)
                        .then(|| inner.threads.keys().next().cloned())
                        .flatten()
                });
            if let Some(k) = key {
                if let Some(tx) = inner.threads.get(&k) {
                    let _ = tx.send(msg);
                }
            }
        }
    }

    /// Send a request and await its reply (`result` on success, `error.message`
    /// on failure), with a hard timeout so a wedged server can't hang a caller.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let (id, rx, flushed) = {
            let mut g = self.0.lock().await;
            let inner = g
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
            let id = inner.next_id;
            inner.next_id += 1;
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(id, tx);
            let (flush_tx, flush_rx) = oneshot::channel();
            let line = encode_request(id, method, params);
            inner
                .write_tx
                .send((line.into_bytes(), Some(flush_tx)))
                .map_err(|_| anyhow::anyhow!("codex app-server writer closed"))?;
            (id, rx, flush_rx)
        };
        // Arm the reply timeout only AFTER the bytes actually flushed to the
        // child's stdin: with the background writer, enqueue-then-time would let a
        // blocked writer burn the whole budget and a "timed out" side-effecting
        // call (turn/start) be delivered LATE — starting a duplicate, uncancelable
        // turn after the caller already fell back. A flush that cannot complete
        // within the same budget means the connection is factually dead: reap it
        // so the queued bytes die with the process instead of firing later.
        match tokio::time::timeout(Duration::from_secs(60), flushed).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                anyhow::bail!("codex app-server {method}: writer closed before flush")
            }
            Err(_) => {
                self.fail_pending_and_reap("codex app-server stdin flush stalled")
                    .await;
                anyhow::bail!("codex app-server {method}: stdin flush stalled")
            }
        }
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => anyhow::bail!("codex app-server {method}: {e}"),
            Ok(Err(_)) => anyhow::bail!("codex app-server {method}: reply dropped"),
            Err(_) => {
                if let Some(inner) = self.0.lock().await.as_mut() {
                    inner.pending.remove(&id);
                }
                anyhow::bail!("codex app-server {method}: timed out")
            }
        }
    }

    /// Fire-and-forget notification (no reply expected).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> anyhow::Result<()> {
        let g = self.0.lock().await;
        let inner = g
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
        let line = encode_notification(method, params);
        inner
            .write_tx
            .send((line.into_bytes(), None))
            .map_err(|_| anyhow::anyhow!("codex app-server writer closed"))?;
        Ok(())
    }

    /// Subscribe a session to a thread_id's events/approvals.
    pub async fn subscribe(&self, thread_id: &str) -> mpsc::UnboundedReceiver<ThreadMsg> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(inner) = self.0.lock().await.as_mut() {
            inner.threads.insert(thread_id.to_string(), tx);
        }
        rx
    }

    /// Whether a session is already subscribed (its consumer task is running).
    pub async fn is_subscribed(&self, thread_id: &str) -> bool {
        self.0
            .lock()
            .await
            .as_ref()
            .map(|i| i.threads.contains_key(thread_id))
            .unwrap_or(false)
    }

    /// Drop one thread sink. Used when a newly-created thread fails before
    /// its first turn and must not occupy the subscription map forever.
    pub async fn unsubscribe(&self, thread_id: &str) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            inner.threads.remove(thread_id);
            inner.active_turn.remove(thread_id);
        }
    }

    /// Record the in-flight turn id for a thread (for a later interrupt).
    pub async fn set_active_turn(&self, thread_id: &str, turn_id: &str) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            inner
                .active_turn
                .insert(thread_id.to_string(), turn_id.to_string());
        }
    }

    /// Forget a thread's in-flight turn (called at turn end), so a later
    /// `active_turn` only reports a genuinely live app-server turn — letting
    /// the interrupt path tell an app-server turn from an exec fallback.
    pub async fn clear_active_turn(&self, thread_id: &str) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            inner.active_turn.remove(thread_id);
        }
    }

    /// The in-flight turn id for a thread, if any.
    pub async fn active_turn(&self, thread_id: &str) -> Option<String> {
        self.0
            .lock()
            .await
            .as_ref()?
            .active_turn
            .get(thread_id)
            .cloned()
    }

    // ── typed drive-loop helpers ──
    pub async fn start_thread(&self, cwd: &str) -> anyhow::Result<String> {
        let r = self
            .request("thread/start", thread_start_params(cwd))
            .await?;
        thread_id_of(&r).ok_or_else(|| anyhow::anyhow!("thread/start: no thread.id"))
    }
    pub async fn resume_thread(&self, thread_id: &str) -> anyhow::Result<()> {
        self.request("thread/resume", thread_resume_params(thread_id))
            .await
            .map(|_| ())
    }
    pub async fn read_thread(&self, thread_id: &str) -> anyhow::Result<ThreadInfo> {
        let result = self
            .request("thread/read", thread_read_params(thread_id))
            .await?;
        thread_info_of(&result)
            .ok_or_else(|| anyhow::anyhow!("thread/read: invalid thread metadata"))
    }
    /// Fork `thread_id`, omitting turns after `last_turn_id` from the fork.
    /// Returns the NEW thread id. See [`thread_fork_params`] for the None case.
    pub async fn fork_thread(
        &self,
        thread_id: &str,
        last_turn_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let r = self
            .request("thread/fork", thread_fork_params(thread_id, last_turn_id))
            .await?;
        thread_id_of(&r).ok_or_else(|| anyhow::anyhow!("thread/fork: no thread.id"))
    }
    pub async fn start_turn(&self, thread_id: &str, text: &str) -> anyhow::Result<String> {
        let r = self
            .request("turn/start", turn_start_params(thread_id, text))
            .await?;
        turn_id_of(&r).ok_or_else(|| anyhow::anyhow!("turn/start: no turn.id"))
    }
    /// Like [`start_turn`](Self::start_turn) but also hands over `image_paths`
    /// as `localImage` input items (engine.rs's codex attachment spill, for
    /// the app-server transport only — exec keeps its plain-text path listing).
    /// An empty slice is identical to `start_turn` (plain params, no images key
    /// churn on the wire for the common no-attachment turn).
    pub async fn start_turn_with_images(
        &self,
        thread_id: &str,
        text: &str,
        image_paths: &[String],
    ) -> anyhow::Result<String> {
        let params = if image_paths.is_empty() {
            turn_start_params(thread_id, text)
        } else {
            turn_start_params_with_images(thread_id, text, image_paths)
        };
        let r = self.request("turn/start", params).await?;
        turn_id_of(&r).ok_or_else(|| anyhow::anyhow!("turn/start: no turn.id"))
    }
    pub async fn interrupt(&self, thread_id: &str, turn_id: &str) -> anyhow::Result<()> {
        self.request("turn/interrupt", turn_interrupt_params(thread_id, turn_id))
            .await
            .map(|_| ())
    }

    /// Steer the in-flight turn (see [`turn_steer_params`]). Returns the turn
    /// id acknowledged by the server.
    pub async fn steer_turn(
        &self,
        thread_id: &str,
        expected_turn_id: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        let r = self
            .request("turn/steer", turn_steer_params(thread_id, expected_turn_id, text))
            .await?;
        Ok(r["turnId"].as_str().unwrap_or(expected_turn_id).to_string())
    }
    async fn reply_result(&self, id: &Value, result: Value) -> anyhow::Result<()> {
        let g = self.0.lock().await;
        let inner = g
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
        let line = encode_response(id, result);
        inner
            .write_tx
            .send((line.into_bytes(), None))
            .map_err(|_| anyhow::anyhow!("codex app-server writer closed"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_turn_start_with_text_input_array() {
        let line = encode_request(7, "turn/start", turn_start_params("t_1", "hello"));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "turn/start");
        assert_eq!(v["params"]["threadId"], "t_1");
        // input is an ARRAY of {type:"text", text}, not a bare object / "input_text".
        assert_eq!(v["params"]["input"][0]["type"], "text");
        assert_eq!(v["params"]["input"][0]["text"], "hello");
        assert!(v.get("jsonrpc").is_none()); // codex envelope has no jsonrpc field
    }

    #[test]
    fn encodes_turn_steer_with_expected_turn_precondition() {
        let v = turn_steer_params("t_1", "turn_9", "redirect");
        assert_eq!(v["threadId"], "t_1");
        assert_eq!(v["expectedTurnId"], "turn_9");
        assert_eq!(v["input"][0]["type"], "text");
        assert_eq!(v["input"][0]["text"], "redirect");
    }

    #[test]
    fn encodes_thread_start_with_full_launch_config() {
        let v = thread_start_params_configured(
            "/tmp/wt",
            "never",
            "workspace-write",
            Some("http://127.0.0.1:47810/bus/1/3/mcp"),
            false,
        );
        assert_eq!(v["cwd"], "/tmp/wt");
        assert_eq!(v["approvalPolicy"], "never");
        assert_eq!(v["sandbox"], "workspace-write");
        assert_eq!(
            v["config"]["mcp_servers"]["weft-bus"]["url"],
            "http://127.0.0.1:47810/bus/1/3/mcp"
        );
        assert_eq!(v["threadSource"], THREAD_SOURCE);
        assert!(v.get("ephemeral").is_none());
        // No bus URL → the config key is omitted entirely.
        let bare = thread_start_params_configured("/tmp/wt", "never", "read-only", None, true);
        assert!(bare.get("config").is_none());
        assert_eq!(bare["ephemeral"], true);
    }

    #[test]
    fn encodes_turn_start_with_output_schema() {
        let schema = json!({ "type": "object", "properties": { "tier": { "type": "string" } } });
        let v = turn_start_params_with_schema("t_1", "analyze", schema.clone());
        assert_eq!(v["threadId"], "t_1");
        assert_eq!(v["input"][0]["type"], "text");
        assert_eq!(v["outputSchema"], schema);
    }

    /// turn_start_params_with_images: the text item stays first (unchanged
    /// shape from the plain `turn_start_params`), followed by one `localImage`
    /// item per path, `path` riding as a plain string with no `detail` key
    /// (see the function's own doc for the source-verified wire shape).
    #[test]
    fn encodes_turn_start_with_local_image_input_items() {
        let params = turn_start_params_with_images(
            "t_1",
            "look at this",
            &["/tmp/weft-attachments/msg1-0.png".to_string(), "/tmp/weft-attachments/msg1-1.jpg".to_string()],
        );
        let line = encode_request(7, "turn/start", params);
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["params"]["threadId"], "t_1");
        let input = v["params"]["input"].as_array().expect("input array");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], "look at this");
        assert_eq!(input[1]["type"], "localImage");
        assert_eq!(input[1]["path"], "/tmp/weft-attachments/msg1-0.png");
        // No `detail` key at all — the server's field is `#[serde(default)]`
        // (no `skip_serializing_if`), so omitting it entirely is valid on the
        // wire and matches this function's existing `text`/`textElements`
        // convention of not emitting server-defaultable fields.
        assert!(input[1].get("detail").is_none());
        assert_eq!(input[2]["type"], "localImage");
        assert_eq!(input[2]["path"], "/tmp/weft-attachments/msg1-1.jpg");

        // An empty path list degenerates to exactly the plain params (no
        // `localImage` items, no dangling empty array quirks).
        let plain = turn_start_params_with_images("t_1", "hi", &[]);
        assert_eq!(plain, turn_start_params("t_1", "hi"));
    }

    #[test]
    fn interrupt_carries_both_ids() {
        let v: Value = serde_json::from_str(
            encode_request(9, "turn/interrupt", turn_interrupt_params("t_1", "turn_9")).trim(),
        )
        .unwrap();
        assert_eq!(v["params"]["threadId"], "t_1");
        assert_eq!(v["params"]["turnId"], "turn_9");
    }

    #[test]
    fn fork_params_carry_last_turn_id_only_when_set() {
        let v: Value = serde_json::from_str(
            encode_request(11, "thread/fork", thread_fork_params("t_1", Some("turn_3"))).trim(),
        )
        .unwrap();
        assert_eq!(v["method"], "thread/fork");
        assert_eq!(v["params"]["threadId"], "t_1");
        assert_eq!(v["params"]["lastTurnId"], "turn_3");
        // None = full-history fork: the key is omitted, never null.
        let v2: Value = serde_json::from_str(
            encode_request(12, "thread/fork", thread_fork_params("t_1", None)).trim(),
        )
        .unwrap();
        assert_eq!(v2["params"]["threadId"], "t_1");
        assert!(v2["params"].get("lastTurnId").is_none());
    }

    #[test]
    fn fork_response_yields_new_thread_id() {
        // thread/fork answers with the forked thread — same envelope as
        // thread/start, so thread_id_of extracts `result.thread.id`.
        let r = json!({"thread": {"id": "t_fork_2", "turns": []}});
        assert_eq!(thread_id_of(&r).as_deref(), Some("t_fork_2"));
        assert!(thread_id_of(&json!({"thread": {}})).is_none());
    }

    #[test]
    fn thread_read_requests_metadata_only_and_parses_fork_origin() {
        let params = thread_read_params("t_fork_2");
        assert_eq!(params["threadId"], "t_fork_2");
        assert_eq!(params["includeTurns"], false);

        let info = thread_info_of(&json!({
            "thread": {
                "id": "t_fork_2",
                "forkedFromId": "t_primary",
                "name": "Alternative plan",
                "preview": "Try the API-first route"
            }
        }))
        .expect("thread metadata");
        assert_eq!(info.id, "t_fork_2");
        assert_eq!(info.forked_from_id.as_deref(), Some("t_primary"));
        assert_eq!(info.name.as_deref(), Some("Alternative plan"));
        assert_eq!(info.preview, "Try the API-first route");
    }

    #[test]
    fn notification_has_no_id() {
        let v: Value =
            serde_json::from_str(encode_notification("initialized", None).trim()).unwrap();
        assert_eq!(v["method"], "initialized");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn classify_distinguishes_message_kinds() {
        assert_eq!(
            classify(r#"{"id":7,"result":{"turn":{"id":"turn_9"}}}"#),
            Incoming::Response {
                id: 7,
                result: json!({"turn":{"id":"turn_9"}})
            }
        );
        assert!(matches!(
            classify(r#"{"id":7,"error":{"code":-32600,"message":"bad"}}"#),
            Incoming::Error {
                id: 7,
                code: -32600,
                ..
            }
        ));
        assert!(matches!(
            classify(r#"{"method":"turn/completed","params":{"turn":{"status":"completed"}}}"#),
            Incoming::Notification { .. }
        ));
        // server request: has BOTH method and id → must be answered.
        match classify(
            r#"{"id":"a1","method":"item/commandExecution/requestApproval","params":{}}"#,
        ) {
            Incoming::ServerRequest { id, method, .. } => {
                assert_eq!(id, json!("a1"));
                assert!(is_approval_request(&method));
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(classify("not json"), Incoming::Other);
    }

    #[test]
    fn maps_streaming_notifications_to_events() {
        // agent text streams token-by-token.
        match notification_to_event(
            "item/agentMessage/delta",
            &json!({"threadId":"t","turnId":"u","itemId":"i","delta":"He"}),
        ) {
            Some(ChatEvent::TextDelta { text, item, agent_thread }) => {
                assert_eq!(text, "He");
                // Deltas carry their item id: parallel streams (collab sub-agents)
                // key their own rows instead of interleaving into one bubble.
                assert_eq!(item.as_deref(), Some("i"));
                // The envelope's threadId rides along RAW — the engine,
                // not this mapper, decides mainline vs a sub-agent branch.
                assert_eq!(agent_thread.as_deref(), Some("t"));
            }
            e => panic!("{e:?}"),
        }
        // commandExecution started → a running tool row with id + input (camelCase
        // shape verified live, codex-cli 0.139.0).
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"call_1","type":"commandExecution","command":"echo hi","cwd":"/tmp","status":"inProgress"}}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                assert_eq!(tools[0].name, "commandExecution");
                assert_eq!(tools[0].id, "call_1");
                assert_eq!(tools[0].summary, "echo hi");
                assert_eq!(tools[0].input["command"], "echo hi");
                assert!(tools[0].output.is_none());
            }
            e => panic!("{e:?}"),
        }
        // commandExecution completed → ToolResults (aggregatedOutput + exitCode).
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"call_1","type":"commandExecution","aggregatedOutput":"hi\n","exitCode":0,"status":"completed"}}),
        ) {
            Some(ChatEvent::ToolResults { items }) => {
                assert_eq!(items[0].id, "call_1");
                assert_eq!(items[0].output, "hi\n");
                assert!(!items[0].is_error);
            }
            e => panic!("{e:?}"),
        }
        // fileChange completed → its diff(s) as output; non-zero exit / failed = error.
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"call_2","type":"fileChange","changes":[{"path":"/r/x","kind":{"type":"add"},"diff":"hi\n"}],"status":"completed"}}),
        ) {
            Some(ChatEvent::ToolResults { items }) => assert_eq!(items[0].output, "hi\n"),
            e => panic!("{e:?}"),
        }
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"call_3","type":"commandExecution","aggregatedOutput":"","exitCode":1,"status":"completed"}}),
        ) {
            Some(ChatEvent::ToolResults { items }) => assert!(items[0].is_error),
            e => panic!("{e:?}"),
        }
        // a declined approval completes without running → error, not complete.
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"call_4","type":"commandExecution","status":"declined"}}),
        ) {
            Some(ChatEvent::ToolResults { items }) => assert!(items[0].is_error),
            e => panic!("{e:?}"),
        }
        // thread/tokenUsage/updated → current context (last.inputTokens) + window.
        match notification_to_event(
            "thread/tokenUsage/updated",
            &json!({"tokenUsage":{"last":{"inputTokens":18440},"modelContextWindow":258400}}),
        ) {
            Some(ChatEvent::Usage {
                context_tokens,
                window,
            }) => {
                assert_eq!(context_tokens, 18440);
                assert_eq!(window, Some(258400));
            }
            e => panic!("{e:?}"),
        }
        // error item → text; userMessage/reasoning + lifecycle ignored on completion
        // (agentMessage completion finalizes — see
        // agent_message_completion_finalizes_streamed_text).
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"i","type":"error","message":"unknown slash command"}}),
        ) {
            Some(ChatEvent::TextDelta { text, item, agent_thread }) => {
                assert_eq!(text, "unknown slash command");
                // Error items go to the anonymous slot — never merged into an
                // agentMessage row (codex's own reconnect banners arrive here) —
                // and never attributed to a sub-agent branch either, even though
                // this notification carries no threadId to attribute anyway.
                assert_eq!(item, None);
                assert_eq!(agent_thread, None);
            }
            e => panic!("{e:?}"),
        }
        // An error item's completion must NOT resurface its text (its started
        // already did) — that would duplicate the row.
        assert!(notification_to_event(
            "item/completed",
            &json!({"item":{"id":"i","type":"error","message":"unknown slash command"}}),
        )
        .is_none());
        assert!(notification_to_event(
            "item/completed",
            &json!({"item":{"id":"i","type":"userMessage","text":"hi"}}),
        )
        .is_none());
        assert!(
            notification_to_event("item/started", &json!({"item":{"type":"userMessage"}}))
                .is_none()
        );
        assert!(matches!(
            notification_to_event("turn/completed", &json!({"turn":{"status":"completed"}})),
            Some(ChatEvent::TurnEnd {
                is_error: false,
                ..
            })
        ));
        assert!(matches!(
            notification_to_event("turn/completed", &json!({"turn":{"status":"failed"}})),
            Some(ChatEvent::TurnEnd { is_error: true, .. })
        ));
        assert!(
            notification_to_event("item/started", &json!({"item":{"type":"reasoning"}})).is_none()
        );
        assert!(notification_to_event("turn/started", &json!({"threadId":"t"})).is_none());
    }

    #[test]
    fn agent_message_completion_finalizes_streamed_text() {
        // Regression (lingering streaming caret / interleaved parallel streams):
        // agentMessage's *completion* finalizes ITS OWN row — TextDone keyed by
        // item id, carrying the authoritative full body. Dropping it (→ None)
        // left the caret lit; mapping it without the item id (the old Assistant
        // trigger) closed whichever row happened to be open, chopping parallel
        // collab-agent streams into fragments.
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"i","type":"agentMessage","text":"done"}}),
        ) {
            Some(ChatEvent::TextDone { item, text, agent_thread }) => {
                assert_eq!(item.as_deref(), Some("i"));
                assert_eq!(text.as_deref(), Some("done"));
                // No threadId on this notification → mainline.
                assert_eq!(agent_thread, None);
            }
            e => panic!("{e:?}"),
        }
        // item/started for agentMessage still opens no row — text arrives via deltas.
        assert!(
            notification_to_event("item/started", &json!({"item":{"type":"agentMessage"}}))
                .is_none()
        );
    }

    #[test]
    fn parallel_item_deltas_keep_their_item_keys() {
        // Two items streaming concurrently (main narration + a collab sub-agent)
        // must map to deltas keyed by THEIR item ids — the engine turns each key
        // into its own row, so the texts can never char-interleave in one bubble.
        let d = |item: &str, s: &str| {
            notification_to_event(
                "item/agentMessage/delta",
                &json!({"threadId":"t","turnId":"u","itemId":item,"delta":s}),
            )
        };
        for (item, s) in [("a", "我先只读梳理"), ("b", "我会并行追踪"), ("a", " Cargo 结构")] {
            match d(item, s) {
                Some(ChatEvent::TextDelta { text, item: got, agent_thread }) => {
                    assert_eq!(text, s);
                    assert_eq!(got.as_deref(), Some(item));
                    // Same threadId ("t") on every delta here — a REAL parallel
                    // pair (main narration vs a collab sub-agent) would differ,
                    // which `collab_thread_delta_gets_its_own_agent_thread` below
                    // covers explicitly.
                    assert_eq!(agent_thread.as_deref(), Some("t"));
                }
                e => panic!("{e:?}"),
            }
        }
    }

    #[test]
    fn collab_thread_delta_gets_its_own_agent_thread() {
        // The falsifying case from PR #132's review: main narration and a collab
        // sub-agent's own text differ ONLY by threadId, never by item id shape —
        // agent_thread (not arrival order) is what the engine keys grouping on.
        let main = notification_to_event(
            "item/agentMessage/delta",
            &json!({"threadId":"lead-1","turnId":"u","itemId":"m","delta":"main"}),
        );
        let sub = notification_to_event(
            "item/agentMessage/delta",
            &json!({"threadId":"sub-1","turnId":"u","itemId":"s","delta":"sub"}),
        );
        match (main, sub) {
            (
                Some(ChatEvent::TextDelta { agent_thread: a, .. }),
                Some(ChatEvent::TextDelta { agent_thread: b, .. }),
            ) => {
                assert_eq!(a.as_deref(), Some("lead-1"));
                assert_eq!(b.as_deref(), Some("sub-1"));
                assert_ne!(a, b);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn appserver_caps_large_tool_input() {
        // A huge MCP arguments payload must be truncated before it lands in the
        // persisted row (cap_input collapses an oversized object to a string).
        let big = "x".repeat(20_000);
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"m","type":"mcpToolCall","tool":"t","arguments":{"blob":big}}}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                let s = tools[0]
                    .input
                    .as_str()
                    .expect("oversized input capped to string");
                assert!(s.ends_with("… (truncated)"));
                assert!(s.chars().count() < 17_000);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn mcp_object_result_and_plan_text_render() {
        // mcpToolCall result is an MCP result object ({content:[{text}]}) — render
        // its text, not a blank row.
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"m","type":"mcpToolCall","result":{"content":[{"type":"text","text":"task #3"}]},"status":"completed"}}),
        ) {
            Some(ChatEvent::ToolResults { items }) => assert_eq!(items[0].output, "task #3"),
            e => panic!("{e:?}"),
        }
        // /plan content item carries text only on completion → a standalone
        // completed row (it never streamed, so nothing to finalize).
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"p","type":"plan","text":"1. x","status":"completed"}}),
        ) {
            Some(ChatEvent::TextDone { item, text, agent_thread }) => {
                assert_eq!(item.as_deref(), Some("p"));
                assert_eq!(text.as_deref(), Some("1. x"));
                assert_eq!(agent_thread, None);
            }
            e => panic!("{e:?}"),
        }
        // a payload-less plan item still opens no row and surfaces nothing.
        assert!(notification_to_event(
            "item/completed",
            &json!({"item":{"id":"p","type":"plan","status":"completed"}}),
        )
        .is_none());
    }

    #[test]
    fn top_level_error_notification_surfaces_text() {
        // A turn-level failure (auth / usage-limit / context-window) arrives as a
        // bare `error` notification — surface it so the turn doesn't end blank.
        match notification_to_event("error", &json!({"message":"usage limit reached"})) {
            Some(ChatEvent::TextDelta { text, item: None, agent_thread: None }) => {
                assert_eq!(text, "usage limit reached")
            }
            e => panic!("{e:?}"),
        }
        match notification_to_event("error", &json!({"error":{"message":"nested"}})) {
            Some(ChatEvent::TextDelta { text, item: None, agent_thread: None }) => {
                assert_eq!(text, "nested")
            }
            e => panic!("{e:?}"),
        }
        // An empty error message yields nothing (turn/completed still flags error).
        assert!(notification_to_event("error", &json!({"message":""})).is_none());
    }

    #[test]
    fn turn_completed_error_message_is_surfaced() {
        // A failure reported only on turn/completed carries turn.error.message.
        assert_eq!(
            turn_error_text(
                &json!({"turn":{"status":"failed","error":{"message":"context window exceeded"}}})
            )
            .as_deref(),
            Some("context window exceeded")
        );
        // A clean turn carries no error text.
        assert!(turn_error_text(&json!({"turn":{"status":"completed"}})).is_none());
    }

    #[test]
    fn turn_quota_detection_requires_a_structured_code() {
        assert!(turn_reports_quota_exceeded(&json!({
            "turn": {"error": {"code": "rate_limit_reached"}}
        })));
        assert!(turn_reports_quota_exceeded(&json!({
            "turn": {"error": {"details": {"code": "quota_exceeded"}}}
        })));
        assert!(!turn_reports_quota_exceeded(&json!({
            "turn": {"error": {"message": "rate limit exceeded"}}
        })));
    }

    #[test]
    fn only_real_tool_item_types_open_rows() {
        // The tool-call item types per the 0.139.0 ThreadItem union — note the collab
        // type is `collabAgentToolCall` (NOT the README's `collabToolCall`), and
        // `dynamicToolCall` is a tool too.
        for ty in [
            "commandExecution",
            "fileChange",
            "mcpToolCall",
            "collabAgentToolCall",
            "dynamicToolCall",
        ] {
            assert!(
                matches!(
                    notification_to_event(
                        "item/started",
                        &json!({"item":{"id":"c","type":ty,"status":"inProgress"}}),
                    ),
                    Some(ChatEvent::Assistant { .. })
                ),
                "{ty} should open a tool row"
            );
        }
        // a content/lifecycle item must NOT open an empty tool row.
        assert!(notification_to_event(
            "item/started",
            &json!({"item":{"id":"c","type":"reasoning","status":"inProgress"}}),
        )
        .is_none());
    }

    #[test]
    fn collab_tool_call_carries_receiver_thread_ids() {
        // `receiverThreadIds` is the minimal backend signal the
        // frontend groups on — verified against the real `CollabAgentToolCall`
        // ThreadItem shape (codex-rs app-server-protocol/src/protocol/v2/item.rs).
        // A spawn's item/started has no receiver yet (the child doesn't exist).
        match notification_to_event(
            "item/started",
            &json!({"item":{
                "id":"call_1","type":"collabAgentToolCall","tool":"spawnAgent",
                "status":"inProgress","senderThreadId":"lead-1","receiverThreadIds":[],
                "agentsStates":{},
            }}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                assert_eq!(tools[0].name, "collabAgentToolCall");
                assert_eq!(tools[0].collab_threads, Vec::<String>::new());
            }
            e => panic!("{e:?}"),
        }
        // The spawn's item/completed reveals the newly spawned agent's thread
        // id — THIS is the only place a spawn call's collab_threads is ever
        // non-empty (its item/started, above, had none): ToolResults, unlike
        // Assistant, must carry it too, or the frontend could never anchor
        // that thread's branch for the by-far most common collab pattern
        // (spawn once, then `wait`). engine.rs's `merge_tool_results` merges
        // this into the row alongside output/is_error (mutates the ORIGINAL
        // Assistant-inserted content in place, never rebuilds it).
        match notification_to_event(
            "item/completed",
            &json!({"item":{
                "id":"call_1","type":"collabAgentToolCall","tool":"spawnAgent",
                "status":"completed","senderThreadId":"lead-1",
                "receiverThreadIds":["sub-1"],"agentsStates":{},
            }}),
        ) {
            Some(ChatEvent::ToolResults { items }) => {
                assert_eq!(items[0].collab_threads, vec!["sub-1"]);
            }
            e => panic!("{e:?}"),
        }
        // A send/wait call already knows its target — receiverThreadIds populated
        // immediately at item/started, unlike spawn.
        match notification_to_event(
            "item/started",
            &json!({"item":{
                "id":"call_2","type":"collabAgentToolCall","tool":"wait",
                "status":"inProgress","senderThreadId":"lead-1",
                "receiverThreadIds":["sub-1","sub-2"],"agentsStates":{},
            }}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                assert_eq!(tools[0].collab_threads, vec!["sub-1", "sub-2"]);
            }
            e => panic!("{e:?}"),
        }
        // A non-collab tool never carries collab_threads even if the field name
        // were somehow present — only collabAgentToolCall's shape has it.
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"c","type":"commandExecution","command":"echo hi"}}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                assert!(tools[0].collab_threads.is_empty());
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn approval_methods_recognized() {
        // All three approval asks (command, file-change, generic permissions) route
        // to the owning session; ordinary notifications don't.
        assert!(is_approval_request("item/commandExecution/requestApproval"));
        assert!(is_approval_request("item/fileChange/requestApproval"));
        assert!(is_approval_request("item/permissions/requestApproval"));
        assert!(!is_approval_request("item/completed"));
    }

    #[test]
    fn elicitation_is_not_an_approval() {
        // Elicitation blocks the turn but is not an approval; both are declined.
        assert!(is_elicitation_request("mcpServer/elicitation/request"));
        assert!(!is_approval_request("mcpServer/elicitation/request"));
        // requestUserInput is likewise not an approval → declined generically.
        assert!(!is_approval_request("item/tool/requestUserInput"));
        assert!(!is_elicitation_request("item/tool/requestUserInput"));
    }

    #[test]
    fn decline_responses_match_schema() {
        // Verified vs the 0.139.0 app-server JSON schema.
        assert_eq!(
            decline_response("item/commandExecution/requestApproval"),
            Some(json!({ "decision": "decline" }))
        );
        assert_eq!(
            decline_response("item/fileChange/requestApproval"),
            Some(json!({ "decision": "decline" }))
        );
        assert_eq!(
            decline_response("item/permissions/requestApproval"),
            Some(json!({ "permissions": {} }))
        );
        assert_eq!(
            decline_response("mcpServer/elicitation/request"),
            Some(json!({ "action": "decline" }))
        );
        assert_eq!(
            decline_response("item/tool/requestUserInput"),
            Some(json!({ "answers": {} }))
        );
        // No in-protocol decline for an unknown blocking request → JSON-RPC error.
        assert_eq!(decline_response("item/tool/call"), None);
    }

    #[test]
    fn error_response_encodes_id_and_message() {
        let v: Value =
            serde_json::from_str(encode_error_response(&json!("a1"), -32601, "nope").trim())
                .unwrap();
        assert_eq!(v["id"], json!("a1"));
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "nope");
        assert!(v.get("result").is_none());
    }

    #[test]
    fn ignores_non_tool_content_items() {
        // plan/review/todo content items must not open empty tool rows.
        for ty in ["plan", "review", "todoList", "webSearch"] {
            assert!(
                notification_to_event(
                    "item/started",
                    &json!({"item":{"id":"x","type":ty,"status":"inProgress"}}),
                )
                .is_none(),
                "{ty}"
            );
            assert!(
                notification_to_event(
                    "item/completed",
                    &json!({"item":{"id":"x","type":ty,"status":"completed"}}),
                )
                .is_none(),
                "{ty}"
            );
        }
    }

    #[test]
    fn extracts_ids_from_responses() {
        assert_eq!(
            thread_id_of(&json!({"thread":{"id":"th_1"}})).as_deref(),
            Some("th_1")
        );
        assert_eq!(
            turn_id_of(&json!({"turn":{"id":"tn_1"}})).as_deref(),
            Some("tn_1")
        );
        assert_eq!(thread_id_of(&json!({})), None);
    }
}
