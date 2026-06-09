//! PTY session manager: spawn the native `claude` TUI in a materialized
//! worktree, stream frame-batched output to the frontend, accept keystrokes
//! back, capture the native session id (persisting it to the DB), and resume in
//! the SAME cwd.
//!
//! M2 holds many sessions keyed by the DB `session.id`. Per-session metadata
//! (cwd, native id, tool) lives in the DB `session` row, not in this state.

use crate::batch::FrameBatcher;
use crate::store::{repo, Db};
use anyhow::{Context, Result};
use base64::Engine;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

const OUTPUT_EVENT: &str = "pty://output";
const EXIT_EVENT: &str = "pty://exit";
const SESSION_ID_EVENT: &str = "session://id";
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const FRAME_MAX_BYTES: usize = 64 * 1024;

/// The live OS objects for the running child. Recreated on resume.
struct Active {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    alive: Arc<AtomicBool>,
    direction_id: i32,
}

/// Tauri-managed state: live PTY sessions keyed by the DB `session.id`.
#[derive(Default)]
pub struct PtyState {
    sessions: Mutex<HashMap<i32, Active>>,
}

impl PtyState {
    /// Write `data` to the live session of `direction_id`, if any. Returns true
    /// if a session was found and written to.
    pub fn wake_direction(&self, direction_id: i32, data: &str) -> bool {
        let mut g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for a in g.values_mut() {
            if a.direction_id == direction_id {
                let _ = a.writer.write_all(data.as_bytes());
                let _ = a.writer.flush();
                return true;
            }
        }
        false
    }
}

#[derive(Serialize, Clone)]
pub struct SessionInfo {
    pub session_id: i32,
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub tool: String,
    pub resumed: bool,
    pub native_id: Option<String>,
}

#[derive(Serialize, Clone)]
struct OutputPayload {
    session_id: i32,
    /// base64 of raw PTY bytes (terminal output is binary).
    data: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Watchdog caps in seconds, read once per session from env. 0 disables.
fn idle_cap_secs() -> u64 {
    env_secs("WEFT_IDLE_WATCHDOG_SECS", 1800)
} // 30 min
fn wall_cap_secs() -> u64 {
    env_secs("WEFT_WALL_CAP_SECS", 7200)
} // 2 h
fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
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

/// Resume the same native conversation when one was ever captured; otherwise a
/// fresh dispatch is the only way to (re)start the task. Pure so it's unit-tested
/// without a DB or live PTY.
#[derive(Debug)]
pub(crate) enum DriveChoice {
    Resume(i32),
    Fresh,
}

pub(crate) fn drive_choice(latest: Option<(i32, Option<&str>)>) -> DriveChoice {
    match latest {
        Some((session_id, Some(_native))) => DriveChoice::Resume(session_id),
        _ => DriveChoice::Fresh,
    }
}

/// Read-only snapshot backing the observe surface: the worktree to read
/// transcript/diff from, plus the latest session's identity/status if any.
/// `None` only when the (direction, repo) has no materialized worktree.
#[derive(Serialize, Clone)]
pub struct ObserveRef {
    pub worktree: String,
    pub branch: String,
    pub tool: String,
    pub session_id: Option<i32>,
    pub native_id: Option<String>,
    pub status: Option<String>,
}

#[tauri::command]
pub async fn session_for(
    db: State<'_, Db>,
    direction_id: i32,
    repo_id: i32,
) -> Result<Option<ObserveRef>, String> {
    session_for_impl(&db, direction_id, repo_id)
        .await
        .map_err(|e| e.to_string())
}

async fn session_for_impl(
    db: &Db,
    direction_id: i32,
    repo_id: i32,
) -> Result<Option<ObserveRef>> {
    let wt = match repo::worktree_for(db, direction_id, repo_id).await? {
        Some(w) => w,
        None => return Ok(None),
    };
    let dir = match repo::get_direction(db, direction_id).await? {
        Some(d) => d,
        None => return Ok(None),
    };
    let latest = repo::latest_session_for(db, direction_id, repo_id).await?;
    Ok(Some(ObserveRef {
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        session_id: latest.as_ref().map(|s| s.id),
        native_id: latest.as_ref().and_then(|s| s.native_session_id.clone()),
        status: latest.as_ref().map(|s| s.status.clone()),
    }))
}

/// The ONLY process-touching open path a human/click takes. Resume the same
/// native conversation when one exists; only fall back to a fresh dispatch
/// (which re-seeds the brief + flips status→working) when no native id was ever
/// captured. Never re-runs a finished task from scratch on a plain open.
#[tauri::command]
pub async fn drive_session(
    app: AppHandle,
    db: State<'_, Db>,
    state: State<'_, PtyState>,
    direction_id: i32,
    repo_id: i32,
    lang: Option<String>,
) -> Result<SessionInfo, String> {
    drive_session_impl(app, &db, &state, direction_id, repo_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())
}

async fn drive_session_impl(
    app: AppHandle,
    db: &Db,
    state: &PtyState,
    direction_id: i32,
    repo_id: i32,
    lang: &str,
) -> Result<SessionInfo> {
    let latest = repo::latest_session_for(db, direction_id, repo_id).await?;
    match drive_choice(latest.as_ref().map(|s| (s.id, s.native_session_id.as_deref()))) {
        // lang is not re-applied on resume: the original brief already carries the language directive.
        DriveChoice::Resume(session_id) => resume_impl(app, db, state, session_id).await,
        DriveChoice::Fresh => open_session_impl(app, db, state, direction_id, repo_id, lang).await,
    }
}

/// Decide whether a session should be force-stopped. Pure → unit-tested.
/// `has_open_ask` = the session is legitimately blocked on the human, so its
/// silence is expected → never idle-kill. Wall-clock always applies.
fn watchdog_verdict(
    now: u64,
    start: u64,
    last_activity: u64,
    wall_cap: u64,
    idle_cap: u64,
    has_open_ask: bool,
) -> Option<String> {
    if wall_cap > 0 && now.saturating_sub(start) >= wall_cap {
        return Some(format!("ran for over {}", human_dur(wall_cap)));
    }
    if idle_cap > 0 && !has_open_ask && now.saturating_sub(last_activity) >= idle_cap {
        return Some(format!("no activity for {}", human_dur(idle_cap)));
    }
    None
}

/// Agent-output language directive (ARCHITECTURE §4.8, layer 2). Appended to the
/// lead prompt / worker brief so prose follows the operator's UI language; code
/// and identifiers always stay English. Empty for English (the default).
fn lang_directive(lang: &str) -> &'static str {
    if lang == "zh" {
        "\n\n用中文撰写所有自然语言产出(计划、摘要、bus 消息、PR/commit 文案);代码、标识符与技术约定始终用英文。"
    } else {
        ""
    }
}

/// Spawn the direction's tool into a fresh PTY at `cwd` and wire up
/// output/exit/capture. PLAIN binaries — no permission overrides; each tool's
/// own config applies. Tool-specific spawn/resume/capture lives in `drivers`.
fn spawn(
    app: &AppHandle,
    tool: &str,
    direction_id: i32,
    inject_args: &[String],
    cwd: &PathBuf,
    resume_id: Option<&str>,
    session_id: i32,
    db: Db,
) -> Result<Active> {
    let pair = native_pty_system().openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // Pre-accept claude's folder-trust gate for this weft-created cwd so an
    // unattended dispatch starts instead of stalling on the trust prompt. Not a
    // permission bypass — per-action approvals still surface via the Ask Bridge.
    if tool == "claude" {
        crate::claude::ensure_trusted(cwd);
    } else if tool == "codex" {
        crate::codex::ensure_codex_trusted(cwd);
    }

    let driver = crate::drivers::driver_for(tool);
    let spec = crate::drivers::SpawnSpec {
        cwd: cwd.clone(),
        resume_id: resume_id.map(|s| s.to_string()),
    };
    let (program, dargs) = driver.command(&spec);
    let mut cmd = CommandBuilder::new(&program);
    for a in inject_args.iter().chain(dargs.iter()) {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let alive = Arc::new(AtomicBool::new(true));
    let last_activity = Arc::new(AtomicU64::new(now_secs()));

    // --- shared pending buffer drained by the flusher ---
    let pending = Arc::new(Mutex::new(FrameBatcher::new(FRAME_MAX_BYTES)));

    // reader thread: append bytes to the batcher
    {
        let pending = pending.clone();
        let alive_r = alive.clone();
        let last_activity_r = last_activity.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        last_activity_r.store(now_secs(), Ordering::SeqCst);
                        pending.lock().unwrap_or_else(|e| e.into_inner()).push(&buf[..n]);
                    }
                }
            }
            alive_r.store(false, Ordering::SeqCst);
            // flush any tail, then signal exit
            if let Some(frame) = pending.lock().unwrap_or_else(|e| e.into_inner()).take_frame() {
                emit_output(&app, session_id, &frame);
            }
            let _ = app.emit(EXIT_EVENT, serde_json::json!({ "sessionId": session_id }));
        });
    }

    // flusher thread: every ~16ms drain one coalesced frame
    {
        let pending = pending.clone();
        let alive_f = alive.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            while alive_f.load(Ordering::SeqCst) {
                std::thread::sleep(FRAME_INTERVAL);
                let frame = pending.lock().unwrap_or_else(|e| e.into_inner()).take_frame();
                if let Some(frame) = frame {
                    emit_output(&app, session_id, &frame);
                }
            }
        });
    }

    // watchdog thread: force-stop a runaway/stuck session (wall-clock + idle).
    {
        let app = app.clone();
        let alive_w = alive.clone();
        let last_activity_w = last_activity.clone();
        let start = now_secs();
        let wall_cap = wall_cap_secs();
        let idle_cap = idle_cap_secs();
        std::thread::spawn(move || {
            if wall_cap == 0 && idle_cap == 0 {
                return;
            }
            loop {
                std::thread::sleep(Duration::from_secs(30));
                if !alive_w.load(Ordering::SeqCst) {
                    return;
                }
                let now = now_secs();
                let last = last_activity_w.load(Ordering::SeqCst);
                // Workers are keyed by their direction id; a LEAD spawns with a
                // synthetic negative id and its permission asks carry the literal
                // "lead" (empty-dir is matched too, defensively). Match both so a
                // lead blocked on a human is never idle-killed.
                let needle = direction_id.to_string();
                let is_lead = direction_id < 0;
                let has_open_ask = app
                    .try_state::<crate::ask::AskRegistry>()
                    .map(|a| {
                        a.open().iter().any(|k| {
                            k.dir == needle || (is_lead && (k.dir == "lead" || k.dir.is_empty()))
                        })
                    })
                    .unwrap_or(false);
                if let Some(reason) =
                    watchdog_verdict(now, start, last, wall_cap, idle_cap, has_open_ask)
                {
                    escalate(&app, session_id, direction_id, reason);
                    return;
                }
            }
        });
    }

    // capture thread: poll the tool's session store for the native id
    if resume_id.is_none() {
        let app = app.clone();
        let cwd = cwd.clone();
        let alive_c = alive.clone();
        let t0 = now_secs();
        let tool = tool.to_string();
        std::thread::spawn(move || {
            // Poll for the id for as long as the session is alive. The tool
            // doesn't persist a session until it actually starts — AFTER the
            // user clears trust / onboarding gates, which can take well over a
            // minute. A fixed short deadline would expire mid-gate and the id
            // would never be captured (Resume could never arm). The 10-min cap
            // is just a zombie-thread backstop in case `alive` never flips.
            let driver = crate::drivers::driver_for(&tool);
            let backstop = Instant::now() + Duration::from_secs(600);
            while alive_c.load(Ordering::SeqCst) && Instant::now() < backstop {
                if let Some(id) = driver.capture_session_id(&cwd, t0) {
                    let _ = app.emit(
                        SESSION_ID_EVENT,
                        serde_json::json!({ "sessionId": session_id, "nativeId": id }),
                    );
                    let db = db.clone();
                    let id2 = id.clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = repo::set_session_native_id(&db, session_id, &id2).await;
                    });
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        });
    }

    Ok(Active {
        child,
        writer,
        master: pair.master,
        alive,
        direction_id,
    })
}

fn emit_output(app: &AppHandle, session_id: i32, bytes: &[u8]) {
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let _ = app.emit(OUTPUT_EVENT, OutputPayload { session_id, data });
}

// ===================== Tauri commands =====================

/// Open a brand-new session on an already-materialized worktree (Task 7) for the
/// given direction + repo. Creates the DB `session` row, spawns plain `claude`
/// in the worktree cwd, and registers the live PTY keyed by the session id.
#[tauri::command]
pub async fn open_session(
    app: AppHandle,
    db: State<'_, Db>,
    state: State<'_, PtyState>,
    direction_id: i32,
    repo_id: i32,
    lang: Option<String>,
) -> Result<SessionInfo, String> {
    open_session_impl(app, &db, &state, direction_id, repo_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())
}

async fn open_session_impl(
    app: AppHandle,
    db: &Db,
    state: &PtyState,
    direction_id: i32,
    repo_id: i32,
    lang: &str,
) -> Result<SessionInfo> {
    let wt = repo::worktree_for(db, direction_id, repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no materialized worktree for that direction+repo"))?;
    let dir = {
        use sea_orm::EntityTrait;
        crate::store::entities::direction::Entity::find_by_id(direction_id)
            .one(&db.0)
            .await?
            .ok_or_else(|| anyhow::anyhow!("direction not found"))?
    };
    let cwd = PathBuf::from(&wt.path);
    let sess = repo::create_session(db, direction_id, repo_id, &dir.tool, &wt.path).await?;

    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &dir.tool,
        &cwd,
    );

    // Dispatch the worker WITH its brief (ARCHITECTURE §4.10): objective, scope,
    // contracts, non-goals. Seeded as the initial message, BEFORE --mcp-config
    // (claude's variadic flag would otherwise eat it). Best-effort: a bare
    // session still opens if the brief can't be assembled.
    let mut brief = crate::brief::assemble(db, direction_id).await.unwrap_or_default();
    if !brief.is_empty() {
        brief.push_str(lang_directive(lang));
    }
    let ask = crate::bus::inject::inject_ask_hook(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &dir.tool,
        &cwd,
    );
    let mut args: Vec<String> = Vec::new();
    if !brief.trim().is_empty() {
        // Per-tool seeding: positional for claude/codex, --prompt for opencode.
        args.extend(crate::drivers::driver_for(&dir.tool).seed_args(&brief));
    }
    args.extend(ask.args);
    args.extend(inj.args);

    let active = spawn(&app, &dir.tool, direction_id, &args, &cwd, None, sess.id, db.clone())
        .context("spawn agent")?;
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(sess.id, active);
    // Dispatch moves the task into "working"; the agent advances it from there.
    let _ = repo::set_direction_status(db, direction_id, "working").await;

    Ok(SessionInfo {
        session_id: sess.id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        resumed: false,
        native_id: None,
    })
}

/// Resume a session in its stable worktree cwd using the persisted native id.
#[tauri::command]
pub async fn resume_session(
    app: AppHandle,
    db: State<'_, Db>,
    state: State<'_, PtyState>,
    session_id: i32,
) -> Result<SessionInfo, String> {
    resume_impl(app, &db, &state, session_id)
        .await
        .map_err(|e| e.to_string())
}

async fn resume_impl(
    app: AppHandle,
    db: &Db,
    state: &PtyState,
    session_id: i32,
) -> Result<SessionInfo> {
    let s = repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no session"))?;
    let native = s
        .native_session_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("native id not captured yet"))?;
    let wt = repo::worktree_for(db, s.direction_id, s.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("worktree gone"))?;
    // kill the old live process if present
    if let Some(mut a) = state.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&session_id) {
        a.alive.store(false, Ordering::SeqCst);
        let _ = a.child.kill();
        let _ = a.child.wait();
    }
    let cwd = PathBuf::from(&wt.path);
    let tid = {
        use sea_orm::EntityTrait;
        crate::store::entities::direction::Entity::find_by_id(s.direction_id)
            .one(&db.0)
            .await?
            .map(|d| d.thread_id)
            .unwrap_or(0)
    };
    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(&base, tid, &s.direction_id.to_string(), &s.tool, &cwd);
    let ask = crate::bus::inject::inject_ask_hook(
        &base,
        tid,
        &s.direction_id.to_string(),
        &s.tool,
        &cwd,
    );
    let mut args: Vec<String> = Vec::new();
    args.extend(ask.args);
    args.extend(inj.args);
    let active = spawn(&app, &s.tool, s.direction_id, &args, &cwd, Some(&native), session_id, db.clone())
        .context("spawn agent --resume")?;
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id, active);
    Ok(SessionInfo {
        session_id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: s.tool,
        resumed: true,
        native_id: Some(native),
    })
}

/// Forward keystrokes from xterm to the child's stdin (Ctrl-C, chars, etc.).
#[tauri::command]
pub fn write_pty(state: State<PtyState>, session_id: i32, data: String) -> Result<(), String> {
    let mut g = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    let a = g.get_mut(&session_id).ok_or("no such session")?;
    a.writer
        .write_all(data.as_bytes())
        .map_err(|e| e.to_string())?;
    a.writer.flush().map_err(|e| e.to_string())
}

/// Keep the PTY size in sync with the xterm viewport.
#[tauri::command]
pub fn resize_pty(
    state: State<PtyState>,
    session_id: i32,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let g = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = g.get(&session_id) {
        a.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Force-stop a runaway/stuck session and surface it via Needs-you. Reuses the
/// kill path, then posts a bus ask from the direction so it appears as a
/// Needs-you item (no dedicated UI for round 1).
fn escalate(app: &AppHandle, session_id: i32, direction_id: i32, reason: String) {
    if let Some(state) = app.try_state::<PtyState>() {
        if let Some(mut a) = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id)
        {
            a.alive.store(false, Ordering::SeqCst);
            let _ = a.child.kill();
            let _ = a.child.wait();
        }
    }
    let _ = app.emit(EXIT_EVENT, serde_json::json!({ "sessionId": session_id }));

    if let (Some(bus), Some(db)) = (
        app.try_state::<crate::bus::BusRegistry>(),
        app.try_state::<crate::store::Db>(),
    ) {
        let bus = (*bus).clone();
        let db = crate::store::Db(db.0.clone());
        tauri::async_runtime::spawn(async move {
            if let Ok(Some(d)) = crate::store::repo::get_direction(&db, direction_id).await {
                bus.ask_human(
                    d.thread_id,
                    &direction_id.to_string(),
                    &format!("⚠️ Worker auto-stopped by the runaway guard: {reason}. Review and resume if it was still needed."),
                );
            }
        });
    }
}

/// Terminate one session.
#[tauri::command]
pub fn kill_session(state: State<PtyState>, session_id: i32) -> Result<(), String> {
    if let Some(mut a) = state.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&session_id) {
        a.alive.store(false, Ordering::SeqCst);
        let _ = a.child.kill();
        let _ = a.child.wait();
    }
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct LeadInfo {
    pub session_id: i32,
    pub thread_id: i32,
    pub cwd: String,
    pub tool: String,
}

/// The conversational lead prompt. The lead is the human's main collaborator for
/// the thread: it discusses the work, and the plan EMERGES from that conversation
/// rather than from a one-shot propose-and-exit. It proposes when (and only when)
/// the human has converged with it, and may re-propose after more discussion.
fn lead_prompt() -> &'static str {
    "You are the lead for this thread in weft — the human's main collaborator. \
Start by greeting briefly and using the weft_planner MCP tools to orient: call get_task to read \
what's being asked, and get_repo_map to learn each repo's role and the cross-repo dependency graph. \
Then DISCUSS the approach with the human; ask clarifying questions when it matters. You do not write \
code — you plan and drive. When you and the human have converged on how to split the work, call \
propose_directions with a short rationale and the directions (name, tool, writes[]); only list repos \
each direction must WRITE (reads are free). The human reviews and confirms in weft; you can re-propose \
after more discussion. Prefer splitting frontend/backend/shared work to run in parallel, owner of a \
shared contract first."
}

/// Spawn (or replace) the thread's PERSISTENT read-only lead conversation: a
/// fresh agent in a per-thread scratch dir with the planner MCP injected and the
/// conversational lead prompt seeded. No worktree, no DB row — the lead plans via
/// the planner MCP; the human keeps talking to it in the dock and confirms its
/// proposals in the scope-confirm step. Keyed in PtyState by a synthetic negative
/// id (`-thread_id`) so it never collides with worker ids and is stable per thread.
#[tauri::command]
pub async fn plan_with_lead(
    app: AppHandle,
    db: State<'_, Db>,
    state: State<'_, PtyState>,
    thread_id: i32,
    lang: Option<String>,
) -> Result<LeadInfo, String> {
    plan_with_lead_impl(app, &db, &state, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())
}

async fn plan_with_lead_impl(
    app: AppHandle,
    db: &Db,
    state: &PtyState,
    thread_id: i32,
    lang: &str,
) -> Result<LeadInfo> {
    // Validate the thread exists (the lead reads its task via the planner MCP).
    repo::get_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    // Lead default tool (leadAgent override is a later milestone).
    let tool = "claude".to_string();

    let cwd = crate::paths::weft_home()?
        .join("leads")
        .join(thread_id.to_string());
    std::fs::create_dir_all(&cwd).context("create lead scratch dir")?;
    // git-init the scratch dir so claude's session store (keyed by cwd) and the
    // injected config behave like any other cwd; harmless if it already exists.
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status();

    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject_planner(&base, thread_id, &tool, &cwd);
    // The lead is read-only planning, but install the Ask Bridge too so any
    // permission prompt it hits still surfaces instead of stalling.
    let ask = crate::bus::inject::inject_ask_hook(&base, thread_id, "lead", &tool, &cwd);

    // Seed the planning prompt as the agent's initial positional message. It must
    // come BEFORE --mcp-config: claude's --mcp-config is variadic and would
    // otherwise swallow the prompt as a second config path (ENAMETOOLONG).
    let mut args = vec![format!("{}{}", lead_prompt(), lang_directive(lang))];
    args.extend(ask.args);
    args.extend(inj.args);

    let session_id = -thread_id; // synthetic, ephemeral, collision-free
    // Replace any prior live lead session for this thread.
    if let Some(mut a) = state.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(&session_id) {
        a.alive.store(false, Ordering::SeqCst);
        let _ = a.child.kill();
    }
    let active = spawn(&app, &tool, -1, &args, &cwd, None, session_id, db.clone())
        .context("spawn lead")?;
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id, active);

    Ok(LeadInfo {
        session_id,
        thread_id,
        cwd: cwd.to_string_lossy().to_string(),
        tool,
    })
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;
    #[test]
    fn drive_choice_resumes_when_native_present() {
        assert!(matches!(
            super::drive_choice(Some((7, Some("abc")))),
            super::DriveChoice::Resume(7)
        ));
    }

    #[test]
    fn drive_choice_fresh_when_no_session() {
        assert!(matches!(super::drive_choice(None), super::DriveChoice::Fresh));
    }

    #[test]
    fn drive_choice_fresh_when_native_missing() {
        assert!(matches!(
            super::drive_choice(Some((7, None))),
            super::DriveChoice::Fresh
        ));
    }

    #[test]
    fn wall_cap_fires_regardless_of_activity() {
        assert!(watchdog_verdict(10_000, 0, 9_999, 7200, 1800, false)
            .unwrap()
            .contains("ran for over 2h"));
    }
    #[test]
    fn idle_fires_when_silent_and_not_waiting_on_human() {
        assert!(watchdog_verdict(5_000, 4_000, 3_000, 0, 1800, false)
            .unwrap()
            .contains("no activity"));
    }
    #[test]
    fn idle_suppressed_while_waiting_on_human() {
        assert_eq!(watchdog_verdict(5_000, 4_000, 3_000, 0, 1800, true), None);
    }
    #[test]
    fn active_session_is_kept() {
        assert_eq!(watchdog_verdict(1_000, 0, 999, 7200, 1800, false), None);
    }
    #[test]
    fn zero_caps_disable_each_check() {
        assert_eq!(watchdog_verdict(1_000_000, 0, 0, 0, 0, false), None);
    }
    #[test]
    fn human_dur_formats() {
        assert_eq!(human_dur(7200), "2h");
        assert_eq!(human_dur(1800), "30min");
        assert_eq!(human_dur(45), "45s");
    }
}
