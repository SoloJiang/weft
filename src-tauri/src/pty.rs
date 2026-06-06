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
use std::sync::atomic::{AtomicBool, Ordering};
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

    // --- shared pending buffer drained by the flusher ---
    let pending = Arc::new(Mutex::new(FrameBatcher::new(FRAME_MAX_BYTES)));

    // reader thread: append bytes to the batcher
    {
        let pending = pending.clone();
        let alive_r = alive.clone();
        let app = app.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
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
) -> Result<SessionInfo, String> {
    open_session_impl(app, &db, &state, direction_id, repo_id)
        .await
        .map_err(|e| e.to_string())
}

async fn open_session_impl(
    app: AppHandle,
    db: &Db,
    state: &PtyState,
    direction_id: i32,
    repo_id: i32,
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
    let brief = crate::brief::assemble(db, direction_id).await.unwrap_or_default();
    let ask = crate::bus::inject::inject_ask_hook(
        &base,
        dir.thread_id,
        &direction_id.to_string(),
        &dir.tool,
        &cwd,
    );
    let mut args: Vec<String> = Vec::new();
    if !brief.trim().is_empty() {
        args.push(brief);
    }
    args.extend(ask.args);
    args.extend(inj.args);

    let active = spawn(&app, &dir.tool, direction_id, &args, &cwd, None, sess.id, db.clone())
        .context("spawn agent")?;
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(sess.id, active);

    Ok(SessionInfo {
        session_id: sess.id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        resumed: false,
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
    let active = spawn(&app, &s.tool, s.direction_id, &inj.args, &cwd, Some(&native), session_id, db.clone())
        .context("spawn agent --resume")?;
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id, active);
    Ok(SessionInfo {
        session_id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: s.tool,
        resumed: true,
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

/// The planning prompt the lead is seeded with. It is told to drive the planner
/// MCP and propose a write-only decomposition; the human confirms in weft.
fn lead_prompt() -> &'static str {
    "You are the planning lead in weft. Use the weft_planner MCP tools to plan, do not write code. \
Steps: (1) call get_task to read the task; (2) call get_repo_map to see each repo's role and the \
cross-repo dependency graph; (3) decide which repos each parallel direction must WRITE (reads are \
free — never list them); (4) call propose_directions with a short rationale and the directions \
(name, tool, writes[]). Prefer splitting frontend/backend/shared work so directions run in \
parallel; put a shared contract's owner first. The human reviews and confirms your proposal."
}

/// Spawn an EPHEMERAL read-only lead session to plan a thread: a fresh agent in
/// a per-thread scratch dir with the planner MCP injected and the planning
/// prompt seeded. No worktree, no DB row — the lead proposes via the planner MCP
/// and exits; the human confirms in the scope-confirm step. Keyed in PtyState by
/// a synthetic negative id (`-thread_id`) so it never collides with worker ids.
#[tauri::command]
pub async fn plan_with_lead(
    app: AppHandle,
    db: State<'_, Db>,
    state: State<'_, PtyState>,
    thread_id: i32,
) -> Result<LeadInfo, String> {
    plan_with_lead_impl(app, &db, &state, thread_id)
        .await
        .map_err(|e| e.to_string())
}

async fn plan_with_lead_impl(
    app: AppHandle,
    db: &Db,
    state: &PtyState,
    thread_id: i32,
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
    let ask = crate::bus::inject::inject_ask_hook(&base, thread_id, "", &tool, &cwd);

    // Seed the planning prompt as the agent's initial positional message. It must
    // come BEFORE --mcp-config: claude's --mcp-config is variadic and would
    // otherwise swallow the prompt as a second config path (ENAMETOOLONG).
    let mut args = vec![lead_prompt().to_string()];
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
