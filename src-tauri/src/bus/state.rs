//! In-memory thread-bus state: per-thread inboxes (keyed by direction), a shared
//! JSON state blob, the message timeline, and the set of known member directions.
//! Identity is always supplied by the caller (the HTTP handler derives it from
//! the URL path), never trusted from agent input.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
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
}

/// What an `Ask` represents and how the Needs-you card should render it — ONE
/// discriminant instead of the `answerable: bool` this replaces, now that a
/// display-only NOTICE (`answerable == false`, in the old shape) itself splits
/// in two: most notices (the stall hint, the stopped-worker hint, an ordinary
/// PR/MR readiness or probe-error update) are retracted automatically by a
/// background process once the condition they describe changes, but the ONE
/// PR/MR "gave up tracking" notice (`host::judge::give_up_text`) is NOT — the
/// row backing it drops out of the monitor's sweep entirely (see
/// `host::monitor`'s `MAX_CONSECUTIVE_PROBE_FAILURES` doc), so nothing will
/// ever re-check and clear it without an explicit external re-trigger
/// (`register_pr`). Rendering the same generic "clears itself automatically"
/// footer under THAT notice directly contradicts its own body text — exactly
/// the bug this discriminant exists to let the frontend avoid (see
/// `NeedsRows.tsx`'s `AskRow`). Three states driving the same rendering
/// decision belong in one enum, not a second bool bolted onto `answerable`
/// (CLAUDE.md's discriminated-state rule).
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// A real question awaiting a human answer — the only kind `answer_ask`
    /// accepts and the only kind the UI renders an answer box for.
    Question,
    /// A display-only NOTICE a background process retracts on its own once
    /// the condition it describes changes (the stall hint, the stopped-worker
    /// hint, an ordinary PR/MR update). Renders the "clears itself
    /// automatically" footer.
    Notice,
    /// A display-only NOTICE that will NOT be retracted by any background
    /// process — only an explicit external action (named in the notice's own
    /// text) makes tracking resume, at which point a fresh notice (or none)
    /// eventually replaces this one. Must NOT render the "clears itself
    /// automatically" footer, which would contradict this notice's own text.
    NoticeActionRequired,
}

impl AskKind {
    /// Whether `answer_ask` may accept a reply for this kind — true only for
    /// [`AskKind::Question`]; both notice kinds are display-only.
    pub fn is_answerable(self) -> bool {
        matches!(self, AskKind::Question)
    }
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
    /// See [`AskKind`]. Determines both `answer_ask`'s eligibility check and
    /// the Needs-you card's rendering.
    pub kind: AskKind,
}

#[derive(Default)]
struct ThreadBus {
    inboxes: HashMap<String, Vec<Msg>>, // dir -> unread
    log: Vec<Msg>,                      // full timeline (for the UI later)
    state: serde_json::Value,           // shared thread_state blob (object)
    members: HashSet<String>,           // dirs that have connected
    asks: Vec<Ask>,                     // questions awaiting a human answer
}

/// Cloneable handle to all threads' buses.
#[derive(Default, Clone)]
pub struct BusRegistry {
    inner: Arc<Mutex<HashMap<i32, ThreadBus>>>,
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

    /// Install the channel the IM bridge listens on for human-ask events and
    /// atomically snapshot every open ask across threads. The lock order stays
    /// `inner -> ask_notify`, matching `push_ask`, so an ask is represented
    /// exactly once: either in this snapshot or as a later channel event.
    pub fn set_ask_notifier(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<HumanAskEvent>,
    ) -> Vec<(i32, Ask)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *self.ask_notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        Self::open_ask_snapshot_from(&inner)
    }

    fn open_ask_snapshot_from(inner: &HashMap<i32, ThreadBus>) -> Vec<(i32, Ask)> {
        let mut snapshot = inner
            .iter()
            .flat_map(|(thread, bus)| {
                bus.asks
                    .iter()
                    .filter(|ask| !ask.answered)
                    .cloned()
                    .map(|ask| (*thread, ask))
            })
            .collect::<Vec<_>>();
        snapshot.sort_by_key(|(thread, ask)| (ask.ts, *thread, ask.id));
        snapshot
    }

    /// Snapshot every currently open human ask without replacing the notifier.
    /// Used when an IM bridge gains its first owner after startup snapshots were
    /// intentionally skipped while no delivery target existed.
    pub fn open_ask_snapshot(&self) -> Vec<(i32, Ask)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Self::open_ask_snapshot_from(&inner)
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
        bus.members.insert(dir.to_string());
        if !bus.state.is_object() {
            bus.state = serde_json::json!({});
        }
    }

    /// Post a message from `from` to a specific `to` direction.
    pub fn post(&self, thread: i32, from: &str, to: &str, text: &str, kind: &str) {
        let m = Msg {
            from: from.to_string(),
            to: to.to_string(),
            text: text.to_string(),
            ts: now(),
            kind: kind.to_string(),
        };
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            bus.log.push(m.clone());
            bus.inboxes.entry(to.to_string()).or_default().push(m);
        }
        self.emit_wake(thread, to);
    }

    /// Broadcast from `from` to every other member of the thread.
    pub fn broadcast(&self, thread: i32, from: &str, text: &str, kind: &str) {
        let m = Msg {
            from: from.to_string(),
            to: "*".to_string(),
            text: text.to_string(),
            ts: now(),
            kind: kind.to_string(),
        };
        let targets: Vec<String> = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
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
        bus.inboxes.remove(me).unwrap_or_default()
    }

    pub fn state_get(&self, thread: i32) -> serde_json::Value {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let bus = g.entry(thread).or_default();
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
        g.entry(thread).or_default().log.clone()
    }

    /// Record a question from direction `from` to the human; returns its id.
    /// Also lands in the timeline (kind = "ask") and wakes the human sentinel
    /// so the UI knows attention is needed without polling.
    pub fn ask_human(&self, thread: i32, from: &str, text: &str) -> u64 {
        self.push_ask(thread, from, text, AskKind::Question)
    }

    /// Post a display-only, SELF-CLEARING NOTICE to the human — same
    /// Needs-you/IM surfacing and wake as `ask_human`, but `answer_ask`
    /// refuses it (a reply can't inject a stray bus message) and the UI shows
    /// no answer box. For hints (the stall/stopped-worker notices, an
    /// ordinary PR/MR update) that a background process retracts once the
    /// condition it describes changes — there is no answer to give, and
    /// nothing further for the human to do to make it go away. See
    /// `notify_human_action_required` for the one notice kind this does NOT
    /// cover.
    pub fn notify_human(&self, thread: i32, from: &str, text: &str) -> u64 {
        self.push_ask(thread, from, text, AskKind::Notice)
    }

    /// Post a display-only NOTICE like `notify_human`, but one that will NOT
    /// be retracted by any background process — only an explicit external
    /// re-trigger (named in `text` itself) makes tracking resume. Currently
    /// only `host::monitor`'s PR/MR give-up notice (`host::judge::
    /// give_up_text`) uses this; every other notice in the codebase keeps
    /// calling `notify_human` and is unaffected.
    pub fn notify_human_action_required(&self, thread: i32, from: &str, text: &str) -> u64 {
        self.push_ask(thread, from, text, AskKind::NoticeActionRequired)
    }

    fn push_ask(&self, thread: i32, from: &str, text: &str, kind: AskKind) -> u64 {
        let id = self.next_ask_id.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = now();
        let ask = Ask {
            id,
            from: from.to_string(),
            text: text.to_string(),
            ts,
            answered: false,
            kind,
        };
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            bus.asks.push(ask.clone());
            bus.log.push(Msg {
                from: from.to_string(),
                to: HUMAN.to_string(),
                text: text.to_string(),
                ts,
                kind: "ask".to_string(),
            });
            self.emit_ask_event(HumanAskEvent::Asked { thread, ask });
        }
        self.emit_wake(thread, HUMAN);
        id
    }

    /// The unanswered asks in a thread, oldest first.
    pub fn open_asks(&self, thread: i32) -> Vec<Ask> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(thread)
            .or_default()
            .asks
            .iter()
            .filter(|a| !a.answered)
            .cloned()
            .collect()
    }

    /// Count of open asks the human must actually act on — excludes EITHER
    /// display-only NOTICE kind (`AskKind::Notice` / `AskKind::
    /// NoticeActionRequired`, e.g. the self-clearing stall hint), which
    /// surfaces in Needs-you but has no answer to give. Single source so
    /// every "needs you" count (the workspace switcher, badges) agrees on
    /// what's pending: a notice must not inflate a number that promises "N
    /// things need your action" (issue #105).
    pub fn open_answerable_ask_count(&self, thread: i32) -> usize {
        self.open_asks(thread).iter().filter(|a| a.kind.is_answerable()).count()
    }

    /// Answer an open ask: mark it answered and deliver `text` to the asking
    /// direction's inbox (as if from the human). Returns false if not found —
    /// including a display-only NOTICE (either `AskKind`), which has no
    /// answer to give, so a stray reply never reaches the asker's inbox.
    pub fn answer_ask(&self, thread: i32, ask_id: u64, text: &str) -> bool {
        let target = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            let hit = match bus
                .asks
                .iter_mut()
                .find(|a| a.id == ask_id && !a.answered && a.kind.is_answerable())
            {
                Some(a) => {
                    a.answered = true;
                    Some((a.from.clone(), a.text.clone()))
                }
                None => None,
            };
            if let Some((from, question)) = &hit {
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
            Some((dir, _question)) => {
                self.post(thread, HUMAN, &dir, text, "message");
                true
            }
            None => false,
        }
    }

    fn cancel_open_asks_matching(
        &self,
        thread: i32,
        mut should_cancel: impl FnMut(&Ask) -> bool,
    ) -> usize {
        let cancelled = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            let mut cancelled = Vec::new();
            for ask in &mut bus.asks {
                if ask.answered || !should_cancel(ask) {
                    continue;
                }
                ask.answered = true;
                cancelled.push(ask.id);
            }
            cancelled
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

    /// Withdraw a single open human ask by id — for self-clearing notices (e.g.
    /// the task-stall hint, retracted on recovery/turn-end) that must NOT touch a
    /// worker's other open asks from the same direction. No message is delivered
    /// back. Returns whether an open ask with that id was found and cancelled.
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
        // Precision is the whole point: a stall notice must be retractable WITHOUT
        // touching the worker's other real open asks from the same direction.
        let r = BusRegistry::new();
        let stall = r.ask_human(1, "10", "stall hint");
        let question = r.ask_human(1, "10", "real question"); // same dir — must survive
        assert!(r.cancel_open_asks_by_id(1, stall));
        let open = r.open_asks(1);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, question);
        // Unknown / already-cancelled id → no-op false.
        assert!(!r.cancel_open_asks_by_id(1, stall));
        assert!(!r.cancel_open_asks_by_id(1, 9999));
    }

    #[test]
    fn open_answerable_ask_count_excludes_notices() {
        // A self-clearing NOTICE (`notify_human`, e.g. the stall hint) still
        // surfaces via `open_asks` — the queue must render it — but must NOT
        // inflate the "needs you" count, which promises real pending items
        // (issue #105: badge showed 4, only 1 was actually awaiting an answer).
        let r = BusRegistry::new();
        r.notify_human(1, "10", "stall hint");
        assert_eq!(r.open_asks(1).len(), 1, "the notice still surfaces in the queue");
        assert_eq!(r.open_answerable_ask_count(1), 0, "but doesn't count as pending");

        let q = r.ask_human(1, "10", "real question");
        assert_eq!(r.open_asks(1).len(), 2);
        assert_eq!(r.open_answerable_ask_count(1), 1, "a real question does count");

        assert!(r.answer_ask(1, q, "ok"));
        assert_eq!(r.open_answerable_ask_count(1), 0, "answered questions drop out");
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
    fn notify_human_is_a_non_answerable_notice() {
        // A notice surfaces in Needs-you like an ask, but answering it must be a
        // no-op: no reply reaches the asker's inbox (a stale stall notice answered
        // in the sweep gap can't inject a stray bus message), and it stays open
        // until it's explicitly retracted.
        let r = BusRegistry::new();
        r.join(1, "10");
        let id = r.notify_human(1, "10", "⏳ task stalled");
        let open = r.open_asks(1);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, AskKind::Notice, "an ordinary notify_human posts a self-clearing Notice");
        assert!(!open[0].kind.is_answerable());
        // Answering is refused — no delivery, and the notice remains open.
        assert!(!r.answer_ask(1, id, "hurry up"));
        assert!(r.inbox(1, "10").is_empty());
        assert_eq!(r.open_asks(1).len(), 1);
        // An explicit retract (what the watchdog does on recover/idle) clears it.
        assert!(r.cancel_open_asks_by_id(1, id));
        assert!(r.open_asks(1).is_empty());
    }

    #[test]
    fn notify_human_action_required_is_a_distinct_non_answerable_kind() {
        // The give-up PR/MR notice: still a non-answerable NOTICE (surfaces,
        // refuses a reply, counts against neither open_answerable_ask_count),
        // but tagged with the OTHER notice kind so the frontend can tell it
        // apart from a self-clearing one and skip the contradictory footer.
        let r = BusRegistry::new();
        let id = r.notify_human_action_required(1, "10", "🛑 gave up tracking");
        let open = r.open_asks(1);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, AskKind::NoticeActionRequired);
        assert!(!open[0].kind.is_answerable());
        assert_eq!(r.open_answerable_ask_count(1), 0, "an action-required notice still isn't a pending question");
        assert!(!r.answer_ask(1, id, "ignored"), "answering is refused, same as an ordinary notice");
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
    async fn human_ask_notifier_snapshots_preexisting_open_asks() {
        let r = BusRegistry::new();
        let first = r.ask_human(2, "20", "already open");
        let answered = r.ask_human(1, "10", "already answered");
        assert!(r.answer_ask(1, answered, "done"));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let snapshot = r.set_ask_notifier(tx);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, 2);
        assert_eq!(snapshot[0].1.id, first);

        let later = r.ask_human(3, "30", "opened after install");
        assert!(matches!(
            rx.recv().await,
            Some(HumanAskEvent::Asked { thread: 3, ask }) if ask.id == later
        ));
        assert!(
            rx.try_recv().is_err(),
            "snapshot asks must not be emitted again"
        );
    }

    #[tokio::test]
    async fn cancel_open_asks_marks_thread_asks_and_notifies() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = r.set_ask_notifier(tx);
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
        let _ = r.set_ask_notifier(tx);
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
