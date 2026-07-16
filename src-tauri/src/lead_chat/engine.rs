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

/// 一条待发排队消息的前端视图。`images`/`files` 仅给个数（栈里显示角标用）。
#[derive(Clone, serde::Serialize)]
pub struct QueuedItem {
    pub id: i32,
    pub text: String,
    pub images: usize,
    pub files: usize,
    /// True when the original send carried files or images; disables inline edit.
    pub has_attachments: bool,
}

pub(crate) fn queue_items(turn: &TurnState) -> Vec<QueuedItem> {
    turn.queue
        .iter()
        .filter_map(|o| {
            o.queue_id.map(|id| QueuedItem {
                id,
                text: o.text.clone(),
                images: o.images.len(),
                // files are appended into text; count is not separately tracked
                files: 0,
                has_attachments: o.has_attachments,
            })
        })
        .collect()
}

/// How many user-visible (tracked) messages are queued — what the cap counts.
/// Hidden plumbing deliveries (queue_id == None) are excluded.
fn visible_queued(turn: &TurnState) -> usize {
    turn.queue.iter().filter(|o| o.queue_id.is_some()).count()
}

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
        queue: Vec<QueuedItem>,
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

/// 进行中 turn 最多排队多少条人类消息（满后 send 拒绝入队）。
pub const MAX_QUEUED: usize = 5;

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
    /// 入队时持久化的 queued 行 id；删/改/重排和交付落库按它定位。None=直接发送。
    pub queue_id: Option<i32>,
    /// True when the original send carried files or images (computed from the
    /// ORIGINAL inputs before per-turn image-spill clears out.images).
    pub has_attachments: bool,
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
                    queue_id: None,
                    has_attachments: false,
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

    /// 删除某条仍排队的消息；true=删掉了。
    pub fn remove(&mut self, id: i32) -> bool {
        let Some(pos) = self.queue.iter().position(|o| o.queue_id == Some(id)) else {
            return false;
        };
        self.queue.remove(pos);
        // A coalesced bus-read sits at a FIFO index; dropping an item before it
        // shifts that index left, else a later send would jump ahead of the wake.
        if let Some(n) = self.bus_read_pos {
            if pos < n {
                self.bus_read_pos = Some(n - 1);
            }
        }
        true
    }

    /// 改某条排队消息文本；true=改了。
    pub fn edit(&mut self, id: i32, text: &str) -> bool {
        for o in self.queue.iter_mut() {
            if o.queue_id == Some(id) {
                o.text = text.to_string();
                return true;
            }
        }
        false
    }

    /// 按 id 列表重排；order 必须是当前可见(有 queue_id)项的排列，否则不动并返回 false。
    pub fn reorder(&mut self, order: &[i32]) -> bool {
        // A pending invisible bus wake pins a FIFO index (bus_read_pos); reordering
        // would mis-place it relative to the wake, so refuse while one is pending
        // (rare/transient — the composer re-syncs the drag from the re-emitted state).
        if self.bus_read_pos.is_some() {
            return false;
        }
        let tracked: Vec<i32> = self.queue.iter().filter_map(|o| o.queue_id).collect();
        if order.len() != tracked.len() {
            return false;
        }
        let mut a = order.to_vec();
        let mut b = tracked.clone();
        a.sort_unstable();
        b.sort_unstable();
        if a != b {
            return false;
        }
        // Reorder only the visible (user) items; untracked deliveries — internal
        // nudges / bus replies queued mid-turn — keep their absolute slots so a user
        // drag never drops or resequences them.
        let slots: Vec<bool> = self.queue.iter().map(|o| o.queue_id.is_some()).collect();
        let mut by_id: HashMap<i32, Outgoing> = HashMap::new();
        let mut untracked: VecDeque<Outgoing> = VecDeque::new();
        for o in self.queue.drain(..) {
            match o.queue_id {
                Some(id) => {
                    by_id.insert(id, o);
                }
                None => untracked.push_back(o),
            }
        }
        let mut order_it = order.iter();
        let mut next: VecDeque<Outgoing> = VecDeque::new();
        for is_tracked in slots {
            if is_tracked {
                if let Some(o) = order_it.next().and_then(|id| by_id.remove(id)) {
                    next.push_back(o);
                }
            } else if let Some(o) = untracked.pop_front() {
                next.push_back(o);
            }
        }
        self.queue = next;
        true
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
    let res = match out.queue_id {
        Some(id) => repo::complete_queued_by_id(db, id).await,
        None => repo::complete_queued(db, thread_id, session_id).await,
    };
    match res {
        Ok(Some(m)) => {
            // Stamp a delivery-order seq so reordered rows appear in send order
            // (not creation order) in the transcript after restart.
            if let Err(e) = repo::assign_delivery_seq(db, thread_id, m.id).await {
                eprintln!("[weft] assign_delivery_seq failed: {e}");
            }
            // Carry the (possibly edited) text so the transcript shows what was
            // delivered, not the stale original Push::Message body.
            let content = finalize_text(&m, out);
            let _ = app.emit(
                EVENT,
                Push::Finalize {
                    thread_id,
                    message_id: m.id,
                    status: "complete".into(),
                    content,
                },
            );
        }
        Ok(None) => {}
        Err(e) => eprintln!("[weft] queued message complete failed: {e}"),
    }
}

/// A delivered row's text, but only for a plain text row with no attachments. The
/// finalize-content channel wraps content as `{text}`, which would mangle a command
/// row or drop image/file chips — those keep their original cached body (None).
fn finalize_text(
    m: &crate::store::entities::lead_message::Model,
    out: &Outgoing,
) -> Option<String> {
    // Only plain text rows round-trip: command rows ({command,args}) and
    // attachment-bearing rows keep their cached body (finalize wraps as {text},
    // which would drop the image/file chips).
    if m.kind != "text" || !out.images.is_empty() {
        return None;
    }
    // Check the PERSISTED attachments too: per-turn dialects spill pasted images to
    // temp files and clear out.images, but m.content still carries images/files —
    // replacing such a row with {text} would lose its preview.
    let attach_free = serde_json::from_str::<serde_json::Value>(&m.content)
        .ok()
        .map(|v| {
            let empty = |k: &str| {
                v.get(k).and_then(|x| x.as_array()).map(|a| a.is_empty()).unwrap_or(true)
            };
            empty("images") && empty("files")
        })
        .unwrap_or(true);
    if !attach_free {
        return None;
    }
    // Source the text from the in-memory Outgoing (already reflects any edit) so a
    // not-yet-persisted edit can't make the live finalize show stale text.
    Some(out.text.clone())
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
    queue: Vec<QueuedItem>,
) {
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id,
            state: if busy { "busy" } else { "idle" }.into(),
            queue,
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
        queue_items(&inner.turn),
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
        queue_items(&inner.turn),
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
    emit_turn_state(app, thread_id, session_id, false, Vec::new());
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
/// message — action_card / plan_card become their own rows, list_repos triggers
/// a hidden stdin-style reply. Errors are logged but never abort the stream.
/// Shared by the exec/claude reader and the codex app-server consumer so both
/// transports render the cards and answer list_repos.
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
                persist_card_row(app, db, inner, thread_id, "action_card", &json).await;
            }
            super::sentinels::Sentinel::PlanCard(json) => {
                persist_card_row(app, db, inner, thread_id, "plan_card", &json).await;
            }
            super::sentinels::Sentinel::TestCases(md) => {
                // Issue-level document — only the LEAD may write it. Chat-mode
                // workers share this engine (session_id set); a worker echoing
                // protocol text (or prompt-injected repo content) must not
                // replace the issue's cases from its own timeline. (Extraction
                // is already lead-gated; this is defense in depth.)
                if inner.session_id.is_some() {
                    eprintln!(
                        "[weft] worker sentinel: test_cases ignored (lead-only, issue-level doc)"
                    );
                    continue;
                }
                // A user edit saved MID-TURN supersedes whatever this turn
                // emits — the emit was authored without seeing it. The queue
                // check is the fast path (undelivered feedback); the write
                // itself is an ATOMIC compare-and-swap in SQL, so a save
                // landing at any point before the UPDATE still wins.
                if has_pending_user_test_update(&inner.turn) {
                    eprintln!(
                        "[weft] lead sentinel: test_cases skipped — a queued user edit \
                         supersedes this turn's emit"
                    );
                    continue;
                }
                // Raw markdown body: upsert the document (single source of
                // truth), then drop a summary card into the timeline — the
                // panel always reads the table, never the card.
                let md = md.trim();
                if md.is_empty() {
                    eprintln!("[weft] lead sentinel: test_cases body is empty — dropped");
                } else {
                    match repo::lead_upsert_test_plan(
                        db,
                        thread_id,
                        md,
                        inner.clock.started_millis,
                    )
                    .await
                    {
                        Ok(true) => {
                            let summary = super::test_plan::summarize(md).to_string();
                            persist_card_row(app, db, inner, thread_id, "test_cases", &summary)
                                .await;
                        }
                        Ok(false) => {
                            eprintln!(
                                "[weft] lead sentinel: test_cases skipped — a user edit \
                                 saved mid-turn supersedes this emit"
                            );
                        }
                        Err(e) => {
                            eprintln!("[weft] lead sentinel: upsert test_plan failed: {e}")
                        }
                    }
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
                        queue_id: None,
                        has_attachments: false,
                    };
                    queue_hidden_delivery(app, inner, out);
                }
            }
        }
    }
}

/// True when an undelivered user edit of the test-case document is still
/// queued for this engine: the in-flight turn was authored WITHOUT seeing it,
/// so any `<weft:test_cases>` it emits is stale relative to the user's save
/// and must not overwrite the user-sourced row.
fn has_pending_user_test_update(turn: &TurnState) -> bool {
    turn.queue
        .iter()
        .any(|o| o.text.contains("<weft:test_cases_updated>"))
}


/// Persist one card sentinel (`action_card` / `plan_card`) as its own timeline
/// row and push it to the UI. Rejects anything that isn't a JSON object so the
/// UI can rely on the card's fields; errors are logged, never fatal.
async fn persist_card_row(
    app: &AppHandle,
    db: &Db,
    inner: &EngineInner,
    thread_id: i32,
    kind: &str,
    json: &str,
) {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) if v.is_object() => {
            let (sid, turn) = (inner.session_id, inner.turn_id);
            match repo::insert_lead_message(
                db, thread_id, sid, turn, "assistant", kind, json, "complete",
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
                Err(e) => eprintln!("[weft] lead sentinel: insert {kind} failed: {e}"),
            }
        }
        Ok(_) => eprintln!("[weft] lead sentinel: {kind} payload is not an object — dropped"),
        Err(e) => eprintln!("[weft] lead sentinel: {kind} JSON parse failed: {e}"),
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
        let (clean, sentinels) =
            super::sentinels::extract_sentinels_with(&text, inner.session_id.is_none());
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
            queue: Vec::new(),
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
    /// Unix-MILLISECONDS stamp of the in-flight turn's start (0 = never
    /// begun). Same clock as `test_plan.updated_at`, so "did the user save
    /// mid-turn?" is a plain comparison — millisecond resolution keeps an
    /// idle-save immediately followed by the feedback turn (same second)
    /// from being misread as a mid-turn save.
    pub started_millis: u64,
}

impl Default for TurnClock {
    fn default() -> Self {
        Self {
            started: None,
            last_activity: std::time::Instant::now(),
            started_millis: 0,
        }
    }
}

impl TurnClock {
    pub(crate) fn begin_turn(&mut self) {
        self.started = Some(std::time::Instant::now());
        self.last_activity = std::time::Instant::now();
        self.started_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
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
    /// Bumped whenever a stop/reset clears the turn (stop_quiet). A send captures
    /// this at Phase 1; if it advances before the send commits, the send was
    /// invalidated by a stop — even one immediately followed by a restart, which
    /// resets `stopped`/`busy` and so slips past those flags — and must not
    /// deliver onto the fresh turn.
    pub reset_epoch: u64,
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
    /// Out-of-band probe ordering: `probe_seq` hands a ticket to each probe as
    /// it STARTS (`lead_session_meta` / `session_meta` take one before
    /// gathering); `probe_committed` records the newest absorbed ticket. A
    /// result bearing an older ticket lost the race — its usage was read
    /// before a newer probe's and may only fill holes, never overwrite.
    pub probe_seq: u64,
    pub probe_committed: u64,
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

/// Engine meta snapshot persisted across app restarts (thread.lead_meta /
/// session.meta). Written at init/turn-end, read back on engine (re)creation,
/// so the Session panel shows the last-known context/model/MCP state instead of
/// blanking until the next turn's events.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PersistedMeta {
    pub context_tokens: Option<u64>,
    pub window: Option<u64>,
    pub model: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<super::proto::McpServer>,
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Whether a transport's live event stream reports context usage itself.
/// claude (`result.usage`) and codex (TokenCount / turn-end usage) do — for
/// them a probe's tokens only fill a hole, since a probe started mid-turn can
/// carry the PREVIOUS turn's usage and land after the turn-end checkpoint.
/// opencode has no usage-bearing event (turns end on EOF); its sidecar probe
/// is the ONLY usage source, so probe tokens must stay updatable or the count
/// freezes at the first probed value forever.
fn usage_events_authoritative(tool: &str) -> bool {
    tool != "opencode"
}

impl PersistedMeta {
    /// Merge an out-of-band probe snapshot (`session_meta::gather`). `None`
    /// fields keep existing values — a transient probe failure must never
    /// blank anything. Returns whether anything changed.
    ///
    /// `freshest` — this result carries the newest probe ticket (or ran with
    /// no engine, hence no race): only then may it OVERWRITE. A stale result
    /// (older ticket, or ticketless while an engine is now live) read state
    /// before a newer probe did, so every field degrades to fill-a-hole.
    /// `usage_from_events` further gates `context_tokens` (see
    /// [`usage_events_authoritative`]): claude/codex usage is owned by the
    /// event stream, so probes only ever fill its holes.
    fn merge_probe(
        &mut self,
        snap: &crate::session_meta::SessionMetaSnapshot,
        freshest: bool,
        usage_from_events: bool,
    ) -> bool {
        let mut changed = false;
        if let Some(v) = &snap.model {
            let accept = if freshest {
                self.model.as_deref() != Some(v)
            } else {
                self.model.is_none()
            };
            if accept {
                self.model = Some(v.clone());
                changed = true;
            }
        }
        if let Some(v) = snap.window {
            let accept = if freshest {
                self.window != Some(v)
            } else {
                self.window.is_none()
            };
            if accept {
                self.window = Some(v);
                changed = true;
            }
        }
        if let Some(v) = snap.context_tokens {
            let updatable = freshest && !usage_from_events;
            let accept = if updatable {
                self.context_tokens != Some(v)
            } else {
                self.context_tokens.is_none()
            };
            if accept {
                self.context_tokens = Some(v);
                changed = true;
            }
        }
        if let Some(v) = &snap.mcp_servers {
            // Only the freshest result may touch MCP at all. An empty list is
            // indistinguishable from "user just removed every server" (an
            // authoritative empty result from a newer probe), so a stale
            // result must not even fill it — resurrection is worse than one
            // probe-cycle of latency on a cold start (whose first probe is
            // freshest anyway).
            if freshest && self.mcp_servers != *v {
                self.mcp_servers = v.clone();
                changed = true;
            }
        }
        changed
    }
}

/// Snapshot the engine's last-known meta and persist it. Awaited inline (like
/// every other DB write on the event loop) rather than spawned: an Init write
/// racing a TurnEnd write from independent tasks could land last and revert the
/// snapshot to pre-turn values. A single-column UPDATE of a few hundred bytes —
/// the await is negligible. Failures only log; the snapshot is best-effort.
async fn persist_engine_meta(db: &Db, inner: &EngineInner) {
    let snap = PersistedMeta {
        context_tokens: inner.last_context_tokens,
        window: inner.last_window,
        model: inner.last_model.clone(),
        mcp_servers: inner.last_mcp_servers.clone(),
        tools: inner.last_tools.clone(),
    };
    let json = match serde_json::to_string(&snap) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("[weft] engine meta serialize failed: {e}");
            return;
        }
    };
    let r = match inner.session_id {
        Some(sid) => repo::save_session_meta(db, sid, &json).await,
        None => repo::save_lead_meta(db, inner.thread_id, &json).await,
    };
    if let Err(e) = r {
        eprintln!("[weft] engine meta persist failed: {e}");
    }
}

/// Fold an out-of-band probe snapshot (`session_meta` / `lead_session_meta`)
/// into the engine's cached meta — and into the persisted snapshot, so it
/// survives a relaunch. codex/opencode model/window/MCP only exist via these
/// probes, never via engine events; without this the turn-end snapshot writes
/// `model: null` + empty MCP for those transports and a relaunch shows a blank
/// panel until the next probe (or forever if it keeps failing). Works with or
/// without a live engine: on a fresh relaunch the panel probes before any
/// engine is spawned, so the no-engine path merges straight into the stored JSON.
/// Hand out a probe ticket for the live engine (if any) at probe START.
/// `absorb_probe_meta` later compares the ticket against the newest committed
/// one, so a slow probe that returns after a fresher one can't roll usage back.
/// No engine → None: nothing is running, so there is no race to order.
pub async fn take_probe_ticket(app: &AppHandle, thread_id: i32, session_id: Option<i32>) -> Option<u64> {
    let key = match session_id {
        Some(sid) => sid as i64,
        None => -(thread_id as i64),
    };
    let eng = app.state::<LeadChatState>().get(key)?;
    let mut inner = eng.lock().await;
    inner.probe_seq += 1;
    Some(inner.probe_seq)
}

pub async fn absorb_probe_meta(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    ticket: Option<u64>,
    snap: &crate::session_meta::SessionMetaSnapshot,
) {
    let key = match session_id {
        Some(sid) => sid as i64,
        None => -(thread_id as i64),
    };
    if let Some(eng) = app.state::<LeadChatState>().get(key) {
        let mut inner = eng.lock().await;
        // Freshest = carries the newest ticket. A ticketless result on a LIVE
        // engine started before the engine existed — it has no ordering claim
        // against probes ticketed since, so it degrades to fill-only too.
        let freshest = ticket.is_some_and(|t| t > inner.probe_committed);
        if let Some(t) = ticket {
            inner.probe_committed = inner.probe_committed.max(t);
        }
        let mut m = PersistedMeta {
            context_tokens: inner.last_context_tokens,
            window: inner.last_window,
            model: inner.last_model.clone(),
            mcp_servers: inner.last_mcp_servers.clone(),
            tools: inner.last_tools.clone(),
        };
        if m.merge_probe(snap, freshest, usage_events_authoritative(&inner.tool)) {
            inner.last_context_tokens = m.context_tokens;
            inner.last_window = m.window;
            inner.last_model = m.model.clone();
            inner.last_mcp_servers = m.mcp_servers.clone();
            persist_engine_meta(db, &inner).await;
        }
        return;
    }
    // No live engine (e.g. right after a relaunch): merge into the stored JSON.
    // No ticket ordering needed — with no engine there is no turn running and
    // the probes were issued against the same resting state.
    let (existing, tool) = match session_id {
        Some(sid) => match repo::get_session(db, sid).await.ok().flatten() {
            Some(s) => (Some(s.meta), s.tool),
            None => (None, String::new()),
        },
        None => match repo::get_thread(db, thread_id).await.ok().flatten() {
            Some(t) => (Some(t.lead_meta), t.lead_tool),
            None => (None, String::new()),
        },
    };
    let mut m: PersistedMeta = existing
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    // Engine-less all the way through: nothing ran concurrently, so this
    // result IS the freshest view of the resting session.
    if !m.merge_probe(snap, true, usage_events_authoritative(&tool)) {
        return;
    }
    let Ok(json) = serde_json::to_string(&m) else {
        return;
    };
    let r = match session_id {
        Some(sid) => repo::save_session_meta(db, sid, &json).await,
        None => repo::save_lead_meta(db, thread_id, &json).await,
    };
    if let Err(e) = r {
        eprintln!("[weft] probe meta persist failed: {e}");
    }
}

/// Restore a persisted meta snapshot into a freshly built engine (the inverse of
/// [`persist_engine_meta`]). Empty/corrupt JSON is a silent no-op — the panel
/// just waits for the next turn like before.
pub fn apply_persisted_meta(inner: &mut EngineInner, json: &str) {
    if json.is_empty() {
        return;
    }
    let Ok(m) = serde_json::from_str::<PersistedMeta>(json) else {
        return;
    };
    inner.last_context_tokens = m.context_tokens;
    inner.last_window = m.window;
    inner.last_model = m.model;
    inner.last_mcp_servers = m.mcp_servers;
    inner.last_tools = m.tools;
}

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

    pub fn remove(&self, key: i64) -> Option<EngineRef> {
        self.0.remove(&key).map(|r| r.1)
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
        .env("PATH", crate::detect::tool_path())
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

/// Undo the turn reservation made by `send` Phase 1 when later persistence
/// fails. Leaves the engine idle and forgets the incremented turn id so the
/// next send can start a fresh turn.
///
/// Only undoes a reservation this send still OWNS: the lock was dropped for the
/// failing DB work, so a stop/reset (reset_epoch) or a newer direct reservation
/// (turn_id) may own the state by now — undoing then would clear the busy flag
/// or decrement the turn id of THAT turn, canceling or corrupting it. A leaked
/// increment on our abandoned id is harmless (turn ids are monotonic); a wrong
/// decrement is not.
/// Undo a canceled direct send's Phase-1 reservation and restore the engine's
/// invariants — ownership-guarded: `reset_epoch` + `turn_id` + `busy` must all
/// still match this send, else a stop/reset cleared the turn itself or a newer
/// reservation owns the state, and this no-ops.
///
/// When it does undo, three invariants are restored, not just the busy flag:
/// - `interrupting` is cleared: the interrupt that canceled this send targeted
///   OUR nascent turn, and for per-turn/codex paths no child or TurnEnd will
///   ever exist to clear the flag — leaving it set would reject every later
///   direct send until restart.
/// - messages that queued behind the canceled turn are cleared and finalized
///   `queue_status` ("interrupted" for a cancel, "error" for a persistence
///   failure): an idle engine must never keep a non-empty queue — nothing
///   drains it, and a later send would run ahead of it.
/// - the session activity Phase 1 optimistically persisted as "running" is
///   re-persisted ("stopped"/"idle" per current state): a stop landing between
///   ensure_running_for_send and Phase 1 has its unlocked "stopped" write
///   overtaken by Phase 1's locked "running" write.
async fn rollback_canceled_send(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    ctx: &SendContext,
    queue_status: &str,
) {
    let (stopped, thread_id, session_id, drained) = {
        let mut inner = eng.lock().await;
        if !ctx.direct
            || inner.reset_epoch != ctx.reset_epoch
            || inner.turn_id != ctx.turn
            || !inner.turn.busy
        {
            return;
        }
        inner.turn.busy = false;
        inner.turn_id -= 1;
        inner.current_origin_tag = None;
        inner.clock.started = None;
        inner.interrupting = false;
        // Capture EXACTLY the rows drained here: a blanket per-session sweep
        // would also catch a concurrent send's row inserted after this lock is
        // released — finalizing a message that is about to be delivered.
        let drained: Vec<i32> = inner.turn.queue.iter().filter_map(|o| o.queue_id).collect();
        inner.turn.queue.clear();
        (inner.stopped, inner.thread_id, inner.session_id, drained)
    };
    let status = if stopped { STATUS_STOPPED } else { "idle" };
    persist_activity(db, ctx.session_id, ctx.thread_id, status).await;
    emit_turn_state(app, thread_id, session_id, false, Vec::new());
    if !drained.is_empty() {
        match repo::set_queued_status_by_ids(db, &drained, queue_status).await {
            Ok(rows) => {
                for m in rows {
                    emit_finalize(app, thread_id, m.id, queue_status);
                }
            }
            Err(e) => eprintln!("[weft] canceled-send queue finalize failed: {e}"),
        }
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

/// Snapshot of engine state taken while reserving a send slot. Carrying it
/// across await points lets later phases re-verify the reservation is still
/// valid before mutating the engine.
#[derive(Clone)]
struct SendContext {
    thread_id: i32,
    session_id: Option<i32>,
    turn: i32,
    direct: bool,
    is_command: bool,
    tool: String,
    origin_tag: Option<String>,
    /// The engine's reset_epoch captured at Phase 1. If it advances before the send
    /// commits, a stop/reset invalidated this send (see send_reservation_valid).
    reset_epoch: u64,
}

/// True when the reservation made in `send` Phase 1 is still valid in Phase 3.
/// Stop/reset can race while the engine lock is dropped, so we re-check
/// `stopped`, turn identity, and (for direct sends) the busy flag before
/// writing to stdin, queueing, or spawning.
///
/// Queued sends do not reserve a specific turn; they only observed that the
/// engine was busy in Phase 1. The active turn may finish (and `turn_id`
/// advance) while the lock is dropped, so queued sends tolerate a turn-id
/// change as long as the engine itself has not been stopped.
fn send_reservation_valid(inner: &EngineInner, ctx: &SendContext) -> bool {
    if inner.stopped {
        return false;
    }
    // A stop/reset since Phase 1 — even one immediately followed by a restart that
    // cleared `stopped` and set `busy` again — bumps reset_epoch (stop_quiet). That
    // invalidates this send so it can't be delivered onto a turn the user canceled.
    if inner.reset_epoch != ctx.reset_epoch {
        return false;
    }
    // An interrupt can land while a direct send is still in Phase 2, before there is
    // any stdin write / child / active turn for interrupt() to act on — it only sets
    // `interrupting`. A direct send IS that turn, so honor the cancel and don't
    // deliver it. (Queued sends target a later turn and survive interrupting the
    // current one, so they are not rejected here.)
    if ctx.direct && inner.interrupting {
        return false;
    }
    if ctx.direct {
        inner.turn_id == ctx.turn && inner.turn.busy
    } else {
        // Queued sends don't own the busy flag: whether to enqueue (still busy),
        // promote into a fresh direct turn (turn ended → idle), or cancel
        // (interrupt teardown in flight) is decided at Phase 3 commit time from
        // CURRENT state. Only a stop/reset (checked above) invalidates them here.
        true
    }
}

/// Phase-3 promotion: a queued send found the engine IDLE at commit time (the
/// active turn ended while the send persisted), so it claims a fresh direct turn
/// instead of appending to a queue nothing will drain. Pure state transition —
/// the caller persists status / dispatches. Returns the promoted turn id.
fn promote_queued_reservation(inner: &mut EngineInner, origin_tag: Option<String>) -> i32 {
    let _ = inner.turn.try_begin_send();
    inner.turn_id += 1;
    inner.clock.begin_turn();
    inner.current_origin_tag = origin_tag;
    inner.turn_id
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
    // Pre-flight agent resolution: if the configured CLI can't be found on PATH, a
    // spawn would fail deep inside with a raw "No such file or directory (os error
    // 2)" that surfaces only as a generic "errored" label. Surface a friendly,
    // localizable row up front instead — this one check covers every transport
    // (resident, per-turn, codex app-server) — and skip the turn so the user can
    // install/point the agent and retry.
    //
    // Guards: (a) IDLE only — a busy engine means a turn already resolved+spawned
    // the CLI, so a follow-up must queue via try_begin_send(), not advance turn_id
    // here. (b) unix only — Windows GUIs inherit PATH fine, and which_on_path has
    // no PATHEXT/.exe lookup, so it would false-negative a valid `codex.exe`.
    if !cfg!(windows) {
        let (tool, command, thread_id, sid, busy) = {
            let g = eng.lock().await;
            (
                g.tool.clone(),
                crate::tool_command::effective(g.command.as_deref(), &g.tool),
                g.thread_id,
                g.session_id,
                g.turn.busy,
            )
        };
        // Match how the actual turn spawns: a bare `Command::new(command)` on the
        // augmented PATH. resolve_tool_path's Codex app-bundle fallback would say
        // "found" for a bundle the bare spawn can't reach, so use the PATH-only check.
        if !busy && !crate::detect::resolves_on_path(&command) {
            let turn = {
                let mut g = eng.lock().await;
                g.turn_id += 1;
                g.turn_id
            };
            let image_uris: Vec<String> = images
                .iter()
                .map(|(mt, data)| format!("data:{mt};base64,{data}"))
                .collect();
            let user = serde_json::json!({ "text": text, "images": image_uris, "files": files })
                .to_string();
            // Propagate insert failures (e.g. a locked/full DB): if the rows aren't
            // durably recorded we must NOT clear the composer, so `?` returns Err and
            // the normal error path preserves the draft. Only the emits are best-effort.
            let user_row =
                repo::insert_lead_message(db, thread_id, sid, turn, "user", "text", &user, "error")
                    .await?;
            let _ = app.emit(EVENT, Push::Message { thread_id, message: user_row });
            let notice =
                serde_json::json!({ "terminal": "agent_not_found", "tool": tool }).to_string();
            let notice_row = repo::insert_lead_message(
                db, thread_id, sid, turn, "assistant", "text", &notice, "error",
            )
            .await?;
            let _ = app.emit(EVENT, Push::Message { thread_id, message: notice_row });
            // Both rows are durably recorded, so resolve OK: returning Err here would
            // trip the composer's error path and restore the draft → duplicate on retry.
            return Ok(());
        }
    }
    ensure_running_for_send(app, db, eng).await?;

    // Phase 1: acquire the lock only long enough to reserve turn state and
    // snapshot the fields needed for persistence. All slow IO (DB writes,
    // image spills, stdin writes) happens after the lock drops so
    // stop/interrupt/status stay responsive for the session.
    let ctx = {
        let mut inner = eng.lock().await;
        let direct = inner.turn.try_begin_send();
        // Count only tracked (user-visible) items: hidden plumbing deliveries
        // (queue_id == None) are filtered out of the UI, so they must not eat the budget.
        if !direct && visible_queued(&inner.turn) >= MAX_QUEUED {
            return Err(anyhow::anyhow!("queue_full"));
        }
        if direct {
            inner.turn_id += 1;
            inner.clock.begin_turn();
            // This send starts a turn now → its tag IS the in-flight turn's tag.
            inner.current_origin_tag = origin_tag.clone();
            crate::power::on_turn_began(app);
            // Persist "running" WHILE holding the lock so it is ordered before any
            // concurrent stop's "stopped" write: stop_quiet must take this same lock
            // first, so its later "stopped" write can't be overtaken and leave a
            // stopped session recorded as running (which boot-revive would resume).
            persist_activity(db, inner.session_id, inner.thread_id, "running").await;
        }
        SendContext {
            thread_id: inner.thread_id,
            session_id: inner.session_id,
            turn: inner.turn_id,
            direct,
            is_command: text.trim_start().starts_with('/'),
            tool: inner.tool.clone(),
            origin_tag: origin_tag.clone(),
            reset_epoch: inner.reset_epoch,
        }
    };

    let kind = if ctx.is_command { "command" } else { "text" };
    let status = if ctx.direct { "complete" } else { "queued" };
    let image_uris: Vec<String> = images
        .iter()
        .map(|(mt, data)| format!("data:{mt};base64,{data}"))
        .collect();
    let content = if ctx.is_command {
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

    // Phase 2: persist the user row and spill per-turn image attachments without
    // holding the engine lock.
    let m = match repo::insert_lead_message(
        db,
        ctx.thread_id,
        ctx.session_id,
        ctx.turn,
        "user",
        kind,
        &content,
        status,
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            // Phase 1 already reserved turn state; undo it (and restore the
            // engine invariants — activity, queue, interrupt flag) so the
            // session isn't left with a stuck busy flag or an orphaned turn id.
            rollback_canceled_send(app, db, eng, &ctx, "error").await;
            return Err(e.into());
        }
    };
    let row_id = m.id;
    let _ = app.emit(
        EVENT,
        Push::Message {
            thread_id: ctx.thread_id,
            message: m,
        },
    );

    let mut outbound = text.to_string();
    // Capture BEFORE images may be spilled to temp files below (per-turn dialects
    // clear out.images after spill; has_attachments must reflect the original inputs).
    let has_attachments = !files.is_empty() || !images.is_empty();
    if !files.is_empty() {
        outbound.push_str("\n\nAttached files (read them as needed):\n");
        for f in &files {
            outbound.push_str(&format!("- {f}\n"));
        }
    }
    // Per-turn dialects take no inline image blocks: spill pasted images to
    // temp files and hand over paths — every agent can read those itself.
    let images = if per_turn(&ctx.tool) && !images.is_empty() {
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
        origin_tag: ctx.origin_tag.clone(),
        queue_id: if ctx.direct { None } else { Some(row_id) },
        has_attachments,
    };

    // Phase 3: re-acquire the lock and COMMIT against CURRENT state — deliver,
    // enqueue, promote, or abort is decided here, not enforced from the Phase-1
    // snapshot (which only decided the row's optimistic status). The lock drops
    // before any turn-spawning awaits.
    let is_codex_appserver = ctx.tool == "codex" && codex_appserver_enabled();
    let spawn_now = ctx.direct && per_turn(&ctx.tool) && !is_codex_appserver;
    // Set when a queued send found the engine idle and claimed a fresh turn.
    let mut promoted: Option<i32> = None;
    {
        let mut inner = eng.lock().await;
        // Stop/reset can race between Phase 1 and Phase 3. If the reservation
        // was invalidated while the lock was dropped, abort instead of writing
        // to a dead stdin, queueing on a drained turn, or spawning after stop.
        if !send_reservation_valid(&inner, &ctx) {
            drop(inner);
            // A stop/interrupt can land between ensure_running_for_send and
            // Phase 3, leaving Phase 1's reservation on an engine that will
            // never run it. Undo it and restore the invariants (busy, activity,
            // interrupt flag, anything queued behind the canceled turn) — the
            // rollback is ownership-guarded, so if a stop/reset cleared the turn
            // itself or a newer reservation owns the state, it no-ops.
            rollback_canceled_send(app, db, eng, &ctx, "interrupted").await;
            let _ = repo::update_lead_message(db, row_id, &content, "interrupted").await;
            emit_finalize(app, ctx.thread_id, row_id, "interrupted");
            return Err(anyhow::anyhow!(
                "send could not be delivered: the turn ended or the engine stopped while it was persisting"
            ));
        }
        if ctx.direct && !spawn_now && !is_codex_appserver {
            if let Err(e) = write_user(&mut inner, &out).await {
                drop(inner);
                rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content).await;
                return Err(e);
            }
        } else if !ctx.direct {
            if inner.turn.busy {
                // The queue cap was checked in Phase 1, but multiple queued sends
                // can race through DB/attachment I/O and observe the same count.
                // Re-check under the lock before appending to keep the limit real.
                if visible_queued(&inner.turn) >= MAX_QUEUED {
                    drop(inner);
                    let _ = repo::update_lead_message(db, row_id, &content, "error").await;
                    emit_finalize(app, ctx.thread_id, row_id, "error");
                    return Err(anyhow::anyhow!("queue_full"));
                }
                inner.turn.queue.push_back(out.clone());
            } else if inner.interrupting {
                // The turn this send queued behind is mid-interrupt-teardown;
                // promoting into that teardown would hand the interrupt a fresh
                // turn to kill. Cancel instead — the composer restores the draft
                // on the error path, so nothing is lost.
                drop(inner);
                let _ = repo::update_lead_message(db, row_id, &content, "interrupted").await;
                emit_finalize(app, ctx.thread_id, row_id, "interrupted");
                return Err(anyhow::anyhow!(
                    "send could not be delivered: the turn was interrupted while it was persisting"
                ));
            } else {
                // The active turn ENDED while this send persisted: nothing drains
                // an idle queue, so deliver NOW by promoting into a fresh direct
                // turn — the same commit-time decision a direct send makes.
                promoted = Some(promote_queued_reservation(&mut inner, ctx.origin_tag.clone()));
                crate::power::on_turn_began(app);
                // Under the lock for the same ordering reason as Phase 1's direct
                // write: a concurrent stop's "stopped" write must not be overtaken.
                persist_activity(db, inner.session_id, inner.thread_id, "running").await;
                if !per_turn(&ctx.tool) && !is_codex_appserver {
                    // Resident tool: deliver through the live stdin under this
                    // lock, exactly like a direct resident send.
                    if let Err(e) = write_user(&mut inner, &out).await {
                        // Still under the lock, so the promotion is provably ours
                        // to undo inline.
                        inner.turn.busy = false;
                        inner.turn_id -= 1;
                        inner.current_origin_tag = None;
                        inner.clock.started = None;
                        drop(inner);
                        let _ = repo::update_lead_message(db, row_id, &content, "error").await;
                        emit_finalize(app, ctx.thread_id, row_id, "error");
                        return Err(e);
                    }
                }
            }
        }
        let _ = app.emit(
            EVENT,
            Push::Turn {
                thread_id: ctx.thread_id,
                session_id: ctx.session_id,
                state: if inner.turn.busy { "busy" } else { "idle" }.into(),
                queue: queue_items(&inner.turn),
            },
        );
    }

    // Phase 4: turn spawning runs without the engine lock. The spawn helpers
    // re-check `stopped` AND the send's reset_epoch atomically with the child
    // snapshot, so neither a plain stop nor a stop-then-restart landing in the
    // Phase-3-to-spawn window can launch a child for a canceled send.
    if spawn_now {
        if let Err(e) =
            spawn_turn(app.clone(), db.clone(), eng.clone(), out, Some(ctx.reset_epoch)).await
        {
            rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content).await;
            return Err(e);
        }
    } else if ctx.direct && is_codex_appserver {
        if let Err(e) = spawn_codex_turn_or_exec(
            app.clone(),
            db.clone(),
            eng.clone(),
            out,
            Some(ctx.reset_epoch),
        )
        .await
        {
            rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content).await;
            return Err(e);
        }
    } else if let Some(pturn) = promoted {
        // The promoted send owns a fresh turn now. Flip its row to delivered
        // (complete + delivery seq + finalize emit, same as a drained queue item),
        // then spawn the turn for per-turn tools; resident stdin was already
        // written under the Phase 3 lock.
        mark_queued_delivered(app, db, ctx.thread_id, ctx.session_id, &out).await;
        let dispatched = Outgoing {
            queue_id: None,
            ..out.clone()
        };
        if per_turn(&ctx.tool) && !is_codex_appserver {
            if let Err(e) = spawn_turn(
                app.clone(),
                db.clone(),
                eng.clone(),
                dispatched,
                Some(ctx.reset_epoch),
            )
            .await
            {
                rollback_failed_visible_turn(app, db, eng, pturn, row_id, &content).await;
                return Err(e);
            }
        } else if is_codex_appserver {
            if let Err(e) = spawn_codex_turn_or_exec(
                app.clone(),
                db.clone(),
                eng.clone(),
                dispatched,
                Some(ctx.reset_epoch),
            )
            .await
            {
                rollback_failed_visible_turn(app, db, eng, pturn, row_id, &content).await;
                return Err(e);
            }
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
    expected_epoch: Option<u64>,
) -> anyhow::Result<()> {
    let (native, cwd, sid, thread_id_i, system_prompt, extra_args, existing, program) = {
        let i = eng.lock().await;
        // Atomic with the snapshot: don't start a codex turn for a stopped engine
        // (a stop racing the send's Phase-3-to-spawn window, which is widest on the
        // app-server path because connection/start awaits happen after this) — nor
        // for a send whose reservation epoch a stop-then-restart invalidated. The
        // caller rolls back or falls through — and the exec fallback (spawn_turn)
        // makes the same check — so returning here is safe.
        if i.stopped || expected_epoch.is_some_and(|e| e != i.reset_epoch) {
            return Err(anyhow::anyhow!("engine stopped; not starting a codex turn"));
        }
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
    // stop_quiet may have run during the connect / start_thread / subscribe awaits
    // above, when there was no `codex_client` for it to shut down. If the stop won
    // that race — including a stop-then-restart, which clears `stopped` but bumps
    // the epoch — tear the freshly connected client down and abort rather than
    // starting a turn the user canceled. (The early snapshot check only covers
    // stops that happened before the connect.)
    let stop_won = {
        let g = eng.lock().await;
        g.stopped || expected_epoch.is_some_and(|e| e != g.reset_epoch)
    };
    if stop_won {
        if let Some(c) = eng.lock().await.codex_client.take() {
            c.shutdown().await;
        }
        return Err(anyhow::anyhow!(
            "engine stopped during codex app-server connect"
        ));
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
    expected_epoch: Option<u64>,
) -> anyhow::Result<()> {
    if let Err(e) =
        spawn_codex_turn(app.clone(), db.clone(), eng.clone(), out.clone(), expected_epoch).await
    {
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
        spawn_turn(app, db, eng, out, expected_epoch).await?;
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
            ThreadMsg::Event(ChatEvent::Assistant { texts, tools }) => {
                // Codex streams text via deltas; non-text items are tool calls →
                // inline `kind:"tool"` rows, filled by their item.completed result.
                let mut inner = eng.lock().await;
                inner.clock.last_activity = std::time::Instant::now();
                // Finalize the open text row when the agent message *completes*
                // (texts non-empty) so its streaming caret clears at text-end — and
                // before any tool row, so later deltas open a fresh row BELOW it,
                // keeping "I'll inspect…" → command → explanation flows in order
                // instead of stacking post-tool prose above the tool. The accumulated
                // deltas are the body; `texts` is only the finalize trigger.
                if !texts.is_empty() || !tools.is_empty() {
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
                // Turn end is the natural checkpoint: last_* is at its freshest,
                // and one write per turn keeps the persistence cost trivial.
                persist_engine_meta(&db, &inner).await;
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
                // Captured at dequeue, under this lock: if the flush below falls
                // back to exec after a stop-then-restart, `stopped` is clear again
                // but the epoch has advanced — the canceled message must not launch.
                let dequeue_epoch = inner.reset_epoch;
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
                        queue: queue_items(&inner.turn),
                    },
                );
                drop(inner);
                // This turn is over: drop its active-turn id so a subsequent
                // interrupt won't target a finished turn (the flush below re-sets
                // it for the next turn).
                client.clear_active_turn(&thread).await;
                // Flush: start the next queued message as a fresh turn on this thread.
                // Gated on the dequeue-time epoch: a stop — or stop-then-restart,
                // which clears `stopped` but bumps the epoch — since the pop must
                // not deliver the canceled message on the app-server path either
                // (the exec fallback below already checks it). The TOCTOU after
                // this check is bounded: stop shuts the client down, failing
                // pending requests and closing this consumer.
                let flush_stop_won = {
                    let g = eng.lock().await;
                    g.stopped || g.reset_epoch != dequeue_epoch
                };
                if flush_stop_won {
                    // Ownership-guarded (turn_id + busy): no-ops when the stop
                    // already reset the turn itself.
                    if let Some(turn_id) = next_turn_id {
                        rollback_failed_turn(&app, &db, &eng, turn_id).await;
                    }
                } else if let (Some(n), Some(turn_id)) = (next, next_turn_id) {
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
                                match spawn_turn(
                                    app.clone(),
                                    db.clone(),
                                    eng.clone(),
                                    n.clone(),
                                    Some(dequeue_epoch),
                                )
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
async fn spawn_turn(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    out: Outgoing,
    expected_epoch: Option<u64>,
) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    // Atomic with the child snapshot below: never launch a per-turn process for a
    // stopped engine — a stop that raced into the send's Phase-3-to-spawn window,
    // or a queued turn drained just as the human stopped. Send-originated spawns
    // also pass their reservation's reset_epoch: a stop-THEN-RESTART clears
    // `stopped` again, but bumps the epoch, so the canceled send still can't
    // launch a child onto the restarted engine. Every caller already rolls back
    // or propagates a spawn error, so returning here is safe.
    if inner.stopped || expected_epoch.is_some_and(|e| e != inner.reset_epoch) {
        return Err(anyhow::anyhow!("engine stopped; not spawning a turn"));
    }
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
        .env("PATH", crate::detect::tool_path())
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

/// 取消一条还在队列中的消息；幂等（消息已交付则静默成功）。
pub async fn queue_remove(app: &AppHandle, db: &Db, eng: &EngineRef, message_id: i32) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    if !inner.turn.remove(message_id) {
        return Ok(());
    }
    emit_turn_state(app, inner.thread_id, inner.session_id, inner.turn.busy, queue_items(&inner.turn));
    // Delete under the lock so a concurrent stop (mark_queued_status also takes
    // the lock) cannot re-finalize a row we just removed from memory.
    repo::delete_message(db, message_id).await?;
    Ok(())
}

/// 编辑一条还在队列中的消息文本；text 为空或有附件时返回 Err。
pub async fn queue_edit(app: &AppHandle, db: &Db, eng: &EngineRef, message_id: i32, text: &str) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        return Err(anyhow::anyhow!("empty"));
    }
    let thread_id = {
        let mut inner = eng.lock().await;
        // Reject edits on attachment-bearing rows: they carry image/file chips
        // in their content that the text-only edit path would silently drop.
        if inner.turn.queue.iter().any(|o| o.queue_id == Some(message_id) && o.has_attachments) {
            return Err(anyhow::anyhow!("not_editable"));
        }
        if !inner.turn.edit(message_id, text) {
            return Ok(());
        }
        let (tid, sid) = (inner.thread_id, inner.session_id);
        emit_turn_state(app, tid, sid, inner.turn.busy, queue_items(&inner.turn));
        tid
    };
    // Preserve existing images/files; only replace the text field.
    let content = if let Some(row) = repo::get_message(db, message_id).await? {
        let mut val: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::json!({}));
        val["text"] = serde_json::Value::String(text.to_string());
        val.to_string()
    } else {
        serde_json::json!({ "text": text, "images": [], "files": [] }).to_string()
    };
    repo::update_message_content(db, message_id, &content).await?;
    // Push the edited text to the FE cache immediately so a subsequent stop/deliver
    // finalizes the edited version, not the stale original.
    let _ = app.emit(
        EVENT,
        Push::Finalize {
            thread_id,
            message_id,
            status: "queued".into(),
            content: Some(text.to_string()),
        },
    );
    Ok(())
}

/// 重排队列；order 必须是当前队列 id 的排列，否则返回 Err。
pub async fn queue_reorder(app: &AppHandle, _db: &Db, eng: &EngineRef, order: Vec<i32>) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    let ok = inner.turn.reorder(&order);
    let (tid, sid) = (inner.thread_id, inner.session_id);
    // Re-emit the authoritative order even on rejection, so an optimistic drag the
    // backend refused (bad permutation / pending bus wake) snaps back in the UI.
    emit_turn_state(app, tid, sid, inner.turn.busy, queue_items(&inner.turn));
    if !ok {
        return Err(anyhow::anyhow!("bad_order"));
    }
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
        queue_id: None,
        has_attachments: false,
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
            // Captured under the lock: a stop-then-restart before the spawn task
            // runs clears `stopped` but bumps the epoch — a canceled hidden turn
            // (bus read / tool-result nudge) must not launch on the restarted
            // engine, same guard as user-visible sends and queued deliveries.
            let hidden_epoch = inner.reset_epoch;
            drop(inner);
            let res = if codex_appserver {
                spawn_codex_turn_or_exec(
                    app.clone(),
                    db.clone(),
                    eng.clone(),
                    out,
                    Some(hidden_epoch),
                )
                .await
            } else {
                spawn_turn(app.clone(), db.clone(), eng.clone(), out, Some(hidden_epoch)).await
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
    // Invalidate any send that reserved against the turn we just cleared — even one
    // whose Phase 1 ran before this stop but whose Phase 3 runs after, and even a
    // stop-then-restart (which resets `stopped`/`busy` and would otherwise slip
    // past those flags). send_reservation_valid compares the captured reset_epoch.
    inner.reset_epoch += 1;
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
            queue: Vec::new(),
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
                    // Mirror the frontend's metaFromInit invariant: only an init
                    // that carries a model is authoritative (may replace, even
                    // with empty lists — the session truly has no MCP). A
                    // model-less/partial init is fill-only, or the checkpoint
                    // below would persist a blank snapshot over restored meta
                    // merely because the chat was reopened after a relaunch.
                    if model.is_some() {
                        inner.last_mcp_servers = mcp_servers.clone();
                        inner.last_tools = tools.clone();
                        inner.last_model = model.clone();
                        inner.last_window = window;
                    } else {
                        if inner.last_mcp_servers.is_empty() {
                            inner.last_mcp_servers = mcp_servers.clone();
                        }
                        if inner.last_tools.is_empty() {
                            inner.last_tools = tools.clone();
                        }
                    }
                    // Persist at init too: if the app dies mid-turn, the
                    // MCP/model snapshot still survives the relaunch.
                    persist_engine_meta(&db, &inner).await;
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
                        let (clean, sentinels) = super::sentinels::extract_sentinels_with(
                            &full,
                            inner.session_id.is_none(),
                        );
                        let content = serde_json::json!({ "text": clean }).to_string();
                        match inner.current.take() {
                            Some((id, _, _)) => {
                                let _ =
                                    repo::update_lead_message(&db, id, &content, "complete").await;
                                // When sentinels were stripped, the live row still
                                // shows the raw streamed tags — send the cleaned
                                // body so the UI replaces it without a reload
                                // (test_cases bodies are entire documents).
                                let stripped = clean != full;
                                let _ = app.emit(
                                    EVENT,
                                    Push::Finalize {
                                        thread_id,
                                        message_id: id,
                                        status: "complete".into(),
                                        content: if stripped { Some(clean.clone()) } else { None },
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
                    // Same turn-end checkpoint as the app-server consumer: the
                    // reader transports (claude / codex exec / opencode) must
                    // persist too, or their sessions relaunch with stale meta.
                    persist_engine_meta(&db, &inner).await;
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
                        // Captured at dequeue, under this lock: a stop-then-restart
                        // before the spawned task runs clears `stopped` but bumps the
                        // epoch, and the canceled queued message must not launch.
                        let dequeue_epoch = inner.reset_epoch;
                        if per_turn(&inner.tool) {
                            let (a, d, e) = (app.clone(), db.clone(), eng.clone());
                            tauri::async_runtime::spawn(async move {
                                if let Err(err) = spawn_turn(
                                    a.clone(),
                                    d.clone(),
                                    e.clone(),
                                    next.clone(),
                                    Some(dequeue_epoch),
                                )
                                .await
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
                            queue: queue_items(&inner.turn),
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
                // Captured at dequeue, under this lock — see the per-turn drain
                // above: stop-then-restart must not launch a canceled message.
                let dequeue_epoch = inner.reset_epoch;
                let (a, d, e) = (app.clone(), db.clone(), eng.clone());
                tauri::async_runtime::spawn(async move {
                    if let Err(err) = spawn_turn(
                        a.clone(),
                        d.clone(),
                        e.clone(),
                        next.clone(),
                        Some(dequeue_epoch),
                    )
                    .await
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
                    queue: queue_items(&inner.turn),
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
                    queue: Vec::new(),
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

    /// PersistedMeta roundtrip + tolerance: apply restores every last_* field,
    /// while empty/corrupt JSON leaves the fresh engine untouched.
    #[test]
    fn persisted_meta_roundtrip_and_tolerance() {
        let snap = PersistedMeta {
            context_tokens: Some(57_000),
            window: Some(200_000),
            model: Some("claude-sonnet-4-5".into()),
            mcp_servers: vec![super::super::proto::McpServer {
                name: "context7".into(),
                status: "connected".into(),
            }],
            tools: vec!["mcp__context7__query-docs".into()],
        };
        let json = serde_json::to_string(&snap).unwrap();

        let mut inner = test_inner("claude");
        apply_persisted_meta(&mut inner, &json);
        assert_eq!(inner.last_context_tokens, Some(57_000));
        assert_eq!(inner.last_window, Some(200_000));
        assert_eq!(inner.last_model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(inner.last_mcp_servers.len(), 1);
        assert_eq!(inner.last_tools.len(), 1);

        // Empty and corrupt snapshots are silent no-ops.
        let mut fresh = test_inner("claude");
        apply_persisted_meta(&mut fresh, "");
        apply_persisted_meta(&mut fresh, "{not json");
        assert_eq!(fresh.last_context_tokens, None);
        assert!(fresh.last_mcp_servers.is_empty());

        // Old snapshots missing optional arrays still deserialize (serde defaults).
        let mut sparse = test_inner("claude");
        apply_persisted_meta(&mut sparse, r#"{"context_tokens":1,"window":2,"model":null}"#);
        assert_eq!(sparse.last_context_tokens, Some(1));
        assert!(sparse.last_tools.is_empty());
    }

    /// merge_probe semantics: the freshest result may overwrite (usage only on
    /// probe-sourced transports); a stale result degrades to fill-a-hole for
    /// EVERY field — including MCP, so a late non-empty list can't resurrect
    /// servers a newer authoritative empty probe just cleared.
    #[test]
    fn merge_probe_usage_gate() {
        let mcp = |names: &[&str]| {
            names
                .iter()
                .map(|n| super::super::proto::McpServer {
                    name: (*n).into(),
                    status: "connected".into(),
                })
                .collect::<Vec<_>>()
        };
        let snap = crate::session_meta::SessionMetaSnapshot {
            context_tokens: Some(999),
            window: Some(200_000),
            model: Some("gpt-5.6-sol".into()),
            mcp_servers: Some(mcp(&["old-server"])),
            skills: None,
            reasoning_effort: None,
        };
        // Freshest + usage-from-events (claude/codex): usage fills a hole only.
        let mut hole = PersistedMeta::default();
        assert!(hole.merge_probe(&snap, true, true));
        assert_eq!(hole.context_tokens, Some(999));
        let mut known = PersistedMeta {
            context_tokens: Some(57_000),
            ..Default::default()
        };
        known.merge_probe(&snap, true, true);
        assert_eq!(known.context_tokens, Some(57_000), "eventful usage must not be overwritten");
        assert_eq!(known.model.as_deref(), Some("gpt-5.6-sol"), "config updates when freshest");
        // Freshest + probe-sourced usage (opencode): overwrites.
        let mut live = PersistedMeta {
            context_tokens: Some(57_000),
            ..Default::default()
        };
        assert!(live.merge_probe(&snap, true, false));
        assert_eq!(live.context_tokens, Some(999));
        // Stale: every field is fill-only — an authoritative empty MCP list
        // (user removed servers) survives a late non-empty result.
        let mut cleared = PersistedMeta {
            context_tokens: Some(57_000),
            window: Some(100_000),
            model: Some("kept".into()),
            mcp_servers: vec![],
            tools: vec![],
        };
        cleared.merge_probe(&snap, false, false);
        assert_eq!(cleared.context_tokens, Some(57_000));
        assert_eq!(cleared.model.as_deref(), Some("kept"));
        assert_eq!(cleared.window, Some(100_000));
        // MCP never moves on a stale result — an empty list may be a newer
        // probe's authoritative "user removed every server", so even a fill
        // would resurrect them.
        assert!(cleared.mcp_servers.is_empty(), "stale must not touch MCP");
        let mut populated = PersistedMeta {
            mcp_servers: mcp(&["new-server"]),
            ..Default::default()
        };
        populated.merge_probe(&snap, false, false);
        assert_eq!(populated.mcp_servers[0].name, "new-server");
        // Transport mapping.
        assert!(usage_events_authoritative("claude"));
        assert!(usage_events_authoritative("codex"));
        assert!(!usage_events_authoritative("opencode"));
    }

    /// A queued (undelivered) user edit of the test cases marks any in-flight
    /// lead emit as stale; ordinary queued messages do not.
    #[test]
    fn pending_user_test_update_detection() {
        let mk = |text: &str| Outgoing {
            text: text.into(),
            images: vec![],
            tracked: false,
            origin_tag: None,
            queue_id: None,
            has_attachments: false,
        };
        let mut turn = TurnState::default();
        assert!(!has_pending_user_test_update(&turn));
        turn.queue.push_back(mk("hello lead"));
        assert!(!has_pending_user_test_update(&turn));
        turn.queue
            .push_back(mk("<weft:test_cases_updated>{\"source\":\"user\",\"content\":\"# v\"}</weft:test_cases_updated>"));
        assert!(has_pending_user_test_update(&turn));
    }

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
            queue_id: None,
            has_attachments: false,
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
            queue_id: None,
            has_attachments: false,
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
            queue_id: None,
            has_attachments: false,
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
    fn queue_is_capped_at_max() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy（占用一个在飞 turn）
        for i in 0..MAX_QUEUED {
            assert!(!t.try_begin_send()); // busy
            t.queue.push_back(Outgoing { text: format!("m{i}"), ..Default::default() });
        }
        assert_eq!(t.queue.len(), MAX_QUEUED);
        // Full-queue rejection (send() returning Err("queue_full")) is an async/DB path
        // not exercisable at the TurnState level; this test only asserts the queue fills
        // to exactly MAX_QUEUED.
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
    fn queue_items_preserves_order_and_text() {
        let mut t = TurnState::default();
        t.queue.push_back(Outgoing { text: "a".into(), queue_id: Some(1), ..Default::default() });
        t.queue.push_back(Outgoing { text: "b".into(), queue_id: Some(2), ..Default::default() });
        let items = queue_items(&t);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].id, items[0].text.as_str()), (1, "a"));
        assert_eq!((items[1].id, items[1].text.as_str()), (2, "b"));
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
            reset_epoch: 0,
            pending_skill_refresh: false,
            pending_command_refresh: false,
            last_context_tokens: None,
            last_model: None,
            last_window: None,
            last_mcp_servers: vec![],
            last_tools: vec![],
            probe_seq: 0,
            probe_committed: 0,
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
            queue_id: None,
            has_attachments: false,
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
            queue_id: None,
            has_attachments: false,
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
    fn send_reservation_valid_requires_stopped_turn_and_busy_flag() {
        let mut inner = test_inner("claude");
        inner.turn_id = 5;
        inner.turn.busy = true;
        let direct_ctx = SendContext {
            thread_id: 1,
            session_id: None,
            turn: 5,
            direct: true,
            is_command: false,
            tool: "claude".into(),
            origin_tag: None,
            reset_epoch: 0,
        };
        assert!(send_reservation_valid(&inner, &direct_ctx));

        // A reset_epoch bump (a stop/reset since Phase 1 — including a stop that was
        // immediately restarted, which leaves turn_id/busy looking valid) invalidates
        // BOTH a direct and a queued reservation.
        inner.reset_epoch = 1;
        assert!(!send_reservation_valid(&inner, &direct_ctx));
        assert!(!send_reservation_valid(
            &inner,
            &SendContext { direct: false, ..direct_ctx.clone() }
        ));
        inner.reset_epoch = 0;

        // An interrupt mid-send cancels a DIRECT reservation (the direct send IS the
        // current turn), but not a queued one, which targets a later turn.
        inner.interrupting = true;
        assert!(!send_reservation_valid(&inner, &direct_ctx));
        assert!(send_reservation_valid(
            &inner,
            &SendContext { direct: false, ..direct_ctx.clone() }
        ));
        inner.interrupting = false;

        // Stopped engine invalidates any reservation.
        inner.stopped = true;
        assert!(!send_reservation_valid(&inner, &direct_ctx));
        inner.stopped = false;

        // Turn identity mismatch means the reservation was reset.
        inner.turn_id = 6;
        assert!(!send_reservation_valid(&inner, &direct_ctx));
        inner.turn_id = 5;

        // Direct send must still hold the busy flag it reserved.
        inner.turn.busy = false;
        assert!(!send_reservation_valid(&inner, &direct_ctx));
        inner.turn.busy = true;

        // Queued sends don't own the busy flag: the enqueue/promote/cancel decision
        // is made at Phase 3 commit time from CURRENT state, so validation lets them
        // through regardless of turn advance (turn_id) or the turn having ended
        // (busy=false → Phase 3 promotes instead of stranding the message).
        let queued_ctx = SendContext {
            direct: false,
            ..direct_ctx
        };
        assert!(send_reservation_valid(&inner, &queued_ctx)); // busy = true
        inner.turn_id = 6; // active turn advanced, still busy → tolerated
        assert!(send_reservation_valid(&inner, &queued_ctx));
        inner.turn.busy = false; // turn ended → still valid; Phase 3 promotes
        assert!(send_reservation_valid(&inner, &queued_ctx));
        inner.turn.busy = true;
        // An explicit stop still invalidates a queued reservation.
        inner.stopped = true;
        assert!(!send_reservation_valid(&inner, &queued_ctx));
    }

    #[test]
    fn promote_queued_reservation_claims_a_fresh_direct_turn() {
        let mut inner = test_inner("claude");
        inner.turn_id = 7;
        assert!(!inner.turn.busy, "precondition: engine idle");
        let promoted = promote_queued_reservation(&mut inner, Some("tag".into()));
        assert_eq!(promoted, 8, "promotion claims the NEXT turn id");
        assert_eq!(inner.turn_id, 8);
        assert!(inner.turn.busy, "promotion reserves the turn (busy)");
        assert_eq!(inner.current_origin_tag.as_deref(), Some("tag"));
        assert!(inner.clock.started.is_some(), "promotion starts the turn clock");
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
            reset_epoch: 0,
            pending_skill_refresh: false,
            pending_command_refresh: false,
            last_context_tokens: None,
            last_model: None,
            last_window: None,
            last_mcp_servers: vec![],
            last_tools: vec![],
            probe_seq: 0,
            probe_committed: 0,
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

    #[test]
    fn turnstate_remove_edit_reorder() {
        let mut t = TurnState::default();
        for id in [10, 20, 30] {
            t.queue.push_back(Outgoing { text: format!("t{id}"), queue_id: Some(id), ..Default::default() });
        }
        assert!(t.edit(20, "edited"));
        assert_eq!(t.queue[1].text, "edited");

        assert!(t.reorder(&[30, 10, 20]));
        let ids: Vec<i32> = t.queue.iter().filter_map(|o| o.queue_id).collect();
        assert_eq!(ids, vec![30, 10, 20]);

        assert!(t.remove(10));
        let ids: Vec<i32> = t.queue.iter().filter_map(|o| o.queue_id).collect();
        assert_eq!(ids, vec![30, 20]);

        // 非排列 / 未知 id 被拒
        assert!(!t.reorder(&[30])); // 长度不符
        assert!(!t.reorder(&[30, 99])); // same length, unknown id → rejected
        let ids: Vec<i32> = t.queue.iter().filter_map(|o| o.queue_id).collect();
        assert_eq!(ids, vec![30, 20]); // queue untouched
        assert!(!t.remove(999));
        assert!(!t.edit(999, "x"));
    }

    #[test]
    fn reorder_preserves_untracked_items() {
        // Visible T1, an internal untracked delivery, then visible T2.
        let mut t = TurnState::default();
        t.queue.push_back(Outgoing { text: "t1".into(), queue_id: Some(10), ..Default::default() });
        t.queue.push_back(Outgoing { text: "nudge".into(), tracked: false, queue_id: None, ..Default::default() });
        t.queue.push_back(Outgoing { text: "t2".into(), queue_id: Some(20), ..Default::default() });
        // Reorder the two visible items; the untracked nudge must keep its slot.
        assert!(t.reorder(&[20, 10]));
        let ids: Vec<Option<i32>> = t.queue.iter().map(|o| o.queue_id).collect();
        assert_eq!(ids, vec![Some(20), None, Some(10)]);
        assert_eq!(t.queue.len(), 3, "untracked nudge must not be dropped");
        assert_eq!(t.queue[1].text, "nudge");
    }

    #[test]
    fn remove_keeps_bus_read_position_in_sync() {
        // A, B queued; a bus wake lands (read at index 2); then C queued.
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy
        t.queue.push_back(Outgoing { text: "a".into(), queue_id: Some(1), ..Default::default() });
        t.queue.push_back(Outgoing { text: "b".into(), queue_id: Some(2), ..Default::default() });
        assert!(!t.request_bus_read()); // busy → coalesced at index 2
        assert_eq!(t.bus_read_pos, Some(2));
        t.queue.push_back(Outgoing { text: "c".into(), queue_id: Some(3), ..Default::default() });
        // Deleting A (index 0, before the wake) shifts the wake left so C still
        // delivers AFTER the inbox-read, not ahead of it.
        assert!(t.remove(1));
        assert_eq!(t.bus_read_pos, Some(1));
        // Deleting C (index 1, == wake index, not before) leaves the wake put.
        assert!(t.remove(3));
        assert_eq!(t.bus_read_pos, Some(1));
    }

    #[test]
    fn cap_counts_only_visible_items() {
        let mut t = TurnState::default();
        // 4 visible user sends + 1 hidden plumbing delivery interleaved.
        t.queue.push_back(Outgoing { queue_id: Some(1), ..Default::default() });
        t.queue.push_back(Outgoing { queue_id: None, tracked: false, ..Default::default() });
        t.queue.push_back(Outgoing { queue_id: Some(2), ..Default::default() });
        t.queue.push_back(Outgoing { queue_id: Some(3), ..Default::default() });
        t.queue.push_back(Outgoing { queue_id: Some(4), ..Default::default() });
        assert_eq!(t.queue.len(), 5);
        assert_eq!(visible_queued(&t), 4, "hidden delivery must not eat the cap budget");
    }

    #[test]
    fn reorder_refused_while_bus_wake_pending() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy
        t.queue.push_back(Outgoing { queue_id: Some(1), ..Default::default() });
        t.queue.push_back(Outgoing { queue_id: Some(2), ..Default::default() });
        assert!(!t.request_bus_read()); // wake coalesced at index 2
        assert!(t.bus_read_pos.is_some());
        // A valid permutation is still refused while the wake is pending, so the
        // wake can't be mis-placed relative to a dragged message.
        assert!(!t.reorder(&[2, 1]));
        let ids: Vec<i32> = t.queue.iter().filter_map(|o| o.queue_id).collect();
        assert_eq!(ids, vec![1, 2], "queue untouched on refusal");
    }

    #[test]
    fn finalize_text_only_replaces_plain_rows() {
        use crate::store::entities::lead_message::Model;
        let row = |kind: &str, content: &str| Model {
            id: 1,
            thread_id: 1,
            session_id: None,
            turn_id: 1,
            role: "user".into(),
            kind: kind.into(),
            content: content.into(),
            status: "complete".into(),
            created_at: "0".into(),
            seq: None,
        };
        let plain = Outgoing { text: "edited".into(), queue_id: Some(1), ..Default::default() };
        // Plain text, no attachments → use the (edited) Outgoing text.
        assert_eq!(
            finalize_text(&row("text", r#"{"text":"orig","images":[],"files":[]}"#), &plain),
            Some("edited".to_string()),
        );
        // Persisted images but out.images cleared (per-turn spill) → keep cached body.
        let spilled = Outgoing { text: "/tmp/x.png".into(), images: vec![], queue_id: Some(1), ..Default::default() };
        assert_eq!(
            finalize_text(&row("text", r#"{"text":"","images":["data:..."],"files":[]}"#), &spilled),
            None,
        );
        // Resident inline image (out.images non-empty) → keep cached body.
        let resident = Outgoing {
            text: "hi".into(),
            images: vec![("image/png".into(), "abc".into())],
            queue_id: Some(1),
            ..Default::default()
        };
        assert_eq!(finalize_text(&row("text", r#"{"text":"hi"}"#), &resident), None);
        // Command row → keep cached body.
        assert_eq!(finalize_text(&row("command", r#"{"command":"x","args":""}"#), &plain), None);
    }

    /// queue_edit must preserve images/files in the persisted row; only text changes.
    #[tokio::test]
    async fn queue_edit_preserves_images_and_files_in_persisted_row() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        // Insert a queued message that has images and files in its content.
        let original = serde_json::json!({
            "text": "original text",
            "images": [{"data": "abc", "media_type": "image/png"}],
            "files": ["/tmp/attach.txt"]
        })
        .to_string();
        let row = repo::insert_lead_message(&db, 1, None, 1, "user", "text", &original, "queued")
            .await
            .unwrap();

        // Simulate what queue_edit now does: read row, update text only.
        let existing = repo::get_message(&db, row.id).await.unwrap().unwrap();
        let mut val: serde_json::Value =
            serde_json::from_str(&existing.content).unwrap();
        val["text"] = serde_json::Value::String("edited text".into());
        repo::update_message_content(&db, row.id, &val.to_string()).await.unwrap();

        let updated = repo::get_message(&db, row.id).await.unwrap().unwrap();
        let content: serde_json::Value = serde_json::from_str(&updated.content).unwrap();
        assert_eq!(content["text"], "edited text");
        assert!(content["images"].is_array());
        assert_eq!(content["images"].as_array().unwrap().len(), 1, "images must be preserved");
        assert!(content["files"].is_array());
        assert_eq!(content["files"][0], "/tmp/attach.txt", "files must be preserved");
    }

    /// FIX 1: an Outgoing with files or images exposes has_attachments=true via queue_items.
    #[test]
    fn queue_items_exposes_has_attachments() {
        let mut turn = TurnState::default();
        turn.busy = true;
        // One attachment-bearing item.
        turn.queue.push_back(Outgoing {
            text: "look at this".into(),
            images: vec![("image/png".into(), "abc".into())],
            tracked: true,
            queue_id: Some(1),
            has_attachments: true,
            ..Default::default()
        });
        // One plain text item.
        turn.queue.push_back(Outgoing {
            text: "just text".into(),
            tracked: true,
            queue_id: Some(2),
            has_attachments: false,
            ..Default::default()
        });
        let items = queue_items(&turn);
        assert_eq!(items.len(), 2);
        assert!(items[0].has_attachments, "attachment item must report has_attachments=true");
        assert!(!items[1].has_attachments, "plain text item must report has_attachments=false");
    }
}
