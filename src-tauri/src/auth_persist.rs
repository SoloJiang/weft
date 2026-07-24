//! Durable persistence for standing authorization grants (`full` / `always`).
//!
//! The `AskRegistry` keeps grants in memory (fast, DB-agnostic, unit-testable).
//! This module mirrors them to the store so a granted "Full access" or a precise
//! "Always allow" (keyed by the exact `action_key` — issue #89) survives an app
//! restart instead of re-prompting every run — and, crucially, is back in place
//! BEFORE `spawn_revive` re-drives in-flight tasks, so a revived worker runs
//! unattended under the access the human already granted.
//!
//! Single ordered writer: the registry enqueues a `PersistMsg` on every real grant
//! change; THIS consumer is the ONLY writer, draining the channel in FIFO order so
//! the last-enqueued snapshot is the last on disk (never a parallel write that
//! could let a stale snapshot land after a fresh one). A grant-changing command
//! calls `flush` to enqueue an ack'd write and await its completion, so it reports
//! success only after the change is durable. Boot seeds the registry from the store.
//! `dangerous` mode is intentionally not here — a global toggle the frontend persists.

use crate::ask::{AskRegistry, GrantSnapshot, PersistAck, PersistMsg};
use crate::store::{repo, Db};
use tauri::{AppHandle, Manager};

/// The `app_setting` key holding the serialized `GrantSnapshot`.
const K_AUTH_GRANTS: &str = "auth_grants";

/// Load persisted grants. Best-effort: a missing or unparseable row yields an
/// empty set so a schema drift or corrupt value never blocks boot.
pub async fn load_snapshot(db: &Db) -> GrantSnapshot {
    let raw = match repo::get_setting(db, K_AUTH_GRANTS).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return GrantSnapshot::default(),
        Err(err) => {
            eprintln!("[weft] failed to read auth_grants: {err}");
            return GrantSnapshot::default();
        }
    };
    serde_json::from_str(&raw).unwrap_or_else(|err| {
        eprintln!("[weft] ignoring unparseable auth_grants: {err}");
        GrantSnapshot::default()
    })
}

/// Write the current grants, or clear the row entirely when empty. Returns the
/// error as a String so the single writer can propagate it to a command awaiting
/// durability (rather than silently swallowing a failed write).
pub async fn persist_snapshot(db: &Db, snap: &GrantSnapshot) -> Result<(), String> {
    let result = if snap.is_empty() {
        repo::delete_setting(db, K_AUTH_GRANTS).await
    } else {
        match serde_json::to_string(snap) {
            Ok(json) => repo::set_setting(db, K_AUTH_GRANTS, &json).await,
            Err(err) => return Err(format!("serialize auth_grants: {err}")),
        }
    };
    result.map_err(|err| format!("persist auth_grants: {err}"))
}

/// Seed the registry from the store at boot — call BEFORE anything serves asks
/// or `spawn_revive` re-drives tasks. Both Full and Always are inherited: Always
/// is keyed by the exact `action_key` (issue #89), not the lossy display
/// summary, so a restored Always grant re-applies to the SAME action only — it
/// cannot silently widen across a restart the way a summary-keyed grant could.
pub async fn seed(db: &Db, asks: &AskRegistry) {
    let snap = load_snapshot(db).await;
    asks.seed_grants(snap);
}

/// Durably persist the registry's current grants and await it — for a
/// grant-changing command that must not report success until the change is on
/// disk. Routes through the SAME single writer as every other persist (never a
/// parallel write), so writes stay ordered and a stale snapshot can't land after
/// this one — the ordering guarantee round-1's parallel flush lacked. Returns the
/// store error so the command can surface a failed write. A no-op `Ok` when no
/// consumer is installed (only a unit test without one).
pub async fn flush(asks: &AskRegistry) -> Result<(), String> {
    match asks.request_persist_ack() {
        // No writer installed (a unit test without a consumer) → nothing to do.
        PersistAck::NoConsumer => Ok(()),
        // Writer configured but its channel is closed (consumer died) → the durable
        // write did NOT happen; surface it rather than falsely reporting success.
        PersistAck::WriterGone => Err("auth_grants writer unavailable".into()),
        PersistAck::Pending(rx) => rx
            .await
            .unwrap_or_else(|_| Err("auth_grants writer dropped".into())),
    }
}

/// The single ordered writer: drain persist messages in FIFO order, write each to
/// the store, and signal any ack with the result. Being the ONLY writer over an
/// ordered channel is what guarantees the last-enqueued snapshot is the last on
/// disk, so a revoke awaited via `flush` cannot be resurrected by a stale write.
pub(crate) async fn run_consumer(
    db: Db,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistMsg>,
) {
    while let Some(msg) = rx.recv().await {
        let result = persist_snapshot(&db, &msg.snapshot).await;
        match msg.ack {
            Some(ack) => {
                let _ = ack.send(result);
            }
            None => {
                if let Err(err) = result {
                    eprintln!("[weft] auth_grants persist failed: {err}");
                }
            }
        }
    }
}

/// Install the single persist writer: mirror every grant change to the store.
/// Called once at startup (mirrors `trail::spawn`).
pub fn spawn(app: AppHandle) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<PersistMsg>();
    app.state::<AskRegistry>().set_persist_notifier(tx);
    let db = app.state::<Db>().inner().clone();
    tauri::async_runtime::spawn(run_consumer(db, rx));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::{AlwaysGrant, Decision, FullGrant};

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    fn sample() -> GrantSnapshot {
        GrantSnapshot {
            full: vec![FullGrant {
                thread: 1,
                dir: "10".into(),
            }],
            always: vec![AlwaysGrant {
                thread: 2,
                dir: "".into(),
                action_key: "Run: npm test".into(),
            }],
        }
    }

    #[tokio::test]
    async fn load_on_empty_db_is_empty() {
        let db = mem().await;
        assert!(load_snapshot(&db).await.is_empty());
    }

    #[tokio::test]
    async fn persist_then_load_round_trips() {
        let db = mem().await;
        persist_snapshot(&db, &sample()).await.unwrap();
        assert_eq!(load_snapshot(&db).await, sample());
    }

    #[tokio::test]
    async fn persist_empty_clears_a_previous_value() {
        let db = mem().await;
        persist_snapshot(&db, &sample()).await.unwrap();
        persist_snapshot(&db, &GrantSnapshot::default()).await.unwrap();
        assert!(load_snapshot(&db).await.is_empty());
    }

    #[tokio::test]
    async fn corrupt_value_loads_as_empty_not_a_panic() {
        let db = mem().await;
        repo::set_setting(&db, K_AUTH_GRANTS, "{ not json").await.unwrap();
        assert!(load_snapshot(&db).await.is_empty());
    }

    #[tokio::test]
    async fn seed_loads_full_and_precise_always_grants() {
        let db = mem().await;
        // sample() has a Full grant AND a precise (action-key-keyed) Always grant.
        persist_snapshot(&db, &sample()).await.unwrap();
        let asks = AskRegistry::new();
        seed(&db, &asks).await;
        // Full is inherited...
        assert_eq!(asks.auto_decision(1, "10", "anything"), Some(Decision::Allow));
        // ...and so is the precise Always grant — issue #89: it's safe to restore
        // because it's keyed by the exact action, not a lossy display summary.
        assert_eq!(
            asks.auto_decision(2, "", "Run: npm test"),
            Some(Decision::Allow)
        );
    }

    /// Drains one snapshot from the registry's persist channel into the store,
    /// exactly as `spawn`'s consumer loop does — so these end-to-end tests prove
    /// the real wiring, not a hand-built blob.
    async fn drain_once(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistMsg>,
        db: &Db,
    ) {
        let msg = rx.recv().await.expect("a grant change was emitted");
        persist_snapshot(db, &msg.snapshot).await.unwrap();
    }

    /// Acceptance #1: granted Full access survives an app restart — the revived
    /// task auto-allows instead of re-prompting.
    #[tokio::test]
    async fn full_access_grant_survives_a_simulated_restart() {
        let db = mem().await;

        // session 1: the human grants Full access; the consumer persists it.
        let asks = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);
        let (id, _rx) = asks.request(
            7,
            "42",
            "codex",
            "Run: cargo build",
            "cargo build",
            "Run: cargo build",
        );
        assert!(asks.answer(id, crate::ask::Answer::Full));
        drain_once(&mut rx, &db).await;

        // restart: a brand-new registry, seeded from the store before it serves.
        let revived = AskRegistry::new();
        seed(&db, &revived).await;
        assert_eq!(
            revived.auto_decision(7, "42", "Run: anything-else"),
            Some(Decision::Allow),
            "a persisted Full grant must not re-prompt after restart"
        );
    }

    /// Issue #89's core persisted-path acceptance case: a precise Always grant
    /// ALSO survives a restart (unlike PR #87's approach-B, where any on-disk
    /// Always was dropped at boot) — AND stays exact: a different action sharing
    /// the granted action's old lossy display summary still re-prompts.
    #[tokio::test]
    async fn always_grant_survives_a_simulated_restart_and_stays_precise() {
        let db = mem().await;
        let asks = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);
        let (id, _rx) = asks.request(
            7,
            "42",
            "codex",
            "Run: npm test",
            "npm test\necho a",
            "npm test\necho a",
        );
        assert!(asks.answer(id, crate::ask::Answer::Always));
        drain_once(&mut rx, &db).await;

        let revived = AskRegistry::new();
        seed(&db, &revived).await;
        // the exact action_key is restored...
        assert_eq!(
            revived.auto_decision(7, "42", "npm test\necho a"),
            Some(Decision::Allow),
            "a persisted Always grant must survive restart when it's the exact action"
        );
        // ...but a different action sharing the old ask's display summary ("Run:
        // npm test") must still re-prompt — precision, not the label, persisted.
        assert!(revived.auto_decision(7, "42", "npm test\nrm -rf /").is_none());
    }

    /// Acceptance #3: revoking a thread's grants (issue deletion) clears them from
    /// the store, so a later reuse of that thread id starts un-granted.
    #[tokio::test]
    async fn revoking_a_thread_clears_its_persisted_grant() {
        let db = mem().await;
        let asks = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);
        let (id, _rx) = asks.request(7, "42", "codex", "Run: x", "x", "Run: x");
        assert!(asks.answer(id, crate::ask::Answer::Full));
        drain_once(&mut rx, &db).await;

        asks.revoke_thread(7);
        drain_once(&mut rx, &db).await;

        // restart after deletion: nothing seeded, so the id is un-granted again.
        let revived = AskRegistry::new();
        seed(&db, &revived).await;
        assert!(revived.auto_decision(7, "42", "Run: x").is_none());
    }

    #[tokio::test]
    async fn flush_without_a_writer_is_a_noop() {
        // No persist consumer installed (as in a unit test) → flush is a no-op Ok.
        let asks = AskRegistry::new();
        assert!(flush(&asks).await.is_ok());
    }

    #[tokio::test]
    async fn flush_errors_when_the_writer_channel_is_closed() {
        let asks = AskRegistry::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);
        // the single writer is gone → the channel is closed
        drop(rx);
        assert!(
            flush(&asks).await.is_err(),
            "a closed writer channel must surface as a flush error, not false success"
        );
    }

    #[tokio::test]
    async fn flush_persists_full_and_always_and_is_durable() {
        let db = mem().await;
        let asks = AskRegistry::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);
        tokio::spawn(run_consumer(db.clone(), rx));
        // sample() has a Full grant (thread 1) AND a precise Always grant (thread 2)
        asks.seed_grants(sample());
        // flush enqueues to the single writer and awaits its store write
        flush(&asks).await.unwrap();
        let snap = load_snapshot(&db).await;
        // Both persist now — Always is precise (action-key-keyed), so it's safe (#89).
        assert_eq!(snap.full, sample().full);
        assert_eq!(snap.always, sample().always);
        // revoke both threads' grants then flush → nothing left on disk
        asks.revoke_thread(1);
        asks.revoke_thread(2);
        flush(&asks).await.unwrap();
        assert!(load_snapshot(&db).await.is_empty());
    }

    /// The ordering guarantee round-1 lacked: even with a stale Full-grant snapshot
    /// still queued ahead of it, a revoke awaited via `flush` is the LAST write, so
    /// an immediate crash/quit leaves the grant revoked on restart — it must not be
    /// resurrected by the queued grant. (Fails under round-1's parallel flush.)
    #[tokio::test]
    async fn revoke_flushed_then_crash_does_not_resurrect_the_grant() {
        let db = mem().await;
        let asks = AskRegistry::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);

        // grant then revoke BEFORE the writer starts — a stale {full} snapshot is now
        // queued ahead of the {} revoke, exactly the round-1 race window.
        let (id, _rx) = asks.request(7, "42", "codex", "Run: x", "x", "Run: x");
        assert!(asks.answer(id, crate::ask::Answer::Full)); // queues {full}
        asks.revoke_grant(7, "42", None); // queues {}

        // start the single writer, await the revoke's durability, then "crash".
        let writer = tokio::spawn(run_consumer(db.clone(), rx));
        flush(&asks).await.unwrap();
        writer.abort();

        // restart: the store must reflect the revoke, not the earlier grant.
        let revived = AskRegistry::new();
        seed(&db, &revived).await;
        assert!(
            revived.auto_decision(7, "42", "x").is_none(),
            "a flushed revoke must survive a crash; the queued grant must NOT resurrect"
        );
    }
}
