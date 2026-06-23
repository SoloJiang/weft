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

/// A question an agent direction has put to the human, awaiting an answer.
/// This is the clean, non-TUI signal behind the "Needs-you" surface.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Ask {
    pub id: u64,
    pub from: String, // asking direction id (as string)
    pub text: String,
    pub ts: u64,
    pub answered: bool,
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

    /// Install the channel the IM bridge listens on for human-ask events
    /// (called once at startup). Mirrors `set_wake_sender`.
    ///
    /// 与 `AskRegistry::set_notifier` 不同，本方法不返回 open asks 快照
    /// （M1 范围）；只投递安装之后的事件。须在任何 agent 跑起来之前安装——
    /// 安装前已 open 的提问不会补发，registry 也没有跨 thread 枚举接口。
    pub fn set_ask_notifier(&self, tx: tokio::sync::mpsc::UnboundedSender<HumanAskEvent>) {
        *self.ask_notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
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
        let id = self.next_ask_id.fetch_add(1, Ordering::Relaxed) + 1;
        let ts = now();
        let ask = Ask {
            id,
            from: from.to_string(),
            text: text.to_string(),
            ts,
            answered: false,
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

    /// Answer an open ask: mark it answered and deliver `text` to the asking
    /// direction's inbox (as if from the human). Returns false if not found.
    pub fn answer_ask(&self, thread: i32, ask_id: u64, text: &str) -> bool {
        let target = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            let hit = match bus.asks.iter_mut().find(|a| a.id == ask_id && !a.answered) {
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
        r.set_ask_notifier(tx);
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
    async fn cancel_open_asks_marks_thread_asks_and_notifies() {
        let r = BusRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_ask_notifier(tx);
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
        r.set_ask_notifier(tx);
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
