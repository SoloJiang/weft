//! Tauri commands for the chat engine. The lead's engine is keyed by
//! `-thread_id`; chat-mode workers (phase 2) key by `session_id`.

use super::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};
use tauri::{AppHandle, Manager, State};

fn lead_key(thread_id: i32) -> i64 {
    -(thread_id as i64)
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
    let system_prompt = format!(
        "{}{}",
        crate::pty::lead_prompt(),
        crate::pty::lang_directive(lang)
    );
    let inner = engine::EngineInner {
        thread_id,
        session_id: None,
        cwd,
        extra_args: extra,
        system_prompt,
        native_id: repo::lead_native_id(db, thread_id).await.ok().flatten(),
        slash_commands: vec![],
        turn: Default::default(),
        turn_id: repo::next_turn_id(db, thread_id).await.unwrap_or(1) - 1,
        child: None,
        stdin: None,
        current: None,
        interrupting: false,
        generation: 0,
    };
    let eng: EngineRef = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    state.insert(lead_key(thread_id), eng.clone());
    Ok(eng)
}

#[tauri::command]
pub async fn lead_send(
    app: AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
    text: String,
    lang: Option<String>,
) -> Result<(), String> {
    let eng = lead_engine(&app, &db, thread_id, lang.as_deref().unwrap_or("en"))
        .await
        .map_err(|e| e.to_string())?;
    engine::send(&app, &db, &eng, &text).await.map_err(|e| e.to_string())
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
