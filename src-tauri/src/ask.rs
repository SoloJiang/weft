//! The Ask Bridge (ARCHITECTURE §4.3): permission Asks from every tool funnel to
//! one weft endpoint, become Needs-you cards, the human answers, and the
//! decision flows back to the blocked tool. Each tool intercepts at its own
//! structured point (Claude PreToolUse hook, Codex approval-request, OpenCode
//! /event), but they all resolve through THIS registry — never by scraping the
//! terminal. A spawned task that hits an approval no longer hangs silently in a
//! PTY; it surfaces as a card you can answer from the board.

use serde::{Deserialize, Serialize};
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
    /// Always/Full 连带覆盖的 ask 记为人答的那个 Answer）。携带被解决的 Ask
    /// 快照，使消费侧（IM 终态卡 / transcript 结算痕迹）无需回查已移除的 open。
    Resolved {
        ask: Ask,
        answer: Answer,
    },
    Cancelled {
        id: u64,
    },
}

/// The human's answer to a permission Ask. `Always` remembers this action for
/// the asking task; `Full` auto-approves everything from that task. Both are
/// weft-side passthrough rules, scoped per (thread, task), kept in memory.
/// IM 回复作答的中英动词/序号宽松解析见 `im::inbound::parse_verdict`，
/// 落点即本枚举（`parse`/`as_str` 是 verdict 串的严格双向映射）。
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

    /// `parse` 的逆映射；verdict 字符串的单一来源（IM 出站终态卡等消费方
    /// 一律经此取串，不得手写字面量）。
    pub fn as_str(self) -> &'static str {
        match self {
            Answer::Allow => "allow",
            Answer::Deny => "deny",
            Answer::Always => "always",
            Answer::Full => "full",
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

/// A persisted "full access" grant: every ask from this (thread, dir) auto-allows.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FullGrant {
    pub thread: i32,
    pub dir: String,
}

/// A persisted "always allow" grant: this exact `summary` from this (thread, dir)
/// auto-allows.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlwaysGrant {
    pub thread: i32,
    pub dir: String,
    pub summary: String,
}

/// The durable shape of the registry's standing grants (`full` + `always`). This
/// is what gets mirrored to the store and re-seeded at boot so a granted "Full
/// access" survives an app restart instead of re-prompting every run. `dangerous`
/// is deliberately NOT here — it is a global toggle the frontend already persists.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GrantSnapshot {
    #[serde(default)]
    pub full: Vec<FullGrant>,
    #[serde(default)]
    pub always: Vec<AlwaysGrant>,
}

impl GrantSnapshot {
    /// True when there is nothing to persist (used to avoid writing an empty row).
    pub fn is_empty(&self) -> bool {
        self.full.is_empty() && self.always.is_empty()
    }
}

/// A persist request to the SINGLE ordered writer (the `auth_persist` consumer).
/// `ack`, when present, is signalled with the store-write result once THIS message
/// is written — so a grant-changing command can await durability and surface a
/// write failure. A fire-and-forget emit uses `ack: None`. Routing every write
/// through one channel (never a parallel direct write) is what keeps them ordered:
/// the last-enqueued snapshot is the last written, so a stale one can't clobber it.
pub struct PersistMsg {
    pub snapshot: GrantSnapshot,
    pub ack: Option<oneshot::Sender<Result<(), String>>>,
}

/// The outcome of enqueuing a durable-write request (`request_persist_ack`), so a
/// flush can tell "no writer configured" (a unit test — a no-op) apart from "writer
/// configured but its channel is closed" (the consumer died — a real durability
/// failure that must surface as an error, not a false success).
pub enum PersistAck {
    NoConsumer,
    WriterGone,
    Pending(oneshot::Receiver<Result<(), String>>),
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    waiters: HashMap<u64, oneshot::Sender<Decision>>,
    open: Vec<Ask>,
    /// (thread, dir) -> summaries the human has "always allow"-ed. **In-memory only**:
    /// Always grants are NOT persisted in this PR — an Always summary is a lossy
    /// display label (a Claude multi-line command truncated to its first line, an MCP
    /// tool name, a truncated path list), not a precise action key, so persisting it
    /// risks a permanent over-broad grant. Only Full access is persisted here; precise
    /// per-action Always-persistence lands with issue #89's canonical action key.
    always: HashMap<(i32, String), HashSet<String>>,
    /// (thread, dir) granted full access — every ask auto-allows.
    full: HashSet<(i32, String)>,
    /// Dangerous mode: when on, EVERY ask from EVERY agent auto-allows (never
    /// surfaced). The global "skip all permission prompts" setting.
    dangerous: bool,
    /// IM 桥的通知器：装上后 Ask 开/答/撤事件外发；未装时零开销。
    notify: Option<tokio::sync::mpsc::UnboundedSender<AskEvent>>,
    /// transcript 结算痕迹消费者（与 IM 桥独立的第二订阅，始终在桌面端装上）。
    trail: Option<tokio::sync::mpsc::UnboundedSender<AskEvent>>,
    /// 授权落盘订阅（单一有序写者）：`full`/`always` 每次真正变更后收到一条
    /// `PersistMsg`（快照 + 可选 ack）。消费方按序写 store；命令路径经 ack 等其
    /// 落盘完成后再返回。装上后授权跨重启存活；未装时零开销。
    persist: Option<tokio::sync::mpsc::UnboundedSender<PersistMsg>>,
}

impl Inner {
    /// 事件外发（持锁内调用）：两路订阅各自独立，未装的那路零开销、不报错。
    fn emit(&self, ev: AskEvent) {
        if let Some(tx) = &self.trail {
            let _ = tx.send(ev.clone());
        }
        if let Some(tx) = &self.notify {
            let _ = tx.send(ev);
        }
    }

    /// Current DURABLE grants (持锁内调用). Only Full access is persisted in this PR
    /// — Always grants are in-memory only (see #89), so the snapshot's `always` is
    /// always empty.
    fn grant_snapshot(&self) -> GrantSnapshot {
        let full = self
            .full
            .iter()
            .map(|(thread, dir)| FullGrant {
                thread: *thread,
                dir: dir.clone(),
            })
            .collect();
        GrantSnapshot {
            full,
            always: Vec::new(),
        }
    }

    /// Push the current grants to the persistence consumer as a fire-and-forget
    /// (no-ack) message (持锁内调用，仅在 grant 真正变更后调用). 未装消费者时零开销。
    fn emit_persist(&self) {
        if let Some(tx) = &self.persist {
            let _ = tx.send(PersistMsg {
                snapshot: self.grant_snapshot(),
                ack: None,
            });
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
        let cleared: Vec<Ask> = std::mem::take(&mut g.open);
        for ask in cleared {
            if let Some(tx) = g.waiters.remove(&ask.id) {
                let _ = tx.send(Decision::Allow);
            }
            g.emit(AskEvent::Resolved {
                ask,
                answer: Answer::Allow,
            });
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
        // Whether this answer added a NEW standing grant (HashSet::insert is true
        // only on first insertion). Drives a single persist write — an idempotent
        // re-grant of an existing rule writes nothing.
        let granted = match ans {
            Answer::Always => {
                // Record the always-rule in memory (auto_decision uses this), but do
                // NOT persist it: `granted` (which gates the persist emit) stays false,
                // so Always is in-memory only — a lossy display summary must not become
                // a permanent grant. Only Full persists here; #89 adds precise
                // per-action Always-persistence.
                g.always
                    .entry(key.clone())
                    .or_default()
                    .insert(ask.summary.clone());
                false
            }
            Answer::Full => g.full.insert(key.clone()),
            _ => false,
        };

        // Every open ask this answer now covers (the target + any others the new
        // rule sweeps up) resolves to the same verdict.
        let decision = if ans == Answer::Deny {
            Decision::Deny
        } else {
            Decision::Allow
        };
        let covered: Vec<Ask> = g
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
            .cloned()
            .collect();

        let covered_ids: HashSet<u64> = covered.iter().map(|a| a.id).collect();
        g.open.retain(|a| !covered_ids.contains(&a.id));
        for c in covered {
            if let Some(tx) = g.waiters.remove(&c.id) {
                let _ = tx.send(decision);
            }
            g.emit(AskEvent::Resolved {
                ask: c,
                answer: ans,
            });
        }
        // Mirror the new grant to the store (single source: the only place a
        // human-created full/always rule is persisted, so all answer() callers
        // stay unaware of persistence).
        if granted {
            g.emit_persist();
        }
        // Success = the ask was found AND answered (an unfound/already-answered ask
        // returned false above). Whether a waiter was still around to wake is a
        // separate race — a cancelled approval request drops its waiter, but the
        // human's answer and any grant still took effect, so the caller must NOT see
        // that as "expired" while the grant is persisted.
        true
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

    /// Install the transcript-trail consumer's channel (called once at startup,
    /// independent of the IM bridge's `set_notifier`). No snapshot: the trail
    /// only records future resolutions, never replays still-open asks.
    pub fn set_trail_notifier(&self, tx: tokio::sync::mpsc::UnboundedSender<AskEvent>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).trail = Some(tx);
    }

    /// Install the durable-grants consumer's channel (called once at startup). It
    /// receives a `PersistMsg` every time a `full`/`always` grant is added, revoked,
    /// or explicitly flushed; the consumer is the single writer to the store.
    pub fn set_persist_notifier(&self, tx: tokio::sync::mpsc::UnboundedSender<PersistMsg>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).persist = Some(tx);
    }

    /// Enqueue the CURRENT grants to the single writer WITH a completion ack.
    /// Routing through the same channel (not a parallel direct write) keeps writes
    /// ordered, so a stale queued snapshot can never land after this one. Returns
    /// `NoConsumer` (no writer installed — unit test), `WriterGone` (writer installed
    /// but its channel closed — durability failure), or `Pending(rx)` to await.
    pub fn request_persist_ack(&self) -> PersistAck {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tx) = g.persist.as_ref() else {
            return PersistAck::NoConsumer;
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx
            .send(PersistMsg {
                snapshot: g.grant_snapshot(),
                ack: Some(ack_tx),
            })
            .is_err()
        {
            return PersistAck::WriterGone;
        }
        PersistAck::Pending(ack_rx)
    }

    /// Seed standing grants at boot from the persisted snapshot (before serving any
    /// ask). Does NOT re-emit to `persist` — this loads FROM persistence.
    pub fn seed_grants(&self, snap: GrantSnapshot) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for fg in snap.full {
            g.full.insert((fg.thread, fg.dir));
        }
        // A persisted snapshot has no always-rules (Always isn't persisted in this PR),
        // so this is a no-op in practice; kept so a prior-version disk row still loads.
        for ag in snap.always {
            g.always
                .entry((ag.thread, ag.dir))
                .or_default()
                .insert(ag.summary);
        }
    }

    /// Current standing grants, for persistence/inspection.
    pub fn snapshot_grants(&self) -> GrantSnapshot {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .grant_snapshot()
    }

    /// Drop every standing grant belonging to `thread` (issue deletion cascade) and
    /// RETURN exactly what was removed — computed under the SAME lock as the removal,
    /// so a concurrent revoke can't inflate it (that return is what makes an atomic
    /// rollback possible). Persists the reduced snapshot when anything changed.
    pub fn revoke_thread(&self, thread: i32) -> GrantSnapshot {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = GrantSnapshot::default();
        let full_keys: Vec<(i32, String)> =
            g.full.iter().filter(|(t, _)| *t == thread).cloned().collect();
        for key in full_keys {
            g.full.remove(&key);
            removed.full.push(FullGrant {
                thread: key.0,
                dir: key.1,
            });
        }
        // Clear this thread's in-memory always-rules too (delete cleanup). Always is
        // not persisted, so it never goes in `removed` (which drives durable rollback).
        g.always.retain(|(t, _), _| *t != thread);
        if !removed.is_empty() {
            g.emit_persist();
        }
        removed
    }

    /// Revoke a specific standing grant (the human's one-click "undo"):
    /// - `summary == None`  → clear this task's `(thread, dir)` grants entirely
    ///   (its full access AND all its always-rules).
    /// - `summary == Some(s)` → drop only that one always-rule, leaving full
    ///   access (if any) and the task's other always-rules intact.
    /// Returns exactly what was removed (under one lock; see `revoke_thread`).
    /// Persists the reduced snapshot when anything actually changed.
    pub fn revoke_grant(&self, thread: i32, dir: &str, summary: Option<&str>) -> GrantSnapshot {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = (thread, dir.to_string());
        let mut removed = GrantSnapshot::default();
        match summary {
            None => {
                if g.full.remove(&key) {
                    removed.full.push(FullGrant {
                        thread,
                        dir: dir.to_string(),
                    });
                }
                // in-memory always cleared too (not persisted → not in `removed`)
                g.always.remove(&key);
            }
            Some(summary) => {
                // Drop just the in-memory always-rule (not persisted → not in `removed`).
                if let Some(rules) = g.always.get_mut(&key) {
                    rules.remove(summary);
                    if rules.is_empty() {
                        g.always.remove(&key);
                    }
                }
            }
        }
        if !removed.is_empty() {
            g.emit_persist();
        }
        removed
    }

    /// Dispatch a revoke at the caller's granularity and return exactly what was
    /// removed — the single entry `revoke_auth_grant` funnels through (so the
    /// command stays a pure wrapper AND gets an atomic removed-set for rollback):
    /// - `dir == None`  → `revoke_thread` (clear the whole issue's grants).
    /// - `dir == Some`  → `revoke_grant` (one task, or one always-rule via `summary`).
    pub fn revoke(&self, thread: i32, dir: Option<&str>, summary: Option<&str>) -> GrantSnapshot {
        match dir {
            None => self.revoke_thread(thread),
            Some(dir) => self.revoke_grant(thread, dir, summary),
        }
    }

    /// Delete-time cleanup of an issue's WHOLE footprint in this registry: cancel
    /// its still-open asks AND revoke its standing grants. Cancelling the open asks
    /// matters as much as revoking: after the thread rows are gone a lingering card,
    /// if answered Full/Always, would `answer` a FRESH grant for the deleted id and
    /// reopen the id-reuse hole. Used by delete_thread.
    ///
    /// SAFETY INVARIANT (applies to all delete-time cleanup here — `purge_dir`,
    /// `revoke_thread`, `revoke_grant`, and the workspace/repo delete paths): this
    /// cleanup is DEFENSE-IN-DEPTH. The real guard against a deleted issue's grant
    /// being auto-approved for a DIFFERENT future issue is that `thread`/`direction`
    /// ids are SQLite `AUTOINCREMENT` and are never reused — so a stale grant for a
    /// deleted (thread, dir) is inert forever. If that schema invariant ever changes
    /// (id reuse becomes possible), re-evaluate the deferred PR #87 Codex round-3
    /// findings 1/2/4/5 (quiesce producers before purge, extra delete-path coverage,
    /// propagate cleanup-write failures) — a stale grant could then be inherited.
    pub fn purge_thread(&self, thread: i32) {
        let ids: Vec<u64> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.open
                .iter()
                .filter(|a| a.thread == thread)
                .map(|a| a.id)
                .collect()
        };
        for id in ids {
            self.cancel(id);
        }
        self.revoke_thread(thread);
    }

    /// Delete-time cleanup of ONE task's `(thread, dir)` footprint: cancel its open
    /// asks and revoke its standing grant (same rationale as `purge_thread`). Used by
    /// delete_repo, per removed direction.
    pub fn purge_dir(&self, thread: i32, dir: &str) {
        let ids: Vec<u64> = {
            let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.open
                .iter()
                .filter(|a| a.thread == thread && a.dir == dir)
                .map(|a| a.id)
                .collect()
        };
        for id in ids {
            self.cancel(id);
        }
        self.revoke_grant(thread, dir, None);
    }

    /// All Asks across threads (for the workspace-wide Needs-you surface).
    pub fn open(&self) -> Vec<Ask> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open
            .clone()
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

    #[test]
    fn answer_as_str_round_trips_with_parse() {
        for a in [Answer::Allow, Answer::Deny, Answer::Always, Answer::Full] {
            assert_eq!(Answer::parse(a.as_str()), Some(a));
        }
    }

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
        assert_eq!(
            r.auto_decision(1, "10", "Run: npm test"),
            Some(Decision::Allow)
        );
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
        assert_eq!(
            r.auto_decision(1, "10", "Run: anything"),
            Some(Decision::Allow)
        );
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
            AskEvent::Resolved { ask, answer: Answer::Allow } if ask.id == id
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
            if let AskEvent::Resolved { ask, answer } = rx.recv().await.unwrap() {
                assert_eq!(answer, Answer::Full); // 连带覆盖也携带人答的判决
                got.push(ask.id);
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
            if let AskEvent::Resolved { ask, answer } = rx.recv().await.unwrap() {
                assert_eq!(answer, Answer::Allow); // 释放积压记为 Allow
                got.push(ask.id);
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

    // ---- authorization persistence ------------------------------------------

    #[test]
    fn seeded_grants_are_honored_by_auto_decision() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![AlwaysGrant {
                thread: 2,
                dir: "20".into(),
                summary: "Run: npm test".into(),
            }],
        });
        // full → anything in (1,"10") auto-allows
        assert_eq!(r.auto_decision(1, "10", "Run: anything"), Some(Decision::Allow));
        // always → only the exact summary in (2,"20")
        assert_eq!(
            r.auto_decision(2, "20", "Run: npm test"),
            Some(Decision::Allow)
        );
        assert!(r.auto_decision(2, "20", "Run: other").is_none());
        // an unrelated key is unaffected
        assert!(r.auto_decision(3, "30", "x").is_none());
    }

    #[test]
    fn answering_full_persists_a_snapshot_with_that_grant() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        let (id, _rx) = r.request(1, "10", "codex", "Run: a", "a");
        assert!(r.answer(id, Answer::Full));
        // the send is synchronous inside answer(), so try_recv sees it immediately
        let snap = rx.try_recv().expect("full grant must be persisted").snapshot;
        assert_eq!(
            snap.full,
            vec![FullGrant {
                thread: 1,
                dir: "10".into()
            }]
        );
        assert!(snap.always.is_empty());
    }

    #[test]
    fn answering_always_is_in_memory_only_not_persisted() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        let (id, _rx) = r.request(1, "10", "codex", "Run: npm test", "npm test");
        assert!(r.answer(id, Answer::Always));
        // auto-allows this exact summary in memory...
        assert_eq!(
            r.auto_decision(1, "10", "Run: npm test"),
            Some(Decision::Allow)
        );
        // ...but Always grants are in-memory only in this PR — nothing is persisted
        // (only Full persists; precise per-action Always-persistence is #89).
        assert!(
            rx.try_recv().is_err(),
            "an Always answer must not write a durable grant"
        );
        assert!(r.snapshot_grants().always.is_empty());
    }

    #[test]
    fn plain_allow_creates_no_grant_and_does_not_persist() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        let (id, _rx) = r.request(1, "10", "codex", "Run: a", "a");
        assert!(r.answer(id, Answer::Allow));
        assert!(
            rx.try_recv().is_err(),
            "a one-shot allow must not write a standing grant"
        );
    }

    #[test]
    fn re_granting_full_does_not_re_persist() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![],
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        let (id, _rx) = r.request(1, "10", "codex", "Run: a", "a");
        // (1,"10") already has full access — answering Full again changes nothing.
        assert!(r.answer(id, Answer::Full));
        assert!(
            rx.try_recv().is_err(),
            "an unchanged grant set must not trigger a redundant write"
        );
    }

    #[test]
    fn revoke_thread_clears_that_threads_grants_and_persists() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![
                FullGrant {
                    thread: 1,
                    dir: "10".into(),
                },
                FullGrant {
                    thread: 2,
                    dir: "20".into(),
                },
            ],
            always: vec![AlwaysGrant {
                thread: 1,
                dir: "10".into(),
                summary: "x".into(),
            }],
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        r.revoke_thread(1);
        // thread 1 grants gone, thread 2 intact
        assert!(r.auto_decision(1, "10", "anything").is_none());
        assert_eq!(
            r.auto_decision(2, "20", "anything"),
            Some(Decision::Allow)
        );
        let snap = rx
            .try_recv()
            .expect("revocation must persist the reduced set")
            .snapshot;
        assert_eq!(
            snap.full,
            vec![FullGrant {
                thread: 2,
                dir: "20".into()
            }]
        );
        assert!(snap.always.is_empty());
    }

    #[test]
    fn revoke_thread_with_nothing_to_remove_does_not_persist() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        r.revoke_thread(99);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn grant_snapshot_round_trips_through_json_and_reseeds() {
        let snap = GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![AlwaysGrant {
                thread: 2,
                dir: "".into(),
                summary: "Run: x".into(),
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: GrantSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
        // and the round-tripped value seeds real behavior
        let r = AskRegistry::new();
        r.seed_grants(back);
        assert_eq!(r.auto_decision(1, "10", "z"), Some(Decision::Allow));
        assert_eq!(r.auto_decision(2, "", "Run: x"), Some(Decision::Allow));
    }

    #[test]
    fn snapshot_grants_reflects_answered_grants() {
        let r = AskRegistry::new();
        let (id, _rx) = r.request(1, "10", "codex", "Run: a", "a");
        r.answer(id, Answer::Full);
        let snap = r.snapshot_grants();
        assert_eq!(
            snap.full,
            vec![FullGrant {
                thread: 1,
                dir: "10".into()
            }]
        );
    }

    #[test]
    fn revoke_grant_none_clears_full_and_all_always_for_that_dir() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![
                AlwaysGrant {
                    thread: 1,
                    dir: "10".into(),
                    summary: "a".into(),
                },
                AlwaysGrant {
                    thread: 1,
                    dir: "10".into(),
                    summary: "b".into(),
                },
            ],
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        r.revoke_grant(1, "10", None);
        assert!(r.auto_decision(1, "10", "a").is_none());
        assert!(r.auto_decision(1, "10", "anything").is_none()); // full gone too
        let snap = rx
            .try_recv()
            .expect("one-click revoke persists the cleared set")
            .snapshot;
        assert!(snap.is_empty());
    }

    #[test]
    fn revoke_grant_with_summary_drops_only_that_always_rule() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![],
            always: vec![
                AlwaysGrant {
                    thread: 1,
                    dir: "10".into(),
                    summary: "a".into(),
                },
                AlwaysGrant {
                    thread: 1,
                    dir: "10".into(),
                    summary: "b".into(),
                },
            ],
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        r.revoke_grant(1, "10", Some("a"));
        assert!(r.auto_decision(1, "10", "a").is_none()); // dropped
        assert_eq!(r.auto_decision(1, "10", "b"), Some(Decision::Allow)); // kept
        // Always is in-memory only, so a granular always-revoke persists nothing.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn revoke_grant_with_summary_keeps_full_access() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![AlwaysGrant {
                thread: 1,
                dir: "10".into(),
                summary: "a".into(),
            }],
        });
        r.revoke_grant(1, "10", Some("a"));
        // full access is a separate rule — dropping one always must not touch it
        assert_eq!(r.auto_decision(1, "10", "anything"), Some(Decision::Allow));
    }

    #[test]
    fn revoke_grant_with_nothing_to_remove_does_not_persist() {
        let r = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        r.set_persist_notifier(tx);
        r.revoke_grant(1, "10", None);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn revoke_dispatch_routes_by_dir_granularity() {
        let seeded = || {
            let r = AskRegistry::new();
            r.seed_grants(GrantSnapshot {
                full: vec![FullGrant {
                    thread: 1,
                    dir: "10".into(),
                }],
                always: vec![
                    AlwaysGrant {
                        thread: 1,
                        dir: "10".into(),
                        summary: "a".into(),
                    },
                    AlwaysGrant {
                        thread: 1,
                        dir: "11".into(),
                        summary: "b".into(),
                    },
                ],
            });
            r
        };
        // dir=None → the whole issue (every dir under the thread) is cleared
        let r = seeded();
        r.revoke(1, None, None);
        assert!(r.auto_decision(1, "10", "x").is_none());
        assert!(r.auto_decision(1, "11", "b").is_none());
        // dir=Some, summary=None → only that one task; the sibling task survives
        let r = seeded();
        r.revoke(1, Some("10"), None);
        assert!(r.auto_decision(1, "10", "x").is_none());
        assert_eq!(r.auto_decision(1, "11", "b"), Some(Decision::Allow));
        // dir=Some, summary=Some → only that always-rule; the task's full access stays
        let r = seeded();
        r.revoke(1, Some("10"), Some("a"));
        assert_eq!(r.auto_decision(1, "10", "anything"), Some(Decision::Allow));
    }

    #[test]
    fn answering_a_found_ask_whose_waiter_is_gone_still_succeeds() {
        let r = AskRegistry::new();
        let (id, rx) = r.request(1, "10", "codex", "Run: x", "x");
        // the blocked tool's receiver is gone (e.g. its approval request was cancelled)
        drop(rx);
        // the ask is still open, so answering it Full is a SUCCESS (found + answered)
        // — the command must not report "expired" while the grant is being created.
        assert!(r.answer(id, Answer::Full));
        assert_eq!(r.auto_decision(1, "10", "anything"), Some(Decision::Allow));
        // a genuinely unknown / already-answered ask still returns false
        assert!(!r.answer(id, Answer::Full));
    }

    #[test]
    fn revoke_returns_exactly_what_it_removed() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![
                AlwaysGrant {
                    thread: 1,
                    dir: "10".into(),
                    summary: "a".into(),
                },
                AlwaysGrant {
                    thread: 1,
                    dir: "11".into(),
                    summary: "b".into(),
                },
            ],
        });
        // removes (1,"10")'s full + its (in-memory) always "a"; leaves (1,"11")
        let removed = r.revoke(1, Some("10"), None);
        assert_eq!(
            removed.full,
            vec![FullGrant {
                thread: 1,
                dir: "10".into()
            }]
        );
        // Always is in-memory only, so it never appears in the durable removed set...
        assert!(removed.always.is_empty());
        // ...but it IS cleared from memory (auto_decision no longer allows it).
        assert!(r.auto_decision(1, "10", "a").is_none());
        assert_eq!(r.auto_decision(1, "11", "b"), Some(Decision::Allow));
        // revoking nothing returns an empty set
        assert!(r.revoke(2, Some("99"), None).is_empty());
    }

    #[test]
    fn purge_thread_cancels_open_asks_and_revokes_grants() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![],
        });
        let (id1, _rx1) = r.request(1, "10", "codex", "Run: a", "a");
        let (id2, _rx2) = r.request(1, "11", "codex", "Run: b", "b");
        let (keep, _rxk) = r.request(2, "20", "codex", "Run: c", "c");

        r.purge_thread(1);

        // thread 1's grant is revoked...
        assert!(r.auto_decision(1, "10", "x").is_none());
        // ...and its open asks cancelled, while another thread's ask survives.
        let open: Vec<u64> = r.open().iter().map(|a| a.id).collect();
        assert_eq!(open, vec![keep]);
        assert!(!open.contains(&id1) && !open.contains(&id2));
    }

    #[test]
    fn purge_dir_cancels_that_dirs_asks_and_revokes_its_grant() {
        let r = AskRegistry::new();
        r.seed_grants(GrantSnapshot {
            full: vec![
                FullGrant {
                    thread: 1,
                    dir: "10".into(),
                },
                FullGrant {
                    thread: 1,
                    dir: "11".into(),
                },
            ],
            always: vec![],
        });
        let (drop_id, _r1) = r.request(1, "10", "codex", "Run: a", "a");
        let (keep_id, _r2) = r.request(1, "11", "codex", "Run: b", "b");

        r.purge_dir(1, "10");

        // (1,"10") grant + ask gone; the sibling dir (1,"11") is untouched.
        assert!(r.auto_decision(1, "10", "x").is_none());
        assert_eq!(r.auto_decision(1, "11", "x"), Some(Decision::Allow));
        let open: Vec<u64> = r.open().iter().map(|a| a.id).collect();
        assert_eq!(open, vec![keep_id]);
        assert!(!open.contains(&drop_id));
    }
}
