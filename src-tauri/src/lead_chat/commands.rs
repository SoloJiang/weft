//! Tauri commands for the chat engine. The lead's engine is keyed by
//! `-thread_id`; chat-mode workers (phase 2) key by `session_id`.

use super::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};
use tauri::{AppHandle, Manager, State};

fn lead_key(thread_id: i32) -> i64 {
    -(thread_id as i64)
}

/// What a (re)dispatched worker session looks like to the frontend.
#[derive(serde::Serialize, Clone)]
pub struct SessionInfo {
    pub session_id: i32,
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub tool: String,
    pub resumed: bool,
    pub native_id: Option<String>,
}

/// The conversational lead prompt. The lead is the human's main collaborator for
/// the thread: it discusses the work, and the plan EMERGES from that conversation
/// rather than from a one-shot propose-and-exit. It proposes when (and only when)
/// the human has converged with it, and may re-propose after more discussion.
pub fn lead_prompt() -> &'static str {
    "You are the lead for this thread in weft — the human's main collaborator. \
Start by greeting briefly and using the weft_planner MCP tools to orient: call get_task to read \
what's being asked, and get_repo_map to learn each repo's role and the cross-repo dependency graph. \
Then DISCUSS the requirement and approach with the human; ask clarifying questions when it matters. \
You do not write code, and you do not plan the directions' implementations — each worker plans its \
own direction. Your job is to converge the scope and ASSIGN ROLES. When you and the human have \
converged on how to split the work, call propose_directions with a short rationale and the directions \
(name, tool, the ONE repo each writes, reason, mandate); only list repos each direction must WRITE \
(reads are free). Pick mandate per direction: plan+impl (default — the worker plans first) or \
impl-only (small/fully-specified — build straight away). The human reviews and confirms in weft; you \
can re-propose after more discussion. Prefer splitting frontend/backend/shared work to run in \
parallel, owner of a shared contract first."
}

/// Agent-output language directive (ARCHITECTURE §4.8, layer 2). Appended to the
/// lead prompt / worker brief so prose follows the operator's UI language; code
/// and identifiers always stay English. Empty for English (the default).
pub fn lang_directive(lang: &str) -> &'static str {
    if lang == "zh" {
        "\n\n用中文撰写所有自然语言产出(计划、摘要、bus 消息、PR/commit 文案);代码、标识符与技术约定始终用英文。"
    } else {
        ""
    }
}

/// Get-or-create the lead's engine for a thread: scratch cwd, planner MCP +
/// ask bridge injections, conversational lead prompt as the system prompt.
/// Mirrors the retired PTY `plan_with_lead` wiring (spec §2).
async fn lead_engine(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    lang: &str,
) -> anyhow::Result<EngineRef> {
    let state = app.state::<LeadChatState>();
    if let Some(e) = state.get(lead_key(thread_id)) {
        return Ok(e);
    }
    repo::get_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let cwd = crate::paths::weft_home()?.join("leads").join(thread_id.to_string());
    std::fs::create_dir_all(&cwd)?;
    // git-init so claude's session store (keyed by cwd) behaves like any other
    // cwd; harmless if it already exists.
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status();
    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject_planner(&base, thread_id, "claude", &cwd);
    let ask = crate::bus::inject::inject_ask_hook(&base, thread_id, "lead", "claude", &cwd);
    let mut extra = ask.args;
    extra.extend(inj.args);
    let system_prompt = format!("{}{}", lead_prompt(), lang_directive(lang));
    let inner = engine::EngineInner {
        thread_id,
        tool: "claude".into(),
        session_id: None,
        cwd,
        extra_args: extra,
        system_prompt,
        native_id: repo::lead_native_id(db, thread_id).await.ok().flatten(),
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, thread_id).await.unwrap_or(1) - 1,
        ask_dir: "lead".into(),
        clock: Default::default(),
        child: None,
        stdin: None,
        current: None,
        interrupting: false,
        generation: 0,
    };
    let eng: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    Ok(state.get_or_insert(lead_key(thread_id), eng))
}

/// One inbound image attachment from the composer (pasted or picked).
#[derive(serde::Deserialize)]
pub struct ImageIn {
    pub media_type: String,
    /// base64 payload, no data-URI prefix.
    pub data: String,
}

fn to_pairs(images: Option<Vec<ImageIn>>) -> Vec<(String, String)> {
    images
        .unwrap_or_default()
        .into_iter()
        .map(|i| (i.media_type, i.data))
        .collect()
}

#[tauri::command]
pub async fn lead_send(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    text: String,
    lang: Option<String>,
    images: Option<Vec<ImageIn>>,
    files: Option<Vec<String>>,
) -> Result<(), String> {
    let eng = lead_engine(&app, &db, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())?;
    engine::send(&app, &db, &eng, &text, to_pairs(images), files.unwrap_or_default())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn lead_interrupt(app: AppHandle, thread_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::interrupt(&app, &eng).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Make sure the lead engine exists + its process runs (called on console open
/// so the init event delivers slash_commands without waiting for a first send).
#[tauri::command]
pub async fn lead_ensure(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    lang: Option<String>,
) -> Result<(), String> {
    let eng = lead_engine(&app, &db, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())?;
    engine::ensure_running(&app, &db, &eng).await.map_err(|e| e.to_string())
}

/// Stop the lead engine (terminal takeover: the session must have one writer).
#[tauri::command]
pub async fn lead_stop(app: AppHandle, thread_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(lead_key(thread_id)) {
        engine::stop(&app, &eng).await;
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct LeadStateInfo {
    pub state: String,
    pub queued: usize,
    pub native_id: Option<String>,
    pub slash_commands: Vec<String>,
    pub cwd: String,
}

#[tauri::command]
pub async fn lead_state(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<LeadStateInfo, String> {
    let eng = app.state::<LeadChatState>().get(lead_key(thread_id));
    match eng {
        None => Ok(LeadStateInfo {
            state: "stopped".into(),
            queued: 0,
            native_id: repo::lead_native_id(&db, thread_id).await.ok().flatten(),
            slash_commands: vec![],
            cwd: crate::paths::weft_home()
                .map(|h| h.join("leads").join(thread_id.to_string()).to_string_lossy().into_owned())
                .unwrap_or_default(),
        }),
        Some(e) => {
            let mut i = e.lock().await;
            let alive = i
                .child
                .as_mut()
                .map(|c| c.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            Ok(LeadStateInfo {
                state: if !alive {
                    "stopped"
                } else if i.turn.busy {
                    "busy"
                } else {
                    "idle"
                }
                .into(),
                queued: i.turn.queue.len(),
                native_id: i.native_id.clone(),
                slash_commands: i.slash_commands.clone(),
                cwd: i.cwd.to_string_lossy().into_owned(),
            })
        }
    }
}

#[tauri::command]
pub async fn list_lead_messages(
    db: State<'_, Db>,
    thread_id: i32,
) -> Result<Vec<crate::store::entities::lead_message::Model>, String> {
    let msgs = repo::list_lead_messages(&db, thread_id).await.map_err(|e| e.to_string())?;
    if !msgs.iter().any(|m| m.kind != "meta") {
        // Legacy thread: lazily import the old PTY lead's jsonl transcript once.
        if let Ok(n) = import_legacy(&db, thread_id).await {
            if n > 0 {
                return repo::list_lead_messages(&db, thread_id).await.map_err(|e| e.to_string());
            }
        }
    }
    Ok(msgs)
}

// ───────────────────── chat-mode workers ─────────────────────
//
// Every worker (claude/codex/opencode) runs on the engine: a weft-owned chat
// timeline in the SessionView, with per-tool wire dialects (engine::per_turn).
// Each session remains takeover-able in the user's own terminal via its
// native id.

/// Spawn (or resume) a chat-mode worker for a (direction, repo) slot: worktree
/// cwd, thread-bus MCP + ask bridge, the assembled brief as the first user
/// message of a weft-owned conversation.
#[tauri::command]
pub async fn chat_open_worker(
    app: AppHandle,
    db: State<'_, Db>,
    direction_id: i32,
    repo_id: i32,
    lang: Option<String>,
) -> Result<SessionInfo, String> {
    chat_open_worker_impl(&app, &db, direction_id, repo_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())
}

async fn chat_open_worker_impl(
    app: &AppHandle,
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    lang: &str,
) -> anyhow::Result<SessionInfo> {
    use sea_orm::EntityTrait;
    let wt = repo::worktree_for(db, direction_id, repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no materialized worktree for that direction+repo"))?;
    let dir = crate::store::entities::direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction not found"))?;
    let cwd = std::path::PathBuf::from(&wt.path);

    // Resume an earlier conversation when this slot already captured one.
    let prior = repo::latest_session_for(db, direction_id, repo_id).await?;
    let native = prior.as_ref().and_then(|s| s.native_session_id.clone());
    let resumed = native.is_some();
    let sess = match prior {
        Some(s) if s.native_session_id.is_some() => s,
        _ => repo::create_session(db, direction_id, repo_id, &dir.tool, &wt.path).await?,
    };

    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(&base, dir.thread_id, &direction_id.to_string(), &dir.tool, &cwd);
    let ask = crate::bus::inject::inject_ask_hook(&base, dir.thread_id, &direction_id.to_string(), &dir.tool, &cwd);
    let mut extra = ask.args;
    extra.extend(inj.args);

    let state = app.state::<LeadChatState>();
    let key = sess.id as i64;
    let eng = match state.get(key) {
        Some(e) => e,
        None => {
            let inner = engine::EngineInner {
                thread_id: dir.thread_id,
                tool: dir.tool.clone(),
                session_id: Some(sess.id),
                cwd,
                extra_args: extra,
                system_prompt: String::new(),
                native_id: native.clone(),
                slash_commands: vec![],
                turn: Default::default(),
                turn_id: repo::next_turn_id(db, dir.thread_id).await.unwrap_or(1) - 1,
                ask_dir: direction_id.to_string(),
                clock: Default::default(),
                child: None,
                stdin: None,
                current: None,
                interrupting: false,
                generation: 0,
            };
            let e: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
            state.get_or_insert(key, e)
        }
    };
    engine::ensure_running(app, db, &eng).await?;

    // A fresh conversation gets its brief as the opening message (the brief is
    // a message, not a system prompt).
    if !resumed {
        let mut brief = crate::brief::assemble(db, direction_id).await.unwrap_or_default();
        if !brief.trim().is_empty() {
            brief.push_str(lang_directive(lang));
            engine::send(app, db, &eng, &brief, vec![], vec![]).await?;
        }
    }
    // Dispatch enters the mandate's first phase: plan+impl workers start by
    // planning their direction (the brief says so); impl-only build right away.
    // Resume keeps whatever status the agent last reported.
    if !resumed {
        let phase = if repo::normalize_mandate(&dir.mandate) == "impl-only" {
            "working"
        } else {
            "planning"
        };
        let _ = repo::set_direction_status(db, direction_id, phase).await;
    }

    Ok(SessionInfo {
        session_id: sess.id,
        repo: wt.path.clone(),
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        resumed,
        native_id: native,
    })
}

/// Get-or-rebuild a worker's engine from its session row — so a chat worker
/// survives app restarts the same way the lead does: sending resumes it.
async fn worker_engine(app: &AppHandle, db: &Db, session_id: i32) -> anyhow::Result<EngineRef> {
    let state = app.state::<LeadChatState>();
    if let Some(e) = state.get(session_id as i64) {
        return Ok(e);
    }
    use sea_orm::EntityTrait;
    let sess = repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let dir = crate::store::entities::direction::Entity::find_by_id(sess.direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction not found"))?;
    let cwd = std::path::PathBuf::from(&sess.cwd);
    let base = app.state::<crate::BusBase>().0.clone();
    let inj = crate::bus::inject::inject(&base, dir.thread_id, &sess.direction_id.to_string(), &sess.tool, &cwd);
    let ask = crate::bus::inject::inject_ask_hook(&base, dir.thread_id, &sess.direction_id.to_string(), &sess.tool, &cwd);
    let mut extra = ask.args;
    extra.extend(inj.args);
    let inner = engine::EngineInner {
        thread_id: dir.thread_id,
        tool: sess.tool.clone(),
        session_id: Some(sess.id),
        cwd,
        extra_args: extra,
        system_prompt: String::new(),
        native_id: sess.native_session_id.clone(),
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, dir.thread_id).await.unwrap_or(1) - 1,
        ask_dir: sess.direction_id.to_string(),
        clock: Default::default(),
        child: None,
        stdin: None,
        current: None,
        interrupting: false,
        generation: 0,
    };
    let e: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    Ok(state.get_or_insert(session_id as i64, e))
}

#[tauri::command]
pub async fn chat_send(
    app: AppHandle,
    db: State<'_, Db>,
    session_id: i32,
    text: String,
    images: Option<Vec<ImageIn>>,
    files: Option<Vec<String>>,
) -> Result<(), String> {
    let eng = worker_engine(&app, &db, session_id).await.map_err(|e| e.to_string())?;
    engine::send(&app, &db, &eng, &text, to_pairs(images), files.unwrap_or_default())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn chat_interrupt(app: AppHandle, session_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::interrupt(&app, &eng).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn chat_stop(app: AppHandle, session_id: i32) -> Result<(), String> {
    if let Some(eng) = app.state::<LeadChatState>().get(session_id as i64) {
        engine::stop(&app, &eng).await;
    }
    Ok(())
}

/// One-shot import of a legacy PTY-lead transcript (the tool's own jsonl,
/// parsed by the sidecar) into lead_message rows. Best-effort: any failure
/// leaves the timeline empty — history remains reachable in a terminal.
async fn import_legacy(db: &Db, thread_id: i32) -> anyhow::Result<usize> {
    let cwd = crate::paths::weft_home()?.join("leads").join(thread_id.to_string());
    if !cwd.exists() {
        return Ok(0);
    }
    let events = crate::sidecar::read_transcript(&cwd, "claude").await;
    let mut n = 0usize;
    for e in events {
        match e {
            crate::sidecar::NormEvent::Message { role, text, .. } => {
                let content = serde_json::json!({ "text": text }).to_string();
                repo::insert_lead_message(db, thread_id, None, 1, &role, "text", &content, "complete")
                    .await?;
                n += 1;
            }
            crate::sidecar::NormEvent::Tool { name, summary, .. } => {
                let content = serde_json::json!({ "name": name, "summary": summary }).to_string();
                repo::insert_lead_message(db, thread_id, None, 1, "assistant", "tool", &content, "complete")
                    .await?;
                n += 1;
            }
        }
    }
    Ok(n)
}
