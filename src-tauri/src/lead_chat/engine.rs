//! The chat engine: one long-lived headless `claude -p` stream-json process per
//! timeline (lead = `-thread_id`, chat-mode worker = `session_id`). stdin takes
//! user messages; stdout is parsed (proto.rs), persisted (lead_message), and
//! pushed to the frontend over the `lead-chat` Tauri event. Interrupt rides the
//! protocol's control_request (verified live), with a kill fallback; a dead
//! process resumes losslessly via `--resume <native_id>` on the next send.

use crate::store::{repo, Db};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

pub const EVENT: &str = "lead-chat";

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
        slash_commands: Vec<String>,
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
    tool != "claude"
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
        Self { started: None, last_activity: std::time::Instant::now() }
    }
}

impl TurnClock {
    fn begin_turn(&mut self) {
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
    pub slash_commands: Vec<String>,
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
}

pub type EngineRef = Arc<tokio::sync::Mutex<EngineInner>>;

/// All live chat engines, keyed by `-thread_id` (lead) or `session_id` (worker).
#[derive(Default)]
pub struct LeadChatState(pub std::sync::Mutex<HashMap<i64, EngineRef>>);

impl LeadChatState {
    pub fn get(&self, key: i64) -> Option<EngineRef> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned()
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

/// Spawn the process if it isn't alive (fresh or `--resume`), wiring the reader.
/// Per-turn dialects have no resident process — sending spawns one per turn.
pub async fn ensure_running(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    if per_turn(&inner.tool) {
        return Ok(());
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

async fn write_user(inner: &mut EngineInner, out: &Outgoing) {
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
        crate::power::on_turn_began(app);
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
    let m = repo::insert_lead_message(db, thread_id, sid, turn, "user", kind, &content, status).await?;
    let row_id = m.id;
    let _ = app.emit(EVENT, Push::Message { thread_id, message: m });
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
    let out = Outgoing { text: outbound, images, tracked: true };
    let spawn_now = direct && per_turn(&inner.tool);
    if direct && !spawn_now {
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
    }
    Ok(())
}

/// One per-turn process (codex/opencode): the message rides the argv, events
/// stream from stdout, EOF ends the turn (the reader then flushes the queue).
async fn spawn_turn(app: AppHandle, db: Db, eng: EngineRef, out: Outgoing) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    let (program, mut args): (String, Vec<String>) = match inner.tool.as_str() {
        "codex" => {
            crate::codex::ensure_codex_trusted(&inner.cwd);
            let mut a: Vec<String> = vec!["exec".into()];
            a.extend(inner.extra_args.iter().cloned());
            a.push("--json".into());
            a.push("--cd".into());
            a.push(inner.cwd.to_string_lossy().into_owned());
            if let Some(id) = &inner.native_id {
                a.push("resume".into());
                a.push(id.clone());
            }
            ("codex".into(), a)
        }
        _ => {
            // opencode: cwd is the project; injections live in its merged config.
            let mut a: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];
            if let Some(id) = &inner.native_id {
                a.push("--session".into());
                a.push(id.clone());
            }
            ("opencode".into(), a)
        }
    };
    args.push(out.text.clone());
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
    // Per-turn dialects have no interrupt protocol: kill ends the turn (EOF
    // path finalizes as interrupted) and resume picks the session back up.
    if per_turn(&inner.tool) {
        if let Some(c) = inner.child.as_mut() {
            let _ = c.kill().await;
        }
        return Ok(());
    }
    let request_id = format!("weft-int-{}", inner.generation);
    if let Some(stdin) = inner.stdin.as_mut() {
        let req = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        });
        let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
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
    let out = Outgoing { text: text.to_string(), images: vec![], tracked: false };
    if inner.turn.try_begin_send() {
        inner.turn_id += 1;
        inner.clock.begin_turn();
        crate::power::on_turn_began(app);
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

/// Stop the engine outright (e.g. before a terminal takeover).
pub async fn stop(app: &AppHandle, eng: &EngineRef) {
    stop_quiet(eng).await;
    let inner = eng.lock().await;
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
                if let Some(native) = super::proto::extract_native(&inner.tool, &line) {
                    inner.native_id = Some(native.clone());
                    if let Some(sid) = inner.session_id {
                        let _ = repo::set_session_native_id(&db, sid, &native).await;
                    } else {
                        let _ = repo::set_lead_native_id(&db, thread_id, &native).await;
                    }
                    let _ = app.emit(EVENT, Push::Init {
                        thread_id,
                        session_id: inner.session_id,
                        native_id: native,
                        slash_commands: inner.slash_commands.clone(),
                    });
                }
            }
            if !matches!(super::proto::parse_line_for(&inner.tool, &line), super::proto::ChatEvent::Other) {
                saw_event = true;
            }
            match super::proto::parse_line_for(&inner.tool, &line) {
                super::proto::ChatEvent::Init { session_id, slash_commands } => {
                    inner.native_id = Some(session_id.clone());
                    inner.slash_commands = slash_commands.clone();
                    if let Some(sid) = inner.session_id {
                        let _ = repo::set_session_native_id(&db, sid, &session_id).await;
                    } else {
                        let _ = repo::set_lead_native_id(&db, thread_id, &session_id).await;
                    }
                    let _ = app.emit(EVENT, Push::Init {
                        thread_id,
                        session_id: inner.session_id,
                        native_id: session_id,
                        slash_commands,
                    });
                }
                super::proto::ChatEvent::Commands { commands } => {
                    inner.slash_commands = commands.clone();
                    let _ = app.emit(EVENT, Push::Init {
                        thread_id,
                        session_id: inner.session_id,
                        native_id: inner.native_id.clone().unwrap_or_default(),
                        slash_commands: commands,
                    });
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
                                &db, thread_id, sid, turn, "assistant", "text",
                                r#"{"text":""}"#, "streaming",
                            )
                            .await
                            else {
                                continue;
                            };
                            let id = m.id;
                            inner.current = Some((id, text.clone(), std::time::Instant::now()));
                            let _ = app.emit(EVENT, Push::Message { thread_id, message: m });
                            id
                        }
                    };
                    // Throttle DB snapshots; the live UI rides the Delta events.
                    if let Some(c) = &mut inner.current {
                        if c.2.elapsed().as_millis() >= 500 {
                            c.2 = std::time::Instant::now();
                            let content = serde_json::json!({ "text": c.1 }).to_string();
                            let _ = repo::update_lead_message(&db, row, &content, "streaming").await;
                        }
                    }
                    let _ = app.emit(EVENT, Push::Delta { thread_id, message_id: row, text });
                }
                super::proto::ChatEvent::Assistant { texts, tools } => {
                    // A finished text block: finalize the streaming row with the
                    // authoritative full text. Some turns have NO deltas at all —
                    // built-in slash commands reply via a synthetic assistant
                    // message — so a missing streaming row means insert, not drop.
                    if !texts.is_empty() {
                        let full = texts.join("\n\n");
                        let content = serde_json::json!({ "text": full }).to_string();
                        match inner.current.take() {
                            Some((id, _, _)) => {
                                let _ = repo::update_lead_message(&db, id, &content, "complete").await;
                                let _ = app.emit(EVENT, Push::Finalize {
                                    thread_id, message_id: id, status: "complete".into(),
                                });
                            }
                            None => {
                                let (sid, turn) = (inner.session_id, inner.turn_id);
                                if let Ok(m) = repo::insert_lead_message(
                                    &db, thread_id, sid, turn, "assistant", "text", &content, "complete",
                                )
                                .await
                                {
                                    let _ = app.emit(EVENT, Push::Message { thread_id, message: m });
                                }
                            }
                        }
                    }
                    // Tool calls are transient activity, not timeline rows:
                    // show the one currently running, gone when the turn moves on.
                    for (name, summary) in tools {
                        let _ = app.emit(EVENT, Push::Activity {
                            thread_id,
                            session_id: inner.session_id,
                            name,
                            summary,
                        });
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
                            &db, id,
                            &serde_json::json!({ "text": text }).to_string(),
                            status,
                        )
                        .await;
                        let _ = app.emit(EVENT, Push::Finalize {
                            thread_id, message_id: id, status: status.into(),
                        });
                    }
                    if let Some(next) = inner.turn.on_turn_end() {
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
                    inner.clock.on_turn_end(still_busy);
                    let state = if still_busy { "busy" } else { "idle" };
                    let _ = app.emit(EVENT, Push::Turn {
                        thread_id,
                        session_id: inner.session_id,
                        state: state.into(),
                        queued: inner.turn.queue.len(),
                    });
                }
                _ => {}
            }
        }
        // EOF. Per-turn dialects end every turn this way (clean exit); for the
        // long-lived claude process it means a crash/kill — history stays, the
        // next send resumes.
        let mut inner = eng.lock().await;
        if inner.generation == generation && per_turn(&inner.tool) {
            let status = if inner.interrupting { "interrupted" } else { "complete" };
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
                    &db, id,
                    &serde_json::json!({ "text": text }).to_string(),
                    status,
                )
                .await;
                let _ = app.emit(EVENT, Push::Finalize {
                    thread_id: inner.thread_id, message_id: id, status: status.into(),
                });
            }
            inner.child = None;
            if let Some(next) = inner.turn.on_turn_end() {
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
            inner.clock.on_turn_end(still_busy);
            let state = if still_busy { "busy" } else { "idle" };
            let _ = app.emit(EVENT, Push::Turn {
                thread_id: inner.thread_id,
                session_id: inner.session_id,
                state: state.into(),
                queued: inner.turn.queue.len(),
            });
            return;
        }
        if inner.generation == generation {
            // A row still streaming at death closes as interrupted/error.
            let status = if inner.interrupting { "interrupted" } else { "error" };
            inner.interrupting = false;
            if let Some((id, text, _)) = inner.current.take() {
                let _ = repo::update_lead_message(
                    &db, id,
                    &serde_json::json!({ "text": text }).to_string(),
                    status,
                )
                .await;
                let _ = app.emit(EVENT, Push::Finalize {
                    thread_id: inner.thread_id, message_id: id, status: status.into(),
                });
            }
            inner.child = None;
            inner.stdin = None;
            inner.turn = TurnState::default();
            inner.clock = TurnClock::default();
            let _ = app.emit(EVENT, Push::Turn {
                thread_id: inner.thread_id,
                session_id: inner.session_id,
                state: "stopped".into(),
                queued: 0,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_machine() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy: send through
        assert!(!t.try_begin_send()); // busy: enqueue
        t.queue.push_back(Outgoing { text: "second".into(), images: vec![], tracked: true });
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
    fn human_dur_formats() {
        assert_eq!(human_dur(7200), "2h");
        assert_eq!(human_dur(1800), "30min");
        assert_eq!(human_dur(45), "45s");
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
