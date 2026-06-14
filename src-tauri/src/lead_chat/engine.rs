//! The chat engine: each timeline (lead = `-thread_id`, chat-mode worker =
//! `session_id`) runs through the selected tool stored on the thread/session.
//! Claude keeps a long-lived stream-json process; codex/opencode spawn one
//! process per turn. stdout is parsed (proto.rs), persisted (lead_message), and
//! pushed to the frontend over the `lead-chat` Tauri event. Interrupt rides the
//! tool protocol when available, with a kill fallback; a dead process resumes
//! via the stored native session id on the next send.

use crate::store::{repo, Db};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

pub const EVENT: &str = "lead-chat";

/// Persist the turn-activity status for whichever surface this engine drives:
/// a worker session row (`Some`) or the lead's per-thread meta row (`None`).
async fn persist_activity(db: &Db, session_id: Option<i32>, thread_id: i32, status: &str) {
    match session_id {
        Some(sid) => {
            let _ = repo::set_session_status(db, sid, status).await;
        }
        None => {
            let _ = repo::set_lead_status(db, thread_id, status).await;
        }
    }
}

/// 流式节流间隔（ms）：每过这么久把当前累积文本落一次 DB 快照，并向 IM 桥发一帧
/// LeadDelta（飞书 CardKit 流式卡据此逐帧更新）。桌面 UI 不受影响——它吃的是每个
/// token 的原始 `Push::Delta`。150ms 是流式卡看着流畅的下限；再大就一顿一顿的。
const STREAM_THROTTLE_MS: u128 = 150;

/// Incremental pushes to the frontend. snake_case-tagged to match the TS side.
#[derive(Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Push {
    Message {
        thread_id: i32,
        message: crate::store::entities::lead_message::Model,
    },
    Delta {
        thread_id: i32,
        message_id: i32,
        text: String,
    },
    Finalize {
        thread_id: i32,
        message_id: i32,
        status: String,
    },
    Turn {
        thread_id: i32,
        /// Some(session) for chat-mode workers; None for the lead.
        session_id: Option<i32>,
        state: String,
        queued: usize,
    },
    Init {
        thread_id: i32,
        session_id: Option<i32>,
        native_id: String,
        slash_commands: Vec<super::proto::SlashCmd>,
    },
    /// The tool call currently executing — transient: rendered while it runs,
    /// replaced by the next one, cleared by the Turn event. Never persisted.
    Activity {
        thread_id: i32,
        session_id: Option<i32>,
        name: String,
        summary: String,
    },
}

/// One outbound human message: text plus optional image attachments
/// (media_type, base64). Queued whole while a turn is running.
#[derive(Clone, Default)]
pub struct Outgoing {
    pub text: String,
    pub images: Vec<(String, String)>,
    /// true = backed by a queued timeline row (flips to complete on flush);
    /// false = invisible plumbing (coordinator nudges).
    pub tracked: bool,
    /// Opaque per-turn reply target carried from the caller (IM bridge) onto this
    /// turn's output frames. None for every non-IM send. Rides the queue so a
    /// queued turn keeps its own tag even when emitted after later sends.
    pub origin_tag: Option<String>,
}

/// Busy/queue bookkeeping for one engine. Mirrors the TUI's own semantics:
/// input during a turn is queued whole and delivered in order once the turn
/// ends — never silently dropped, never interleaved mid-turn. Pure — tested.
#[derive(Default)]
pub struct TurnState {
    pub busy: bool,
    pub queue: VecDeque<Outgoing>,
}

impl TurnState {
    /// true = caller may write to stdin now; false = caller must enqueue.
    pub fn try_begin_send(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        true
    }

    /// Turn finished: pop the next queued message (stays busy) or go idle.
    pub fn on_turn_end(&mut self) -> Option<Outgoing> {
        match self.queue.pop_front() {
            Some(next) => Some(next),
            None => {
                self.busy = false;
                None
            }
        }
    }
}

/// Per-turn dialects (codex `exec --json`, opencode `run --format json`) spawn
/// one process per human turn; only claude keeps a long-lived stream process.
pub fn per_turn(tool: &str) -> bool {
    crate::adapters::adapter_for(tool).is_some_and(|a| a.per_turn())
}

/// Watchdog clocks for the in-flight turn (§7 跑飞护栏). An idle engine burns
/// nothing, so only busy turns are clocked.
pub struct TurnClock {
    /// Wall-clock start of the in-flight turn; None while idle.
    pub started: Option<std::time::Instant>,
    /// Last stdout line seen from the child (any event counts as activity).
    pub last_activity: std::time::Instant,
}

impl Default for TurnClock {
    fn default() -> Self {
        Self {
            started: None,
            last_activity: std::time::Instant::now(),
        }
    }
}

impl TurnClock {
    pub(crate) fn begin_turn(&mut self) {
        self.started = Some(std::time::Instant::now());
        self.last_activity = std::time::Instant::now();
    }
    /// Re-sync with the queue state after a turn ends (queued pop = new turn).
    fn on_turn_end(&mut self, still_busy: bool) {
        if still_busy {
            self.begin_turn();
        } else {
            self.started = None;
        }
    }
}

pub struct EngineInner {
    pub thread_id: i32,
    /// claude | codex | opencode — selects the wire dialect + process model.
    pub tool: String,
    /// Chat-mode worker session; None for the lead.
    pub session_id: Option<i32>,
    pub cwd: std::path::PathBuf,
    /// Ask-hook + MCP injection args, appended to every spawn.
    pub extra_args: Vec<String>,
    pub system_prompt: String,
    pub native_id: Option<String>,
    pub slash_commands: Vec<super::proto::SlashCmd>,
    pub turn: TurnState,
    pub turn_id: i32,
    /// Ask-bridge identity for suppressing the idle watchdog while the agent is
    /// legitimately blocked on a human: a direction id for workers, "lead" for
    /// the lead.
    pub ask_dir: String,
    /// Runaway-guard clocks for the in-flight turn.
    pub clock: TurnClock,
    pub child: Option<Child>,
    pub stdin: Option<ChildStdin>,
    /// Streaming assistant row being built: (row id, accumulated text, last DB flush).
    pub current: Option<(i32, String, std::time::Instant)>,
    /// Set while a protocol interrupt is in flight so the closing row/status
    /// reads `interrupted` instead of `error`.
    pub interrupting: bool,
    /// Bumped per spawn; stale reader tasks compare and exit.
    pub generation: u64,
    /// Set on idle when skills changed; the next send silently restarts the
    /// resident process so it picks up newly-injected skills. UI never sees it.
    pub pending_skill_refresh: bool,
    /// Opaque tag of the turn whose output is currently being emitted. Set at
    /// every turn-start (including None turns) so a prior concierge reply target
    /// never leaks into a later non-IM turn. Stamped onto each emitted frame.
    pub current_origin_tag: Option<String>,
}

pub type EngineRef = Arc<tokio::sync::Mutex<EngineInner>>;

/// All live chat engines, keyed by `-thread_id` (lead) or `session_id` (worker).
#[derive(Default)]
pub struct LeadChatState(pub std::sync::Mutex<HashMap<i64, EngineRef>>);

impl LeadChatState {
    pub fn get(&self, key: i64) -> Option<EngineRef> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    /// Atomic get-or-insert: concurrent constructors (e.g. React StrictMode's
    /// double-mount firing two ensures) must converge on ONE engine — a lost
    /// race would orphan a duplicate headless process writing the same session.
    pub fn get_or_insert(&self, key: i64, eng: EngineRef) -> EngineRef {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert(eng)
            .clone()
    }
}

fn build_args(inner: &EngineInner) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-p".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--include-partial-messages".into(),
        "--verbose".into(),
    ];
    if !inner.system_prompt.is_empty() {
        a.push("--append-system-prompt".into());
        a.push(inner.system_prompt.clone());
    }
    if let Some(id) = &inner.native_id {
        a.push("--resume".into());
        a.push(id.clone());
    }
    a.extend(inner.extra_args.iter().cloned());
    a
}

fn merge_init_slash_commands(
    existing: &[super::proto::SlashCmd],
    init: Vec<super::proto::SlashCmd>,
) -> Vec<super::proto::SlashCmd> {
    if init.is_empty() {
        return existing.to_vec();
    }
    if existing.is_empty() {
        return init;
    }

    let by_name: HashMap<&str, &super::proto::SlashCmd> =
        existing.iter().map(|c| (c.name.as_str(), c)).collect();
    init.into_iter()
        .map(|mut incoming| {
            if let Some(old) = by_name.get(incoming.name.as_str()) {
                if incoming
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
                {
                    incoming.description = old.description.clone();
                }
                if incoming.arg_hint.as_deref().unwrap_or_default().is_empty() {
                    incoming.arg_hint = old.arg_hint.clone();
                }
            }
            incoming
        })
        .collect()
}

/// Spawn the process if it isn't alive (fresh or `--resume`), wiring the reader.
/// Per-turn dialects have no resident process — sending spawns one per turn.
pub async fn ensure_running(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    if per_turn(&inner.tool) {
        return Ok(());
    }
    if inner.tool != "claude" {
        anyhow::bail!("unknown lead tool {}", inner.tool);
    }
    if let Some(c) = inner.child.as_mut() {
        if c.try_wait().ok().flatten().is_none() {
            return Ok(()); // alive
        }
    }
    crate::claude::ensure_trusted(&inner.cwd);
    let mut child = Command::new("claude")
        .args(build_args(&inner))
        .current_dir(&inner.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    inner.stdin = child.stdin.take();
    // Ask for the command list NOW: the init system message only ships with the
    // first user turn, so the palette would stay empty until the human speaks.
    if let Some(stdin) = inner.stdin.as_mut() {
        let req = serde_json::json!({
            "type": "control_request",
            "request_id": "weft-initialize",
            "request": { "subtype": "initialize" }
        });
        let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
        let _ = stdin.flush().await;
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout not piped"))?;
    inner.child = Some(child);
    inner.generation += 1;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    inner.current = None;
    inner.interrupting = false;
    let generation = inner.generation;
    drop(inner);
    spawn_reader(app.clone(), db.clone(), eng.clone(), stdout, generation);
    Ok(())
}

pub(crate) async fn write_user(inner: &mut EngineInner, out: &Outgoing) {
    if let Some(stdin) = inner.stdin.as_mut() {
        let mut content = vec![serde_json::json!({ "type": "text", "text": out.text })];
        for (media_type, data) in &out.images {
            content.push(serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data }
            }));
        }
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": content }
        });
        let _ = stdin.write_all(format!("{msg}\n").as_bytes()).await;
        let _ = stdin.flush().await;
    }
}

/// Send a human message: optimistic-persist + either write through or queue.
/// `images` ride the outbound message as base64 blocks; `files` are appended
/// as plain paths (the agent reads them with its own tools).
pub async fn send(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    text: &str,
    images: Vec<(String, String)>,
    files: Vec<String>,
    origin_tag: Option<String>,
) -> anyhow::Result<()> {
    // Skill-refresh: a flag set on idle means newly-injected skills are waiting.
    // Silently bounce the resident process so the relaunch (resume) reads them.
    // Invisible: no "stopped" emit; UI goes straight idle→busy on this send.
    let pending = { eng.lock().await.pending_skill_refresh };
    if pending {
        stop_quiet(eng).await;
        eng.lock().await.pending_skill_refresh = false;
    }
    ensure_running(app, db, eng).await?;
    let mut inner = eng.lock().await;
    let thread_id = inner.thread_id;
    let sid = inner.session_id;
    let is_command = text.trim_start().starts_with('/');
    let kind = if is_command { "command" } else { "text" };
    let direct = inner.turn.try_begin_send();
    if direct {
        inner.turn_id += 1;
        inner.clock.begin_turn();
        // This send starts a turn now → its tag IS the in-flight turn's tag.
        inner.current_origin_tag = origin_tag.clone();
        crate::power::on_turn_began(app);
        persist_activity(db, inner.session_id, inner.thread_id, "running").await;
    }
    let turn = inner.turn_id;
    let status = if direct { "complete" } else { "queued" };
    let image_uris: Vec<String> = images
        .iter()
        .map(|(mt, data)| format!("data:{mt};base64,{data}"))
        .collect();
    let content = if is_command {
        let trimmed = text.trim_start();
        let mut it = trimmed.splitn(2, ' ');
        serde_json::json!({
            "command": it.next().unwrap_or_default(),
            "args": it.next().unwrap_or_default(),
        })
        .to_string()
    } else {
        serde_json::json!({ "text": text, "images": image_uris, "files": files }).to_string()
    };
    let m =
        repo::insert_lead_message(db, thread_id, sid, turn, "user", kind, &content, status).await?;
    let row_id = m.id;
    let _ = app.emit(
        EVENT,
        Push::Message {
            thread_id,
            message: m,
        },
    );
    let mut outbound = text.to_string();
    if !files.is_empty() {
        outbound.push_str("\n\nAttached files (read them as needed):\n");
        for f in &files {
            outbound.push_str(&format!("- {f}\n"));
        }
    }
    // Per-turn dialects take no inline image blocks: spill pasted images to
    // temp files and hand over paths — every agent can read those itself.
    let images = if per_turn(&inner.tool) && !images.is_empty() {
        use base64::Engine as _;
        let dir = std::env::temp_dir().join("weft-attachments");
        let _ = std::fs::create_dir_all(&dir);
        outbound.push_str("\n\nAttached images (read them as needed):\n");
        for (i, (mt, data)) in images.iter().enumerate() {
            let ext = mt.rsplit('/').next().unwrap_or("png");
            let p = dir.join(format!("msg{row_id}-{i}.{ext}"));
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                if std::fs::write(&p, bytes).is_ok() {
                    outbound.push_str(&format!("- {}\n", p.display()));
                }
            }
        }
        vec![]
    } else {
        images
    };
    let out = Outgoing {
        text: outbound,
        images,
        tracked: true,
        // Rides the turn (and the queue, if queued) so output frames recover it.
        origin_tag: origin_tag.clone(),
    };
    // codex (app-server): no resident stdin, no per-turn process — the shared
    // connection drives the turn after the lock drops. Gated; default stays exec.
    //
    // BUT the app-server path (spawn_codex_turn) injects neither extra_args (the
    // per-thread MCP, e.g. weft_global) nor system_prompt — only exec does. So any
    // thread that needs them (the Concierge: non-empty system_prompt + weft_global
    // MCP) must take the exec path, or it answers with no Weft tools and no prompt.
    let is_codex_appserver =
        inner.tool == "codex" && codex_appserver_enabled() && inner.system_prompt.is_empty();
    let spawn_now = direct && per_turn(&inner.tool) && !is_codex_appserver;
    if direct && !spawn_now && !is_codex_appserver {
        write_user(&mut inner, &out).await;
    } else if !direct {
        inner.turn.queue.push_back(out.clone());
    }
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id: sid,
            state: if inner.turn.busy { "busy" } else { "idle" }.into(),
            queued: inner.turn.queue.len(),
        },
    );
    drop(inner);
    if spawn_now {
        spawn_turn(app.clone(), db.clone(), eng.clone(), out).await?;
    } else if direct && is_codex_appserver {
        // Fall back to exec if the app-server can't be reached (old codex, crash):
        // the thread id is shared with exec's rollout store, so resume is seamless.
        if let Err(e) = spawn_codex_turn(app.clone(), db.clone(), eng.clone(), out.clone()).await {
            eprintln!("[weft][codex] app-server unavailable ({e}) — falling back to exec");
            spawn_turn(app.clone(), db.clone(), eng.clone(), out).await?;
        }
    }
    Ok(())
}

/// Codex app-server transport selector. Default = exec; app-server is opt-in via
/// `WEFT_CODEX_APPSERVER=1` because its `initialized` handshake may initialize
/// ChatGPT Apps and fail on network/auth state before a normal Weft turn starts.
fn codex_appserver_enabled() -> bool {
    crate::adapters::codex_prefers_appserver()
}

/// Drive a codex turn over the shared, multiplexed `codex app-server` connection
/// (gated by [`codex_appserver_enabled`]). Resolves/creates the thread (id ==
/// native session id), ensures one long-lived [`codex_consumer`] per session,
/// then starts the turn. Streaming + finalize + queue-flush live in the consumer.
async fn spawn_codex_turn(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    out: Outgoing,
) -> anyhow::Result<()> {
    let client = crate::codex_app_server::client().await?;
    let (native, cwd, sid, thread_id_i) = {
        let i = eng.lock().await;
        (
            i.native_id.clone(),
            i.cwd.to_string_lossy().into_owned(),
            i.session_id,
            i.thread_id,
        )
    };
    let had_native = native.is_some();
    let thread = match native {
        Some(t) => t,
        None => {
            let t = client.start_thread(&cwd).await?;
            eng.lock().await.native_id = Some(t.clone());
            if let Some(sid) = sid {
                let _ = repo::set_session_native_id(&db, sid, &t).await;
            } else {
                let _ = repo::set_lead_native_id(&db, thread_id_i, &t).await;
            }
            t
        }
    };
    if !client.is_subscribed(&thread).await {
        // First attach this process: a pre-existing thread is resumed so the
        // app-server re-loads its rollout; a just-started one is already loaded.
        if had_native {
            let _ = client.resume_thread(&thread).await;
        }
        let rx = client.subscribe(&thread).await;
        let (a, d, e, c, th) = (
            app.clone(),
            db.clone(),
            eng.clone(),
            client.clone(),
            thread.clone(),
        );
        tauri::async_runtime::spawn(async move { codex_consumer(a, d, e, c, th, rx).await });
    }
    let turn = client.start_turn(&thread, &out.text).await?;
    client.set_active_turn(&thread, &turn).await;
    Ok(())
}

/// One long-lived task per codex session: consume the thread's app-server
/// stream, driving the SAME timeline-row / Push pipeline the stdout reader uses,
/// and flushing the queue on turn end. Mirrors [`spawn_reader`]'s event handling.
async fn codex_consumer(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    client: crate::codex_app_server::Client,
    thread: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::codex_app_server::ThreadMsg>,
) {
    use super::proto::ChatEvent;
    use crate::codex_app_server::ThreadMsg;
    while let Some(msg) = rx.recv().await {
        match msg {
            ThreadMsg::Event(ChatEvent::TextDelta { text }) => {
                let mut inner = eng.lock().await;
                inner.clock.last_activity = std::time::Instant::now();
                let thread_id = inner.thread_id;
                let (sid, turn) = (inner.session_id, inner.turn_id);
                let row = match &mut inner.current {
                    Some(c) => {
                        c.1.push_str(&text);
                        c.0
                    }
                    None => {
                        let Ok(m) = repo::insert_lead_message(
                            &db,
                            thread_id,
                            sid,
                            turn,
                            "assistant",
                            "text",
                            r#"{"text":""}"#,
                            "streaming",
                        )
                        .await
                        else {
                            continue;
                        };
                        let id = m.id;
                        inner.current = Some((id, text.clone(), std::time::Instant::now()));
                        let _ = app.emit(
                            EVENT,
                            Push::Message {
                                thread_id,
                                message: m,
                            },
                        );
                        id
                    }
                };
                // Read the in-flight turn's tag before borrowing inner.current mutably.
                let origin_tag = inner.current_origin_tag.clone();
                if let Some(c) = &mut inner.current {
                    if c.2.elapsed().as_millis() >= STREAM_THROTTLE_MS {
                        c.2 = std::time::Instant::now();
                        let content = serde_json::json!({ "text": c.1 }).to_string();
                        let _ = repo::update_lead_message(&db, row, &content, "streaming").await;
                        emit_lead_delta(&app, thread_id, row, &c.1, false, origin_tag);
                    }
                }
                let _ = app.emit(
                    EVENT,
                    Push::Delta {
                        thread_id,
                        message_id: row,
                        text,
                    },
                );
            }
            ThreadMsg::Event(ChatEvent::Assistant { texts: _, tools }) => {
                // Codex streams text via deltas; only non-text items arrive here,
                // as transient activity pills.
                let inner = eng.lock().await;
                let (thread_id, sid) = (inner.thread_id, inner.session_id);
                drop(inner);
                for (name, summary) in tools {
                    let _ = app.emit(
                        EVENT,
                        Push::Activity {
                            thread_id,
                            session_id: sid,
                            name,
                            summary,
                        },
                    );
                }
            }
            ThreadMsg::Event(ChatEvent::TurnEnd { is_error }) => {
                let mut inner = eng.lock().await;
                let thread_id = inner.thread_id;
                let status = if inner.interrupting {
                    "interrupted"
                } else if is_error {
                    "error"
                } else {
                    "complete"
                };
                inner.interrupting = false;
                if let Some((id, text, _)) = inner.current.take() {
                    let _ = repo::update_lead_message(
                        &db,
                        id,
                        &serde_json::json!({ "text": text }).to_string(),
                        status,
                    )
                    .await;
                    let _ = app.emit(
                        EVENT,
                        Push::Finalize {
                            thread_id,
                            message_id: id,
                            status: status.into(),
                        },
                    );
                    if status == "complete" {
                        emit_lead_out(&app, thread_id, id, &text, inner.current_origin_tag.clone());
                    }
                } else if let Ok(Some(m)) = insert_terminal_assistant_if_missing(
                    &db,
                    thread_id,
                    inner.session_id,
                    inner.turn_id,
                    status,
                )
                .await
                {
                    let _ = app.emit(
                        EVENT,
                        Push::Message {
                            thread_id,
                            message: m,
                        },
                    );
                }
                let next = inner.turn.on_turn_end();
                // Next turn's tag becomes the in-flight tag (None when going idle),
                // so the dequeued turn's output frames carry its own origin_tag.
                inner.current_origin_tag = next.as_ref().and_then(|n| n.origin_tag.clone());
                if let Some(n) = &next {
                    inner.turn_id += 1;
                    if n.tracked {
                        let _ = repo::complete_queued(&db, thread_id, &n.text).await;
                    }
                }
                let still_busy = inner.turn.busy;
                persist_activity(&db, inner.session_id, thread_id, if still_busy { "running" } else { "idle" }).await;
                inner.clock.on_turn_end(still_busy);
                let _ = app.emit(
                    EVENT,
                    Push::Turn {
                        thread_id,
                        session_id: inner.session_id,
                        state: if still_busy { "busy" } else { "idle" }.into(),
                        queued: inner.turn.queue.len(),
                    },
                );
                drop(inner);
                // Flush: start the next queued message as a fresh turn on this thread.
                if let Some(n) = next {
                    match client.start_turn(&thread, &n.text).await {
                        Ok(t) => client.set_active_turn(&thread, &t).await,
                        Err(e) => eprintln!("[weft][codex] flush next turn: {e}"),
                    }
                }
            }
            ThreadMsg::Event(_) => {}
            ThreadMsg::Approval { id, method, params } => {
                // Route the app-server's approval request through Weft's Ask Bridge
                // (the same Needs-you surface the exec path uses), then reply with
                // the v2 decision. accept = run, decline = deny but continue.
                let (thread_id, dir) = {
                    let i = eng.lock().await;
                    (i.thread_id, i.ask_dir.clone())
                };
                let (tool, summary) = if method.contains("commandExecution") {
                    (
                        "Bash",
                        params["command"]
                            .as_str()
                            .unwrap_or("(command)")
                            .to_string(),
                    )
                } else {
                    ("Edit", "apply file changes".to_string())
                };
                let detail = params["cwd"].as_str().unwrap_or_default().to_string();
                let registry = app.state::<crate::ask::AskRegistry>().inner().clone();
                let decision = match registry.auto_decision(thread_id, &dir, &summary) {
                    Some(d) => d, // dangerous mode / full access / always-allow
                    None => {
                        let (_aid, rx) = registry.request(thread_id, &dir, tool, &summary, &detail);
                        // Receiver dropped (timeout/cancel) → deny-but-continue (safe).
                        rx.await.unwrap_or(crate::ask::Decision::Deny)
                    }
                };
                let verdict = match decision {
                    crate::ask::Decision::Allow => "accept",
                    crate::ask::Decision::Deny => "decline",
                };
                let _ = client.reply_approval(&id, verdict).await;
            }
        }
    }
}

/// One per-turn process (codex/opencode): the message rides the argv, events
/// stream from stdout, EOF ends the turn (the reader then flushes the queue).
async fn spawn_turn(app: AppHandle, db: Db, eng: EngineRef, out: Outgoing) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    // Per-turn argv (incl. codex's message-on-argv and opencode's /cmd→--command
    // peel) is built by the tool's adapter; `prepare` does the folder-trust
    // pre-accept. Identical output to the former inline match.
    let adapter = crate::adapters::adapter_for(&inner.tool)
        .ok_or_else(|| anyhow::anyhow!("unknown per-turn lead tool {}", inner.tool))?;
    adapter.prepare(&inner.cwd);
    let (program, args) = adapter.build_argv(&crate::adapters::AdapterContext {
        cwd: &inner.cwd,
        system_prompt: &inner.system_prompt,
        extra_args: &inner.extra_args,
        native_id: inner.native_id.as_deref(),
        message: &out.text,
        slash_commands: &inner.slash_commands,
    })?;
    let mut child = Command::new(&program)
        .args(&args)
        .current_dir(&inner.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        // stderr → app log: a per-turn CLI that dies prints its reason there.
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout not piped"))?;
    inner.stdin = None;
    inner.child = Some(child);
    inner.generation += 1;
    inner.current = None;
    let generation = inner.generation;
    drop(inner);
    spawn_reader(app, db, eng, stdout, generation);
    Ok(())
}

/// Interrupt the current turn: protocol control_request first (verified live:
/// control_response + result{terminal_reason:aborted_streaming}); kill after 3s
/// as the hard fallback. Either way `--resume` recovers the session next send.
pub async fn interrupt(app: &AppHandle, eng: &EngineRef) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    if !inner.turn.busy {
        return Ok(());
    }
    inner.interrupting = true;
    // codex app-server: no child to kill — interrupt the in-flight turn over the
    // shared connection (turn/interrupt {threadId, turnId}); the consumer's
    // TurnEnd then finalizes the row as `interrupted`.
    if inner.tool == "codex" && codex_appserver_enabled() {
        let thread = inner.native_id.clone();
        drop(inner);
        if let (Some(thread), Ok(client)) = (thread, crate::codex_app_server::client().await) {
            if let Some(turn) = client.active_turn(&thread).await {
                let _ = client.interrupt(&thread, &turn).await;
            }
        }
        return Ok(());
    }
    // Process-tool interrupt by transport (via the adapter): per-turn dialects
    // (codex exec / opencode) kill the per-turn child; the claude resident gets a
    // protocol interrupt payload + the delayed kill below.
    let kind = crate::adapters::adapter_for(&inner.tool).map(|a| a.interrupt());
    if !matches!(kind, Some(crate::adapters::Interrupt::Protocol)) {
        if let Some(c) = inner.child.as_mut() {
            let _ = c.kill().await;
        }
        return Ok(());
    }
    let payload = crate::adapters::adapter_for(&inner.tool)
        .map(|a| a.interrupt_payload(inner.generation))
        .unwrap_or_default();
    if let Some(stdin) = inner.stdin.as_mut() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.flush().await;
    }
    let gen = inner.generation;
    drop(inner);
    let eng2 = eng.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let mut inner = eng2.lock().await;
        if inner.generation == gen && inner.turn.busy {
            if let Some(c) = inner.child.as_mut() {
                let _ = c.kill().await; // reader hits EOF and reports stopped
            }
        }
        drop(inner);
        let _ = &app2;
    });
    Ok(())
}

/// Invisible coordinator nudge: deliver plumbing text to the agent WITHOUT a
/// timeline row — bus wakes are infrastructure, not conversation. Busy engines
/// queue it (processed after the current turn, same as the TUI's queue).
pub async fn nudge(app: &AppHandle, db: &Db, eng: &EngineRef, text: &str) -> anyhow::Result<()> {
    ensure_running(app, db, eng).await?;
    let mut inner = eng.lock().await;
    let out = Outgoing {
        text: text.to_string(),
        images: vec![],
        tracked: false,
        origin_tag: None,
    };
    if inner.turn.try_begin_send() {
        inner.turn_id += 1;
        inner.clock.begin_turn();
        // Plumbing nudge starts a turn directly (not via send): keep the invariant.
        inner.current_origin_tag = None;
        crate::power::on_turn_began(app);
        // A nudge is a turn-start too (e.g. a coordinator bus-wake on an idle
        // engine): persist `running` like send, so a crash mid-nudge-turn is
        // revivable instead of reading a stale `idle`.
        persist_activity(db, inner.session_id, inner.thread_id, "running").await;
        if per_turn(&inner.tool) {
            drop(inner);
            spawn_turn(app.clone(), db.clone(), eng.clone(), out).await?;
        } else {
            write_user(&mut inner, &out).await;
        }
    } else {
        inner.turn.queue.push_back(out);
    }
    Ok(())
}

fn human_dur(secs: u64) -> String {
    if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}min", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

async fn insert_terminal_assistant_if_missing(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    turn_id: i32,
    status: &str,
) -> anyhow::Result<Option<crate::store::entities::lead_message::Model>> {
    let text = match status {
        "error" => "The agent turn ended with an error before producing output.",
        "interrupted" => "The agent turn was interrupted before producing output.",
        _ => return Ok(None),
    };
    let m = repo::insert_lead_message(
        db,
        thread_id,
        session_id,
        turn_id,
        "assistant",
        "text",
        &serde_json::json!({ "text": text }).to_string(),
        status,
    )
    .await?;
    Ok(Some(m))
}

/// Decide whether the in-flight turn should be force-stopped (§7 跑飞护栏).
/// `busy_secs` = None means the engine is idle → never touched (an idle engine
/// burns nothing). `has_open_ask` = the agent is legitimately blocked on the
/// human, so its silence is expected → never idle-kill. Wall-clock always
/// applies. Both gates require the turn to be at least cap-old, so a young
/// turn is never killed by a stale clock. Pure → unit-tested.
pub(crate) fn turn_verdict(
    busy_secs: Option<u64>,
    quiet_secs: u64,
    wall_cap: u64,
    idle_cap: u64,
    has_open_ask: bool,
) -> Option<String> {
    let busy = busy_secs?;
    if wall_cap > 0 && busy >= wall_cap {
        return Some(format!("the turn ran for over {}", human_dur(wall_cap)));
    }
    if idle_cap > 0 && !has_open_ask && busy >= idle_cap && quiet_secs >= idle_cap {
        return Some(format!("no activity for {}", human_dur(idle_cap)));
    }
    None
}

/// Runaway guard (§7 跑飞护栏): every 30s, sweep all live engines and force-stop
/// a turn that ran past the wall cap or went silent past the idle cap. The
/// stopped engine surfaces via Needs-you (bus ask) and resumes losslessly on
/// the next send (`--resume`). Caps come from GuardrailState (Settings / WEFT_*
/// env); 0 disables a cap.
pub fn spawn_watchdog(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let Some(guard) = app.try_state::<crate::commands::GuardrailState>() else {
                continue;
            };
            let (idle_cap, wall_cap) = guard.get();
            if idle_cap == 0 && wall_cap == 0 {
                continue;
            }
            let engines: Vec<EngineRef> = {
                let state = app.state::<LeadChatState>();
                let g = state.0.lock().unwrap_or_else(|e| e.into_inner());
                g.values().cloned().collect()
            };
            for eng in engines {
                let (verdict, thread_id, ask_dir) = {
                    let inner = eng.lock().await;
                    if !inner.turn.busy {
                        continue;
                    }
                    let busy = inner.clock.started.map(|t| t.elapsed().as_secs());
                    let quiet = inner.clock.last_activity.elapsed().as_secs();
                    let has_open_ask = app
                        .try_state::<crate::ask::AskRegistry>()
                        .map(|a| {
                            a.open().iter().any(|k| {
                                k.dir == inner.ask_dir
                                    || (inner.ask_dir == "lead" && k.dir.is_empty())
                            })
                        })
                        .unwrap_or(false);
                    (
                        turn_verdict(busy, quiet, wall_cap, idle_cap, has_open_ask),
                        inner.thread_id,
                        inner.ask_dir.clone(),
                    )
                };
                let Some(reason) = verdict else { continue };
                stop(&app, &eng).await;
                if let Some(bus) = app.try_state::<crate::bus::BusRegistry>() {
                    bus.ask_human(
                        thread_id,
                        &ask_dir,
                        &format!("⚠️ Agent auto-stopped by the runaway guard: {reason}. Review and resume if it was still needed."),
                    );
                }
            }
        }
    });
}

/// Kill the live child + reset turn state WITHOUT emitting a "stopped" event —
/// the UI keeps its last (idle) state. Used by the skill-refresh restart so the
/// bounce is invisible; `stop` wraps this and then emits "stopped".
pub async fn stop_quiet(eng: &EngineRef) {
    let mut inner = eng.lock().await;
    inner.generation += 1; // orphan the reader so EOF handling is ours
    if let Some(c) = inner.child.as_mut() {
        let _ = c.kill().await;
    }
    inner.child = None;
    inner.stdin = None;
    inner.current = None;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
}

/// Stop the engine outright (e.g. before a terminal takeover or by the runaway
/// guard). Persists idle so a stopped/taken-over session can't be falsely
/// revived into a COMPETING headless process on the next boot (see L204).
pub async fn stop(app: &AppHandle, eng: &EngineRef) {
    stop_quiet(eng).await;
    let inner = eng.lock().await;
    if let Some(db) = app.try_state::<Db>() {
        persist_activity(&db, inner.session_id, inner.thread_id, "idle").await;
    }
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id: inner.thread_id,
            session_id: inner.session_id,
            state: "stopped".into(),
            queued: 0,
        },
    );
}

fn spawn_reader(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    stdout: tokio::process::ChildStdout,
    generation: u64,
) {
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut saw_event = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let mut inner = eng.lock().await;
            if inner.generation != generation {
                return; // superseded by a respawn/stop
            }
            inner.clock.last_activity = std::time::Instant::now();
            let thread_id = inner.thread_id;
            // Per-turn dialects carry the native session id on their events.
            if inner.native_id.is_none() {
                if let Some(native) = crate::adapters::adapter_for(&inner.tool)
                    .and_then(|a| a.extract_native_id(&line))
                {
                    inner.native_id = Some(native.clone());
                    if let Some(sid) = inner.session_id {
                        let _ = repo::set_session_native_id(&db, sid, &native).await;
                    } else {
                        let _ = repo::set_lead_native_id(&db, thread_id, &native).await;
                    }
                    let _ = app.emit(
                        EVENT,
                        Push::Init {
                            thread_id,
                            session_id: inner.session_id,
                            native_id: native,
                            slash_commands: inner.slash_commands.clone(),
                        },
                    );
                }
            }
            let event = crate::adapters::adapter_for(&inner.tool)
                .map(|a| a.parse_line(&line))
                .unwrap_or(super::proto::ChatEvent::Other);
            if !matches!(event, super::proto::ChatEvent::Other) {
                saw_event = true;
            }
            match event {
                super::proto::ChatEvent::Init {
                    session_id,
                    slash_commands,
                } => {
                    inner.native_id = Some(session_id.clone());
                    let slash_commands =
                        merge_init_slash_commands(&inner.slash_commands, slash_commands);
                    inner.slash_commands = slash_commands.clone();
                    if let Some(sid) = inner.session_id {
                        let _ = repo::set_session_native_id(&db, sid, &session_id).await;
                    } else {
                        let _ = repo::set_lead_native_id(&db, thread_id, &session_id).await;
                    }
                    let _ = app.emit(
                        EVENT,
                        Push::Init {
                            thread_id,
                            session_id: inner.session_id,
                            native_id: session_id,
                            slash_commands,
                        },
                    );
                }
                super::proto::ChatEvent::Commands { commands } => {
                    let commands = merge_init_slash_commands(&inner.slash_commands, commands);
                    inner.slash_commands = commands.clone();
                    let _ = app.emit(
                        EVENT,
                        Push::Init {
                            thread_id,
                            session_id: inner.session_id,
                            native_id: inner.native_id.clone().unwrap_or_default(),
                            slash_commands: commands,
                        },
                    );
                }
                super::proto::ChatEvent::TextDelta { text } => {
                    let sid = inner.session_id;
                    let turn = inner.turn_id;
                    let row = match &mut inner.current {
                        Some(c) => {
                            c.1.push_str(&text);
                            c.0
                        }
                        None => {
                            let Ok(m) = repo::insert_lead_message(
                                &db,
                                thread_id,
                                sid,
                                turn,
                                "assistant",
                                "text",
                                r#"{"text":""}"#,
                                "streaming",
                            )
                            .await
                            else {
                                continue;
                            };
                            let id = m.id;
                            inner.current = Some((id, text.clone(), std::time::Instant::now()));
                            let _ = app.emit(
                                EVENT,
                                Push::Message {
                                    thread_id,
                                    message: m,
                                },
                            );
                            id
                        }
                    };
                    // Throttle DB snapshots + IM streaming frames; the live UI rides raw Delta events.
                    // Read the in-flight turn's tag before borrowing inner.current mutably.
                    let origin_tag = inner.current_origin_tag.clone();
                    if let Some(c) = &mut inner.current {
                        if c.2.elapsed().as_millis() >= STREAM_THROTTLE_MS {
                            c.2 = std::time::Instant::now();
                            let content = serde_json::json!({ "text": c.1 }).to_string();
                            let _ =
                                repo::update_lead_message(&db, row, &content, "streaming").await;
                            emit_lead_delta(&app, thread_id, row, &c.1, false, origin_tag);
                        }
                    }
                    let _ = app.emit(
                        EVENT,
                        Push::Delta {
                            thread_id,
                            message_id: row,
                            text,
                        },
                    );
                }
                super::proto::ChatEvent::Assistant { texts, tools } => {
                    // A finished text block: finalize the streaming row with the
                    // authoritative full text. Some turns have NO deltas at all —
                    // built-in slash commands reply via a synthetic assistant
                    // message — so a missing streaming row means insert, not drop.
                    if !texts.is_empty() {
                        // Sentinels are scanned across the joined body, so the
                        // join+extract order is load-bearing — a marker split
                        // across two text blocks would otherwise slip through.
                        let full = texts.join("\n\n");
                        // Fork <weft:*> sentinels out of the body before persisting:
                        // action_card lives as its own row so the UI can render the
                        // card without parsing prose; list_repos triggers a stdin
                        // reply (handled below) and produces no row of its own.
                        let (clean, sentinels) = super::sentinels::extract_sentinels(&full);
                        let content = serde_json::json!({ "text": clean }).to_string();
                        match inner.current.take() {
                            Some((id, _, _)) => {
                                let _ =
                                    repo::update_lead_message(&db, id, &content, "complete").await;
                                let _ = app.emit(
                                    EVENT,
                                    Push::Finalize {
                                        thread_id,
                                        message_id: id,
                                        status: "complete".into(),
                                    },
                                );
                                emit_lead_out(
                                    &app,
                                    thread_id,
                                    id,
                                    &clean,
                                    inner.current_origin_tag.clone(),
                                );
                            }
                            None => {
                                let (sid, turn) = (inner.session_id, inner.turn_id);
                                if let Ok(m) = repo::insert_lead_message(
                                    &db,
                                    thread_id,
                                    sid,
                                    turn,
                                    "assistant",
                                    "text",
                                    &content,
                                    "complete",
                                )
                                .await
                                {
                                    let mid = m.id;
                                    let _ = app.emit(
                                        EVENT,
                                        Push::Message {
                                            thread_id,
                                            message: m,
                                        },
                                    );
                                    emit_lead_out(
                                        &app,
                                        thread_id,
                                        mid,
                                        &clean,
                                        inner.current_origin_tag.clone(),
                                    );
                                }
                            }
                        }
                        // Persist / answer sentinels in encounter order. Errors are
                        // logged but never abort the reader — a malformed card must
                        // not wedge the chat stream.
                        for s in sentinels {
                            match s {
                                super::sentinels::Sentinel::ActionCard(json) => {
                                    // Reject anything that isn't a JSON object so the
                                    // UI can rely on `card.title / actions / …`.
                                    match serde_json::from_str::<serde_json::Value>(&json) {
                                        Ok(v) if v.is_object() => {
                                            let (sid, turn) = (inner.session_id, inner.turn_id);
                                            match repo::insert_lead_message(
                                                &db, thread_id, sid, turn,
                                                "assistant", "action_card", &json, "complete",
                                            )
                                            .await
                                            {
                                                Ok(m) => {
                                                    let _ = app.emit(EVENT, Push::Message {
                                                        thread_id, message: m,
                                                    });
                                                }
                                                Err(e) => eprintln!(
                                                    "[weft] lead sentinel: insert action_card failed: {e}"
                                                ),
                                            }
                                        }
                                        Ok(_) => eprintln!(
                                            "[weft] lead sentinel: action_card payload is not an object — dropped"
                                        ),
                                        Err(e) => eprintln!(
                                            "[weft] lead sentinel: action_card JSON parse failed: {e}"
                                        ),
                                    }
                                }
                                super::sentinels::Sentinel::ListRepos => {
                                    // Look up workspace via the thread row (engine
                                    // doesn't cache it; one extra query per call is
                                    // cheap and avoids a wider refactor).
                                    let ws_id = match repo::get_thread(&db, thread_id).await {
                                        Ok(Some(t)) => Some(t.workspace_id),
                                        Ok(None) => {
                                            eprintln!(
                                                "[weft] lead sentinel: list_repos — thread {thread_id} not found"
                                            );
                                            None
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "[weft] lead sentinel: list_repos — get_thread failed: {e}"
                                            );
                                            None
                                        }
                                    };
                                    if let Some(workspace_id) = ws_id {
                                        let repos = match repo::list_repos(&db, workspace_id).await
                                        {
                                            Ok(r) => r,
                                            Err(e) => {
                                                eprintln!(
                                                    "[weft] lead sentinel: list_repos query failed: {e}"
                                                );
                                                Vec::new()
                                            }
                                        };
                                        let payload = serde_json::json!({
                                            "repos": repos.iter().map(|r| serde_json::json!({
                                                "id": r.id,
                                                "name": r.name,
                                                "slug": r.slug,
                                                "local_git_path": r.local_git_path,
                                                "base_ref": r.base_ref,
                                            })).collect::<Vec<_>>()
                                        });
                                        let body = match serde_json::to_string(&payload) {
                                            Ok(s) => s,
                                            Err(e) => {
                                                eprintln!(
                                                    "[weft] lead sentinel: serialize list_repos_result failed: {e}"
                                                );
                                                continue;
                                            }
                                        };
                                        let reply = format!(
                                            "<weft:list_repos_result>{body}</weft:list_repos_result>"
                                        );
                                        // Invisible plumbing: tracked=false keeps this
                                        // off the timeline; the agent reads it as a
                                        // tool-result-style user turn.
                                        let out = Outgoing {
                                            text: reply,
                                            images: Vec::new(),
                                            tracked: false,
                                            origin_tag: None,
                                        };
                                        write_user(&mut inner, &out).await;
                                    }
                                }
                            }
                        }
                    }
                    // Tool calls are transient activity, not timeline rows:
                    // show the one currently running, gone when the turn moves on.
                    for (name, summary) in tools {
                        let _ = app.emit(
                            EVENT,
                            Push::Activity {
                                thread_id,
                                session_id: inner.session_id,
                                name,
                                summary,
                            },
                        );
                    }
                }
                super::proto::ChatEvent::TurnEnd { is_error } => {
                    let status = if inner.interrupting {
                        "interrupted"
                    } else if is_error {
                        "error"
                    } else {
                        "complete"
                    };
                    inner.interrupting = false;
                    if let Some((id, text, _)) = inner.current.take() {
                        let _ = repo::update_lead_message(
                            &db,
                            id,
                            &serde_json::json!({ "text": text }).to_string(),
                            status,
                        )
                        .await;
                        let _ = app.emit(
                            EVENT,
                            Push::Finalize {
                                thread_id,
                                message_id: id,
                                status: status.into(),
                            },
                        );
                        if status == "complete" {
                            emit_lead_out(&app, thread_id, id, &text, inner.current_origin_tag.clone());
                        }
                    } else if let Ok(Some(m)) = insert_terminal_assistant_if_missing(
                        &db,
                        thread_id,
                        inner.session_id,
                        inner.turn_id,
                        status,
                    )
                    .await
                    {
                        let _ = app.emit(
                            EVENT,
                            Push::Message {
                                thread_id,
                                message: m,
                            },
                        );
                    }
                    let next = inner.turn.on_turn_end();
                    // The next turn's tag becomes the in-flight tag (None when going
                    // idle), set BEFORE its input is dispatched so its frames carry it.
                    inner.current_origin_tag = next.as_ref().and_then(|n| n.origin_tag.clone());
                    if let Some(next) = next {
                        inner.turn_id += 1;
                        if next.tracked {
                            let _ = repo::complete_queued(&db, thread_id, &next.text).await;
                        }
                        if per_turn(&inner.tool) {
                            let (a, d, e) = (app.clone(), db.clone(), eng.clone());
                            tauri::async_runtime::spawn(async move {
                                let _ = spawn_turn(a, d, e, next).await;
                            });
                        } else {
                            write_user(&mut inner, &next).await;
                        }
                    }
                    let still_busy = inner.turn.busy;
                    persist_activity(&db, inner.session_id, thread_id, if still_busy { "running" } else { "idle" }).await;
                    inner.clock.on_turn_end(still_busy);
                    let state = if still_busy { "busy" } else { "idle" };
                    let _ = app.emit(
                        EVENT,
                        Push::Turn {
                            thread_id,
                            session_id: inner.session_id,
                            state: state.into(),
                            queued: inner.turn.queue.len(),
                        },
                    );
                }
                _ => {}
            }
        }
        // EOF. Per-turn dialects end every turn this way (clean exit); for the
        // long-lived claude process it means a crash/kill — history stays, the
        // next send resumes.
        let mut inner = eng.lock().await;
        if inner.generation == generation && per_turn(&inner.tool) {
            let status = if inner.interrupting {
                "interrupted"
            } else {
                "complete"
            };
            inner.interrupting = false;
            // A turn that produced ZERO events died on startup (auth, bad args,
            // session lock …) — surface it instead of completing silently.
            if !saw_event && status == "complete" {
                if let Ok(m) = repo::insert_lead_message(
                    &db,
                    inner.thread_id,
                    inner.session_id,
                    inner.turn_id,
                    "assistant",
                    "text",
                    r#"{"text":"(the agent process exited without producing any output — check the app log)"}"#,
                    "error",
                )
                .await
                {
                    let _ = app.emit(EVENT, Push::Message { thread_id: inner.thread_id, message: m });
                }
            }
            if let Some((id, text, _)) = inner.current.take() {
                let _ = repo::update_lead_message(
                    &db,
                    id,
                    &serde_json::json!({ "text": text }).to_string(),
                    status,
                )
                .await;
                let _ = app.emit(
                    EVENT,
                    Push::Finalize {
                        thread_id: inner.thread_id,
                        message_id: id,
                        status: status.into(),
                    },
                );
                // 仅 complete 才回流 IM——interrupted/error 的半截不应上桥。
                if status == "complete" {
                    emit_lead_out(&app, inner.thread_id, id, &text, inner.current_origin_tag.clone());
                }
            }
            inner.child = None;
            let next = inner.turn.on_turn_end();
            // Carry the dequeued turn's tag (None when going idle) onto its frames.
            inner.current_origin_tag = next.as_ref().and_then(|n| n.origin_tag.clone());
            if let Some(next) = next {
                inner.turn_id += 1;
                if next.tracked {
                    let _ = repo::complete_queued(&db, inner.thread_id, &next.text).await;
                }
                let (a, d, e) = (app.clone(), db.clone(), eng.clone());
                tauri::async_runtime::spawn(async move {
                    let _ = spawn_turn(a, d, e, next).await;
                });
            }
            let still_busy = inner.turn.busy;
            persist_activity(&db, inner.session_id, inner.thread_id, if still_busy { "running" } else { "idle" }).await;
            inner.clock.on_turn_end(still_busy);
            let state = if still_busy { "busy" } else { "idle" };
            let _ = app.emit(
                EVENT,
                Push::Turn {
                    thread_id: inner.thread_id,
                    session_id: inner.session_id,
                    state: state.into(),
                    queued: inner.turn.queue.len(),
                },
            );
            return;
        }
        if inner.generation == generation {
            // A row still streaming at death closes as interrupted/error.
            let status = if inner.interrupting {
                "interrupted"
            } else {
                "error"
            };
            inner.interrupting = false;
            if let Some((id, text, _)) = inner.current.take() {
                let _ = repo::update_lead_message(
                    &db,
                    id,
                    &serde_json::json!({ "text": text }).to_string(),
                    status,
                )
                .await;
                let _ = app.emit(
                    EVENT,
                    Push::Finalize {
                        thread_id: inner.thread_id,
                        message_id: id,
                        status: status.into(),
                    },
                );
            }
            inner.child = None;
            inner.stdin = None;
            inner.turn = TurnState::default();
            inner.clock = TurnClock::default();
            // The turn is unconditionally reset to idle here; persist that so a
            // resident-process death (incl. interrupt→kill) doesn't leave the row
            // stuck "running" and falsely revive an engine on the next boot.
            persist_activity(&db, inner.session_id, inner.thread_id, "idle").await;
            let _ = app.emit(
                EVENT,
                Push::Turn {
                    thread_id: inner.thread_id,
                    session_id: inner.session_id,
                    state: "stopped".into(),
                    queued: 0,
                },
            );
        }
    });
}

/// M2-4 tap: 把 assistant 段「complete」时的清洗文本广播给订阅者
/// （IM 桥据此回流到飞书话题）。`LeadOutHub` 未注册或无订阅都静默——
/// 单测/单进程跑的 `tauri::test::mock_app` 没注册该状态也不会 panic。
fn emit_lead_out(
    app: &AppHandle,
    thread_id: i32,
    message_id: i32,
    text: &str,
    origin_tag: Option<String>,
) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    if let Some(hub) = app.try_state::<super::out_hub::LeadOutHub>() {
        hub.emit(super::out_hub::LeadOut {
            thread_id,
            message_id,
            text: t.to_string(),
            origin_tag: origin_tag.clone(),
        });
    }
    // streaming 收尾：每个「段落完成」处同时发一帧 done（与 LeadOut 同源、同清洗后文本），
    // IM 桥据 done 定稿流式卡片。中间帧由两处 500ms 节流点发（见 emit_lead_delta）。
    emit_lead_delta(app, thread_id, message_id, t, true, origin_tag);
}

/// streaming 增量帧。`accumulated` 是到当前为止的全文；`done` 标记最后一帧。
/// 未注册 LeadDeltaHub（如 mock_app 测试）静默——不 panic。
fn emit_lead_delta(
    app: &AppHandle,
    thread_id: i32,
    message_id: i32,
    accumulated: &str,
    done: bool,
    origin_tag: Option<String>,
) {
    if let Some(hub) = app.try_state::<super::delta_hub::LeadDeltaHub>() {
        hub.emit(super::delta_hub::LeadDelta {
            thread_id,
            message_id,
            accumulated: accumulated.to_string(),
            done,
            origin_tag,
        });
    }
    // streaming 收尾：每个「段落完成」处同时发一帧 done（与 LeadOut 同源、同清洗后文本），
    // IM 桥据 done 定稿流式卡片。中间帧由两处 500ms 节流点发（见 emit_lead_delta）。
    emit_lead_delta(app, thread_id, message_id, t, true);
}

/// streaming 增量帧。`accumulated` 是到当前为止的全文；`done` 标记最后一帧。
/// 未注册 LeadDeltaHub（如 mock_app 测试）静默——不 panic。
fn emit_lead_delta(
    app: &AppHandle,
    thread_id: i32,
    message_id: i32,
    accumulated: &str,
    done: bool,
) {
    if let Some(hub) = app.try_state::<super::delta_hub::LeadDeltaHub>() {
        hub.emit(super::delta_hub::LeadDelta {
            thread_id,
            message_id,
            accumulated: accumulated.to_string(),
            done,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_machine() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy: send through
        assert!(!t.try_begin_send()); // busy: enqueue
        t.queue.push_back(Outgoing {
            text: "second".into(),
            images: vec![],
            tracked: true,
            origin_tag: None,
        });
        let next = t.on_turn_end();
        assert_eq!(next.map(|o| o.text).as_deref(), Some("second"));
        assert!(t.busy); // popped → still busy
        assert!(t.on_turn_end().is_none()); // empty queue → idle
        assert!(!t.busy);
    }

    #[test]
    fn wall_cap_fires_regardless_of_activity() {
        assert!(turn_verdict(Some(7200), 1, 7200, 1800, false)
            .unwrap()
            .contains("ran for over 2h"));
    }

    #[test]
    fn idle_fires_when_silent_and_not_waiting_on_human() {
        assert!(turn_verdict(Some(2000), 1900, 0, 1800, false)
            .unwrap()
            .contains("no activity for 30min"));
    }

    #[test]
    fn young_turn_never_idle_killed_even_with_stale_clock() {
        // quiet since before the turn began (stale/foreign clock): age gates it.
        assert_eq!(turn_verdict(Some(60), 99_999, 0, 1800, false), None);
    }

    #[test]
    fn idle_suppressed_while_waiting_on_human() {
        assert_eq!(turn_verdict(Some(2000), 1900, 0, 1800, true), None);
    }

    #[test]
    fn active_turn_is_kept() {
        assert_eq!(turn_verdict(Some(1000), 5, 7200, 1800, false), None);
    }

    #[test]
    fn idle_engine_never_touched() {
        assert_eq!(turn_verdict(None, 99_999, 60, 60, false), None);
    }

    #[test]
    fn zero_caps_disable_each_check() {
        assert_eq!(turn_verdict(Some(1_000_000), 1_000_000, 0, 0, false), None);
    }

    #[test]
    fn initialize_metadata_survives_later_bare_init_list() {
        let rich = vec![crate::lead_chat::proto::SlashCmd {
            name: "compact".into(),
            description: Some("Summarize context".into()),
            arg_hint: None,
        }];
        let bare = vec![crate::lead_chat::proto::SlashCmd::bare("compact")];

        let merged = merge_init_slash_commands(&rich, bare);

        assert_eq!(merged, rich);
    }

    #[test]
    fn initialize_merge_adds_new_dynamic_commands() {
        let existing = vec![crate::lead_chat::proto::SlashCmd {
            name: "compact".into(),
            description: Some("Summarize context".into()),
            arg_hint: None,
        }];
        let init = vec![
            crate::lead_chat::proto::SlashCmd::bare("compact"),
            crate::lead_chat::proto::SlashCmd {
                name: "superpowers:requesting-code-review".into(),
                description: Some("Review current work".into()),
                arg_hint: None,
            },
        ];

        let merged = merge_init_slash_commands(&existing, init);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], existing[0]);
        assert_eq!(merged[1].name, "superpowers:requesting-code-review");
    }

    #[tokio::test]
    async fn terminal_error_without_current_row_is_persisted() {
        let db = Db::connect("sqlite::memory:").await.unwrap();

        let m = insert_terminal_assistant_if_missing(&db, 7, None, 3, "error")
            .await
            .unwrap()
            .expect("error turn should create an assistant row");

        assert_eq!(m.thread_id, 7);
        assert_eq!(m.turn_id, 3);
        assert_eq!(m.role, "assistant");
        assert_eq!(m.kind, "text");
        assert_eq!(m.status, "error");
        assert!(m.content.contains("ended with an error"));
        let all = repo::list_lead_messages(&db, 7).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, m.id);

        let complete = insert_terminal_assistant_if_missing(&db, 7, None, 4, "complete")
            .await
            .unwrap();
        assert!(complete.is_none());
    }

    #[test]
    fn human_dur_formats() {
        assert_eq!(human_dur(7200), "2h");
        assert_eq!(human_dur(1800), "30min");
        assert_eq!(human_dur(45), "45s");
    }

    #[test]
    fn per_turn_only_accepts_known_per_turn_tools() {
        assert!(!per_turn("claude"));
        assert!(per_turn("codex"));
        assert!(per_turn("opencode"));
        assert!(!per_turn("mystery"));
    }

    #[test]
    fn turn_clock_follows_queue() {
        let mut c = TurnClock::default();
        assert!(c.started.is_none());
        c.begin_turn();
        assert!(c.started.is_some());
        c.on_turn_end(true); // queued message popped → new turn
        assert!(c.started.is_some());
        c.on_turn_end(false); // queue drained → idle
        assert!(c.started.is_none());
    }

    #[test]
    fn build_args_fresh_vs_resume() {
        let mut inner = EngineInner {
            thread_id: 1,
            tool: "claude".into(),
            session_id: None,
            cwd: "/tmp".into(),
            extra_args: vec!["--mcp-config".into(), "x".into()],
            system_prompt: "be lead".into(),
            native_id: None,
            slash_commands: vec![],
            turn: TurnState::default(),
            turn_id: 0,
            ask_dir: "lead".into(),
            clock: TurnClock::default(),
            child: None,
            stdin: None,
            current: None,
            interrupting: false,
            generation: 0,
            pending_skill_refresh: false,
            current_origin_tag: None,
        };
        let fresh = build_args(&inner);
        assert!(fresh.contains(&"--append-system-prompt".to_string()));
        assert!(!fresh.contains(&"--resume".to_string()));
        assert_eq!(fresh.last(), Some(&"x".to_string()));
        inner.native_id = Some("abc".into());
        let resumed = build_args(&inner);
        let i = resumed.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(resumed[i + 1], "abc");
    }
}
