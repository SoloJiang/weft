//! The chat engine: each timeline (lead = `-thread_id`, chat-mode worker =
//! `session_id`) runs through the selected tool stored on the thread/session.
//! Claude keeps a long-lived stream-json process; codex/opencode spawn one
//! process per turn. stdout is parsed (proto.rs), persisted (lead_message), and
//! pushed to the frontend over the `lead-chat` Tauri event. Interrupt rides the
//! tool protocol when available, with a kill fallback; a dead process resumes
//! via the stored native session id on the next send.

use crate::store::{repo, Db};
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

pub const EVENT: &str = "lead-chat";

/// Persisted activity status for a session/lead deliberately stopped by the
/// human (terminal takeover) or the runaway guard. Distinct from "idle" (a turn
/// ended cleanly) so single-writer-sensitive paths — boot revive AND coordinator
/// bus-wake delivery — can refuse to spawn a COMPETING headless process for a
/// session the human may be driving in their own terminal.
pub const STATUS_STOPPED: &str = "stopped";

/// The invisible prompt a bus wake delivers: tell the agent to drain its inbox.
/// One `bus_inbox` call reads every unread message, so a single read covers any
/// number of coalesced wakes (see `TurnState::request_bus_read`).
pub const BUS_WAKE_PROMPT: &str =
    "You have new messages on the thread bus. Call the bus_inbox tool to read them.";

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

/// Upper bound on a single resident-stdin write held under the engine lock (see
/// [`write_user`]). Generous: a healthy child drains instantly, so this only
/// trips on a wedged/dead child to keep the session from becoming unstoppable.
const WRITE_USER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
        /// Cleaned final content, set only when the streamed row differs from what
        /// was persisted (codex app-server strips `<weft:*>` sentinels AFTER they
        /// streamed raw) — the frontend replaces the row text so the tags vanish.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
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
        /// claude `system/init` 才有(随首条消息到达);其余 init 推空。
        mcp_servers: Vec<super::proto::McpServer>,
        tools: Vec<String>,
        model: Option<String>,
        window: Option<u64>,
    },
    /// The tool call currently executing — transient: rendered while it runs,
    /// replaced by the next one, cleared by the Turn event. Never persisted.
    /// Used for codex pills, which carry no input/output to expand.
    Activity {
        thread_id: i32,
        session_id: Option<i32>,
        name: String,
        summary: String,
    },
    /// 每个 turn 结束推一次当前上下文占用;window 跟随最近一次 init 的 model。
    Usage {
        thread_id: i32,
        session_id: Option<i32>,
        context_tokens: u64,
        window: Option<u64>,
        model: Option<String>,
    },
    /// A persisted `kind:"tool"` row received its result: replace the row's
    /// content (now carrying output) and status. Pairs with the earlier
    /// Push::Message that inserted the running row.
    ToolResult {
        thread_id: i32,
        message_id: i32,
        content: String,
        status: String,
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
    /// A bus wake landed while this engine was busy. Rather than queue one "read
    /// your inbox" turn per wake, we remember the wake's FIFO position — the
    /// number of messages already queued when it arrived — and synthesize a
    /// SINGLE inbox-read there. Messages queued BEFORE the wake drain first, then
    /// the read, then anything queued after (so a later user send can't jump
    /// ahead of an earlier bus message). One `bus_inbox` reads everything, so any
    /// number of wakes during a turn coalesce into this one read (`is_none`
    /// guard keeps the earliest position). No timer, never interleaved mid-turn.
    pub bus_read_pos: Option<usize>,
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

    /// A bus wake arrived. Returns true if the caller should start a read turn
    /// right now (engine idle); false means it was coalesced into the running
    /// turn and will be read at its FIFO position once the queue drains there
    /// (see `on_turn_end`). The `is_none` guard keeps the earliest wake's
    /// position so a later wake can't push the read behind newer messages.
    pub fn request_bus_read(&mut self) -> bool {
        if self.busy {
            if self.bus_read_pos.is_none() {
                self.bus_read_pos = Some(self.queue.len());
            }
            false
        } else {
            self.busy = true;
            true
        }
    }

    /// Turn finished: deliver the next thing in FIFO order. Messages queued
    /// before a coalesced bus wake drain first; when the wake's position is
    /// reached, synthesize one invisible inbox-read turn; then the rest; finally
    /// go idle.
    pub fn on_turn_end(&mut self) -> Option<Outgoing> {
        match self.bus_read_pos {
            // The wake sits at the front: read the inbox now (stays busy).
            Some(0) => {
                self.bus_read_pos = None;
                Some(Outgoing {
                    text: BUS_WAKE_PROMPT.to_string(),
                    images: vec![],
                    tracked: false,
                    origin_tag: None,
                })
            }
            // A message queued before the wake goes first; the wake slides up one.
            Some(n) => {
                let next = self.queue.pop_front();
                self.bus_read_pos = Some(n.saturating_sub(1));
                // Defensive: if the queue emptied early, read at turn-end anyway
                // rather than stranding the pending wake.
                next.or_else(|| self.on_turn_end())
            }
            None => match self.queue.pop_front() {
                Some(next) => Some(next),
                None => {
                    self.busy = false;
                    None
                }
            },
        }
    }
}

/// Per-turn dialects (codex `exec --json`, opencode `run --format json`) spawn
/// one process per human turn; only claude keeps a long-lived stream process.
pub fn per_turn(tool: &str) -> bool {
    crate::adapters::adapter_for(tool).is_some_and(|a| a.per_turn())
}

#[derive(Debug, PartialEq, Eq)]
enum HiddenDelivery {
    Noop,
    Queue,
    SpawnTurn,
    WriteResident,
}

fn hidden_delivery(tool: &str, busy: bool, has_stdin: bool, stopped: bool) -> HiddenDelivery {
    if stopped {
        HiddenDelivery::Noop
    } else if busy {
        HiddenDelivery::Queue
    } else if per_turn(tool) {
        HiddenDelivery::SpawnTurn
    } else if has_stdin {
        HiddenDelivery::WriteResident
    } else {
        HiddenDelivery::Noop
    }
}

fn mark_hidden_turn_started(inner: &mut EngineInner) -> i32 {
    let _ = inner.turn.try_begin_send();
    inner.turn_id += 1;
    inner.clock.begin_turn();
    // Plumbing starts a turn directly (not via send): keep the invariant.
    inner.current_origin_tag = None;
    inner.turn_id
}

fn reset_failed_hidden_turn(inner: &mut EngineInner, turn_id: i32) -> bool {
    if inner.turn_id != turn_id || !inner.turn.busy {
        return false;
    }
    inner.turn.busy = false;
    inner.turn.queue.clear();
    inner.clock.started = None;
    inner.current_origin_tag = None;
    inner.child = None;
    inner.stdin = None;
    inner.current = None;
    inner.interrupting = false;
    true
}

fn emit_finalize(app: &AppHandle, thread_id: i32, message_id: i32, status: &str) {
    let _ = app.emit(
        EVENT,
        Push::Finalize {
            thread_id,
            message_id,
            status: status.into(),
            content: None,
        },
    );
}

async fn mark_queued_delivered(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    out: &Outgoing,
) {
    if !out.tracked {
        return;
    }
    match repo::complete_queued(db, thread_id, session_id).await {
        Ok(Some(m)) => emit_finalize(app, thread_id, m.id, "complete"),
        Ok(None) => {}
        Err(e) => eprintln!("[weft] queued message complete failed: {e}"),
    }
}

async fn mark_queued_status(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    status: &str,
) {
    match repo::set_queued_status(db, thread_id, session_id, status).await {
        Ok(rows) => {
            for m in rows {
                emit_finalize(app, thread_id, m.id, status);
            }
        }
        Err(e) => eprintln!("[weft] queued message {status} finalize failed: {e}"),
    }
}

async fn mark_queued_failed(app: &AppHandle, db: &Db, thread_id: i32, session_id: Option<i32>) {
    mark_queued_status(app, db, thread_id, session_id, "error").await;
}

fn emit_turn_state(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    busy: bool,
    queued: usize,
) {
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id,
            state: if busy { "busy" } else { "idle" }.into(),
            queued,
        },
    );
}

async fn begin_hidden_turn(app: &AppHandle, db: &Db, inner: &mut EngineInner) -> i32 {
    let turn_id = mark_hidden_turn_started(inner);
    crate::power::on_turn_began(app);
    // Hidden delivery is a turn-start too, so persist `running`; otherwise a
    // crash mid-action can leave stale `idle` state and skip boot revive.
    persist_activity(db, inner.session_id, inner.thread_id, "running").await;
    emit_turn_state(
        app,
        inner.thread_id,
        inner.session_id,
        inner.turn.busy,
        inner.turn.queue.len(),
    );
    turn_id
}

fn queue_hidden_delivery(app: &AppHandle, inner: &mut EngineInner, out: Outgoing) {
    inner.turn.queue.push_back(out);
    emit_turn_state(
        app,
        inner.thread_id,
        inner.session_id,
        inner.turn.busy,
        inner.turn.queue.len(),
    );
}

async fn rollback_failed_turn(app: &AppHandle, db: &Db, eng: &EngineRef, turn_id: i32) {
    let mut inner = eng.lock().await;
    if !reset_failed_hidden_turn(&mut inner, turn_id) {
        return;
    }
    let thread_id = inner.thread_id;
    let session_id = inner.session_id;
    persist_activity(db, session_id, thread_id, "idle").await;
    emit_turn_state(app, thread_id, session_id, false, 0);
    drop(inner);
    mark_queued_failed(app, db, thread_id, session_id).await;
}

async fn rollback_failed_visible_turn(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    turn_id: i32,
    message_id: i32,
    content: &str,
) {
    let thread_id = { eng.lock().await.thread_id };
    let _ = repo::update_lead_message(db, message_id, content, "error").await;
    emit_finalize(app, thread_id, message_id, "error");
    rollback_failed_turn(app, db, eng, turn_id).await;
}

/// Finalize tool rows still awaiting a result, marking each `status` and pushing
/// the update. Called wherever a turn ends — clean TurnEnd, stop/takeover,
/// runaway kill, or process EOF — so a `tool_use` whose `tool_result` never
/// arrived stops spinning in the timeline and the DB.
async fn finalize_orphan_tool_rows(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    rows: Vec<(i32, serde_json::Value)>,
    status: &str,
) {
    for (row_id, content) in rows {
        let content_str = content.to_string();
        let _ = repo::update_lead_message(db, row_id, &content_str, status).await;
        let _ = app.emit(
            EVENT,
            Push::ToolResult {
                thread_id,
                message_id: row_id,
                content: content_str,
                status: status.into(),
            },
        );
    }
}

/// Persisted status for a tool row. A row with output is terminal
/// (error/complete). A running row streams ONLY if it carries an id to correlate
/// its later result by — an id-less running row is stored complete so it can't
/// spin forever (nothing could ever fill it).
fn tool_row_status(has_output: bool, trackable: bool, is_error: bool) -> &'static str {
    if has_output {
        if is_error {
            "error"
        } else {
            "complete"
        }
    } else if trackable {
        "streaming"
    } else {
        "complete"
    }
}

/// Persist a turn's tool calls as `kind:"tool"` rows (running until their result
/// arrives). Shared by spawn_reader (claude/exec) and codex_consumer (app-server).
async fn persist_tool_calls(
    app: &AppHandle,
    db: &Db,
    inner: &mut EngineInner,
    tools: Vec<super::proto::ToolCall>,
) {
    let thread_id = inner.thread_id;
    for call in tools {
        let (sid, turn) = (inner.session_id, inner.turn_id);
        let running = call.output.is_none();
        let trackable = running && !call.id.is_empty();
        let status = tool_row_status(!running, trackable, call.is_error);
        let content = serde_json::json!({
            "name": call.name,
            "summary": call.summary,
            "input": call.input,
            "output": call.output.unwrap_or_default(),
            "is_error": call.is_error,
        });
        let content_str = content.to_string();
        let call_id = call.id;
        match repo::insert_lead_message(
            db,
            thread_id,
            sid,
            turn,
            "assistant",
            "tool",
            &content_str,
            status,
        )
        .await
        {
            Ok(m) => {
                let row_id = m.id;
                let _ = app.emit(
                    EVENT,
                    Push::Message {
                        thread_id,
                        message: m,
                    },
                );
                if trackable {
                    inner.tool_rows.insert(call_id, (row_id, content));
                }
            }
            Err(e) => eprintln!("[weft] lead tool row insert failed: {e}"),
        }
    }
}

/// Merge tool results into their running rows (claude tool_result / codex
/// item.completed); a result for an untracked row is dropped.
async fn merge_tool_results(
    app: &AppHandle,
    db: &Db,
    inner: &mut EngineInner,
    items: Vec<super::proto::ToolResultItem>,
) {
    let thread_id = inner.thread_id;
    for item in items {
        let Some((row_id, mut content)) = inner.tool_rows.remove(&item.id) else {
            continue;
        };
        if let Some(obj) = content.as_object_mut() {
            obj.insert("output".into(), item.output.into());
            obj.insert("is_error".into(), item.is_error.into());
        }
        let status = if item.is_error { "error" } else { "complete" };
        let content_str = content.to_string();
        let _ = repo::update_lead_message(db, row_id, &content_str, status).await;
        let _ = app.emit(
            EVENT,
            Push::ToolResult {
                thread_id,
                message_id: row_id,
                content: content_str,
                status: status.into(),
            },
        );
    }
}

/// Persist / answer the `<weft:*>` sentinels forked out of a finalized assistant
/// message — action_card becomes its own row, list_repos triggers a hidden
/// stdin-style reply. Errors are logged but never abort the stream. Shared by the
/// exec/claude reader and the codex app-server consumer so both transports render
/// action cards and answer list_repos.
async fn apply_lead_sentinels(
    app: &AppHandle,
    db: &Db,
    inner: &mut EngineInner,
    thread_id: i32,
    sentinels: Vec<super::sentinels::Sentinel>,
) {
    for s in sentinels {
        match s {
            super::sentinels::Sentinel::ActionCard(json) => {
                // Reject anything that isn't a JSON object so the UI can rely on
                // `card.title / actions / …`.
                match serde_json::from_str::<serde_json::Value>(&json) {
                    Ok(v) if v.is_object() => {
                        let (sid, turn) = (inner.session_id, inner.turn_id);
                        match repo::insert_lead_message(
                            db,
                            thread_id,
                            sid,
                            turn,
                            "assistant",
                            "action_card",
                            &json,
                            "complete",
                        )
                        .await
                        {
                            Ok(m) => {
                                let _ = app.emit(
                                    EVENT,
                                    Push::Message {
                                        thread_id,
                                        message: m,
                                    },
                                );
                            }
                            Err(e) => {
                                eprintln!("[weft] lead sentinel: insert action_card failed: {e}")
                            }
                        }
                    }
                    Ok(_) => eprintln!(
                        "[weft] lead sentinel: action_card payload is not an object — dropped"
                    ),
                    Err(e) => eprintln!("[weft] lead sentinel: action_card JSON parse failed: {e}"),
                }
            }
            super::sentinels::Sentinel::ListRepos => {
                // Look up workspace via the thread row (engine doesn't cache it; one
                // extra query per call is cheap and avoids a wider refactor).
                let ws_id = match repo::get_thread(db, thread_id).await {
                    Ok(Some(t)) => Some(t.workspace_id),
                    Ok(None) => {
                        eprintln!(
                            "[weft] lead sentinel: list_repos — thread {thread_id} not found"
                        );
                        None
                    }
                    Err(e) => {
                        eprintln!("[weft] lead sentinel: list_repos — get_thread failed: {e}");
                        None
                    }
                };
                if let Some(workspace_id) = ws_id {
                    let repos = match repo::list_repos(db, workspace_id).await {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("[weft] lead sentinel: list_repos query failed: {e}");
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
                    let reply = format!("<weft:list_repos_result>{body}</weft:list_repos_result>");
                    // Invisible plumbing: tracked=false keeps this off the timeline;
                    // the agent reads it as a tool-result-style user turn.
                    let out = Outgoing {
                        text: reply,
                        images: Vec::new(),
                        tracked: false,
                        origin_tag: None,
                    };
                    queue_hidden_delivery(app, inner, out);
                }
            }
        }
    }
}

/// Finalize the open streaming text row (codex app-server): fork `<weft:*>`
/// sentinels out of the body on a clean finish, persist the cleaned text, close
/// its IM streaming card, and clear `inner.current`. Called both at a tool
/// boundary — so post-tool deltas open a NEW row BELOW the tool, keeping inline
/// tool history in order — and at turn end. No-op when no row is open.
async fn finalize_current_text(app: &AppHandle, db: &Db, inner: &mut EngineInner, status: &str) {
    let Some((id, text, _)) = inner.current.take() else {
        return;
    };
    let thread_id = inner.thread_id;
    let origin_tag = inner.current_origin_tag.clone();
    // `stripped` = the cleaned body differs from what streamed (sentinels removed),
    // so the live row still shows the raw tags and must be replaced, not just status.
    let (clean, stripped) = if status == "complete" {
        let (clean, sentinels) = super::sentinels::extract_sentinels(&text);
        let stripped = clean != text;
        apply_lead_sentinels(app, db, inner, thread_id, sentinels).await;
        (clean, stripped)
    } else {
        (text, false)
    };
    let _ = repo::update_lead_message(
        db,
        id,
        &serde_json::json!({ "text": clean }).to_string(),
        status,
    )
    .await;
    let _ = app.emit(
        EVENT,
        Push::Finalize {
            thread_id,
            message_id: id,
            status: status.into(),
            content: stripped.then(|| clean.clone()),
        },
    );
    if status == "complete" {
        emit_lead_out(app, thread_id, id, &clean, origin_tag);
    }
}

async fn cleanup_disconnected_turn(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    fallback_status: &str,
) {
    let mut inner = eng.lock().await;
    if !inner.turn.busy
        && inner.current.is_none()
        && inner.turn.queue.is_empty()
        && inner.tool_rows.is_empty()
    {
        return;
    }
    let thread_id = inner.thread_id;
    let session_id = inner.session_id;
    let had_busy_turn = inner.turn.busy;
    let turn_id = inner.turn_id;
    let status = if inner.interrupting {
        "interrupted"
    } else {
        fallback_status
    };
    let current = inner.current.take().map(|(id, text, _)| (id, text));
    let orphan_tools: Vec<(i32, serde_json::Value)> =
        inner.tool_rows.drain().map(|(_, v)| v).collect();
    inner.interrupting = false;
    inner.child = None;
    inner.stdin = None;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    inner.current_origin_tag = None;
    inner.stopped = true;
    persist_activity(db, session_id, thread_id, "stopped").await;
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id,
            state: "stopped".into(),
            queued: 0,
        },
    );
    drop(inner);
    if let Ok(Some(row)) = persist_disconnected_turn_row(
        db,
        thread_id,
        session_id,
        turn_id,
        status,
        had_busy_turn,
        current,
    )
    .await
    {
        match row {
            DisconnectedTurnRow::Finalized { message_id } => {
                emit_finalize(app, thread_id, message_id, status);
            }
            DisconnectedTurnRow::Inserted(message) => {
                let _ = app.emit(EVENT, Push::Message { thread_id, message });
            }
        }
    }
    finalize_orphan_tool_rows(app, db, thread_id, orphan_tools, status).await;
    mark_queued_status(app, db, thread_id, session_id, status).await;
}

enum DisconnectedTurnRow {
    Finalized { message_id: i32 },
    Inserted(crate::store::entities::lead_message::Model),
}

async fn persist_disconnected_turn_row(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    turn_id: i32,
    status: &str,
    had_busy_turn: bool,
    current: Option<(i32, String)>,
) -> anyhow::Result<Option<DisconnectedTurnRow>> {
    if let Some((id, text)) = current {
        let _ = repo::update_lead_message(
            db,
            id,
            &serde_json::json!({ "text": text }).to_string(),
            status,
        )
        .await;
        return Ok(Some(DisconnectedTurnRow::Finalized { message_id: id }));
    }
    if had_busy_turn {
        if let Some(message) =
            insert_terminal_assistant_if_missing(db, thread_id, session_id, turn_id, status).await?
        {
            return Ok(Some(DisconnectedTurnRow::Inserted(message)));
        }
    }
    Ok(None)
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
    /// Per-session command pin (from thread.lead_command / session.command).
    /// None = resolve the spawn binary from the global tool→command override map;
    /// Some = this session was frozen to a specific command (alias opt-out).
    pub command: Option<String>,
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
    /// Set when the tool's command override (alias) changed under this live
    /// engine; the next send silently bounces the resident process / codex client
    /// so it respawns from the new command. Like `pending_skill_refresh`, invisible
    /// to the UI.
    pub pending_command_refresh: bool,
    /// 会话信息面板的最近快照,供 lead_state / session_for 重挂回填(claude:init
    /// 解析出 mcp/model/window,turn 结束更新 context_tokens)。
    pub last_context_tokens: Option<u64>,
    pub last_model: Option<String>,
    pub last_window: Option<u64>,
    pub last_mcp_servers: Vec<super::proto::McpServer>,
    pub last_tools: Vec<String>,
    /// Opaque tag of the turn whose output is currently being emitted. Set at
    /// every turn-start (including None turns) so a prior concierge reply target
    /// never leaks into a later non-IM turn. Stamped onto each emitted frame.
    pub current_origin_tag: Option<String>,
    /// Maps an in-flight tool call's id (claude `tool_use_id` / codex item id) to
    /// its persisted `kind:"tool"` row id and content JSON, so the out-of-band
    /// result merges its output without re-reading the row. Cleared per turn.
    pub tool_rows: std::collections::HashMap<String, (i32, serde_json::Value)>,
    /// Explicit user/guard stop. Hidden plumbing must not resurrect stopped
    /// engines; explicit sends/ensure clear this and restart as needed.
    pub stopped: bool,
    /// Per-session `codex app-server` connection (app-server transport only),
    /// spawned lazily on the first turn with this session's `-c mcp_servers` args.
    pub codex_client: Option<crate::codex_app_server::Client>,
}

pub type EngineRef = Arc<tokio::sync::Mutex<EngineInner>>;

/// All live chat engines, keyed by `-thread_id` (lead) or `session_id` (worker).
///
/// A [`DashMap`] (sharded, lock-free at the map level) rather than a
/// `Mutex<HashMap>`: every accessor returns a cloned [`EngineRef`] (an `Arc`), so
/// there is NO map-wide guard a caller could accidentally hold across an
/// `eng.lock().await` — the audit's #1 fragility (registry guard held across the
/// per-engine async lock) is structurally impossible here. The only rule for
/// callers is the natural one DashMap already enforces: don't keep a per-entry
/// `Ref`/`RefMut` alive across an `.await` (clone the value out and drop it).
#[derive(Default)]
pub struct LeadChatState(pub DashMap<i64, EngineRef>);

impl LeadChatState {
    pub fn get(&self, key: i64) -> Option<EngineRef> {
        self.0.get(&key).map(|r| r.value().clone())
    }

    /// Atomic get-or-insert: concurrent constructors (e.g. React StrictMode's
    /// double-mount firing two ensures) must converge on ONE engine — a lost
    /// race would orphan a duplicate headless process writing the same session.
    /// DashMap's `entry` takes the shard lock for the get-or-insert, so this stays
    /// race-free; the `RefMut` is dropped at the end of the statement.
    pub fn get_or_insert(&self, key: i64, eng: EngineRef) -> EngineRef {
        self.0.entry(key).or_insert(eng).value().clone()
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

/// Spawn the resident process if it isn't alive, under the CALLER's already-held
/// lock. Returns the new child's stdout + generation when a process was spawned
/// (the caller must `spawn_reader` after it drops the lock), or `None` when no
/// spawn was needed (stopped, per-turn, or already alive). Keeping the spawn
/// under one continuous lock lets a caller reserve a turn slot atomically with
/// ensuring the process — no window for a racing send to slip a turn in.
async fn ensure_running_locked(
    inner: &mut EngineInner,
) -> anyhow::Result<Option<(tokio::process::ChildStdout, u64)>> {
    if inner.stopped {
        return Ok(None);
    }
    if per_turn(&inner.tool) {
        return Ok(None);
    }
    if inner.tool != "claude" {
        anyhow::bail!("unknown lead tool {}", inner.tool);
    }
    if let Some(c) = inner.child.as_mut() {
        if c.try_wait().ok().flatten().is_none() {
            return Ok(None); // alive
        }
    }
    crate::claude::ensure_trusted(&inner.cwd);
    // Resolve the actual binary: a per-session pin, else the global override for
    // "claude" (e.g. a user-aliased `cc-claude`), else "claude" itself.
    let program = crate::tool_command::effective(inner.command.as_deref(), &inner.tool);
    let mut child = Command::new(&program)
        .args(build_args(inner))
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
    Ok(Some((stdout, inner.generation)))
}

/// Spawn the process if it isn't alive (fresh or `--resume`), wiring the reader.
/// Per-turn dialects have no resident process — sending spawns one per turn.
pub async fn ensure_running(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    let reader = ensure_running_locked(&mut inner).await?;
    drop(inner);
    if let Some((stdout, generation)) = reader {
        spawn_reader(app.clone(), db.clone(), eng.clone(), stdout, generation);
    }
    Ok(())
}

pub async fn ensure_running_for_send(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
) -> anyhow::Result<()> {
    eng.lock().await.stopped = false;
    ensure_running(app, db, eng).await
}

/// Drop the resident child and its stdin so the next send respawns a clean
/// process. Used when a write fails or times out mid-line: a partial JSON
/// message may be stuck in the old stdin pipe, so reusing that pipe would
/// corrupt the next turn (the next message concatenates onto the prefix), and
/// the child is evidently wedged or dead. Killing it (and clearing the handles)
/// makes `ensure_running_locked` respawn on the next send. `kill_on_drop` would
/// also reap it, but `start_kill` makes the intent explicit and immediate.
pub(crate) fn invalidate_resident(inner: &mut EngineInner) {
    // Orphan the existing reader (mirrors `stop_quiet`): once we kill the child,
    // its stdout EOFs and the reader wakes; without bumping the generation it
    // would still see itself as current and process that EOF/buffered output as
    // the live turn — emitting a spurious stopped/error or clearing the queue of
    // the freshly respawned process. The generation bump makes it exit instead.
    inner.generation += 1;
    inner.stdin = None;
    if let Some(mut child) = inner.child.take() {
        let _ = child.start_kill();
    }
}

pub(crate) async fn write_user(inner: &mut EngineInner, out: &Outgoing) -> anyhow::Result<()> {
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
    let line = format!("{msg}\n");
    let Some(stdin) = inner.stdin.as_mut() else {
        return Err(anyhow::anyhow!("resident stdin is unavailable"));
    };
    // Time-box the write. This runs while the caller holds the engine lock, so an
    // unbounded write to a child that has stopped reading its stdin would pin the
    // lock forever — wedging stop/interrupt/status for the whole session. A live
    // child (we only write-through when it's idle and reading) drains in
    // microseconds; the timeout only fires on a genuinely stuck process.
    let res = tokio::time::timeout(WRITE_USER_TIMEOUT, async {
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await
    })
    .await;
    match res {
        Ok(Ok(())) => Ok(()),
        // Either failure mode can leave a partial line in the pipe and an
        // unresponsive child. Invalidate the resident process so the next send
        // respawns clean instead of appending to a corrupt prefix or re-targeting
        // a wedged child; the caller rolls the turn back.
        Ok(Err(e)) => {
            invalidate_resident(inner);
            Err(anyhow::Error::new(e).context("resident stdin write failed"))
        }
        Err(_) => {
            invalidate_resident(inner);
            Err(anyhow::anyhow!(
                "resident stdin write timed out (child not reading)"
            ))
        }
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
    // Skill refresh is only ever flagged while idle, so it bounces now. A command
    // refresh (alias change) can be flagged mid-turn; defer its bounce until the
    // engine is idle so a follow-up sent during a running turn just queues (never
    // kills the in-flight turn / clears its rows) — the bounce fires on the next
    // idle send instead.
    let (skill_pending, cmd_now) = {
        let g = eng.lock().await;
        (
            g.pending_skill_refresh,
            g.pending_command_refresh && !g.turn.busy,
        )
    };
    if skill_pending || cmd_now {
        let (tid, _sid, _current, orphans) = stop_quiet(eng).await;
        {
            let mut g = eng.lock().await;
            g.pending_skill_refresh = false;
            if cmd_now {
                g.pending_command_refresh = false;
            }
        }
        // The bounce fires from idle, so orphans is normally empty; finalize
        // defensively so a still-open tool row can't outlive the bounce.
        finalize_orphan_tool_rows(app, db, tid, orphans, "interrupted").await;
    }
    ensure_running_for_send(app, db, eng).await?;
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
    // codex (app-server): a per-session connection drives the turn after the lock
    // drops (streaming via item/agentMessage/delta). system_prompt is prepended to
    // a new thread's first turn; per-thread bus MCP rides the connection's own
    // `-c mcp_servers` spawn args. Falls back to exec if the app-server is
    // unreachable.
    let is_codex_appserver = inner.tool == "codex" && codex_appserver_enabled();
    let spawn_now = direct && per_turn(&inner.tool) && !is_codex_appserver;
    if direct && !spawn_now && !is_codex_appserver {
        if let Err(e) = write_user(&mut inner, &out).await {
            drop(inner);
            rollback_failed_visible_turn(app, db, eng, turn, row_id, &content).await;
            return Err(e);
        }
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
        if let Err(e) = spawn_turn(app.clone(), db.clone(), eng.clone(), out).await {
            rollback_failed_visible_turn(app, db, eng, turn, row_id, &content).await;
            return Err(e);
        }
    } else if direct && is_codex_appserver {
        if let Err(e) = spawn_codex_turn_or_exec(app.clone(), db.clone(), eng.clone(), out).await {
            rollback_failed_visible_turn(app, db, eng, turn, row_id, &content).await;
            return Err(e);
        }
    }
    Ok(())
}

/// Codex app-server transport selector (default ON; `WEFT_CODEX_APPSERVER=0` →
/// exec). See [`crate::adapters::codex_prefers_appserver`].
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
    let (native, cwd, sid, thread_id_i, system_prompt, extra_args, existing, program) = {
        let i = eng.lock().await;
        (
            i.native_id.clone(),
            i.cwd.clone(),
            i.session_id,
            i.thread_id,
            i.system_prompt.clone(),
            i.extra_args.clone(),
            i.codex_client.clone(),
            // Effective codex binary for THIS session: a per-session pin wins over
            // the global override, so a pinned (opt-out) codex session keeps its
            // command even on the default app-server transport.
            crate::tool_command::effective(i.command.as_deref(), &i.tool),
        )
    };
    // Per-session app-server: reuse the engine's connection or spawn one with this
    // session's `-c mcp_servers` bus flags. Its own process keeps the per-thread
    // MCP isolated (app-server MCP is app-scoped).
    let client = match existing {
        Some(c) if c.is_alive().await => c,
        _ => {
            // Pre-accept folder trust (like the exec adapter's prepare) so the
            // app-server's first thread/start doesn't block on codex's trust prompt.
            crate::codex::ensure_codex_trusted(&cwd);
            let c =
                crate::codex_app_server::Client::connect_session(&program, &extra_args, &cwd)
                    .await?;
            eng.lock().await.codex_client = Some(c.clone());
            c
        }
    };
    let cwd = cwd.to_string_lossy().into_owned();
    let had_native = native.is_some();
    let (thread, freshly_started) = match native {
        Some(t) => (t, false),
        // Don't commit the native id yet: if `turn/start` below fails and we fall
        // back to exec, a None native id lets exec start fresh WITH the system
        // prompt prepended, instead of resuming an empty thread that never got it.
        None => (client.start_thread(&cwd).await?, true),
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
    // codex has no thread/start system-prompt field, so (like the exec adapter)
    // the prompt is prepended to the FIRST turn of a brand-new thread; a resumed
    // thread already carries it in conversation history.
    let first_text = codex_first_turn_text(&system_prompt, &out.text, had_native);
    let turn = client.start_turn(&thread, &first_text).await?;
    client.set_active_turn(&thread, &turn).await;
    // The turn is in flight, so the thread is real and carries the system prompt:
    // now it's safe to persist the native id (a later resume reuses this rollout).
    if freshly_started {
        eng.lock().await.native_id = Some(thread.clone());
        if let Some(sid) = sid {
            let _ = repo::set_session_native_id(&db, sid, &thread).await;
        } else {
            let _ = repo::set_lead_native_id(&db, thread_id_i, &thread).await;
        }
    }
    // Stop pressed while turn/start was in flight? interrupt() ran before the turn
    // id existed (no active turn, no exec child), so it was a no-op — honor it now
    // that the turn is recorded, instead of letting it run despite the user.
    if eng.lock().await.interrupting {
        let _ = client.interrupt(&thread, &turn).await;
    }
    Ok(())
}

/// Start a codex turn on the app-server, falling back to exec per-turn if the
/// app-server can't be reached (the native id is shared with exec's rollout, so
/// resume is seamless). The caller must have already begun the turn (busy/turn_id).
async fn spawn_codex_turn_or_exec(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    out: Outgoing,
) -> anyhow::Result<()> {
    if let Err(e) = spawn_codex_turn(app.clone(), db.clone(), eng.clone(), out.clone()).await {
        // Stop pressed while the app-server start was pending and it then errored:
        // don't resurrect the canceled turn on exec — propagate so the caller rolls
        // it back (otherwise the interrupted turn runs anyway on the fallback).
        if eng.lock().await.interrupting {
            return Err(e);
        }
        // Tear down the half-started app-server first: thread/start + the consumer
        // subscription may already be live, and a lingering consumer (single-thread
        // routing) could finalize/reset the exec fallback turn or break thread-less
        // routing on the next retry. shutdown() drops the child + closes the consumer.
        let stale = eng.lock().await.codex_client.take();
        if let Some(c) = stale {
            c.shutdown().await;
        }
        eprintln!("[weft][codex] app-server unavailable ({e}) — falling back to exec");
        spawn_turn(app, db, eng, out).await?;
    }
    Ok(())
}

/// codex has no thread/start system-prompt field, so — exactly like the exec
/// adapter (`CodexExecAdapter::build_argv`) — the engine's `system_prompt` is
/// prepended to the FIRST turn of a brand-new thread. A resumed thread already
/// carries it in history, so it's added only when `!had_native`.
fn codex_first_turn_text(system_prompt: &str, message: &str, had_native: bool) -> String {
    if !had_native && !system_prompt.is_empty() {
        format!("{system_prompt}\n\n{message}")
    } else {
        message.to_string()
    }
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
    // server-request id → AskRegistry ask id, for the in-flight Needs-you cards
    // this session opened. Shared (lock-free) with the per-ask reply tasks so a
    // later serverRequest/resolved can cancel the right card. Cleared when answered.
    let pending_asks: Arc<crossbeam_skiplist::SkipMap<String, u64>> =
        Arc::new(crossbeam_skiplist::SkipMap::new());
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
                // Codex streams text via deltas; non-text items are tool calls →
                // inline `kind:"tool"` rows, filled by their item.completed result.
                let mut inner = eng.lock().await;
                inner.clock.last_activity = std::time::Instant::now();
                // Close any open text row BEFORE the tool row so later deltas open a
                // fresh row BELOW it — keeps "I'll inspect…" → command → explanation
                // flows in order instead of stacking post-tool prose above the tool.
                if !tools.is_empty() {
                    finalize_current_text(&app, &db, &mut inner, "complete").await;
                }
                persist_tool_calls(&app, &db, &mut inner, tools).await;
            }
            ThreadMsg::Event(ChatEvent::ToolResults { items }) => {
                let mut inner = eng.lock().await;
                merge_tool_results(&app, &db, &mut inner, items).await;
            }
            ThreadMsg::Event(ChatEvent::Usage {
                context_tokens,
                window,
            }) => {
                // app-server's current-context usage (last.inputTokens + window):
                // the accurate Context-panel value codex exec couldn't give.
                let mut inner = eng.lock().await;
                inner.last_context_tokens = Some(context_tokens);
                if window.is_some() {
                    inner.last_window = window;
                }
                let (thread_id, session_id) = (inner.thread_id, inner.session_id);
                let _ = app.emit(
                    EVENT,
                    Push::Usage {
                        thread_id,
                        session_id,
                        context_tokens,
                        window: inner.last_window,
                        model: inner.last_model.clone(),
                    },
                );
            }
            ThreadMsg::Event(ChatEvent::TurnEnd {
                is_error,
                context_tokens,
            }) => {
                let mut inner = eng.lock().await;
                let thread_id = inner.thread_id;
                let session_id = inner.session_id;
                if let Some(ct) = context_tokens {
                    inner.last_context_tokens = Some(ct);
                    let _ = app.emit(
                        EVENT,
                        Push::Usage {
                            thread_id,
                            session_id,
                            context_tokens: ct,
                            window: inner.last_window,
                            model: inner.last_model.clone(),
                        },
                    );
                }
                let status = if inner.interrupting {
                    "interrupted"
                } else if is_error {
                    "error"
                } else {
                    "complete"
                };
                inner.interrupting = false;
                // An interrupted/failed turn can leave a tool row whose
                // item.completed never arrived; finalize it so it stops spinning.
                let orphans: Vec<(i32, serde_json::Value)> =
                    inner.tool_rows.drain().map(|(_, v)| v).collect();
                finalize_orphan_tool_rows(&app, &db, thread_id, orphans, status).await;
                if inner.current.is_some() {
                    // Finalize the open text row (forks <weft:*> sentinels out on a
                    // clean finish, closes its IM card) — same helper the tool
                    // boundary uses, so the final segment is handled identically.
                    finalize_current_text(&app, &db, &mut inner, status).await;
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
                let next_turn_id = if next.is_some() {
                    inner.turn_id += 1;
                    Some(inner.turn_id)
                } else {
                    None
                };
                let still_busy = inner.turn.busy;
                persist_activity(
                    &db,
                    inner.session_id,
                    thread_id,
                    if still_busy { "running" } else { "idle" },
                )
                .await;
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
                // This turn is over: drop its active-turn id so a subsequent
                // interrupt won't target a finished turn (the flush below re-sets
                // it for the next turn).
                client.clear_active_turn(&thread).await;
                // Flush: start the next queued message as a fresh turn on this thread.
                if let (Some(n), Some(turn_id)) = (next, next_turn_id) {
                    match client.start_turn(&thread, &n.text).await {
                        Ok(t) => {
                            mark_queued_delivered(&app, &db, thread_id, session_id, &n).await;
                            client.set_active_turn(&thread, &t).await;
                            // Stop pressed during this flush's start_turn? interrupt()
                            // had no active turn id to act on (same window as a direct
                            // send) — honor it now that the turn is recorded.
                            if eng.lock().await.interrupting {
                                let _ = client.interrupt(&thread, &t).await;
                            }
                        }
                        // App-server died/rejecting between turns: don't drop the
                        // queued message — fall back to the same exec path a direct
                        // send uses (native id is shared, so resume is seamless).
                        Err(e) => {
                            // A Stop during the failed start: roll the queued turn back
                            // interrupted instead of resurrecting it on exec.
                            if eng.lock().await.interrupting {
                                rollback_failed_turn(&app, &db, &eng, turn_id).await;
                            } else {
                                eprintln!(
                                    "[weft][codex] flush via app-server failed ({e}); trying exec"
                                );
                                // Take + shut down the (closing) client first — same as
                                // the direct-send fallback — so THIS consumer sees it's
                                // superseded (ptr_eq) and skips cleanup, instead of
                                // racing spawn_turn and resetting the exec turn.
                                let stale = eng.lock().await.codex_client.take();
                                if let Some(c) = stale {
                                    c.shutdown().await;
                                }
                                match spawn_turn(app.clone(), db.clone(), eng.clone(), n.clone())
                                    .await
                                {
                                    Ok(()) => {
                                        mark_queued_delivered(&app, &db, thread_id, session_id, &n)
                                            .await;
                                    }
                                    Err(e2) => {
                                        eprintln!("[weft][codex] exec fallback for queued turn failed: {e2}");
                                        rollback_failed_turn(&app, &db, &eng, turn_id).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            ThreadMsg::Event(_) => {}
            ThreadMsg::Heartbeat => {
                // outputDelta from a long-running command: no row change, just keep
                // the turn alive so the idle watchdog doesn't reap it mid-output.
                eng.lock().await.clock.last_activity = std::time::Instant::now();
            }
            ThreadMsg::Approval { id, method, params } => {
                // An approval (command / file-change / permissions) — route to Weft's
                // Ask Bridge (the same Needs-you the exec path uses), then reply with
                // the SHAPE that kind wants: permissions `{permissions}` (omitted =
                // denied), everything else `{decision}`. Elicitation / other server
                // asks never reach here — they're declined in the read_loop.
                let is_perm = method.contains("permissions");
                let (thread_id, dir) = {
                    let i = eng.lock().await;
                    (i.thread_id, i.ask_dir.clone())
                };
                // command/cwd may sit at the top level (commandExecution ask) or
                // nested under `item` (the generic permissions ask) — read both.
                let cmd = params["command"]
                    .as_str()
                    .or_else(|| params["item"]["command"].as_str());
                let is_cmd = method.contains("commandExecution") || cmd.is_some();
                let net = params
                    .get("networkApprovalContext")
                    .or_else(|| params["item"].get("networkApprovalContext"))
                    .filter(|v| !v.is_null());
                let has_changes = params["changes"]
                    .as_array()
                    .or_else(|| params["item"]["changes"].as_array())
                    .is_some_and(|c| !c.is_empty());
                // Requested permission profile (also echoed back as the grant on allow).
                let requested = params
                    .get("permissions")
                    .or_else(|| params["item"].get("permissions"))
                    .or_else(|| params["params"].get("permissions"))
                    .filter(|v| !v.is_null())
                    .cloned();
                // Network FIRST: a network-only ask arrives as a commandExecution
                // approval (so is_cmd is true) with the command omitted, so the cmd
                // branch would otherwise mislabel + Always-key it as Bash.
                let (tool, summary) = if let Some(net) = net {
                    let host = net["host"]
                        .as_str()
                        .or_else(|| net["url"].as_str())
                        .or_else(|| net["domain"].as_str())
                        .unwrap_or("network");
                    ("Network", format!("network access: {host}"))
                } else if is_cmd {
                    ("Bash", cmd.unwrap_or("(command)").to_string())
                } else if has_changes {
                    // Include the changed path(s): the AskRegistry keys Always rules
                    // by (thread, dir, summary), so a constant "apply file changes"
                    // would let one Always blanket-allow every later edit.
                    ("Edit", codex_change_approval_summary(&params))
                } else {
                    // A permission escalation — key it by the REQUESTED scope, else an
                    // Always for one profile silently grants a later, different one.
                    let scope = requested
                        .as_ref()
                        .map(|v| v.to_string().chars().take(120).collect::<String>())
                        .unwrap_or_else(|| "(unspecified)".to_string());
                    ("Permission", format!("permission: {scope}"))
                };
                let detail = params["cwd"]
                    .as_str()
                    .or_else(|| params["item"]["cwd"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let registry = app.state::<crate::ask::AskRegistry>().inner().clone();
                match registry.auto_decision(thread_id, &dir, &summary) {
                    // dangerous mode / full access / always-allow: reply inline (fast).
                    Some(d) => {
                        let allow = matches!(d, crate::ask::Decision::Allow);
                        let _ = client
                            .reply_result(
                                &id,
                                crate::codex_app_server::codex_approval_reply(is_perm, allow, requested),
                            )
                            .await;
                    }
                    // Needs a human answer: await it in a SIDE TASK so the consumer
                    // loop keeps draining (TurnEnd / interrupt / cleanup) while the
                    // Needs-you is open — else a Stop can't be processed until the
                    // stale card is answered. A late reply to an already-resolved turn
                    // is harmless (codex ignores it).
                    None => {
                        let (aid, rx) = registry.request(thread_id, &dir, tool, &summary, &detail);
                        // Remember this card by server-request id so a later
                        // serverRequest/resolved can cancel it; clear on answer.
                        let key = id.to_string();
                        pending_asks.insert(key.clone(), aid);
                        let client = client.clone();
                        let pending = pending_asks.clone();
                        tauri::async_runtime::spawn(async move {
                            let allow = matches!(
                                rx.await.unwrap_or(crate::ask::Decision::Deny),
                                crate::ask::Decision::Allow
                            );
                            pending.remove(&key);
                            let _ = client
                                .reply_result(
                                    &id,
                                    crate::codex_app_server::codex_approval_reply(is_perm, allow, requested),
                                )
                                .await;
                        });
                    }
                }
            }
            ThreadMsg::AskResolved { request_id } => {
                // The server cleared this ask (interrupt / otherwise): cancel the
                // matching Needs-you card so it doesn't linger and send a stale
                // reply when clicked. The reply task's rx then errors → it declines.
                if let Some(entry) = pending_asks.remove(&request_id.to_string()) {
                    app.state::<crate::ask::AskRegistry>().inner().cancel(*entry.value());
                }
            }
        }
    }
    // Only a GENUINE disconnect runs the turn cleanup. If the engine's client was
    // taken/replaced (the exec-fallback teardown shut us down on purpose), skip it
    // — else this cleanup races spawn_turn and can kill/stop the fallback turn.
    let still_active = matches!(&eng.lock().await.codex_client, Some(c) if c.ptr_eq(&client));
    if still_active {
        cleanup_disconnected_turn(&app, &db, &eng, "error").await;
    }
}


/// Specific Needs-you summary for an app-server file-change approval: the
/// changed path(s) (top-level or nested under `item`), so each distinct edit
/// gets its own Always-rule key instead of one blanket "apply file changes".
fn codex_change_approval_summary(params: &serde_json::Value) -> String {
    let changes = params["changes"]
        .as_array()
        .or_else(|| params["item"]["changes"].as_array());
    let paths: Vec<&str> = changes
        .map(|cs| cs.iter().filter_map(|c| c["path"].as_str()).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return "apply file changes".to_string();
    }
    let mut s = format!(
        "apply file changes: {}",
        paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
    );
    let more = paths.len().saturating_sub(3);
    if more > 0 {
        s.push_str(&format!(" +{more}"));
    }
    s
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
    let (_program, args) = adapter.build_argv(&crate::adapters::AdapterContext {
        cwd: &inner.cwd,
        system_prompt: &inner.system_prompt,
        extra_args: &inner.extra_args,
        native_id: inner.native_id.as_deref(),
        message: &out.text,
        slash_commands: &inner.slash_commands,
    })?;
    // The adapter's program is the tool identity; resolve it through the
    // per-session pin / global override map so an aliased binary is spawned.
    let program = crate::tool_command::effective(inner.command.as_deref(), &inner.tool);
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
        let client = inner.codex_client.clone();
        drop(inner);
        let mut interrupted = false;
        if let (Some(thread), Some(client)) = (thread, client) {
            if let Some(turn) = client.active_turn(&thread).await {
                let _ = client.interrupt(&thread, &turn).await;
                interrupted = true;
            }
        }
        // No live app-server turn → this turn fell back to exec; kill the per-turn
        // child so the reader hits EOF and finalizes the row as interrupted.
        if !interrupted {
            let mut inner = eng.lock().await;
            if let Some(c) = inner.child.as_mut() {
                let _ = c.kill().await;
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
    send_hidden_existing(app, db, eng, text.to_string()).await
}

/// Coordinator bus wake: drive the agent to read its inbox, coalescing wakes.
/// Idle → read now; busy → reserve the wake's FIFO position (`request_bus_read`)
/// so one inbox-read fires at that spot when the queue drains, never behind a
/// later send; stopped/taken-over → left untouched. Ensuring the process and
/// reserving the slot happen under ONE continuous lock (`ensure = true`), so a
/// racing user send can't slip a turn in ahead of the read — even when the
/// resident process has to be spawned first.
pub async fn nudge_bus_read(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    send_hidden_inner(app, db, eng, BUS_WAKE_PROMPT.to_string(), true, true).await
}

/// Deliver invisible plumbing to an existing engine. Unlike [`nudge`], this
/// intentionally does not start a missing/stopped resident process; action-card
/// callbacks should not resurrect a lead the user stopped. Per-turn engines
/// have no resident stdin, so an idle existing engine still needs a fresh turn.
pub async fn send_hidden_existing(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    text: String,
) -> anyhow::Result<()> {
    send_hidden_inner(app, db, eng, text, false, false).await
}

/// Shared body of [`send_hidden_existing`] and [`nudge_bus_read`]. The single
/// lock makes the busy/idle decision atomic. When `ensure`, the resident process
/// is spawned (if needed) under that same lock so reserving the slot races with
/// no concurrent send. When `bus_read`, a busy engine reserves the wake's FIFO
/// position (coalescing) instead of tail-queuing, and a stopped/not-accepting
/// engine is skipped rather than erroring.
async fn send_hidden_inner(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    text: String,
    bus_read: bool,
    ensure: bool,
) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    if ensure {
        // Spawn the resident process under THIS lock, never releasing it before
        // the slot is reserved below. The reader task blocks on this lock and
        // proceeds once we drop it on return.
        if let Some((stdout, generation)) = ensure_running_locked(&mut inner).await? {
            spawn_reader(app.clone(), db.clone(), eng.clone(), stdout, generation);
        }
    }
    let out = Outgoing {
        text,
        images: vec![],
        tracked: false,
        origin_tag: None,
    };

    match hidden_delivery(
        &inner.tool,
        inner.turn.busy,
        inner.stdin.is_some(),
        inner.stopped,
    ) {
        HiddenDelivery::Noop => {
            if bus_read {
                return Ok(()); // a bus wake is best-effort; don't error
            }
            anyhow::bail!("lead engine is not accepting hidden input");
        }
        HiddenDelivery::Queue => {
            if bus_read {
                // Busy: reserve the wake's FIFO position (coalescing further
                // wakes into one read) instead of tail-queuing, so a later send
                // can't be answered before the inbox read. Atomic under the lock.
                inner.turn.request_bus_read();
            } else {
                queue_hidden_delivery(app, &mut inner, out);
            }
            Ok(())
        }
        HiddenDelivery::WriteResident => {
            let turn_id = begin_hidden_turn(app, db, &mut inner).await;
            if let Err(e) = write_user(&mut inner, &out).await {
                drop(inner);
                rollback_failed_turn(app, db, eng, turn_id).await;
                return Err(e);
            }
            Ok(())
        }
        HiddenDelivery::SpawnTurn => {
            // codex on app-server must stay on app-server even for hidden turns
            // (bus wakes), else an exec turn and the app-server connection diverge
            // on the same thread.
            let codex_appserver = inner.tool == "codex" && codex_appserver_enabled();
            let turn_id = begin_hidden_turn(app, db, &mut inner).await;
            drop(inner);
            let res = if codex_appserver {
                spawn_codex_turn_or_exec(app.clone(), db.clone(), eng.clone(), out).await
            } else {
                spawn_turn(app.clone(), db.clone(), eng.clone(), out).await
            };
            if let Err(e) = res {
                rollback_failed_turn(app, db, eng, turn_id).await;
                return Err(e);
            }
            Ok(())
        }
    }
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
    let terminal = match status {
        "error" => "error_before_output",
        "interrupted" => "interrupted_before_output",
        _ => return Ok(None),
    };
    let m = repo::insert_lead_message(
        db,
        thread_id,
        session_id,
        turn_id,
        "assistant",
        "text",
        &serde_json::json!({ "terminal": terminal }).to_string(),
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
                state.0.iter().map(|r| r.value().clone()).collect()
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
pub async fn stop_quiet(
    eng: &EngineRef,
) -> (
    i32,
    Option<i32>,
    Option<(i32, String)>,
    Vec<(i32, serde_json::Value)>,
) {
    let mut inner = eng.lock().await;
    let target = (inner.thread_id, inner.session_id);
    let current = inner.current.take().map(|(id, text, _)| (id, text));
    // Drain tool rows still awaiting a result, but DON'T finalize here: the
    // caller makes the stop visible (sets `stopped`) first. Awaiting DB/event
    // work while the engine is reset-but-not-yet-stopped would let a concurrent
    // send start a turn on the idle engine that we'd then wrongly mark stopped.
    let orphan_tools: Vec<(i32, serde_json::Value)> =
        inner.tool_rows.drain().map(|(_, v)| v).collect();
    inner.generation += 1; // orphan the reader so EOF handling is ours
    if let Some(c) = inner.child.as_mut() {
        let _ = c.kill().await;
    }
    // Kill the per-session app-server too (its consumer task exits as the sinks
    // close); the next send respawns it, picking up refreshed skills/MCP.
    if let Some(c) = inner.codex_client.take() {
        c.shutdown().await;
    }
    inner.child = None;
    inner.stdin = None;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    (target.0, target.1, current, orphan_tools)
}

/// Stop the engine outright (e.g. before a terminal takeover or by the runaway
/// guard). Persists `STATUS_STOPPED` so a stopped/taken-over session can't be
/// falsely revived into a COMPETING headless process — neither by the boot
/// revive sweep (which only resumes "running") nor by a coordinator bus wake
/// (which skips "stopped"). Distinct from "idle" so a cleanly-idle session can
/// still be driven by a bus post.
pub async fn stop(app: &AppHandle, eng: &EngineRef) {
    let (thread_id, session_id, current, orphans) = stop_quiet(eng).await;
    let mut inner = eng.lock().await;
    inner.stopped = true;
    drop(inner);
    if let Some(db) = app.try_state::<Db>() {
        persist_activity(&db, session_id, thread_id, STATUS_STOPPED).await;
        // Stop is now visible to the engine, so finalizing here can't race a
        // concurrent send into a turn we'd wrongly kill.
        finalize_orphan_tool_rows(app, &db, thread_id, orphans, "interrupted").await;
        if let Some((id, text)) = current {
            let _ = repo::update_lead_message(
                &db,
                id,
                &serde_json::json!({ "text": text }).to_string(),
                "interrupted",
            )
            .await;
            emit_finalize(app, thread_id, id, "interrupted");
        }
        mark_queued_status(app, &db, thread_id, session_id, "interrupted").await;
    }
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id,
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
                            mcp_servers: inner.last_mcp_servers.clone(),
                            tools: inner.last_tools.clone(),
                            model: inner.last_model.clone(),
                            window: inner.last_window,
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
                    mcp_servers,
                    tools,
                    model,
                } => {
                    inner.native_id = Some(session_id.clone());
                    let slash_commands =
                        merge_init_slash_commands(&inner.slash_commands, slash_commands);
                    inner.slash_commands = slash_commands.clone();
                    let window = model.as_deref().and_then(super::window::context_window);
                    inner.last_mcp_servers = mcp_servers.clone();
                    inner.last_tools = tools.clone();
                    inner.last_model = model.clone();
                    inner.last_window = window;
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
                            mcp_servers,
                            tools,
                            model,
                            window,
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
                            mcp_servers: inner.last_mcp_servers.clone(),
                            tools: inner.last_tools.clone(),
                            model: inner.last_model.clone(),
                            window: inner.last_window,
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
                                        content: None,
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
                        // Persist / answer sentinels in encounter order (shared with
                        // the app-server consumer).
                        apply_lead_sentinels(&app, &db, &mut inner, thread_id, sentinels).await;
                    }
                    // Every dialect's tool calls become inline `kind:"tool"` rows.
                    persist_tool_calls(&app, &db, &mut inner, tools).await;
                }
                super::proto::ChatEvent::ToolResults { items } => {
                    merge_tool_results(&app, &db, &mut inner, items).await;
                }
                super::proto::ChatEvent::Usage { .. } => {}
                super::proto::ChatEvent::TurnEnd {
                    is_error,
                    context_tokens,
                } => {
                    if let Some(ct) = context_tokens {
                        inner.last_context_tokens = Some(ct);
                        let _ = app.emit(
                            EVENT,
                            Push::Usage {
                                thread_id,
                                session_id: inner.session_id,
                                context_tokens: ct,
                                window: inner.last_window,
                                model: inner.last_model.clone(),
                            },
                        );
                    }
                    let status = if inner.interrupting {
                        "interrupted"
                    } else if is_error {
                        "error"
                    } else {
                        "complete"
                    };
                    inner.interrupting = false;
                    // Finalize any tool rows still awaiting a result — an
                    // interrupted or errored turn can leave a `tool_use` without
                    // its `tool_result`, which would otherwise spin forever.
                    let orphans: Vec<(i32, serde_json::Value)> =
                        inner.tool_rows.drain().map(|(_, v)| v).collect();
                    finalize_orphan_tool_rows(&app, &db, thread_id, orphans, status).await;
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
                                content: None,
                            },
                        );
                        if status == "complete" {
                            emit_lead_out(
                                &app,
                                thread_id,
                                id,
                                &text,
                                inner.current_origin_tag.clone(),
                            );
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
                        let next_turn_id = inner.turn_id;
                        let session_id = inner.session_id;
                        if per_turn(&inner.tool) {
                            let (a, d, e) = (app.clone(), db.clone(), eng.clone());
                            tauri::async_runtime::spawn(async move {
                                if let Err(err) =
                                    spawn_turn(a.clone(), d.clone(), e.clone(), next.clone()).await
                                {
                                    eprintln!("[weft] queued per-turn delivery failed: {err}");
                                    rollback_failed_turn(&a, &d, &e, next_turn_id).await;
                                } else {
                                    mark_queued_delivered(&a, &d, thread_id, session_id, &next)
                                        .await;
                                }
                            });
                        } else {
                            if let Err(e) = write_user(&mut inner, &next).await {
                                eprintln!("[weft] queued resident delivery failed: {e}");
                                drop(inner);
                                rollback_failed_turn(&app, &db, &eng, next_turn_id).await;
                                return;
                            } else {
                                mark_queued_delivered(&app, &db, thread_id, session_id, &next)
                                    .await;
                            }
                        }
                    }
                    let still_busy = inner.turn.busy;
                    persist_activity(
                        &db,
                        inner.session_id,
                        thread_id,
                        if still_busy { "running" } else { "idle" },
                    )
                    .await;
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
            // A per-turn process killed/crashed after a tool started but before its
            // item.completed leaves a streaming tool row no TurnEnd ever drained —
            // finalize it here too, so it doesn't spin forever in the timeline.
            let eof_thread_id = inner.thread_id;
            let orphans: Vec<(i32, serde_json::Value)> =
                inner.tool_rows.drain().map(|(_, v)| v).collect();
            finalize_orphan_tool_rows(&app, &db, eof_thread_id, orphans, status).await;
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
                        content: None,
                    },
                );
                // 仅 complete 才回流 IM——interrupted/error 的半截不应上桥。
                if status == "complete" {
                    emit_lead_out(
                        &app,
                        inner.thread_id,
                        id,
                        &text,
                        inner.current_origin_tag.clone(),
                    );
                }
            }
            inner.child = None;
            let next = inner.turn.on_turn_end();
            // Carry the dequeued turn's tag (None when going idle) onto its frames.
            inner.current_origin_tag = next.as_ref().and_then(|n| n.origin_tag.clone());
            if let Some(next) = next {
                inner.turn_id += 1;
                let next_turn_id = inner.turn_id;
                let thread_id = inner.thread_id;
                let session_id = inner.session_id;
                let (a, d, e) = (app.clone(), db.clone(), eng.clone());
                tauri::async_runtime::spawn(async move {
                    if let Err(err) =
                        spawn_turn(a.clone(), d.clone(), e.clone(), next.clone()).await
                    {
                        eprintln!("[weft] queued per-turn delivery failed: {err}");
                        rollback_failed_turn(&a, &d, &e, next_turn_id).await;
                    } else {
                        mark_queued_delivered(&a, &d, thread_id, session_id, &next).await;
                    }
                });
            }
            let still_busy = inner.turn.busy;
            persist_activity(
                &db,
                inner.session_id,
                inner.thread_id,
                if still_busy { "running" } else { "idle" },
            )
            .await;
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
            let queued_status = status;
            let thread_id = inner.thread_id;
            let session_id = inner.session_id;
            inner.interrupting = false;
            // claude's long-lived process died mid-turn: finalize any tool rows
            // still awaiting a result so they don't spin forever in the timeline.
            let orphans: Vec<(i32, serde_json::Value)> =
                inner.tool_rows.drain().map(|(_, v)| v).collect();
            finalize_orphan_tool_rows(&app, &db, thread_id, orphans, status).await;
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
                        content: None,
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
            persist_activity(&db, session_id, thread_id, "idle").await;
            let _ = app.emit(
                EVENT,
                Push::Turn {
                    thread_id,
                    session_id,
                    state: "stopped".into(),
                    queued: 0,
                },
            );
            drop(inner);
            mark_queued_status(&app, &db, thread_id, session_id, queued_status).await;
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
    fn bus_read_coalesces_into_one_trailing_turn() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy
                                     // Several wakes during the turn collapse into one pending read.
        assert!(!t.request_bus_read());
        assert!(!t.request_bus_read());
        assert!(!t.request_bus_read());
        // Turn-end with an empty queue synthesizes exactly one invisible read.
        let read = t.on_turn_end().expect("a coalesced read turn");
        assert_eq!(read.text, BUS_WAKE_PROMPT);
        assert!(!read.tracked); // invisible plumbing, no timeline row
        assert!(t.busy); // the read turn keeps the engine busy
                         // No further pending read → the next turn-end goes idle.
        assert!(t.on_turn_end().is_none());
        assert!(!t.busy);
    }

    #[test]
    fn bus_read_runs_after_messages_queued_before_the_wake() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // busy
        t.queue.push_back(Outgoing {
            text: "earlier".into(),
            images: vec![],
            tracked: true,
            origin_tag: None,
        });
        t.request_bus_read(); // wake lands AFTER "earlier" was queued
                              // "earlier" preceded the wake, so it drains first, then the read.
        assert_eq!(t.on_turn_end().map(|o| o.text).as_deref(), Some("earlier"));
        assert_eq!(
            t.on_turn_end().map(|o| o.text).as_deref(),
            Some(BUS_WAKE_PROMPT)
        );
        assert!(t.on_turn_end().is_none());
    }

    #[test]
    fn bus_read_precedes_messages_queued_after_the_wake() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // busy
        t.request_bus_read(); // wake lands first (queue empty → position 0)
        t.queue.push_back(Outgoing {
            text: "later".into(),
            images: vec![],
            tracked: true,
            origin_tag: None,
        });
        // The wake arrived before "later", so the inbox read comes first — the
        // agent can't answer the newer prompt without seeing the bus message.
        assert_eq!(
            t.on_turn_end().map(|o| o.text).as_deref(),
            Some(BUS_WAKE_PROMPT)
        );
        assert_eq!(t.on_turn_end().map(|o| o.text).as_deref(), Some("later"));
        assert!(t.on_turn_end().is_none());
    }

    #[test]
    fn request_bus_read_on_idle_starts_a_turn() {
        let mut t = TurnState::default();
        assert!(t.request_bus_read()); // idle → caller starts a read turn now
        assert!(t.busy);
        assert!(t.bus_read_pos.is_none()); // consumed by starting the turn, not pending
    }

    #[test]
    fn wall_cap_fires_regardless_of_activity() {
        assert!(turn_verdict(Some(7200), 1, 7200, 1800, false)
            .unwrap()
            .contains("ran for over 2h"));
    }

    #[test]
    fn codex_first_turn_prepends_prompt_only_on_new_thread() {
        // brand-new thread + non-empty prompt → prepended to the first message
        assert_eq!(codex_first_turn_text("SYS", "hello", false), "SYS\n\nhello");
        // resumed thread → prompt already in history, message unchanged
        assert_eq!(codex_first_turn_text("SYS", "hello", true), "hello");
        // no prompt → message unchanged even on a new thread
        assert_eq!(codex_first_turn_text("", "hello", false), "hello");
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
        let content: serde_json::Value = serde_json::from_str(&m.content).unwrap();
        assert_eq!(content["terminal"], "error_before_output");
        let all = repo::list_lead_messages(&db, 7).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, m.id);

        let complete = insert_terminal_assistant_if_missing(&db, 7, None, 4, "complete")
            .await
            .unwrap();
        assert!(complete.is_none());
    }

    #[tokio::test]
    async fn disconnected_busy_turn_without_current_row_persists_terminal_error() {
        let db = Db::connect("sqlite::memory:").await.unwrap();

        let row = persist_disconnected_turn_row(&db, 7, None, 3, "error", true, None)
            .await
            .unwrap();

        let all = repo::list_lead_messages(&db, 7).await.unwrap();
        assert_eq!(all.len(), 1);
        match row {
            Some(DisconnectedTurnRow::Inserted(m)) => assert_eq!(m.id, all[0].id),
            _ => panic!("busy disconnected turn without current should insert terminal row"),
        }
        assert_eq!(all[0].turn_id, 3);
        assert_eq!(all[0].role, "assistant");
        assert_eq!(all[0].status, "error");
        let content: serde_json::Value = serde_json::from_str(&all[0].content).unwrap();
        assert_eq!(content["terminal"], "error_before_output");
    }

    #[test]
    fn human_dur_formats() {
        assert_eq!(human_dur(7200), "2h");
        assert_eq!(human_dur(1800), "30min");
        assert_eq!(human_dur(45), "45s");
    }

    #[test]
    fn codex_change_approval_summary_is_path_specific() {
        // distinct edits get distinct summaries → distinct AskRegistry Always keys.
        let a = codex_change_approval_summary(&serde_json::json!({
            "changes": [{"path": "src/a.rs", "kind": {"type": "edit"}}]
        }));
        let b = codex_change_approval_summary(&serde_json::json!({
            "item": {"changes": [{"path": "src/b.rs"}]}
        }));
        assert_eq!(a, "apply file changes: src/a.rs");
        assert_eq!(b, "apply file changes: src/b.rs");
        assert_ne!(a, b);
        // >3 paths are capped with a +N suffix.
        let many = codex_change_approval_summary(&serde_json::json!({
            "changes": [{"path":"1"},{"path":"2"},{"path":"3"},{"path":"4"},{"path":"5"}]
        }));
        assert_eq!(many, "apply file changes: 1, 2, 3 +2");
        // no paths → the generic label (still answerable, just not Always-specific).
        assert_eq!(
            codex_change_approval_summary(&serde_json::json!({})),
            "apply file changes"
        );
    }

    #[test]
    fn tool_row_status_id_less_running_does_not_spin() {
        // a finished row is terminal …
        assert_eq!(tool_row_status(true, false, false), "complete");
        assert_eq!(tool_row_status(true, false, true), "error");
        // … a running row streams only when it has an id to correlate its result …
        assert_eq!(tool_row_status(false, true, false), "streaming");
        // … and an id-less running row is stored complete (never a perpetual spinner).
        assert_eq!(tool_row_status(false, false, false), "complete");
    }

    #[test]
    fn per_turn_only_accepts_known_per_turn_tools() {
        assert!(!per_turn("claude"));
        assert!(per_turn("codex"));
        assert!(per_turn("opencode"));
        assert!(!per_turn("mystery"));
    }

    fn test_inner(tool: &str) -> EngineInner {
        EngineInner {
            thread_id: 1,
            tool: tool.into(),
            command: None,
            session_id: None,
            cwd: "/tmp".into(),
            extra_args: vec![],
            system_prompt: String::new(),
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
            pending_command_refresh: false,
            last_context_tokens: None,
            last_model: None,
            last_window: None,
            last_mcp_servers: vec![],
            last_tools: vec![],
            current_origin_tag: None,
            tool_rows: std::collections::HashMap::new(),
            stopped: false,
            codex_client: None,
        }
    }

    #[test]
    fn mark_hidden_turn_started_sets_busy_and_clears_origin_tag() {
        let mut inner = test_inner("claude");
        inner.current_origin_tag = Some("im-reply-target".into());

        let turn_id = mark_hidden_turn_started(&mut inner);

        assert!(inner.turn.busy);
        assert_eq!(turn_id, 1);
        assert_eq!(inner.turn_id, 1);
        assert!(inner.clock.started.is_some());
        assert!(inner.current_origin_tag.is_none());
    }

    #[test]
    fn reset_failed_hidden_turn_clears_busy_state_for_same_turn() {
        let mut inner = test_inner("claude");
        inner.current_origin_tag = Some("stale".into());
        inner.turn.queue.push_back(Outgoing {
            text: "queued user".into(),
            images: vec![],
            tracked: true,
            origin_tag: None,
        });
        let turn_id = mark_hidden_turn_started(&mut inner);

        assert!(reset_failed_hidden_turn(&mut inner, turn_id));

        assert!(!inner.turn.busy);
        assert!(inner.turn.queue.is_empty());
        assert!(inner.clock.started.is_none());
        assert!(inner.current_origin_tag.is_none());
        assert!(inner.current.is_none());
        assert!(!inner.interrupting);
    }

    #[test]
    fn reset_failed_hidden_turn_ignores_later_turn() {
        let mut inner = test_inner("claude");
        let old_turn = mark_hidden_turn_started(&mut inner);
        inner.turn_id += 1;

        assert!(!reset_failed_hidden_turn(&mut inner, old_turn));
        assert!(inner.turn.busy);
    }

    #[tokio::test]
    async fn write_user_reports_missing_stdin() {
        let mut inner = test_inner("claude");
        let out = Outgoing {
            text: "hello".into(),
            images: vec![],
            tracked: false,
            origin_tag: None,
        };

        let err = write_user(&mut inner, &out).await.unwrap_err();

        assert!(err.to_string().contains("resident stdin is unavailable"));
    }

    #[test]
    fn hidden_delivery_spawns_for_per_turn_tools_without_stdin() {
        assert_eq!(
            hidden_delivery("codex", false, false, false),
            HiddenDelivery::SpawnTurn
        );
        assert_eq!(
            hidden_delivery("opencode", false, false, false),
            HiddenDelivery::SpawnTurn
        );
    }

    #[test]
    fn hidden_delivery_keeps_resident_and_queue_semantics() {
        assert_eq!(
            hidden_delivery("claude", false, true, false),
            HiddenDelivery::WriteResident
        );
        assert_eq!(
            hidden_delivery("claude", false, false, false),
            HiddenDelivery::Noop
        );
        assert_eq!(
            hidden_delivery("codex", true, false, false),
            HiddenDelivery::Queue
        );
    }

    #[test]
    fn hidden_delivery_rejects_stopped_per_turn_engines() {
        assert_eq!(
            hidden_delivery("codex", false, false, true),
            HiddenDelivery::Noop
        );
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
            command: None,
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
            pending_command_refresh: false,
            last_context_tokens: None,
            last_model: None,
            last_window: None,
            last_mcp_servers: vec![],
            last_tools: vec![],
            current_origin_tag: None,
            tool_rows: std::collections::HashMap::new(),
            stopped: false,
            codex_client: None,
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
