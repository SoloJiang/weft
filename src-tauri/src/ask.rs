//! The Ask Bridge (ARCHITECTURE §4.3): permission Asks from every tool funnel to
//! one weft endpoint, become Needs-you cards, the human answers, and the
//! decision flows back to the blocked tool. Each tool intercepts at its own
//! structured point (Claude PreToolUse hook, Codex approval-request, OpenCode
//! /event), but they all resolve through THIS registry — never by scraping the
//! terminal. A spawned task that hits an approval no longer hangs silently in a
//! PTY; it surfaces as a card you can answer from the board.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
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
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    waiters: HashMap<u64, oneshot::Sender<Decision>>,
    open: Vec<Ask>,
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
        g.open.push(Ask {
            id,
            thread,
            dir: dir.to_string(),
            tool: tool.to_string(),
            summary: summary.to_string(),
            detail: detail.to_string(),
            ts: now(),
        });
        (id, rx)
    }

    /// Answer a pending Ask; wakes the blocked tool. False if already resolved.
    pub fn resolve(&self, id: u64, decision: Decision) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.open.retain(|a| a.id != id);
        match g.waiters.remove(&id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Drop a pending Ask without answering (e.g. on timeout) so it leaves the
    /// board. The waiter's receiver errors, which the endpoint treats as fallback.
    pub fn cancel(&self, id: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.open.retain(|a| a.id != id);
        g.waiters.remove(&id);
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
    async fn request_then_resolve_delivers_decision() {
        let r = AskRegistry::new();
        let (id, rx) = r.request(1, "10", "claude", "Run: npm test", "npm test");
        assert_eq!(r.open().len(), 1);
        assert_eq!(r.open()[0].summary, "Run: npm test");
        assert!(r.resolve(id, Decision::Allow));
        assert_eq!(rx.await.unwrap(), Decision::Allow);
        // cleared after resolve
        assert!(r.open().is_empty());
        // double-resolve is a no-op
        assert!(!r.resolve(id, Decision::Deny));
    }

    #[tokio::test]
    async fn cancel_drops_without_answer() {
        let r = AskRegistry::new();
        let (id, rx) = r.request(2, "", "codex", "Edit x", "x");
        r.cancel(id);
        assert!(r.open().is_empty());
        assert!(rx.await.is_err()); // sender dropped
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
