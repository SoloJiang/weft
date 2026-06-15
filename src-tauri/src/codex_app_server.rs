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
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    )
}

/// Map a server notification to the engine's `ChatEvent`. Returns `None` for
/// notifications the streaming pipeline ignores (reasoning, thread/turn
/// lifecycle markers, hook/skills observability — handled separately in Stage
/// 4). Mirrors how the exec dialect renders agent text + tool pills.
pub fn notification_to_event(method: &str, params: &Value) -> Option<ChatEvent> {
    match method {
        "item/agentMessage/delta" => {
            params["delta"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| ChatEvent::TextDelta {
                    text: s.to_string(),
                })
        }
        "item/started" => {
            // Non-text items surface as activity pills as soon as they start;
            // agentMessage waits for its deltas / completion.
            let item = &params["item"];
            match item["type"].as_str() {
                Some("agentMessage") | None => None,
                Some("reasoning") => None,
                Some("error") => Some(ChatEvent::TextDelta {
                    text: crate::lead_chat::proto::error_text_from_item(item),
                }),
                Some(kind) => Some(ChatEvent::Assistant {
                    texts: vec![],
                    tools: vec![crate::lead_chat::proto::ToolCall::pill(
                        kind,
                        item_summary(kind, item),
                    )],
                }),
            }
        }
        // Final item state. Verified live: an agentMessage's text already arrived
        // token-by-token via `item/agentMessage/delta`, so re-emitting it here
        // would double-render — deltas are authoritative. Non-text items showed
        // their pill on item/started; userMessage/reasoning are ignored. (A
        // no-delta config would need an item.text fallback at wire-in, but codex
        // streams deltas in practice.)
        "item/completed" => None,
        "turn/completed" => Some(ChatEvent::TurnEnd {
            is_error: params["turn"]["status"].as_str() != Some("completed"),
            context_tokens: None, // codex app-server usage 解析留给后续里程碑
        }),
        _ => None, // thread/started, turn/started, hook/*, skills/changed → Stage 2/4
    }
}

/// A compact, truncated summary string for a non-text item's activity pill.
fn item_summary(kind: &str, item: &Value) -> String {
    let s = match kind {
        "commandExecution" => item["command"].as_str().unwrap_or_default(),
        "fileChange" => item["changes"][0]["path"].as_str().unwrap_or_default(),
        _ => "",
    };
    s.chars().take(120).collect()
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
    /// Bytes destined for the app-server's stdin are queued here and written by a
    /// dedicated [`write_loop`] task — NEVER under this connection lock. A write
    /// that blocks on stdin pipe backpressure would otherwise hold the lock while
    /// the `read_loop`'s `resolve`/`route` (which need the same lock) stall, the
    /// stdout pipe fills, the child stops reading stdin, and the write never
    /// drains: a textbook two-pipe deadlock. The channel send is non-blocking, so
    /// the lock is only ever held for in-memory bookkeeping.
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
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

/// Serialize every write to the app-server's stdin on one task, OFF the
/// connection lock. A full stdin pipe blocks only this task — never a caller
/// holding the lock — so the `read_loop` keeps draining stdout and the two-pipe
/// deadlock can't form. Exits when every `write_tx` is dropped (i.e. the `Inner`
/// is torn down) or stdin errors; either way it tears the connection down so
/// pending callers fail fast and the next `client()` reconnects.
async fn write_loop(client: Client, mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(buf) = rx.recv().await {
        if stdin.write_all(&buf).await.is_err() || stdin.flush().await.is_err() {
            break; // stdin closed/errored
        }
    }
    // A stdin error with stdout still open would otherwise leave the request
    // mid-write unresolved (its caller waits out the full 60s) and `client()`
    // reporting "connected" until stdout EOFs. Tear down now so the codex
    // fallback/reconnect path runs promptly. No-op if already torn down.
    client.teardown().await;
}

impl Client {
    async fn connect(&self) -> anyhow::Result<()> {
        let mut g = self.0.lock().await;
        if g.is_some() {
            return Ok(());
        }
        let mut child = Command::new("codex")
            .arg("app-server")
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
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        *g = Some(Inner {
            write_tx,
            next_id: 1,
            pending: HashMap::new(),
            threads: HashMap::new(),
            active_turn: HashMap::new(),
            _child: child,
        });
        drop(g);

        // Writer owns stdin and awaits each write off the lock (see `write_tx`).
        tauri::async_runtime::spawn(write_loop(self.clone(), stdin, write_rx));

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
                    if let (Some(ev), Some(tid)) = (
                        notification_to_event(&method, &params),
                        params["threadId"].as_str().map(String::from),
                    ) {
                        self.route(&tid, ThreadMsg::Event(ev)).await;
                    }
                }
                Incoming::ServerRequest { id, method, params } => {
                    if is_approval_request(&method) {
                        if let Some(tid) = params["threadId"].as_str().map(String::from) {
                            self.route(&tid, ThreadMsg::Approval { id, method, params })
                                .await;
                        }
                    }
                }
                Incoming::Other => {}
            }
        }
        // EOF/crash → drop the connection so the next use reconnects + re-resumes.
        self.teardown().await;
    }

    /// Drop the live `Inner`: stops the writer (its `write_tx` is gone), fails
    /// every pending caller at once (their oneshot senders drop → `rx` errors
    /// instead of waiting out the 60s timeout), and kills the child
    /// (`kill_on_drop`) so no buffered request is processed late. The next
    /// `client()` reconnects. Idempotent — safe to call from read_loop, the
    /// writer, and a request timeout.
    async fn teardown(&self) {
        *self.0.lock().await = None;
    }

    async fn resolve(&self, id: i64, res: Result<Value, String>) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            if let Some(tx) = inner.pending.remove(&id) {
                let _ = tx.send(res);
            }
        }
    }

    async fn route(&self, thread_id: &str, msg: ThreadMsg) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            if let Some(tx) = inner.threads.get(thread_id) {
                let _ = tx.send(msg);
            }
        }
    }

    /// Send a request and await its reply (`result` on success, `error.message`
    /// on failure), with a hard timeout so a wedged server can't hang a caller.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        // The pending waiter is keyed by `id` inside the block; the timeout path
        // tears the whole connection down rather than removing one entry, so the
        // id isn't needed out here.
        let rx = {
            let mut g = self.0.lock().await;
            let inner = g
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("codex app-server not connected"))?;
            let id = inner.next_id;
            inner.next_id += 1;
            // Queue the write (non-blocking) under the lock so wire order matches
            // id-allocation order; the awaited write happens on `write_loop`, never
            // here. Register the waiter only after the enqueue succeeds, all while
            // holding the lock — so a reply (resolve, same lock) can't race ahead.
            inner
                .write_tx
                .send(encode_request(id, method, params).into_bytes())
                .map_err(|_| anyhow::anyhow!("codex app-server writer closed"))?;
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(id, tx);
            rx
        };
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => anyhow::bail!("codex app-server {method}: {e}"),
            Ok(Err(_)) => anyhow::bail!("codex app-server {method}: reply dropped"),
            Err(_) => {
                // 60s with no reply ⇒ the connection is wedged. Tear it down so the
                // still-buffered request (notably turn/start) can't be written and
                // run late after the caller already fell back to exec — which would
                // execute the same turn twice. Dropping Inner also discards the
                // queued bytes and kills the child.
                self.teardown().await;
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
            .write_tx
            .send(encode_notification(method, params).into_bytes())
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

    /// Record the in-flight turn id for a thread (for a later interrupt).
    pub async fn set_active_turn(&self, thread_id: &str, turn_id: &str) {
        if let Some(inner) = self.0.lock().await.as_mut() {
            inner
                .active_turn
                .insert(thread_id.to_string(), turn_id.to_string());
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
        inner
            .write_tx
            .send(line.into_bytes())
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
        match notification_to_event(
            "item/agentMessage/delta",
            &json!({"threadId":"t","turnId":"u","itemId":"i","delta":"He"}),
        ) {
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "He"),
            e => panic!("{e:?}"),
        }
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"i","type":"commandExecution","command":"npm test"}}),
        ) {
            Some(ChatEvent::Assistant { tools, .. }) => {
                assert_eq!(tools[0].name, "commandExecution");
                assert_eq!(tools[0].summary, "npm test");
                assert!(tools[0].id.is_empty());
            }
            e => panic!("{e:?}"),
        }
        match notification_to_event(
            "item/started",
            &json!({"item":{"id":"i","type":"error","message":"unknown slash command"}}),
        ) {
            Some(ChatEvent::TextDelta { text }) => assert_eq!(text, "unknown slash command"),
            e => panic!("{e:?}"),
        }
        // agentMessage text already streamed via deltas — item/completed is a
        // no-op to avoid double-rendering (verified against live 0.137.0).
        assert!(notification_to_event(
            "item/completed",
            &json!({"item":{"id":"i","type":"agentMessage","text":"done"}}),
        )
        .is_none());
        assert!(matches!(
            notification_to_event("turn/completed", &json!({"turn":{"status":"completed"}})),
            Some(ChatEvent::TurnEnd { is_error: false, .. })
        ));
        assert!(matches!(
            notification_to_event("turn/completed", &json!({"turn":{"status":"failed"}})),
            Some(ChatEvent::TurnEnd { is_error: true, .. })
        ));
        // reasoning + lifecycle markers are ignored
        assert!(
            notification_to_event("item/started", &json!({"item":{"type":"reasoning"}})).is_none()
        );
        assert!(notification_to_event("turn/started", &json!({"threadId":"t"})).is_none());
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
