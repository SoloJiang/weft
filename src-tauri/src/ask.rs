//! The Ask Bridge (ARCHITECTURE §4.3): permission Asks from every tool funnel to
//! one weft endpoint, become Needs-you cards, the human answers, and the
//! decision flows back to the blocked tool. Each tool intercepts at its own
//! structured point (Claude PreToolUse hook, Codex approval-request, OpenCode
//! /event), but they all resolve through THIS registry — never by scraping the
//! terminal. A spawned task that hits an approval no longer hangs silently in a
//! PTY; it surfaces as a card you can answer from the board.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Registry → IM 桥的通知：第二呈现面（IM 卡片）靠它与桌面保持同步。
/// Opened 在 request() 时发；Resolved 在 answer()（含 Always/Full 连带覆盖、
/// Dangerous 释放积压）时按被解决的每个 ask 发；Cancelled 在 cancel()（超时
/// 回落）时发。没装通知器时零开销。
#[derive(Clone, Debug)]
pub enum AskEvent {
    /// 携带的 Ask 中 `thread_title`/`dir_name` 为空；富化（查 DB 填充）是
    /// 消费侧（桥/命令层）的责任。
    Opened(Ask),
    /// `answer` 是该 ask 的真实判决（Dangerous 释放积压记为 Allow；
    /// Always/Full 连带覆盖的 ask 记为人答的那个 Answer）。
    Resolved { id: u64, answer: Answer },
    Cancelled { id: u64 },
}

/// The human's answer to a permission Ask. `Always` remembers this action for
/// the asking task; `Full` auto-approves everything from that task. Both are
/// weft-side passthrough rules, scoped per (thread, task), kept in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Deny,
    Always,
    Full,
}

impl Answer {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Answer::Allow),
            "deny" => Some(Answer::Deny),
            "always" => Some(Answer::Always),
            "full" => Some(Answer::Full),
            _ => None,
        }
    }
}

/// A pending permission request, awaiting the human's decision.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Ask {
    pub id: u64,
    pub thread: i32,
    /// asking direction id (as string); "" for a lead/planning session.
    pub dir: String,
    pub tool: String,
    /// short human label, e.g. "Run: npm test" or "Edit src/main.rs".
    pub summary: String,
    /// the raw action detail (command / file path / full input).
    pub detail: String,
    pub ts: u64,
    /// Human context, filled when listed (pending_asks): the owning thread's
    /// title and the asking task's name. Empty for a lead/planning session.
    #[serde(default)]
    pub thread_title: String,
    #[serde(default)]
    pub dir_name: String,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    waiters: HashMap<u64, oneshot::Sender<Decision>>,
    open: Vec<Ask>,
    /// (thread, dir) -> summaries the human has "always allow"-ed.
    always: HashMap<(i32, String), HashSet<String>>,
    /// (thread, dir) granted full access — every ask auto-allows.
    full: HashSet<(i32, String)>,
    /// Dangerous mode: when on, EVERY ask from EVERY agent auto-allows (never
    /// surfaced). The global "skip all permission prompts" setting.
    dangerous: bool,
    /// IM 桥的通知器：装上后 Ask 开/答/撤事件外发；未装时零开销。
    notify: Option<tokio::sync::mpsc::UnboundedSender<AskEvent>>,
}

impl Inner {
    /// 事件外发（持锁内调用）：没装通知器时零开销；桥不在线不报错。
    fn emit(&self, ev: AskEvent) {
        if let Some(tx) = &self.notify {
            let _ = tx.send(ev);
        }
    }
}

/// Cloneable handle to all pending Asks.
#[derive(Default, Clone)]
pub struct AskRegistry {
    inner: Arc<Mutex<Inner>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 安装 IM 桥的通知器（重装时替换旧的；旧消费者随 sender drop 收尾）。
    /// 返回挂接瞬间已 open 的 Ask 快照；快照与后续事件流无重叠、无遗漏
    /// （同锁内完成）——供桥重启/重连时补发已有卡片，消除 miss/duplicate 竞态。
    pub fn set_notifier(&self, tx: tokio::sync::mpsc::UnboundedSender<AskEvent>) -> Vec<Ask> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.notify = Some(tx);
        g.open.clone()
    }

    /// Register a permission request; returns its id and a receiver that resolves
    /// when the human (or a timeout) answers. The caller awaits the receiver.
    pub fn request(
        &self,
        thread: i32,
        dir: &str,
        tool: &str,
        summary: &str,
        detail: &str,
    ) -> (u64, oneshot::Receiver<Decision>) {
        let (tx, rx) = oneshot::channel();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.next_id += 1;
        let id = g.next_id;
        g.waiters.insert(id, tx);
        let ask = Ask {
            id,
            thread,
            dir: dir.to_string(),
            tool: tool.to_string(),
            summary: summary.to_string(),
            detail: detail.to_string(),
            ts: now(),
            thread_title: String::new(),
            dir_name: String::new(),
        };
        g.open.push(ask.clone());
        g.emit(AskEvent::Opened(ask));
        (id, rx)
    }

    /// Toggle Dangerous mode (global): every incoming ask auto-allows. Turning it
    /// ON also releases the whole existing backlog — every already-open ask
    /// resolves to Allow, so agents currently blocked on a prompt unblock at once.
    pub fn set_dangerous(&self, on: bool) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.dangerous = on;
        if !on {
            return;
        }
        let ids: Vec<u64> = g.open.iter().map(|a| a.id).collect();
        g.open.clear();
        for id in ids {
            if let Some(tx) = g.waiters.remove(&id) {
                let _ = tx.send(Decision::Allow);
            }
            g.emit(AskEvent::Resolved { id, answer: Answer::Allow });
        }
    }

    /// A standing rule's verdict for an incoming ask, checked BEFORE surfacing:
    /// full access or a matching always-allow → auto-allow (never shown).
    pub fn auto_decision(&self, thread: i32, dir: &str, summary: &str) -> Option<Decision> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.dangerous {
            return Some(Decision::Allow);
        }
        let k = (thread, dir.to_string());
        if g.full.contains(&k) {
            return Some(Decision::Allow);
        }
        if g.always.get(&k).is_some_and(|s| s.contains(summary)) {
            return Some(Decision::Allow);
        }
        None
    }

    /// Answer a pending Ask. `Always` records this action for the task and
    /// `Full` grants the task full access — then both clear any other open asks
    /// they now cover. Returns false if the ask was already resolved.
    pub fn answer(&self, id: u64, ans: Answer) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ask) = g.open.iter().find(|a| a.id == id).cloned() else {
            return false;
        };
        let key = (ask.thread, ask.dir.clone());
        match ans {
            Answer::Always => {
                g.always.entry(key.clone()).or_default().insert(ask.summary.clone());
            }
            Answer::Full => {
                g.full.insert(key.clone());
            }
            _ => {}
        }

        // Every open ask this answer now covers (the target + any others the new
        // rule sweeps up) resolves to the same verdict.
        let decision = if ans == Answer::Deny { Decision::Deny } else { Decision::Allow };
        let covered: Vec<u64> = g
            .open
            .iter()
            .filter(|a| {
                if a.id == id {
                    return true;
                }
                if (a.thread, a.dir.clone()) != key {
                    return false;
                }
                match ans {
                    Answer::Full => true,
                    Answer::Always => a.summary == ask.summary,
                    _ => false,
                }
            })
            .map(|a| a.id)
            .collect();

        g.open.retain(|a| !covered.contains(&a.id));
        let mut woke = false;
        for cid in covered {
            if let Some(tx) = g.waiters.remove(&cid) {
                woke = tx.send(decision).is_ok() || woke;
            }
            g.emit(AskEvent::Resolved { id: cid, answer: ans });
        }
        woke
    }

    /// Drop a pending Ask without answering (e.g. on timeout) so it leaves the
    /// board. The waiter's receiver errors, which the endpoint treats as fallback.
    pub fn cancel(&self, id: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.open.len();
        g.open.retain(|a| a.id != id);
        let hit = g.open.len() != before;
        g.waiters.remove(&id);
        if hit {
            g.emit(AskEvent::Cancelled { id });
        }
    }

    /// All Asks across threads (for the workspace-wide Needs-you surface).
    pub fn open(&self) -> Vec<Ask> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).open.clone()
    }

    /// Open Asks for one thread.
    pub fn open_in(&self, thread: i32) -> Vec<Ask> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open
            .iter()
            .filter(|a| a.thread == thread)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_then_answer_delivers_decision() {
        let r = AskRegistry::new();
        let (id, rx) = r.request(1, "10", "claude", "Run: npm test", "npm test");
        assert_eq!(r.open().len(), 1);
        assert!(r.answer(id, Answer::Allow));
        assert_eq!(rx.await.unwrap(), Decision::Allow);
        assert!(r.open().is_empty());
        // double-answer is a no-op
        assert!(!r.answer(id, Answer::Deny));
    }

    #[tokio::test]
    async fn always_allow_remembers_and_auto_decides() {
        let r = AskRegistry::new();
        let (id, _rx) = r.request(1, "10", "claude", "Run: npm test", "npm test");
        // no rule yet
        assert!(r.auto_decision(1, "10", "Run: npm test").is_none());
        assert!(r.answer(id, Answer::Always));
        // same action in the same task now auto-allows
        assert_eq!(r.auto_decision(1, "10", "Run: npm test"), Some(Decision::Allow));
        // a different action still asks
        assert!(r.auto_decision(1, "10", "Run: rm -rf /").is_none());
        // another task is unaffected
        assert!(r.auto_decision(2, "10", "Run: npm test").is_none());
    }

    #[tokio::test]
    async fn full_access_auto_allows_anything_and_clears_queue() {
        let r = AskRegistry::new();
        let (id1, rx1) = r.request(1, "10", "claude", "Run: a", "a");
        let (_id2, rx2) = r.request(1, "10", "claude", "Edit b", "b");
        // full access on the first clears BOTH open asks for that task
        assert!(r.answer(id1, Answer::Full));
        assert_eq!(rx1.await.unwrap(), Decision::Allow);
        assert_eq!(rx2.await.unwrap(), Decision::Allow);
        assert!(r.open().is_empty());
        // and any future ask auto-allows
        assert_eq!(r.auto_decision(1, "10", "Run: anything"), Some(Decision::Allow));
    }

    #[tokio::test]
    async fn cancel_drops_without_answer() {
        let r = AskRegistry::new();
        let (id, rx) = r.request(2, "", "codex", "Edit x", "x");
        r.cancel(id);
        assert!(r.open().is_empty());
        assert!(rx.await.is_err()); // sender dropped
    }

    #[tokio::test]
    async fn notifier_fires_on_open_resolve_and_cancel() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(r.set_notifier(tx).is_empty()); // 空 registry 挂接 → 空快照
        let (id, _drx) = r.request(1, "10", "claude", "Run: x", "x");
        assert!(matches!(rx.recv().await.unwrap(), AskEvent::Opened(a) if a.id == id));
        r.answer(id, Answer::Allow);
        assert!(matches!(
            rx.recv().await.unwrap(),
            AskEvent::Resolved { id: rid, answer: Answer::Allow } if rid == id
        ));
        let (id2, _drx2) = r.request(1, "10", "claude", "Run: y", "y");
        assert!(matches!(rx.recv().await.unwrap(), AskEvent::Opened(a) if a.id == id2));
        r.cancel(id2);
        assert!(matches!(rx.recv().await.unwrap(), AskEvent::Cancelled { id: c } if c == id2));
    }

    #[tokio::test]
    async fn full_answer_resolves_every_covered_ask_via_notifier() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(r.set_notifier(tx).is_empty());
        let (id1, _a) = r.request(1, "10", "claude", "Run: a", "a");
        let (id2, _b) = r.request(1, "10", "claude", "Run: b", "b");
        assert!(matches!(rx.recv().await.unwrap(), AskEvent::Opened(a) if a.id == id1));
        assert!(matches!(rx.recv().await.unwrap(), AskEvent::Opened(a) if a.id == id2));
        r.answer(id1, Answer::Full); // 覆盖 id2
        let mut got = vec![];
        for _ in 0..2 {
            if let AskEvent::Resolved { id, answer } = rx.recv().await.unwrap() {
                assert_eq!(answer, Answer::Full); // 连带覆盖也携带人答的判决
                got.push(id);
            }
        }
        got.sort();
        assert_eq!(got, vec![id1, id2]);
    }

    #[tokio::test]
    async fn dangerous_release_resolves_backlog_via_notifier() {
        let r = AskRegistry::new();
        let (id1, _a) = r.request(1, "10", "claude", "Run: a", "a");
        let (id2, _b) = r.request(2, "", "codex", "Edit b", "b");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // 挂接晚于 request：快照补齐已 open 的 ask，且不会再收到它们的 Opened
        let snap: Vec<u64> = r.set_notifier(tx).iter().map(|a| a.id).collect();
        assert_eq!(snap, vec![id1, id2]);
        r.set_dangerous(true);
        let mut got = vec![];
        for _ in 0..2 {
            if let AskEvent::Resolved { id, answer } = rx.recv().await.unwrap() {
                assert_eq!(answer, Answer::Allow); // 释放积压记为 Allow
                got.push(id);
            }
        }
        got.sort();
        assert_eq!(got, vec![id1, id2]);
        assert!(r.open().is_empty());
    }

    #[test]
    fn open_in_filters_by_thread() {
        let r = AskRegistry::new();
        let _ = r.request(1, "10", "claude", "a", "a");
        let _ = r.request(2, "20", "codex", "b", "b");
        assert_eq!(r.open_in(1).len(), 1);
        assert_eq!(r.open_in(2).len(), 1);
        assert_eq!(r.open_in(1)[0].thread, 1);
    }
}
