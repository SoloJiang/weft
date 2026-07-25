//! Global multiplexed ACP client pool — one child process per backend id.
//!
//! Protocol-only: no CLI name strings except via [`super::backends::AcpBackend`].
//! Codex app-server must NOT use this module (different wire dialect).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

use super::backends::{backend_for, AcpBackend, McpServerSpec};
use super::jsonrpc::{
    classify, encode_error_response, encode_notification, encode_request, encode_response, Incoming,
};
use super::map::{stop_reason_is_cancelled, stop_reason_is_error, update_to_out, UpdateOut};
use super::permission::{
    intent_key_from_params, pick_option_id, selected_outcome, summary_from_params, AlwaysCache, Want,
};
use crate::lead_chat::proto::{ChatEvent, SlashCmd};

/// Events demuxed to a subscribed Weft session.
#[derive(Debug)]
pub enum SessionEvent {
    Chat(ChatEvent),
    Commands(Vec<SlashCmd>),
    Usage {
        context_tokens: u64,
        window: Option<u64>,
    },
    Meta {
        model: Option<String>,
        thinking: Option<String>,
    },
    /// Streaming reasoning text (agent_thought_chunk) for the busy indicator.
    Thought {
        text: String,
    },
    /// Prompt-task → consumer ordering: consumer replies when this is dequeued.
    DrainBarrier(tokio::sync::oneshot::Sender<()>),
    /// Permission needed — engine/runtime resolves via Ask and replies on the wire.
    /// Carries enough for the default handler; custom handlers may be installed later.
    Permission {
        request_id: Value,
        summary: String,
        detail: String,
        intent_key: String,
        options: Vec<Value>,
    },
}

#[derive(Debug, Clone)]
pub struct UsageBits {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PromptOutcome {
    pub stop_reason: String,
    pub usage: Option<UsageBits>,
    pub is_error: bool,
    pub cancelled: bool,
}

type PendingMap = HashMap<i64, oneshot::Sender<Result<Value, String>>>;
type SessionMap = HashMap<String, SessionRoute>;

struct SessionRoute {
    events: mpsc::UnboundedSender<SessionEvent>,
    always: AlwaysCache,
    /// Optional auto-reply for tests / dangerous mode short-circuit at route layer.
    auto_want: Option<Want>,
}

struct Inner {
    write_tx: mpsc::UnboundedSender<(Vec<u8>, Option<oneshot::Sender<()>>)>,
    next_id: i64,
    pending: PendingMap,
    sessions: SessionMap,
    /// Permission request ids still awaiting a reply, mapped to a monotonic
    /// generation. Safety-net timers capture the generation at arm time so a
    /// stale 1h timer cannot reject a later request that reused omp's id:0.
    pending_permission_ids: std::collections::HashMap<String, u64>,
    permission_gen: u64,
    /// session/update events that arrived before subscribe() installed a route
    /// (OMP often emits available_commands_update right after session/new).
    pending_updates: std::collections::HashMap<String, Vec<SessionEvent>>,
    /// Sessions currently between session/new|resume and subscribe — only these
    /// may buffer pre-subscribe updates. Explicit unsubscribe drops the key.
    opening_sessions: std::collections::HashSet<String>,
    /// True while a session/new|resume|load request is in flight — buffer any
    /// session/update even before we know the sessionId.
    expecting_session: bool,
    /// Generation of this child connection; read_loop only clears inner when
    /// its generation still owns the slot (writer-fail reconnect race).
    connection_gen: u64,
    _child: tokio::process::Child,
    _reg: crate::proc_registry::Registration,
}
/// Result of `session/new` / `session/resume`, including config metadata
/// OMP returns under `configOptions` (model / thinking).
#[derive(Debug, Clone)]
pub struct SessionOpen {
    pub session_id: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// Read `currentValue` for a named entry in `result.configOptions`.
fn config_option_current(result: &Value, id: &str) -> Option<String> {
    let opts = result.get("configOptions")?.as_array()?;
    for opt in opts {
        let oid = opt.get("id").or_else(|| opt.get("configId")).and_then(|v| v.as_str());
        if oid != Some(id) {
            continue;
        }
        if let Some(v) = opt.get("currentValue").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
        if let Some(v) = opt.get("value").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    None
}

#[derive(Clone)]
pub struct ClientHandle {
    backend_id: &'static str,
    inner: Arc<Mutex<Option<Inner>>>,
}

struct Pool {
    clients: Mutex<HashMap<&'static str, ClientHandle>>,
}

static POOL: LazyLock<Pool> = LazyLock::new(|| Pool {
    clients: Mutex::new(HashMap::new()),
});

/// Serializes first-time ACP child creation so two concurrent first turns cannot
/// each spawn a child and race the pool insert.
static CREATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Get or create the global client for `backend_id` (e.g. `"omp"`).
/// `program` is the effective binary (session pin or global override).
pub async fn client(backend_id: &str, program: &str) -> anyhow::Result<ClientHandle> {
    let backend = backend_for(backend_id)
        .ok_or_else(|| anyhow::anyhow!("no ACP backend registered for {backend_id}"))?;
    let id = backend.id();
    let existing = {
        let g = POOL.clients.lock().await;
        g.get(id).cloned()
    };
    if let Some(c) = existing {
        // Already pooled — keep the running child (command pin applies on next
        // full shutdown/respawn). Must not hold POOL.clients across ensure.
        c.ensure_connected(backend, program).await?;
        return Ok(c);
    }
    // Serialize first create so two first turns cannot spawn two children.
    let _create = CREATE_LOCK.lock().await;
    let existing = {
        let g = POOL.clients.lock().await;
        g.get(id).cloned()
    };
    if let Some(c) = existing {
        c.ensure_connected(backend, program).await?;
        return Ok(c);
    }
    let handle = ClientHandle {
        backend_id: id,
        inner: Arc::new(Mutex::new(None)),
    };
    handle.ensure_connected(backend, program).await?;
    POOL.clients.lock().await.insert(id, handle.clone());
    Ok(handle)
}

/// Tear down one backend connection (tool command bounce / tests).
pub async fn shutdown(backend_id: &str) {
    let c = {
        let mut g = POOL.clients.lock().await;
        g.remove(backend_id)
    };
    if let Some(c) = c {
        c.shutdown_and_reap().await;
    }
}

pub async fn shutdown_all() {
    let mut g = POOL.clients.lock().await;
    let all: Vec<_> = g.drain().map(|(_, c)| c).collect();
    drop(g);
    for c in all {
        c.shutdown_and_reap().await;
    }
}

impl ClientHandle {
    pub fn backend_id(&self) -> &'static str {
        self.backend_id
    }

    async fn ensure_connected(
        &self,
        backend: Arc<dyn AcpBackend>,
        program: &str,
    ) -> anyhow::Result<()> {
        if self.inner.lock().await.is_some() {
            return Ok(());
        }
        self.spawn(backend, program).await
    }

    async fn spawn(&self, backend: Arc<dyn AcpBackend>, program: &str) -> anyhow::Result<()> {
        let mut g = self.inner.lock().await;
        if g.is_some() {
            return Ok(());
        }
        let (prog, args) = backend.spawn_argv(program);
        let mut command = Command::new(&prog);
        command.args(&args);
        command.env("PATH", crate::detect::tool_path());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let owner = crate::proc_registry::Owner::global_app_server();
        let configured = crate::proc_registry::configure(&mut command, owner);
        let mut child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {} ACP: {e}", backend.id()))?;
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
        let me_w = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut stdin = stdin;
            while let Some((bytes, ack)) = write_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    me_w.fail_pending_and_reap("ACP stdin writer failed").await;
                    break;
                }
                if let Some(a) = ack {
                    let _ = a.send(());
                }
            }
        });

        let connection_gen = {
            static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        };
        *g = Some(Inner {
            write_tx,
            next_id: 1,
            pending: HashMap::new(),
            sessions: HashMap::new(),
            pending_permission_ids: std::collections::HashMap::new(),
            permission_gen: 0,
            pending_updates: std::collections::HashMap::new(),
            opening_sessions: std::collections::HashSet::new(),
            expecting_session: false,
            connection_gen,
            _child: child,
            _reg: reg,
        });
        drop(g); // request() needs this mutex — never hold across initialize.

        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.read_loop(stdout, connection_gen).await });

        // initialize
        let init_params = json!({
            "protocolVersion": 1,
            "clientInfo": {
                "name": "weft",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "clientCapabilities": backend.client_capabilities(),
        });
        if let Err(e) = self.request("initialize", init_params).await {
            self.shutdown_and_reap().await;
            return Err(e);
        }
        Ok(())
    }

    async fn fail_pending_and_reap(&self, message: &str) {
        if let Some(mut inner) = self.inner.lock().await.take() {
            for (_, tx) in inner.pending.drain() {
                let _ = tx.send(Err(message.to_string()));
            }
            // Tree-aware: OMP may have shell/tool descendants in their own groups.
            crate::proc_registry::reap(&mut inner._child, &inner._reg).await;
        }
    }

    pub async fn shutdown_and_reap(&self) {
        if let Some(mut inner) = self.inner.lock().await.take() {
            crate::proc_registry::reap(&mut inner._child, &inner._reg).await;
        }
    }

    async fn read_loop(&self, stdout: tokio::process::ChildStdout, connection_gen: u64) {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match classify(&line) {
                Incoming::Response { id, result } => self.resolve(id, Ok(result)).await,
                Incoming::Error { id, message, .. } => self.resolve(id, Err(message)).await,
                Incoming::Notification { method, params } => {
                    if method == "session/update" {
                        self.on_session_update(params).await;
                    }
                }
                Incoming::ServerRequest { id, method, params } => {
                    if method == "session/request_permission" {
                        self.on_permission(id, params).await;
                    } else {
                        let _ = self
                            .write_raw(encode_error_response(
                                &id,
                                -32601,
                                &format!("method not supported: {method}"),
                            ))
                            .await;
                    }
                }
                Incoming::Skip => {}
            }
        }
        // Stdout closed: tree-reap if we still own the slot. A writer-fail may
        // have already taken inner and spawned a newer generation.
        let mut g = self.inner.lock().await;
        if g.as_ref().is_some_and(|i| i.connection_gen == connection_gen) {
            if let Some(mut inner) = g.take() {
                drop(g);
                crate::proc_registry::reap(&mut inner._child, &inner._reg).await;
            }
        }
    }

    async fn resolve(&self, id: i64, res: Result<Value, String>) {
        if let Some(inner) = self.inner.lock().await.as_mut() {
            if let Some(tx) = inner.pending.remove(&id) {
                let _ = tx.send(res);
            }
        }
    }

    async fn on_session_update(&self, params: Value) {
        let sid = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        let out = update_to_out(&update);
        let ev = match out {
            UpdateOut::Chat(c) => SessionEvent::Chat(c),
            UpdateOut::Commands(c) => SessionEvent::Commands(c),
            UpdateOut::Usage {
                context_tokens,
                window,
            } => SessionEvent::Usage {
                context_tokens,
                window,
            },
            UpdateOut::Meta { model, thinking } => SessionEvent::Meta { model, thinking },
            UpdateOut::Thought { text } => SessionEvent::Thought { text },
            UpdateOut::Ignore => return,
        };
        if let Some(inner) = self.inner.lock().await.as_mut() {
            if let Some(route) = inner.sessions.get(sid) {
                let _ = route.events.send(ev);
            } else if !sid.is_empty()
                && (inner.opening_sessions.contains(sid) || inner.expecting_session)
            {
                // Buffer while open→subscribe is in flight OR a session/* RPC is
                // outstanding (updates can race ahead of mark_opening). After
                // explicit unsubscribe, expecting_session is false and the sid
                // is not in opening_sessions — drop late updates.
                const MAX_BUFFERED: usize = 64;
                let buf = inner.pending_updates.entry(sid.to_string()).or_default();
                if buf.len() < MAX_BUFFERED {
                    buf.push(ev);
                }
            }
        }
    }

    async fn on_permission(&self, id: Value, params: Value) {
        let sid = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let options = params
            .get("options")
            .and_then(|o| o.as_array())
            .cloned()
            .unwrap_or_default();
        let (summary, detail) = summary_from_params(&params);
        let intent = intent_key_from_params(&params);
        let req_key = Self::permission_id_key(&id);

        let (auto, gen) = {
            let mut g = self.inner.lock().await;
            let Some(inner) = g.as_mut() else {
                drop(g);
                let _ = self
                    .write_raw(encode_error_response(&id, -32000, "not connected"))
                    .await;
                return;
            };
            // pending map keys by JSON-RPC id (omp reuses 0) → generation
            inner.permission_gen = inner.permission_gen.wrapping_add(1);
            let gen = inner.permission_gen;
            inner.pending_permission_ids.insert(req_key.clone(), gen);
            let Some(route) = inner.sessions.get_mut(&sid) else {
                drop(g);
                self.reply_permission(&id, &options, Want::RejectOnce).await;
                return;
            };
            // always-cache keys by intent, NOT request id
            let auto = if let Some(w) = route.always.get(&intent) {
                Some(w)
            } else {
                route.auto_want
            };
            (auto, gen)
        };
        if let Some(want) = auto {
            self.reply_permission(&id, &options, want).await;
            return;
        }

        let delivered = {
            let g = self.inner.lock().await;
            g.as_ref()
                .and_then(|inner| inner.sessions.get(&sid))
                .map(|route| {
                    route
                        .events
                        .send(SessionEvent::Permission {
                            request_id: id.clone(),
                            summary: summary.clone(),
                            detail: detail.clone(),
                            intent_key: intent.clone(),
                            options: options.clone(),
                        })
                        .is_ok()
                })
                .unwrap_or(false)
        };
        if !delivered {
            self.reply_permission(&id, &options, Want::RejectOnce).await;
            return;
        }
        let me = self.clone();
        let id_t = id.clone();
        let opts_t = options.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            me.reply_permission_if_pending_gen(&id_t, &opts_t, Want::RejectOnce, gen)
                .await;
        });
    }

    fn permission_id_key(id: &Value) -> String {
        id.to_string()
    }

    /// Answer a permission request (called by engine after Ask, or auto paths).
    /// Idempotent while the id is pending: once removed, further calls no-op so
    /// a reused JSON-RPC id (omp has used `0`) can be accepted on the next ask.
    pub async fn reply_permission(&self, id: &Value, options: &[Value], want: Want) {
        {
            let mut g = self.inner.lock().await;
            let Some(inner) = g.as_mut() else {
                return;
            };
            let key = Self::permission_id_key(id);
            if inner.pending_permission_ids.remove(&key).is_none() {
                return; // already answered or unknown
            }
        }
        let option_id = pick_option_id(options, want)
            .or_else(|| pick_option_id(options, Want::RejectOnce))
            .unwrap_or_else(|| want.kind_str_fallback().to_string());
        let body = selected_outcome(&option_id);
        let _ = self.write_raw(encode_response(id, body)).await;
    }

    async fn reply_permission_if_pending(&self, id: &Value, options: &[Value], want: Want) {
        self.reply_permission(id, options, want).await;
    }

    /// Timeout path: only reject if the pending generation still matches.
    async fn reply_permission_if_pending_gen(
        &self,
        id: &Value,
        options: &[Value],
        want: Want,
        gen: u64,
    ) {
        {
            let mut g = self.inner.lock().await;
            let Some(inner) = g.as_mut() else {
                return;
            };
            let key = Self::permission_id_key(id);
            match inner.pending_permission_ids.get(&key) {
                Some(g) if *g == gen => {}
                _ => return, // answered, or a newer request reused this id
            }
        }
        self.reply_permission(id, options, want).await;
    }

    /// Record always decision on a session after user picks Always.
    pub async fn remember_always(&self, session_id: &str, intent_key: &str, want: Want) {
        if let Some(inner) = self.inner.lock().await.as_mut() {
            if let Some(route) = inner.sessions.get_mut(session_id) {
                route.always.set(intent_key.to_string(), want);
            }
        }
    }

    /// For tests: auto-allow every permission on this session.
    pub async fn set_auto_want(&self, session_id: &str, want: Option<Want>) {
        if let Some(inner) = self.inner.lock().await.as_mut() {
            if let Some(route) = inner.sessions.get_mut(session_id) {
                route.auto_want = want;
            }
        }
    }

    async fn write_raw(&self, line: String) -> anyhow::Result<()> {
        let g = self.inner.lock().await;
        let inner = g
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ACP not connected"))?;
        inner
            .write_tx
            .send((line.into_bytes(), None))
            .map_err(|_| anyhow::anyhow!("ACP writer closed"))?;
        Ok(())
    }

    /// Short-timeout request (handshake, session lifecycle). Not for prompt.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request_timeout(method, params, Duration::from_secs(60))
            .await
    }

    /// Long-lived request for `session/prompt` — cancel is the abort path.
    pub async fn request_long(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        // 24h ceiling so a totally wedged agent still frees the oneshot eventually.
        self.request_timeout(method, params, Duration::from_secs(86_400))
            .await
    }

    async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        reply_budget: Duration,
    ) -> anyhow::Result<Value> {
        let (id, rx, flushed) = {
            let mut g = self.inner.lock().await;
            let inner = g
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("ACP not connected"))?;
            let id = inner.next_id;
            inner.next_id += 1;
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(id, tx);
            let (flush_tx, flush_rx) = oneshot::channel();
            let line = encode_request(id, method, params);
            inner
                .write_tx
                .send((line.into_bytes(), Some(flush_tx)))
                .map_err(|_| anyhow::anyhow!("ACP writer closed"))?;
            (id, rx, flush_rx)
        };
        match tokio::time::timeout(Duration::from_secs(60), flushed).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => anyhow::bail!("ACP {method}: writer closed before flush"),
            Err(_) => {
                self.fail_pending_and_reap("ACP stdin flush stalled").await;
                anyhow::bail!("ACP {method}: stdin flush stalled");
            }
        }
        match tokio::time::timeout(reply_budget, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => anyhow::bail!("ACP {method}: {e}"),
            Ok(Err(_)) => anyhow::bail!("ACP {method}: reply dropped"),
            Err(_) => {
                if let Some(inner) = self.inner.lock().await.as_mut() {
                    inner.pending.remove(&id);
                }
                anyhow::bail!("ACP {method}: timed out")
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> anyhow::Result<()> {
        let line = encode_notification(method, params);
        self.write_raw(line).await
    }

    fn paint_mcp(backend_id: &str, mcp: Vec<McpServerSpec>) -> Vec<Value> {
        match backend_for(backend_id) {
            Some(b) => b.paint_mcp_servers(mcp),
            None => mcp
                .into_iter()
                .map(|s| {
                    json!({
                        "type": "http",
                        "name": s.name,
                        "url": s.url,
                    })
                })
                .collect(),
        }
    }

    pub async fn is_alive(&self) -> bool {
        self.inner.lock().await.is_some()
    }

    pub async fn is_subscribed(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .await
            .as_ref()
            .is_some_and(|i| i.sessions.contains_key(session_id))
    }

    pub async fn subscribe(
        &self,
        session_id: &str,
        tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<()> {
        let mut g = self.inner.lock().await;
        let inner = g
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ACP not connected"))?;
        inner.opening_sessions.remove(session_id);
        let buffered = inner
            .pending_updates
            .remove(session_id)
            .unwrap_or_default();
        inner.sessions.insert(
            session_id.to_string(),
            SessionRoute {
                events: tx,
                always: AlwaysCache::new(),
                auto_want: None,
            },
        );
        if let Some(route) = inner.sessions.get(session_id) {
            for ev in buffered {
                let _ = route.events.send(ev);
            }
        }
        Ok(())
    }

    pub async fn unsubscribe(&self, session_id: &str) {
        if let Some(inner) = self.inner.lock().await.as_mut() {
            inner.sessions.remove(session_id);
            inner.opening_sessions.remove(session_id);
            inner.pending_updates.remove(session_id);
        }
    }

    async fn mark_opening(&self, session_id: &str) {
        if let Some(inner) = self.inner.lock().await.as_mut() {
            inner.opening_sessions.insert(session_id.to_string());
        }
    }

    async fn set_expecting_session(&self, on: bool) {
        if let Some(inner) = self.inner.lock().await.as_mut() {
            inner.expecting_session = on;
        }
    }

    /// Enqueue a session-scoped event (used for DrainBarrier). Returns false if
    /// the route is gone.
    pub async fn send_session_event(&self, session_id: &str, ev: SessionEvent) -> bool {
        let g = self.inner.lock().await;
        g.as_ref()
            .and_then(|inner| inner.sessions.get(session_id))
            .map(|route| route.events.send(ev).is_ok())
            .unwrap_or(false)
    }

    pub async fn new_session(
        &self,
        cwd: &Path,
        mcp: Vec<McpServerSpec>,
    ) -> anyhow::Result<SessionOpen> {
        let mcp_v = Self::paint_mcp(self.backend_id, mcp);
        self.set_expecting_session(true).await;
        let result = self
            .request(
                "session/new",
                json!({
                    "cwd": path_str(cwd),
                    "mcpServers": mcp_v,
                }),
            )
            .await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                self.set_expecting_session(false).await;
                return Err(e);
            }
        };
        let session_id = match result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .map(str::to_string)
        {
            Some(id) => id,
            None => {
                self.set_expecting_session(false).await;
                return Err(anyhow::anyhow!("session/new missing sessionId"));
            }
        };
        self.mark_opening(&session_id).await;
        self.set_expecting_session(false).await;
        Ok(SessionOpen {
            session_id,
            model: config_option_current(&result, "model"),
            thinking: config_option_current(&result, "thinking")
                .or_else(|| config_option_current(&result, "reasoning")),
        })
    }

    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp: Vec<McpServerSpec>,
    ) -> anyhow::Result<SessionOpen> {
        let mcp_v = Self::paint_mcp(self.backend_id, mcp);
        self.set_expecting_session(true).await;
        // Pre-mark the known id so updates for this sid buffer immediately.
        self.mark_opening(session_id).await;
        let result = self
            .request(
                "session/resume",
                json!({
                    "sessionId": session_id,
                    "cwd": path_str(cwd),
                    "mcpServers": mcp_v,
                }),
            )
            .await;
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                self.set_expecting_session(false).await;
                return Err(e);
            }
        };
        let sid = result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .unwrap_or(session_id)
            .to_string();
        if sid != session_id {
            self.mark_opening(&sid).await;
        }
        self.set_expecting_session(false).await;
        Ok(SessionOpen {
            session_id: sid,
            model: config_option_current(&result, "model"),
            thinking: config_option_current(&result, "thinking")
                .or_else(|| config_option_current(&result, "reasoning")),
        })
    }

    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp: Vec<McpServerSpec>,
    ) -> anyhow::Result<String> {
        let mcp_v = Self::paint_mcp(self.backend_id, mcp);
        let result = self
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": path_str(cwd),
                    "mcpServers": mcp_v,
                }),
            )
            .await?;
        let sid = result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .unwrap_or(session_id)
            .to_string();
        self.mark_opening(&sid).await;
        Ok(sid)
    }

    pub async fn fork_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp: Vec<McpServerSpec>,
    ) -> anyhow::Result<String> {
        let mcp_v = Self::paint_mcp(self.backend_id, mcp);
        let result = self
            .request(
                "session/fork",
                json!({
                    "sessionId": session_id,
                    "cwd": path_str(cwd),
                    "mcpServers": mcp_v,
                }),
            )
            .await?;
        result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("session/fork missing sessionId"))
    }

    pub async fn close_session(&self, session_id: &str) -> anyhow::Result<()> {
        let _ = self
            .request("session/close", json!({ "sessionId": session_id }))
            .await;
        self.unsubscribe(session_id).await;
        Ok(())
    }

    pub async fn cancel(&self, session_id: &str) -> anyhow::Result<()> {
        self.notify("session/cancel", Some(json!({ "sessionId": session_id })))
            .await
    }

    pub async fn prompt(&self, session_id: &str, text: &str) -> anyhow::Result<PromptOutcome> {
        let result = self
            .request_long(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await?;
        let stop = result
            .get("stopReason")
            .and_then(|s| s.as_str())
            .unwrap_or("end_turn")
            .to_string();
        let usage = result.get("usage").map(|u| UsageBits {
            input_tokens: u.get("inputTokens").and_then(|v| v.as_u64()),
            output_tokens: u.get("outputTokens").and_then(|v| v.as_u64()),
            total_tokens: u.get("totalTokens").and_then(|v| v.as_u64()),
            cached_read_tokens: u.get("cachedReadTokens").and_then(|v| v.as_u64()),
        });
        Ok(PromptOutcome {
            is_error: stop_reason_is_error(&stop),
            cancelled: stop_reason_is_cancelled(&stop),
            stop_reason: stop,
            usage,
        })
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl Want {
    fn kind_str_fallback(self) -> &'static str {
        match self {
            Want::AllowOnce => "allow_once",
            Want::AllowAlways => "allow_always",
            Want::RejectOnce => "reject_once",
            Want::RejectAlways => "reject_always",
        }
    }
}

// Silence unused PathBuf import if only Path is used via refs in signatures.
#[allow(dead_code)]
fn _pathbuf_ty(_: PathBuf) {}

#[cfg(test)]
mod tests {

    #[test]
    fn config_options_from_session_new_fixture() {
        let result = serde_json::json!({
            "sessionId": "s-1",
            "configOptions": [
                {"id": "model", "currentValue": "gpt-5"},
                {"id": "thinking", "currentValue": "high"}
            ]
        });
        assert_eq!(config_option_current(&result, "model").as_deref(), Some("gpt-5"));
        assert_eq!(config_option_current(&result, "thinking").as_deref(), Some("high"));
        assert_eq!(config_option_current(&result, "missing"), None);
    }

    use super::*;

    #[tokio::test]
    #[ignore = "requires omp on PATH"]
    async fn omp_acp_pong_live() {
        let c = client("omp", "omp").await.expect("client");
        let cwd = std::env::temp_dir();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let open = c.new_session(&cwd, vec![]).await.expect("new");
        let sid = open.session_id;
        // OMP returns model via configOptions on session/new.
        let _ = open.model;
        c.subscribe(&sid, tx).await.unwrap();
        c.set_auto_want(&sid, Some(Want::AllowOnce)).await;
        let outcome = c
            .prompt(&sid, "Reply with exactly: pong. Do not use tools.")
            .await
            .expect("prompt");
        assert_eq!(outcome.stop_reason, "end_turn");
        // Drain some events
        let mut saw_text = false;
        while let Ok(ev) = rx.try_recv() {
            if let SessionEvent::Chat(ChatEvent::TextDelta { text, .. }) = ev {
                if text.contains("pong") {
                    saw_text = true;
                }
            }
        }
        let _ = saw_text;
        let _ = c.cancel(&sid).await;
        shutdown("omp").await;
    }
}
