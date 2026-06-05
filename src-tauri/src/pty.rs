//! PTY session manager: spawn the native `claude` TUI in a materialized
//! worktree, stream frame-batched output to the frontend, accept keystrokes
//! back, capture the native session id (persisting it to the DB), and resume in
//! the SAME cwd.
//!
//! M2 holds many sessions keyed by the DB `session.id`. Per-session metadata
//! (cwd, native id, tool) lives in the DB `session` row, not in this state.

use crate::batch::FrameBatcher;
use crate::claude;
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
use tauri::{AppHandle, Emitter, State};

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
}

/// Tauri-managed state: live PTY sessions keyed by the DB `session.id`.
#[derive(Default)]
pub struct PtyState {
    sessions: Mutex<HashMap<i32, Active>>,
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
    /// base64 of raw PTY bytes (terminal output is binary).
    data: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Spawn `claude` into a fresh PTY at `cwd` and wire up output/exit/capture.
/// NOTE: PLAIN claude — no permission overrides (see claude.rs).
fn spawn(
    app: &AppHandle,
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

    let mut cmd = CommandBuilder::new("claude");
    if let Some(id) = resume_id {
        cmd.arg("--resume");
        cmd.arg(id);
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
                        pending.lock().unwrap().push(&buf[..n]);
                    }
                }
            }
            alive_r.store(false, Ordering::SeqCst);
            // flush any tail, then signal exit
            if let Some(frame) = pending.lock().unwrap().take_frame() {
                emit_output(&app, &frame);
            }
            let _ = app.emit(EXIT_EVENT, ());
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
                let frame = pending.lock().unwrap().take_frame();
                if let Some(frame) = frame {
                    emit_output(&app, &frame);
                }
            }
        });
    }

    // capture thread: poll the sidecar dir for the native session id
    if resume_id.is_none() {
        let app = app.clone();
        let cwd = cwd.clone();
        let alive_c = alive.clone();
        let t0 = now_secs();
        std::thread::spawn(move || {
            // Poll for the session id for as long as the session is alive. The
            // jsonl isn't written until Claude actually starts a session, which
            // is AFTER the user clears the trust / onboarding gate screens — and
            // a human can take well over a minute to do that. A fixed short
            // deadline would expire mid-gate and the id would never be captured
            // (then Resume can never arm). The 10-min cap is just a zombie-thread
            // backstop in case `alive` somehow never flips.
            let backstop = Instant::now() + Duration::from_secs(600);
            while alive_c.load(Ordering::SeqCst) && Instant::now() < backstop {
                if let Ok(dir) = claude::projects_dir_for(&cwd) {
                    if let Some(id) = claude::capture_session_id(&dir, t0) {
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
    })
}

fn emit_output(app: &AppHandle, bytes: &[u8]) {
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let _ = app.emit(OUTPUT_EVENT, OutputPayload { data });
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

    let active = spawn(&app, &cwd, None, sess.id, db.clone()).context("spawn claude")?;
    state.sessions.lock().unwrap().insert(sess.id, active);

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
    if let Some(mut a) = state.sessions.lock().unwrap().remove(&session_id) {
        a.alive.store(false, Ordering::SeqCst);
        let _ = a.child.kill();
        let _ = a.child.wait();
    }
    let cwd = PathBuf::from(&wt.path);
    let active =
        spawn(&app, &cwd, Some(&native), session_id, db.clone()).context("spawn claude --resume")?;
    state.sessions.lock().unwrap().insert(session_id, active);
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
    let mut g = state.sessions.lock().unwrap();
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
    let g = state.sessions.lock().unwrap();
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
    if let Some(mut a) = state.sessions.lock().unwrap().remove(&session_id) {
        a.alive.store(false, Ordering::SeqCst);
        let _ = a.child.kill();
        let _ = a.child.wait();
    }
    Ok(())
}
