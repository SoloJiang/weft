//! The chat engine: one long-lived headless `claude -p` stream-json process per
//! timeline (lead = `-thread_id`, chat-mode worker = `session_id`). stdin takes
//! user messages; stdout is parsed (proto.rs), persisted (lead_message), and
//! pushed to the frontend over the `lead-chat` Tauri event. Interrupt rides the
//! protocol's control_request (verified live), with a kill fallback; a dead
//! process resumes losslessly via `--resume <native_id>` on the next send.

use crate::store::{repo, Db};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
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

pub struct EngineInner {
    pub thread_id: i32,
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
    pub child: Option<Child>,
    pub stdin: Option<ChildStdin>,
    /// Streaming assistant row being built: (row id, accumulated text, last DB flush).
    pub current: Option<(i32, String, std::time::Instant)>,
    /// Set while a protocol interrupt is in flight so the closing row/status
    /// reads `interrupted` instead of `error`.
    pub interrupting: bool,
    /// Bumped per spawn; stale reader tasks compare and exit.
    pub generation: u64,
}

pub type EngineRef = Arc<tokio::sync::Mutex<EngineInner>>;

/// All live chat engines, keyed by `-thread_id` (lead) or `session_id` (worker).
#[derive(Default)]
pub struct LeadChatState(pub std::sync::Mutex<HashMap<i64, EngineRef>>);

impl LeadChatState {
    pub fn get(&self, key: i64) -> Option<EngineRef> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned()
    }

    pub fn insert(&self, key: i64, eng: EngineRef) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(key, eng);
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
pub async fn ensure_running(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
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
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout not piped"))?;
    inner.child = Some(child);
    inner.generation += 1;
    inner.turn = TurnState::default();
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
    ensure_running(app, db, eng).await?;
    let mut inner = eng.lock().await;
    let thread_id = inner.thread_id;
    let sid = inner.session_id;
    let is_command = text.trim_start().starts_with('/');
    let kind = if is_command { "command" } else { "text" };
    let direct = inner.turn.try_begin_send();
    if direct {
        inner.turn_id += 1;
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
    let _ = app.emit(EVENT, Push::Message { thread_id, message: m });
    let mut outbound = text.to_string();
    if !files.is_empty() {
        outbound.push_str("\n\nAttached files (read them as needed):\n");
        for f in &files {
            outbound.push_str(&format!("- {f}\n"));
        }
    }
    let out = Outgoing { text: outbound, images };
    if direct {
        write_user(&mut inner, &out).await;
    } else {
        inner.turn.queue.push_back(out);
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

/// Stop the engine outright (e.g. before a terminal takeover).
pub async fn stop(app: &AppHandle, eng: &EngineRef) {
    let mut inner = eng.lock().await;
    inner.generation += 1; // orphan the reader so EOF handling is ours
    if let Some(c) = inner.child.as_mut() {
        let _ = c.kill().await;
    }
    inner.child = None;
    inner.stdin = None;
    inner.current = None;
    inner.turn = TurnState::default();
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
        while let Ok(Some(line)) = lines.next_line().await {
            let mut inner = eng.lock().await;
            if inner.generation != generation {
                return; // superseded by a respawn/stop
            }
            let thread_id = inner.thread_id;
            match super::proto::parse_line(&line) {
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
                    // authoritative full text (deltas can drop on the floor).
                    if !texts.is_empty() {
                        if let Some((id, _, _)) = inner.current.take() {
                            let full = texts.join("\n\n");
                            let _ = repo::update_lead_message(
                                &db, id,
                                &serde_json::json!({ "text": full }).to_string(),
                                "complete",
                            )
                            .await;
                            let _ = app.emit(EVENT, Push::Finalize {
                                thread_id, message_id: id, status: "complete".into(),
                            });
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
                        write_user(&mut inner, &next).await;
                        let _ = repo::complete_queued(&db, thread_id, &next.text).await;
                    }
                    let state = if inner.turn.busy { "busy" } else { "idle" };
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
        // EOF: the process died (or was killed). Leave history intact; the next
        // send resumes via --resume.
        let mut inner = eng.lock().await;
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
        t.queue.push_back(Outgoing { text: "second".into(), images: vec![] });
        let next = t.on_turn_end();
        assert_eq!(next.map(|o| o.text).as_deref(), Some("second"));
        assert!(t.busy); // popped → still busy
        assert!(t.on_turn_end().is_none()); // empty queue → idle
        assert!(!t.busy);
    }

    #[test]
    fn build_args_fresh_vs_resume() {
        let mut inner = EngineInner {
            thread_id: 1,
            session_id: None,
            cwd: "/tmp".into(),
            extra_args: vec!["--mcp-config".into(), "x".into()],
            system_prompt: "be lead".into(),
            native_id: None,
            slash_commands: vec![],
            turn: TurnState::default(),
            turn_id: 0,
            child: None,
            stdin: None,
            current: None,
            interrupting: false,
            generation: 0,
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
