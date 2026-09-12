//! Thread bus: per-issue participant inboxes. Participant identity comes from
//! the MCP URL path (`/bus/:issue/:party/mcp`), never from tool arguments —
//! an agent cannot spoof `from`.
//!
//! Memory is the live delivery queue; unsettled `bus_message` rows are its
//! durable source of truth and are restored after a daemon restart. A `post`
//! also fires a wake signal — Stage 2's orchestrator subscribes to
//! it and delivers the message into the recipient's Codex thread
//! (`turn/steer` while busy, `turn/start` when idle; see the migration spec
//! §6 for the verified semantics).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Msg {
    #[serde(skip_serializing, default)]
    pub id: i64,
    pub from: String,
    pub text: String,
    pub kind: String,
    pub ts: String,
}

struct Inner {
    inboxes: HashMap<(i64, String), Vec<Msg>>,
    wake: broadcast::Sender<(i64, String)>,
}

/// Process-wide bus registry. Clone to share (Arc inside).
#[derive(Clone)]
pub struct BusRegistry(Arc<Mutex<Inner>>);

impl BusRegistry {
    pub fn new() -> Self {
        let (wake, _rx) = broadcast::channel(256);
        Self(Arc::new(Mutex::new(Inner {
            inboxes: HashMap::new(),
            wake,
        })))
    }

    /// Deliver `msg` into `(issue, to)`'s inbox and notify wakers.
    pub fn post(&self, issue: i64, to: &str, msg: Msg) {
        let wake = {
            let mut g = match self.0.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            g.inboxes
                .entry((issue, to.to_string()))
                .or_default()
                .push(msg);
            g.wake.clone()
        };
        let _ = wake.send((issue, to.to_string()));
    }

    /// Restore one durable undelivered row during daemon boot without firing
    /// a wake before the delivery loop and HTTP listener are ready.
    pub fn restore(&self, issue: i64, to: &str, msg: Msg) {
        let mut g = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.inboxes
            .entry((issue, to.to_string()))
            .or_default()
            .push(msg);
    }

    /// Snapshot parties with queued messages. The orchestrator uses this once
    /// after boot to flush restored durable inboxes.
    pub fn pending_parties(&self) -> Vec<(i64, String)> {
        let g = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut parties: Vec<(i64, String)> = g.inboxes.keys().cloned().collect();
        parties.sort();
        parties
    }

    /// Drain `(issue, party)`'s live inbox, oldest first.
    pub fn drain(&self, issue: i64, party: &str) -> Vec<Msg> {
        let mut g = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.inboxes
            .remove(&(issue, party.to_string()))
            .unwrap_or_default()
    }

    /// Put previously-drained messages back at the FRONT of the inbox
    /// (preserving order), WITHOUT firing a wake. Used by the orchestrator
    /// when injecting into the recipient's Codex thread failed: the messages
    /// stay pullable via `bus_read`, and the next `post` to this party fires
    /// a wake that re-attempts delivery of the whole inbox.
    pub fn requeue_front(&self, issue: i64, party: &str, msgs: Vec<Msg>) {
        if msgs.is_empty() {
            return;
        }
        let mut g = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let inbox = g.inboxes.entry((issue, party.to_string())).or_default();
        let mut restored = msgs;
        restored.append(inbox);
        *inbox = restored;
    }

    /// Subscribe to wake signals `(issue, to_party)` fired on every post.
    pub fn subscribe_wake(&self) -> broadcast::Receiver<(i64, String)> {
        let g = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.wake.subscribe()
    }
}

impl Default for BusRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(from: &str, text: &str) -> Msg {
        Msg {
            id: 1,
            from: from.to_string(),
            text: text.to_string(),
            kind: "message".to_string(),
            ts: "0".to_string(),
        }
    }

    #[tokio::test]
    async fn post_drain_and_wake() {
        let bus = BusRegistry::new();
        let mut wakes = bus.subscribe_wake();
        bus.post(7, "3", msg("lead", "hi"));
        bus.post(7, "3", msg("lead", "again"));
        bus.post(7, "lead", msg("3", "report"));
        // Wake fired for each post, addressed to the recipient.
        let w = wakes.recv().await.expect("wake 1");
        assert_eq!(w, (7, "3".to_string()));
        let drained = bus.drain(7, "3");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "hi");
        assert_eq!(drained[1].from, "lead");
        // Drain is destructive.
        assert!(bus.drain(7, "3").is_empty());
        // The other inbox is untouched.
        assert_eq!(bus.drain(7, "lead").len(), 1);
    }

    #[tokio::test]
    async fn requeue_restores_order_and_stays_silent() {
        let bus = BusRegistry::new();
        bus.post(7, "3", msg("lead", "one"));
        bus.post(7, "3", msg("lead", "two"));
        let drained = bus.drain(7, "3");
        // A wake fired per post; requeue must NOT add another (a wake loop
        // would re-deliver forever when the recipient thread is down).
        let mut wakes = bus.subscribe_wake();
        bus.requeue_front(7, "3", drained);
        assert!(wakes.try_recv().is_err());
        // A later post lands BEHIND the requeued messages.
        bus.post(7, "3", msg("lead", "three"));
        let all = bus.drain(7, "3");
        let texts: Vec<&str> = all.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["one", "two", "three"]);
    }

    #[test]
    fn restore_preserves_rows_without_waking() {
        let bus = BusRegistry::new();
        let mut wakes = bus.subscribe_wake();
        bus.restore(9, "lead", msg("2", "done"));
        assert!(wakes.try_recv().is_err());
        assert_eq!(bus.pending_parties(), vec![(9, "lead".to_string())]);
        assert_eq!(bus.drain(9, "lead")[0].text, "done");
    }
}
