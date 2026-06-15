//! Codex `app-server` protocol layer (Stage 1 of the exec→app-server migration,
//! spec: docs/superpowers/specs/2026-06-12-codex-app-server-migration-design.md).
//!
//! This module is the PURE, source-verified wire layer: it encodes client→server
//! requests, classifies incoming lines, and maps server notifications to the
//! engine's existing `ChatEvent`. It is intentionally NOT yet wired into the
//! engine — codex still runs via `exec` — so nothing here can break the live
//! path. Wiring (a single global multiplexed `codex app-server` keyed by
//! thread_id), approval round-trips, and the hard min-version switch are Stage
//! 2+, which require validation against a live `codex app-server` binary.
//!
//! Wire format (verified against openai/codex main, app-server-protocol):
//! codex uses a JSON-RPC-LIKE envelope with NO `"jsonrpc":"2.0"` field. Messages
//! are distinguished structurally:
//!   - request   (either direction): has `method` AND `id`            -> needs a response
//!   - notification (server→client): has `method`, NO `id`
//!   - response  (to our request):   has `id` AND `result`
//!   - error     (to our request):   has `id` AND `error{code,message}`
//! `id` (RequestId) is untagged: a JSON string or integer. We send integer ids.
#![allow(dead_code)] // Stage 1: protocol layer landed + tested; engine wire-in is Stage 2.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::lead_chat::proto::ChatEvent;

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

pub fn thread_resume_params(thread_id: &str) -> Value {
    json!({ "threadId": thread_id })
}

/// turn/start: `input` is a Vec<UserInput>; a plain message is the `text` variant
/// (serde tag "type" = "text"). NOT a single object, NOT "input_text".
pub fn turn_start_params(thread_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "input": [ { "type": "text", "text": text } ]
    })
}

/// turn/interrupt requires BOTH threadId and turnId (turnId is load-bearing —
/// omitting it fails to deserialize server-side).
pub fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
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

/// Extract `result.turn.id` from a turn/start response.
pub fn turn_id_of(result: &Value) -> Option<String> {
    result["turn"]["id"].as_str().map(String::from)
}

/// Whether a server→client request is an approval ask (Stage 2 routes these to
/// the Ask Bridge). Both command-exec and file-change approvals qualify.
pub fn is_approval_request(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    )
}

/// Map a server notification to the engine's `ChatEvent`. Tool items (camelCase:
/// commandExecution/fileChange/mcpToolCall …) become a running tool row on
/// `item/started` and its result on `item/completed`; agent text streams via
/// deltas; `thread/tokenUsage/updated` carries the current-context usage.
pub fn notification_to_event(method: &str, params: &Value) -> Option<ChatEvent> {
    use crate::lead_chat::proto::ChatEvent;
    let item = &params["item"];
    match method {
        "item/agentMessage/delta" => params["delta"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| ChatEvent::TextDelta { text: s.to_string() }),
        "item/started" => match item["type"].as_str() {
            Some("error") => Some(ChatEvent::TextDelta {
                text: crate::lead_chat::proto::error_text_from_item(item),
            }),
            // Only real tool items open a row; agentMessage/reasoning/plan/review/
            // other content items are ignored so they don't show as empty rows.
            Some("commandExecution" | "fileChange" | "mcpToolCall") => Some(ChatEvent::Assistant {
                texts: vec![],
                tools: vec![appserver_tool_call(item)],
            }),
            _ => None,
        },
        // agentMessage text already streamed via deltas → no-op; tool items deliver
        // their result here, merged into the running row by item id.
        "item/completed" => match item["type"].as_str() {
            Some("commandExecution" | "fileChange" | "mcpToolCall") => {
                Some(ChatEvent::ToolResults {
                    items: vec![appserver_tool_result(item)],
                })
            }
            // already streamed (agentMessage) or carries no display payload.
            Some("agentMessage" | "userMessage" | "reasoning") | None => None,
            // Other content items (/plan, /review …) don't stream via agentMessage
            // deltas, so surface any text they carry instead of dropping it.
            Some(_) => crate::lead_chat::proto::codex_content_item_text(item)
                .map(|text| ChatEvent::TextDelta { text }),
        },
        // Top-level failure (auth / usage-limit / context-window …): surface the
        // message so the turn doesn't end blank, then turn/completed marks it error.
        "error" => {
            let text = params["message"]
                .as_str()
                .or_else(|| params["error"]["message"].as_str())
                .or_else(|| params["error"].as_str())
                .unwrap_or("Codex reported an error.")
                .trim();
            (!text.is_empty()).then(|| ChatEvent::TextDelta {
                text: text.to_string(),
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

/// Running `ToolCall` from an app-server `item.started` tool item.
fn appserver_tool_call(item: &Value) -> crate::lead_chat::proto::ToolCall {
    crate::lead_chat::proto::ToolCall {
        id: item["id"].as_str().unwrap_or_default().to_string(),
        name: item["type"].as_str().unwrap_or("tool").to_string(),
        input: appserver_tool_input(item),
        summary: appserver_tool_summary(item),
        output: None,
        is_error: false,
    }
}

/// Result of an app-server `item.completed` tool item, keyed by item id.
fn appserver_tool_result(item: &Value) -> crate::lead_chat::proto::ToolResultItem {
    crate::lead_chat::proto::ToolResultItem {
        id: item["id"].as_str().unwrap_or_default().to_string(),
        output: appserver_tool_output(item),
        is_error: appserver_tool_is_error(item),
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
    // Cap like the exec/claude path: a big MCP `arguments` or `changes` payload
    // would otherwise bloat the persisted row + its push even though output is
    // capped. Small inputs pass through unchanged (UI still renders the object).
    crate::lead_chat::proto::cap_input(Value::Object(obj))
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

// ───────────────────── runtime client (Stage 1.5 — UNWIRED) ─────────────────
//
// One global, multiplexed `codex app-server` connection: spawn once, handshake
// once, route every session's turns/notifications/approvals by thread_id. This
// is the decided architecture made concrete; NOTHING calls `client()` yet, so it
// cannot affect the live (exec) codex path. It compiles and reuses the
// unit-tested protocol helpers above, but the live handshake/turn/approval
// round-trips are UNVALIDATED until run against a real `codex app-server` binary
// — that validation is the gate before Stage 2 wires this into the engine and
// flips the hard switch.

/// What the demux delivers to a session subscribed on a thread_id.
#[derive(Debug)]
pub enum ThreadMsg {
    /// A streaming event for the session's timeline.
    Event(ChatEvent),
    /// A liveness ping (e.g. command output-delta while a long command runs) that
    /// carries no timeline change — the consumer uses it only to refresh the
    /// runaway-guard's last-activity clock so a busy command isn't idle-killed.
    Heartbeat,
    /// An approval ask the session must answer via [`Client::reply_approval`]
    /// (echoing `id`), else the turn hangs. `decision` ∈ accept | acceptForSession
    /// | decline | cancel.
    Approval {
        id: Value,
        method: String,
        params: Value,
    },
}

struct Inner {
    stdin: ChildStdin,
    next_id: i64,
    /// our request id → awaiting caller (Ok(result) / Err(message)).
    pending: HashMap<i64, oneshot::Sender<Result<Value, String>>>,
    /// thread_id → that session's event sink.
    threads: HashMap<String, mpsc::UnboundedSender<ThreadMsg>>,
    /// thread_id → the in-flight turn id (needed by turn/interrupt).
    active_turn: HashMap<String, String>,
    _child: tokio::process::Child,
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

impl Client {
    /// Spawn + handshake a fresh `codex app-server`, injecting `extra_args` (a
    /// session's `-c mcp_servers...` bus flags) and running in `cwd`. Each session
    /// gets its OWN process so its per-thread MCP config is isolated — app-server
    /// MCP is app-scoped, so one shared connection couldn't carry per-thread bus URLs.
    pub async fn connect_session(
        extra_args: &[String],
        cwd: &std::path::Path,
    ) -> anyhow::Result<Client> {
        let client = Client(Arc::new(Mutex::new(None)));
        client.spawn_inner(extra_args, Some(cwd)).await?;
        Ok(client)
    }

    /// Whether the connection is still alive (read_loop clears the inner on EOF).
    pub async fn is_alive(&self) -> bool {
        self.0.lock().await.is_some()
    }

    /// Kill the connection: drops the child (kill_on_drop) and closes the thread
    /// sinks, so the per-session consumer task exits.
    pub async fn shutdown(&self) {
        *self.0.lock().await = None;
    }

    async fn connect(&self) -> anyhow::Result<()> {
        self.spawn_inner(&[], None).await
    }

    async fn spawn_inner(
        &self,
        extra_args: &[String],
        cwd: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        let mut g = self.0.lock().await;
        if g.is_some() {
            return Ok(());
        }
        let mut command = Command::new("codex");
        command.arg("app-server").arg("--stdio").args(extra_args);
        if let Some(c) = cwd {
            command.current_dir(c);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        *g = Some(Inner {
            stdin,
            next_id: 1,
            pending: HashMap::new(),
            threads: HashMap::new(),
            active_turn: HashMap::new(),
            _child: child,
        });
        drop(g);

        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.read_loop(stdout).await });

        // Handshake: initialize (await), then the `initialized` notification.
        self.request(
            "initialize",
            initialize_params("weft", env!("CARGO_PKG_VERSION")),
        )
        .await?;
        self.notify("initialized", None).await?;
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
                        self.route_resolved(tid.as_deref(), ThreadMsg::Event(ev))
                            .await;
                    } else if method.ends_with("/outputDelta") {
                        // A long command is still producing output; keep the turn
                        // marked alive so the idle watchdog doesn't kill it.
                        self.route_resolved(tid.as_deref(), ThreadMsg::Heartbeat)
                            .await;
                    }
                }
                Incoming::ServerRequest { id, method, params } => {
                    if is_approval_request(&method) {
                        let tid = params["threadId"].as_str().map(String::from);
                        self.route_resolved(
                            tid.as_deref(),
                            ThreadMsg::Approval { id, method, params },
                        )
                        .await;
                    }
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
        let (id, rx) = {
            let mut g = self.0.lock().await;
            let inner = g
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
            let id = inner.next_id;
            inner.next_id += 1;
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(id, tx);
            inner
                .stdin
                .write_all(encode_request(id, method, params).as_bytes())
                .await?;
            inner.stdin.flush().await?;
            (id, rx)
        };
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
        let mut g = self.0.lock().await;
        let inner = g
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
        inner
            .stdin
            .write_all(encode_notification(method, params).as_bytes())
            .await?;
        inner.stdin.flush().await?;
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
    pub async fn start_turn(&self, thread_id: &str, text: &str) -> anyhow::Result<String> {
        let r = self
            .request("turn/start", turn_start_params(thread_id, text))
            .await?;
        turn_id_of(&r).ok_or_else(|| anyhow::anyhow!("turn/start: no turn.id"))
    }
    pub async fn interrupt(&self, thread_id: &str, turn_id: &str) -> anyhow::Result<()> {
        self.request("turn/interrupt", turn_interrupt_params(thread_id, turn_id))
            .await
            .map(|_| ())
    }
    /// Answer an approval request. `decision` ∈ accept | acceptForSession | decline | cancel.
    pub async fn reply_approval(&self, id: &Value, decision: &str) -> anyhow::Result<()> {
        let mut g = self.0.lock().await;
        let inner = g
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
        let line = encode_response(id, json!({ "decision": decision }));
        inner.stdin.write_all(line.as_bytes()).await?;
        inner.stdin.flush().await?;
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
    fn interrupt_carries_both_ids() {
        let v: Value = serde_json::from_str(
            encode_request(9, "turn/interrupt", turn_interrupt_params("t_1", "turn_9")).trim(),
        )
        .unwrap();
        assert_eq!(v["params"]["threadId"], "t_1");
        assert_eq!(v["params"]["turnId"], "turn_9");
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
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "He"),
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
            Some(ChatEvent::Usage { context_tokens, window }) => {
                assert_eq!(context_tokens, 18440);
                assert_eq!(window, Some(258400));
            }
            e => panic!("{e:?}"),
        }
        // error item → text; agentMessage/userMessage/reasoning + lifecycle ignored.
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"i","type":"error","message":"unknown slash command"}}),
        ) {
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "unknown slash command"),
            e => panic!("{e:?}"),
        }
        assert!(notification_to_event(
            "item/completed",
            &json!({"item":{"id":"i","type":"agentMessage","text":"done"}}),
        )
        .is_none());
        assert!(
            notification_to_event("item/started", &json!({"item":{"type":"userMessage"}})).is_none()
        );
        assert!(matches!(
            notification_to_event("turn/completed", &json!({"turn":{"status":"completed"}})),
            Some(ChatEvent::TurnEnd { is_error: false, .. })
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
    fn appserver_caps_large_tool_input() {
        // A huge MCP arguments payload must be truncated before it lands in the
        // persisted row (cap_input collapses an oversized object to a string).
        let big = "x".repeat(20_000);
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"m","type":"mcpToolCall","tool":"t","arguments":{"blob":big}}}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                let s = tools[0].input.as_str().expect("oversized input capped to string");
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
        // /plan content item carries text only on completion → surface as text.
        match notification_to_event(
            "item/completed",
            &json!({"item":{"id":"p","type":"plan","text":"1. x","status":"completed"}}),
        ) {
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "1. x"),
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
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "usage limit reached"),
            e => panic!("{e:?}"),
        }
        match notification_to_event("error", &json!({"error":{"message":"nested"}})) {
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "nested"),
            e => panic!("{e:?}"),
        }
        // An empty error message yields nothing (turn/completed still flags error).
        assert!(notification_to_event("error", &json!({"message":""})).is_none());
    }

    #[test]
    fn approval_methods_recognized() {
        // All three approval asks (command, file-change, generic permissions) route
        // to the Ask Bridge; ordinary notifications don't.
        assert!(is_approval_request("item/commandExecution/requestApproval"));
        assert!(is_approval_request("item/fileChange/requestApproval"));
        assert!(is_approval_request("item/permissions/requestApproval"));
        assert!(!is_approval_request("item/completed"));
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
