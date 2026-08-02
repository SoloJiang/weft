//! In-memory thread-bus state: per-thread inboxes (keyed by direction), a shared
//! JSON state blob, the message timeline, and the set of known member directions.
//! Identity is always supplied by the caller (the HTTP handler derives it from
//! the URL path), never trusted from agent input.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

/// The sentinel "direction" id for the human operator. Agents address the human
/// through this; a wake on it tells the UI an ask is waiting.
pub const HUMAN: &str = "you";

/// The sentinel "direction" id for the thread lead. Workers address the lead
/// through this; a wake on it drives the lead engine to read its inbox. Unlike a
/// worker, the lead has no numeric direction id, so it joins the bus under this
/// stable name (one lead per thread).
pub const LEAD: &str = "lead";

/// Emitted when a direction should be woken to read its inbox.
#[derive(Clone, Debug)]
pub struct Wake {
    pub thread: i32,
    pub dir: String,
}

/// Bus → IM 桥的通知：agent 的人类提问（ask_human）开/答。镜像 wake 的
/// set_sender 模式；没装时零开销。Ask 的 from 是 direction id 字符串，
/// 富化（thread 标题、direction 名）是消费侧查 DB 的责任。
#[derive(Clone, Debug)]
pub enum HumanAskEvent {
    Asked {
        thread: i32,
        ask: Ask,
    },
    /// 携带人答的 text：飞书卡片终态要显示答案，而桌面侧作答时桥拿不到
    /// 文本，必须由事件携带。`from`/`question` 供 transcript 结算痕迹消费者
    /// 把痕迹落到提问 worker 的 transcript（并显示原问题）。
    Answered {
        thread: i32,
        ask_id: u64,
        /// 提问方向 id（字符串）；"" 为 lead/规划会话。
        from: String,
        /// 原问题文本（区别于人答的 `text`）。
        question: String,
        text: String,
    },
    /// The backing thread/workspace is gone; clear any external cards without
    /// delivering a synthetic answer back to an agent that is being stopped.
    Cancelled {
        thread: i32,
        ask_id: u64,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Msg {
    pub from: String,
    pub to: String, // "*" for broadcast
    pub text: String,
    pub ts: u64,
    pub kind: String, // "message" | "interface" | "ask"
    /// Durable human_request identity. The asking agent acknowledges these
    /// messages explicitly with `bus_ack` after incorporating the answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
}

/// Bus asks are concrete, answerable human questions. Informational notices
/// belong in timeline/status telemetry and never enter this channel.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    Question,
}

/// A question an agent direction has put to the human, awaiting an answer.
/// This is the clean, non-TUI signal behind the "Needs-you" surface.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Ask {
    pub id: u64,
    pub from: String, // asking direction id (as string)
    pub text: String,
    pub ts: u64,
    pub answered: bool,
    /// See [`AskKind`]. The single variant makes informational notices
    /// unrepresentable in this actionable channel.
    pub kind: AskKind,
    /// True only for an ask backed by human_request. Internal routing uses this
    /// to attach a durable delivery id without changing the public ask shape.
    #[serde(skip)]
    pub durable: bool,
}

#[derive(Default)]
struct ThreadBus {
    inboxes: HashMap<String, Vec<Msg>>, // dir -> unread
    log: Vec<Msg>,                      // full timeline (for the UI later)
    state: serde_json::Value,           // shared thread_state blob (object)
    members: HashSet<String>,           // dirs that have connected
    asks: Vec<Ask>,                     // questions awaiting a human answer
    /// Irreversible process-local tombstone installed once thread deletion
    /// starts. Late answer/post/restore work must not recreate a deleted bus.
    closed: bool,
    /// Reversible deletion admission fence. Existing state is retained until
    /// the DB cascade commits; a failure can reopen it without losing messages.
    closing: bool,
}

impl ThreadBus {
    fn unavailable(&self) -> bool {
        self.closed || self.closing
    }
}

/// Cloneable handle to all threads' buses.
#[derive(Default, Clone)]
pub struct BusRegistry {
    inner: Arc<Mutex<HashMap<i32, ThreadBus>>>,
    /// Serializes durable human-request lifecycle transitions that cross the
    /// SQLite/process-local boundary. Answer and delete must share this gate:
    /// otherwise an answer can commit in SQLite while a concurrent delete has
    /// fenced delivery, then the delete can fail and reopen a still-live bus
    /// whose ask disagrees with the durable row.
    lifecycle_gates: Arc<Mutex<HashMap<i32, Weak<tokio::sync::Mutex<()>>>>>,
    wake: Arc<Mutex<Option<Sender<Wake>>>>,
    next_ask_id: Arc<AtomicU64>,
    ask_notify: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<HumanAskEvent>>>>,
    /// transcript 结算痕迹消费者的通道（与 IM 桥 `ask_notify` 独立的第二订阅，
    /// 桌面端始终装上）。
    ask_trail: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<HumanAskEvent>>>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl BusRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Per-thread async gate for operations that must linearize a durable DB
    /// transition with its in-memory bus effect. Callers hold the returned
    /// mutex only for the affected thread; unrelated issues remain concurrent.
    pub fn thread_lifecycle_gate(&self, thread: i32) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self
            .lifecycle_gates
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(gate) = gates.get(&thread).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(thread, Arc::downgrade(&gate));
        gate
    }

    /// Install the channel the coordinator listens on (called once at startup).
    pub fn set_wake_sender(&self, tx: Sender<Wake>) {
        *self.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    fn emit_wake(&self, thread: i32, dir: &str) {
        if let Some(tx) = self.wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(Wake {
                thread,
                dir: dir.to_string(),
            });
        }
    }

    /// Install the channel the IM bridge listens on for human-ask events
    /// (called once at startup). Mirrors `set_wake_sender`.
    ///
    /// Mirrors `AskRegistry::set_notifier`: install the edge-event sender and
    /// atomically return every currently open ask for IM card replay.
    pub fn set_ask_notifier(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<HumanAskEvent>,
    ) -> Vec<(i32, Ask)> {
        // Same lock order as push_ask_with_id: inner -> ask_notify. Taking the
        // snapshot while installing the sender gives IM restart replay a clean
        // no-miss/no-duplicate boundary.
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *self.ask_notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        let mut snapshot: Vec<(i32, Ask)> = g
            .iter()
            .flat_map(|(thread, bus)| {
                if bus.unavailable() {
                    return Vec::new().into_iter();
                }
                bus.asks
                    .iter()
                    .filter(|ask| !ask.answered)
                    .cloned()
                    .map(|ask| (*thread, ask))
                    .collect::<Vec<_>>()
                    .into_iter()
            })
            .collect();
        snapshot.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.ts.cmp(&right.1.ts))
                .then_with(|| left.1.id.cmp(&right.1.id))
        });
        snapshot
    }

    /// Install the transcript-trail consumer's channel (called once at startup,
    /// independent of the IM bridge's `set_ask_notifier`).
    pub fn set_ask_trail_notifier(&self, tx: tokio::sync::mpsc::UnboundedSender<HumanAskEvent>) {
        *self.ask_trail.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// 须在持 `inner` 锁内调用，以保证通道顺序与状态迁移一致（事件是
    /// edge-triggered、带 per-ask 身份，Asked/Answered 不可乱序）。锁顺序
    /// 固定 inner → ask_notify；UnboundedSender::send 非阻塞，锁内发送安全。
    fn emit_ask_event(&self, ev: HumanAskEvent) {
        if let Some(tx) = self
            .ask_trail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(ev.clone());
        }
        if let Some(tx) = self
            .ask_notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(ev);
        }
    }

    /// Register `dir` as a member of `thread` (idempotent). Called on connect.
    pub fn join(&self, thread: i32, dir: &str) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return;
        }
        bus.members.insert(dir.to_string());
        if !bus.state.is_object() {
            bus.state = serde_json::json!({});
        }
    }

    /// Post a message from `from` to a specific `to` direction.
    pub fn post(&self, thread: i32, from: &str, to: &str, text: &str, kind: &str) {
        let _ = self.post_with_request_id(thread, from, to, text, kind, None, true);
    }

    fn post_with_request_id(
        &self,
        thread: i32,
        from: &str,
        to: &str,
        text: &str,
        kind: &str,
        request_id: Option<u64>,
        wake: bool,
    ) -> bool {
        let m = Msg {
            from: from.to_string(),
            to: to.to_string(),
            text: text.to_string(),
            ts: now(),
            kind: kind.to_string(),
            request_id,
        };
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            if bus.unavailable() {
                return false;
            }
            bus.log.push(m.clone());
            bus.inboxes.entry(to.to_string()).or_default().push(m);
        }
        if wake {
            self.emit_wake(thread, to);
        }
        true
    }

    fn post_durable_answer(&self, thread: i32, request_id: u64, to: &str, text: &str) -> bool {
        self.post_with_request_id(
            thread,
            HUMAN,
            to,
            text,
            "message",
            Some(request_id),
            true,
        )
    }

    /// Broadcast from `from` to every other member of the thread.
    pub fn broadcast(&self, thread: i32, from: &str, text: &str, kind: &str) {
        let m = Msg {
            from: from.to_string(),
            to: "*".to_string(),
            text: text.to_string(),
            ts: now(),
            kind: kind.to_string(),
            request_id: None,
        };
        let targets: Vec<String> = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            if bus.unavailable() {
                return;
            }
            let mut targets: Vec<String> = bus
                .members
                .iter()
                .filter(|d| d.as_str() != from)
                .cloned()
                .collect();
            // The lead joins the bus only when its process connects, so an idle
            // or not-yet-opened lead is not in `members` and would silently miss
            // thread-wide messages. Always route a broadcast to the lead's inbox
            // (and wake it) so it sees the message when next driven — issue-thread
            // broadcasts are the only callers, so this never targets a phantom.
            if from != LEAD && !targets.iter().any(|d| d == LEAD) {
                targets.push(LEAD.to_string());
            }
            bus.log.push(m.clone());
            for d in &targets {
                bus.inboxes.entry(d.clone()).or_default().push(m.clone());
            }
            targets
        };
        for d in targets {
            self.emit_wake(thread, &d);
        }
    }

    /// Read and clear `me`'s unread messages.
    pub fn inbox(&self, thread: i32, me: &str) -> Vec<Msg> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return Vec::new();
        }
        bus.inboxes.remove(me).unwrap_or_default()
    }

    /// Restore a just-taken inbox when the durable outbox query fails. Messages
    /// go back in front of anything that arrived while the database read was in
    /// flight, preserving FIFO order for the retry.
    pub fn restore_inbox(&self, thread: i32, me: &str, mut messages: Vec<Msg>) {
        if messages.is_empty() {
            return;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return;
        }
        let existing = bus.inboxes.remove(me).unwrap_or_default();
        messages.extend(existing);
        bus.inboxes.insert(me.to_string(), messages);
    }

    /// Emit queued wakes after startup restoration once the coordinator sender
    /// has been installed. The channel can buffer these before coordinator::run
    /// starts consuming it during Tauri setup.
    pub fn wake_pending_inboxes(&self) {
        let mut targets: Vec<(i32, String)> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.iter()
                .flat_map(|(thread, bus)| {
                    if bus.unavailable() {
                        return Vec::new().into_iter();
                    }
                    bus.inboxes
                        .iter()
                        .filter(|(_, messages)| !messages.is_empty())
                        .map(|(direction, _)| (*thread, direction.clone()))
                        .collect::<Vec<_>>()
                        .into_iter()
                })
                .collect()
        };
        targets.sort();
        for (thread, direction) in targets {
            self.emit_wake(thread, &direction);
        }
    }

    pub fn state_get(&self, thread: i32) -> serde_json::Value {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return serde_json::json!({});
        }
        if bus.state.is_object() {
            bus.state.clone()
        } else {
            serde_json::json!({})
        }
    }

    /// Shallow-merge `patch` (object) into the shared state.
    pub fn state_set(&self, thread: i32, patch: serde_json::Value) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return;
        }
        if !bus.state.is_object() {
            bus.state = serde_json::json!({});
        }
        if let (Some(dst), Some(src)) = (bus.state.as_object_mut(), patch.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
    }

    /// The full timeline for a thread (for the UI in v1b).
    pub fn log(&self, thread: i32) -> Vec<Msg> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return Vec::new();
        }
        bus.log.clone()
    }

    /// Record a question from direction `from` to the human; returns its id.
    /// Also lands in the timeline (kind = "ask") and wakes the human sentinel
    /// so the UI knows attention is needed without polling.
    pub fn ask_human(&self, thread: i32, from: &str, text: &str) -> u64 {
        self.push_ask(thread, from, text, AskKind::Question, false)
    }

    /// Register a durable question under its database id. Used by the MCP bus
    /// so desktop and IM observe the same stable identity across restarts.
    pub fn ask_human_with_id(&self, thread: i32, from: &str, text: &str, id: u64) -> u64 {
        self.next_ask_id.fetch_max(id, Ordering::Relaxed);
        self.push_ask_with_id(thread, from, text, AskKind::Question, id, true)
    }

    fn push_ask(
        &self,
        thread: i32,
        from: &str,
        text: &str,
        kind: AskKind,
        durable: bool,
    ) -> u64 {
        let id = self.next_ask_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.push_ask_with_id(thread, from, text, kind, id, durable)
    }

    fn push_ask_with_id(
        &self,
        thread: i32,
        from: &str,
        text: &str,
        kind: AskKind,
        id: u64,
        durable: bool,
    ) -> u64 {
        let ts = now();
        let ask = Ask {
            id,
            from: from.to_string(),
            text: text.to_string(),
            ts,
            answered: false,
            kind,
            durable,
        };
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            if bus.unavailable() {
                return id;
            }
            bus.asks.push(ask.clone());
            bus.log.push(Msg {
                from: from.to_string(),
                to: HUMAN.to_string(),
                text: text.to_string(),
                ts,
                kind: "ask".to_string(),
                request_id: None,
            });
            self.emit_ask_event(HumanAskEvent::Asked { thread, ask });
        }
        self.emit_wake(thread, HUMAN);
        id
    }

    /// Reconstruct an open durable request without replaying edge-triggered
    /// Asked events or adding a synthetic bus-log entry at process startup.
    pub fn restore_human_request(&self, thread: i32, from: &str, text: &str, id: u64) -> bool {
        self.next_ask_id.fetch_max(id, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return false;
        }
        if bus.asks.iter().any(|ask| ask.id == id) {
            return false;
        }
        bus.asks.push(Ask {
            id,
            from: from.to_string(),
            text: text.to_string(),
            ts: now(),
            answered: false,
            kind: AskKind::Question,
            durable: true,
        });
        true
    }

    /// Reconstruct one answered-but-unconsumed durable row into its asking
    /// direction's inbox. Idempotent by request id and intentionally silent;
    /// wake_pending_inboxes runs after the coordinator sender is installed.
    pub fn restore_durable_answer(
        &self,
        thread: i32,
        request_id: u64,
        to: &str,
        text: &str,
    ) -> bool {
        self.next_ask_id.fetch_max(request_id, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return false;
        }
        if bus
            .inboxes
            .values()
            .flatten()
            .any(|message| message.request_id == Some(request_id))
        {
            return false;
        }
        let message = Msg {
            from: HUMAN.to_string(),
            to: to.to_string(),
            text: text.to_string(),
            ts: now(),
            kind: "message".to_string(),
            request_id: Some(request_id),
        };
        bus.log.push(message.clone());
        bus.inboxes.entry(to.to_string()).or_default().push(message);
        true
    }

    /// The unanswered asks in a thread, oldest first.
    pub fn open_asks(&self, thread: i32) -> Vec<Ask> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return Vec::new();
        }
        bus.asks
            .iter()
            .filter(|a| !a.answered)
            .cloned()
            .collect()
    }

    /// Answer an open ask: mark it answered and deliver `text` to the asking
    /// direction's inbox (as if from the human). Returns false if not found —
    /// including a cancelled or superseded question.
    pub fn answer_ask(&self, thread: i32, ask_id: u64, text: &str) -> bool {
        let target = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            if bus.unavailable() {
                return false;
            }
            let hit = match bus
                .asks
                .iter_mut()
                .find(|a| a.id == ask_id && !a.answered)
            {
                Some(a) => {
                    a.answered = true;
                    Some((a.from.clone(), a.text.clone(), a.durable))
                }
                None => None,
            };
            if let Some((from, question, _)) = &hit {
                self.emit_ask_event(HumanAskEvent::Answered {
                    thread,
                    ask_id,
                    from: from.clone(),
                    question: question.clone(),
                    text: text.to_string(),
                });
            }
            hit
        };
        match target {
            Some((dir, _question, durable)) => {
                if durable {
                    let _ = self.post_durable_answer(thread, ask_id, &dir, text);
                } else {
                    self.post(thread, HUMAN, &dir, text, "message");
                }
                true
            }
            None => false,
        }
    }

    /// Deliver an OCC-resolved durable question when the app restarted after
    /// persistence but before the in-memory bus ask survived. Emit the same
    /// resolution event as `answer_ask` so transcript trail and IM consumers
    /// still observe the answer.
    pub fn deliver_durable_answer(
        &self,
        thread: i32,
        ask_id: u64,
        from: &str,
        question: &str,
        text: &str,
    ) -> bool {
        // Post first: the tombstone check and message insert share the inner
        // mutex, so delete cannot purge between an optimistic check and a late
        // `entry(thread).or_default()` recreation. Only a delivered answer
        // emits trail/IM resolution events.
        if !self.post_durable_answer(thread, ask_id, from, text) {
            return false;
        }
        self.emit_ask_event(HumanAskEvent::Answered {
            thread,
            ask_id,
            from: from.to_string(),
            question: question.to_string(),
            text: text.to_string(),
        });
        true
    }

    /// Remove answered messages whose originating turns were abandoned by a
    /// rewind. Open asks are cancelled separately so their IM cards receive a
    /// Cancelled event; this handles already-answered rows waiting in inboxes.
    pub fn discard_durable_answers(&self, thread: i32, request_ids: &[u64]) -> usize {
        self.discard_durable_answers_matching(thread, request_ids, |_| true)
    }

    /// Remove acknowledged answer messages only from the authenticated route
    /// that issued `bus_ack`. A sibling direction can guess a request id, but
    /// must not be able to drain the asking direction's process-local inbox.
    pub fn discard_durable_answers_for_scope(
        &self,
        thread: i32,
        to: &str,
        request_ids: &[u64],
    ) -> usize {
        self.discard_durable_answers_matching(thread, request_ids, |message| message.to == to)
    }

    fn discard_durable_answers_matching(
        &self,
        thread: i32,
        request_ids: &[u64],
        mut in_scope: impl FnMut(&Msg) -> bool,
    ) -> usize {
        if request_ids.is_empty() {
            return 0;
        }
        let ids: HashSet<u64> = request_ids.iter().copied().collect();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.unavailable() {
            return 0;
        }
        let before: usize = bus.inboxes.values().map(Vec::len).sum();
        for messages in bus.inboxes.values_mut() {
            messages.retain(|message| {
                !in_scope(message)
                    || message
                        .request_id
                        .is_none_or(|request_id| !ids.contains(&request_id))
            });
        }
        bus.log.retain(|message| {
            !in_scope(message)
                || message
                    .request_id
                    .is_none_or(|request_id| !ids.contains(&request_id))
        });
        let after: usize = bus.inboxes.values().map(Vec::len).sum();
        before.saturating_sub(after)
    }

    /// Install a reversible admission fence before deletion starts while
    /// retaining the complete bus for rollback. Returns whether a live bus
    /// existed plus ask ids whose cancellation event must wait for the durable
    /// DB transition. Late answer/post/restore work sees `closing` and no-ops.
    pub fn begin_thread_close(&self, thread: i32) -> (bool, Vec<u64>) {
        let mut g = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.closed {
            return (false, Vec::new());
        }
        let existed = !bus.closing
            && (!bus.inboxes.is_empty()
                || !bus.log.is_empty()
                || !bus.members.is_empty()
                || !bus.asks.is_empty()
                || bus.state.is_object());
        bus.closing = true;
        let cancelled = bus
            .asks
            .iter()
            .filter(|ask| !ask.answered)
            .map(|ask| ask.id)
            .collect();
        (existed, cancelled)
    }

    /// Apply the durable human-request cancellation to retained process-local
    /// state. Ordinary bus messages/state stay available if the later cascade
    /// fails and the close is rolled back; durable answers and asks do not,
    /// because their DB lifecycle is already cancelled.
    pub fn apply_thread_human_cancellation(&self, thread: i32) {
        let mut g = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let bus = g.entry(thread).or_default();
        if !bus.closing || bus.closed {
            return;
        }
        for ask in &mut bus.asks {
            ask.answered = true;
        }
        for messages in bus.inboxes.values_mut() {
            messages.retain(|message| message.request_id.is_none());
        }
        bus.log.retain(|message| message.request_id.is_none());
    }

    /// Apply a committed durable cancellation to one surviving thread without
    /// emitting provider/trail events. The caller batches and de-duplicates
    /// events after every DB effect is known. This also removes answered-but-
    /// unacknowledged inbox/log messages for the cancelled request ids.
    pub fn apply_human_cancellations_by_id(&self, thread: i32, request_ids: &[u64]) -> Vec<u64> {
        if request_ids.is_empty() {
            return Vec::new();
        }
        let ids = request_ids.iter().copied().collect::<HashSet<_>>();
        let mut g = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.closed {
            return Vec::new();
        }
        let mut cancelled = Vec::new();
        for ask in &mut bus.asks {
            if ask.answered || !ids.contains(&ask.id) {
                continue;
            }
            ask.answered = true;
            cancelled.push(ask.id);
        }
        for messages in bus.inboxes.values_mut() {
            messages.retain(|message| {
                message
                    .request_id
                    .is_none_or(|request_id| !ids.contains(&request_id))
            });
        }
        bus.log.retain(|message| {
            message
                .request_id
                .is_none_or(|request_id| !ids.contains(&request_id))
        });
        cancelled
    }

    /// Silently cancel live asks owned by a direction that a committed delete
    /// removed. Durable and legacy asks share this process-local shape; the
    /// command layer emits one de-duplicated event batch afterwards.
    pub fn apply_direction_human_cancellation(&self, thread: i32, from: &str) -> Vec<u64> {
        let mut g = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.closed {
            return Vec::new();
        }
        let mut cancelled = Vec::new();
        for ask in &mut bus.asks {
            if ask.answered || ask.from != from {
                continue;
            }
            ask.answered = true;
            cancelled.push(ask.id);
        }
        cancelled
    }

    /// Commit the irreversible tombstone only after the DB cascade succeeds.
    pub fn commit_thread_close(&self, thread: i32) {
        let mut g = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        g.insert(
            thread,
            ThreadBus {
                closed: true,
                ..ThreadBus::default()
            },
        );
    }

    /// Reopen a retained bus when deletion fails. Human request artifacts that
    /// already transitioned to cancelled stay removed; ordinary state survives.
    pub fn rollback_thread_close(&self, thread: i32) {
        let mut g = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let bus = g.entry(thread).or_default();
        if bus.closing && !bus.closed {
            bus.closing = false;
        }
    }

    /// Publish terminal IM events after the durable cancellation succeeds.
    /// Keeping this separate from purge preserves crash consistency: an app
    /// crash before the DB transition leaves an open provider card open too.
    pub fn notify_cancelled_asks(&self, thread: i32, ask_ids: &[u64]) {
        for ask_id in ask_ids {
            self.emit_ask_event(HumanAskEvent::Cancelled {
                thread,
                ask_id: *ask_id,
            });
        }
    }

    /// Apply and publish a committed cancellation set. Every durable id emits
    /// its terminal event, including requests that were already answered or
    /// acknowledged in memory: their provider cards still need to converge to
    /// the later durable Cancelled state.
    pub fn apply_committed_human_cancellations(&self, thread: i32, ask_ids: &[u64]) {
        self.apply_human_cancellations_by_id(thread, ask_ids);
        self.notify_cancelled_asks(thread, ask_ids);
    }

    fn cancel_open_asks_matching(
        &self,
        thread: i32,
        mut should_cancel: impl FnMut(&Ask) -> bool,
    ) -> usize {
        let cancelled = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            if bus.unavailable() {
                Vec::new()
            } else {
                let mut cancelled = Vec::new();
                for ask in &mut bus.asks {
                    if ask.answered || !should_cancel(ask) {
                        continue;
                    }
                    ask.answered = true;
                    cancelled.push(ask.id);
                }
                cancelled
            }
        };
        for ask_id in &cancelled {
            self.emit_ask_event(HumanAskEvent::Cancelled {
                thread,
                ask_id: *ask_id,
            });
        }
        cancelled.len()
    }

    /// Resolve every open human ask in a thread as cancelled. Used when deleting
    /// the owning workspace: no message is delivered back to the asking direction
    /// because its engine is being stopped and its thread rows are about to go
    /// away.
    pub fn cancel_open_asks(&self, thread: i32) -> usize {
        self.cancel_open_asks_matching(thread, |_| true)
    }

    /// Resolve open human asks from a specific direction in a thread. This covers
    /// historical repo-scoped sessions whose thread belongs to a different
    /// workspace than the repo being deleted.
    pub fn cancel_open_asks_from(&self, thread: i32, from: &str) -> usize {
        self.cancel_open_asks_matching(thread, |ask| ask.from.as_str() == from)
    }

    /// Withdraw one superseded question without touching other questions from
    /// the same direction.
    pub fn cancel_open_asks_by_id(&self, thread: i32, ask_id: u64) -> bool {
        self.cancel_open_asks_matching(thread, |ask| ask.id == ask_id) > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_and_inbox_clears() {
        let r = BusRegistry::new();
        r.join(1, "10");
        r.join(1, "20");
        r.post(1, "10", "20", "hi", "message");
        let got = r.inbox(1, "20");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from, "10");
        assert_eq!(got[0].text, "hi");
        // cleared after read
        assert_eq!(r.inbox(1, "20").len(), 0);
        // other dir unaffected
        assert_eq!(r.inbox(1, "10").len(), 0);
    }

    #[test]
    fn cancel_open_asks_by_id_removes_only_that_ask() {
        // Superseding one question must not touch another from the same scope.
        let r = BusRegistry::new();
        let superseded = r.ask_human(1, "10", "old question");
        let question = r.ask_human(1, "10", "real question"); // same dir — must survive
        assert!(r.cancel_open_asks_by_id(1, superseded));
        let open = r.open_asks(1);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, question);
        // Unknown / already-cancelled id → no-op false.
        assert!(!r.cancel_open_asks_by_id(1, superseded));
        assert!(!r.cancel_open_asks_by_id(1, 9999));
    }

    #[tokio::test]
    async fn committed_cancellation_discards_answer_and_notifies_even_after_answered() {
        let r = BusRegistry::new();
        assert!(r.restore_human_request(7, "10", "REST or GraphQL?", 42));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let snapshot = r.set_ask_notifier(tx);
        assert_eq!(snapshot.len(), 1);
        assert!(r.answer_ask(7, 42, "REST"));
        assert!(matches!(
            rx.recv().await.unwrap(),
            HumanAskEvent::Answered { ask_id: 42, .. }
        ));
        assert!(r
            .log(7)
            .iter()
            .any(|message| message.request_id == Some(42)));

        r.apply_committed_human_cancellations(7, &[42]);

        assert!(!r
            .log(7)
            .iter()
            .any(|message| message.request_id == Some(42)));
        assert!(r.inbox(7, "10").is_empty());
        assert!(matches!(
            rx.recv().await.unwrap(),
            HumanAskEvent::Cancelled {
                thread: 7,
                ask_id: 42
            }
        ));
    }

    #[test]
    fn broadcast_reaches_others_not_self() {
        let r = BusRegistry::new();
        for d in ["10", "20", "30"] {
            r.join(1, d);
        }
        r.broadcast(1, "10", "all hands", "message");
        assert_eq!(r.inbox(1, "10").len(), 0);
        assert_eq!(r.inbox(1, "20").len(), 1);
        assert_eq!(r.inbox(1, "30").len(), 1);
    }

    #[test]
    fn broadcast_reaches_lead_even_when_not_joined() {
        // The lead joins the bus only when its process connects; a worker's
        // broadcast must still land in the lead's inbox (and wake it) so an idle
        // lead never silently misses a thread-wide message.
        let r = BusRegistry::new();
        r.join(1, "10");
        r.broadcast(1, "10", "contract changed", "message");
        assert_eq!(r.inbox(1, LEAD).len(), 1, "idle lead receives the broadcast");
    }

    #[test]
    fn broadcast_from_lead_does_not_self_target() {
        let r = BusRegistry::new();
        r.join(1, "10");
        r.broadcast(1, LEAD, "heads up", "message");
        assert_eq!(r.inbox(1, LEAD).len(), 0, "lead never broadcasts to itself");
        assert_eq!(r.inbox(1, "10").len(), 1);
    }

    #[test]
    fn post_emits_wake() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = BusRegistry::new();
        r.set_wake_sender(tx);
        r.join(1, "10");
        r.post(1, "20", "10", "hi", "message");
        let w = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(w.thread, 1);
        assert_eq!(w.dir, "10");
    }

    #[test]
    fn state_merges() {
        let r = BusRegistry::new();
        r.state_set(1, serde_json::json!({"a": 1}));
        r.state_set(1, serde_json::json!({"b": 2}));
        assert_eq!(r.state_get(1), serde_json::json!({"a": 1, "b": 2}));
    }

    #[test]
    fn threads_isolated() {
        let r = BusRegistry::new();
        r.join(1, "10");
        r.join(2, "10");
        r.post(1, "x", "10", "t1", "message");
        assert_eq!(r.inbox(2, "10").len(), 0);
        assert_eq!(r.inbox(1, "10").len(), 1);
    }

    #[test]
    fn ask_human_is_listed_as_open() {
        let r = BusRegistry::new();
        let id = r.ask_human(1, "10", "Should I bump the major version?");
        let open = r.open_asks(1);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, id);
        assert_eq!(open[0].from, "10");
        assert_eq!(open[0].text, "Should I bump the major version?");
        assert!(!open[0].answered);
    }

    #[test]
    fn answering_clears_the_ask_and_replies_to_asker() {
        let r = BusRegistry::new();
        r.join(1, "10");
        let id = r.ask_human(1, "10", "major or minor?");
        let ok = r.answer_ask(1, id, "minor");
        assert!(ok);
        // no longer open
        assert_eq!(r.open_asks(1).len(), 0);
        // the asking direction receives the answer in its inbox
        let inbox = r.inbox(1, "10");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].from, "you");
        assert_eq!(inbox[0].text, "minor");
    }

    #[test]
    fn answering_unknown_ask_is_a_noop() {
        let r = BusRegistry::new();
        assert!(!r.answer_ask(1, 999, "hi"));
    }

    #[test]
    fn asks_are_isolated_per_thread() {
        let r = BusRegistry::new();
        r.ask_human(1, "10", "q1");
        r.ask_human(2, "20", "q2");
        assert_eq!(r.open_asks(1).len(), 1);
        assert_eq!(r.open_asks(2).len(), 1);
        assert_eq!(r.open_asks(1)[0].text, "q1");
    }

    #[test]
    fn ask_human_notifies_the_human_via_wake() {
        // The human's "direction" sentinel is "you"; a wake on it lets the
        // UI/coordinator know an ask is waiting without polling.
        let (tx, rx) = std::sync::mpsc::channel();
        let r = BusRegistry::new();
        r.set_wake_sender(tx);
        r.ask_human(7, "10", "ping?");
        let w = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(w.thread, 7);
        assert_eq!(w.dir, "you");
    }

    #[tokio::test]
    async fn human_ask_notifier_fires_on_ask_and_answer() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(r.set_ask_notifier(tx).is_empty());
        r.join(1, "10");
        let id = r.ask_human(1, "10", "major or minor?");
        match rx.recv().await.unwrap() {
            HumanAskEvent::Asked { thread, ask } => {
                assert_eq!(thread, 1);
                assert_eq!(ask.id, id);
                assert_eq!(ask.text, "major or minor?");
            }
            e => panic!("unexpected: {e:?}"),
        }
        assert!(r.answer_ask(1, id, "minor"));
        assert!(matches!(rx.recv().await.unwrap(),
            HumanAskEvent::Answered { thread: 1, ask_id, from, question, text }
                if ask_id == id && from == "10" && question == "major or minor?" && text == "minor"));
        // 未命中/重复作答不发事件
        assert!(!r.answer_ask(1, id, "again"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn notifier_snapshot_replays_restored_durable_ask_as_answerable() {
        let r = BusRegistry::new();
        assert!(r.restore_human_request(7, "10", "REST or GraphQL?", 42));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let snapshot = r.set_ask_notifier(tx);

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, 7);
        assert_eq!(snapshot[0].1.id, 42);
        assert_eq!(snapshot[0].1.from, "10");
        assert!(snapshot[0].1.durable);
        assert!(rx.try_recv().is_err(), "snapshot rows are not duplicated as events");
        assert!(r.answer_ask(7, 42, "REST"));
        assert!(matches!(rx.recv().await.unwrap(),
            HumanAskEvent::Answered { thread: 7, ask_id: 42, .. }));
        let inbox = r.inbox(7, "10");
        assert_eq!(inbox[0].request_id, Some(42));
    }

    #[tokio::test]
    async fn durable_answer_without_live_ask_still_emits_trail_event_and_delivers() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_ask_trail_notifier(tx);
        r.join(7, "10");

        r.deliver_durable_answer(7, 42, "10", "REST or GraphQL?", "REST");

        assert!(matches!(rx.recv().await.unwrap(),
            HumanAskEvent::Answered { thread: 7, ask_id: 42, from, question, text }
                if from == "10" && question == "REST or GraphQL?" && text == "REST"));
        let inbox = r.inbox(7, "10");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].from, HUMAN);
        assert_eq!(inbox[0].text, "REST");
    }

    #[test]
    fn durable_answer_cleanup_is_scoped_to_the_acknowledging_route() {
        let r = BusRegistry::new();
        assert!(r.restore_durable_answer(7, 42, "10", "REST"));

        assert_eq!(r.discard_durable_answers_for_scope(7, "20", &[42]), 0);
        let inbox = r.inbox(7, "10");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].request_id, Some(42));
    }

    #[test]
    fn purging_a_deleted_thread_removes_answered_outbox_only_for_that_thread() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(r.set_ask_notifier(tx).is_empty());
        assert!(r.restore_durable_answer(7, 42, "10", "REST"));
        assert!(r.restore_human_request(7, "10", "still open", 44));
        assert!(r.restore_durable_answer(8, 43, "20", "GraphQL"));

        let (existed, cancelled) = r.begin_thread_close(7);
        assert!(existed);
        assert_eq!(cancelled, vec![44]);
        assert!(r.inbox(7, "10").is_empty(), "closing blocks reads before commit");
        assert!(rx.try_recv().is_err(), "purge alone does not outrun DB cancellation");
        r.apply_thread_human_cancellation(7);
        r.notify_cancelled_asks(7, &cancelled);
        assert!(matches!(
            rx.try_recv().unwrap(),
            HumanAskEvent::Cancelled {
                thread: 7,
                ask_id: 44
            }
        ));
        r.commit_thread_close(7);
        assert!(r.inbox(7, "10").is_empty());
        assert!(r.log(7).is_empty());
        assert_eq!(r.inbox(8, "20")[0].request_id, Some(43));
        assert!(!r.begin_thread_close(999).0);
        r.rollback_thread_close(999);
        r.post(999, "10", "lead", "retry after failed deletion", "message");
        assert_eq!(r.inbox(999, "lead").len(), 1);

        // Late work from an answer command that won DB OCC immediately before
        // deletion must not recreate the purged bus.
        assert!(!r.deliver_durable_answer(7, 42, "10", "REST or GraphQL?", "REST"));
        assert!(!r.restore_durable_answer(7, 42, "10", "REST"));
        assert!(!r.restore_human_request(7, "10", "late question", 44));
        r.post(7, "10", "lead", "late", "message");
        r.join(7, "10");
        assert!(r.inbox(7, "10").is_empty());
        assert!(r.log(7).is_empty());
        assert!(r.open_asks(7).is_empty());
    }

    #[tokio::test]
    async fn cancel_open_asks_marks_thread_asks_and_notifies() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(r.set_ask_notifier(tx).is_empty());
        let first = r.ask_human(1, "10", "first?");
        let second = r.ask_human(1, "20", "second?");
        let keep = r.ask_human(2, "30", "keep?");
        for _ in 0..3 {
            assert!(matches!(
                rx.recv().await.unwrap(),
                HumanAskEvent::Asked { .. }
            ));
        }

        assert_eq!(r.cancel_open_asks(1), 2);

        assert!(r.open_asks(1).is_empty());
        assert_eq!(r.open_asks(2)[0].id, keep);
        let mut cancelled = vec![];
        for _ in 0..2 {
            match rx.recv().await.unwrap() {
                HumanAskEvent::Cancelled { thread: 1, ask_id } => cancelled.push(ask_id),
                ev => panic!("unexpected: {ev:?}"),
            }
        }
        cancelled.sort_unstable();
        assert_eq!(cancelled, vec![first, second]);
        assert_eq!(r.cancel_open_asks(1), 0);
    }

    #[tokio::test]
    async fn cancel_open_asks_from_marks_only_that_direction() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(r.set_ask_notifier(tx).is_empty());
        let first = r.ask_human(1, "10", "first?");
        let keep_same_thread = r.ask_human(1, "20", "second?");
        let keep_other_thread = r.ask_human(2, "10", "keep?");
        for _ in 0..3 {
            assert!(matches!(
                rx.recv().await.unwrap(),
                HumanAskEvent::Asked { .. }
            ));
        }

        assert_eq!(r.cancel_open_asks_from(1, "10"), 1);

        assert_eq!(r.open_asks(1)[0].id, keep_same_thread);
        assert_eq!(r.open_asks(2)[0].id, keep_other_thread);
        assert!(matches!(rx.recv().await.unwrap(),
            HumanAskEvent::Cancelled { thread: 1, ask_id } if ask_id == first));
        assert_eq!(r.cancel_open_asks_from(1, "10"), 0);
    }
}
