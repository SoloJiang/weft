//! Durable persistence for standing authorization grants (`full` / `always`).
//!
//! The `AskRegistry` keeps grants in memory (fast, DB-agnostic, unit-testable).
//! This module mirrors them to the store so a granted "Full access" survives an
//! app restart instead of re-prompting every run — and, crucially, is back in
//! place BEFORE `spawn_revive` re-drives in-flight tasks, so a revived worker
//! runs unattended under the access the human already granted.
//!
//! Single source: the registry emits a fresh `GrantSnapshot` on every real grant
//! change; this consumer is the ONLY writer, and boot seeds the registry from the
//! store. `dangerous` mode is intentionally not here — it is a global toggle the
//! frontend already persists.

use crate::ask::{AskRegistry, GrantSnapshot};
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

/// Write the current grants, or clear the row entirely when empty. Best-effort:
/// a failed write never breaks the (already applied) in-memory grant.
pub async fn persist_snapshot(db: &Db, snap: &GrantSnapshot) {
    let result = if snap.is_empty() {
        repo::delete_setting(db, K_AUTH_GRANTS).await
    } else {
        match serde_json::to_string(snap) {
            Ok(json) => repo::set_setting(db, K_AUTH_GRANTS, &json).await,
            Err(err) => {
                eprintln!("[weft] failed to serialize auth_grants: {err}");
                return;
            }
        }
    };
    if let Err(err) = result {
        eprintln!("[weft] failed to persist auth_grants: {err}");
    }
}

/// Seed the registry from the store at boot — call BEFORE anything serves asks
/// or `spawn_revive` re-drives tasks.
pub async fn seed(db: &Db, asks: &AskRegistry) {
    asks.seed_grants(load_snapshot(db).await);
}

/// Durably write the registry's CURRENT grants now and await it — for a
/// grant-changing command that must not report success until the change is on
/// disk. The `spawn` consumer's fire-and-forget emit could be lost on an
/// immediate quit/crash, resurrecting a just-revoked grant; awaiting this closes
/// that window. Idempotent with the consumer (both write the same snapshot); the
/// consumer stays as the backstop for non-command paths (IM / remote bus answers).
pub async fn flush(db: &Db, asks: &AskRegistry) {
    persist_snapshot(db, &asks.snapshot_grants()).await;
}

/// Install the persist consumer: mirror every grant change to the store. Called
/// once at startup (mirrors `trail::spawn`).
pub fn spawn(app: AppHandle) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<GrantSnapshot>();
    app.state::<AskRegistry>().set_persist_notifier(tx);
    let db = app.state::<Db>().inner().clone();
    tauri::async_runtime::spawn(async move {
        while let Some(snap) = rx.recv().await {
            persist_snapshot(&db, &snap).await;
        }
    });
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
                summary: "Run: npm test".into(),
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
        persist_snapshot(&db, &sample()).await;
        assert_eq!(load_snapshot(&db).await, sample());
    }

    #[tokio::test]
    async fn persist_empty_clears_a_previous_value() {
        let db = mem().await;
        persist_snapshot(&db, &sample()).await;
        persist_snapshot(&db, &GrantSnapshot::default()).await;
        assert!(load_snapshot(&db).await.is_empty());
    }

    #[tokio::test]
    async fn corrupt_value_loads_as_empty_not_a_panic() {
        let db = mem().await;
        repo::set_setting(&db, K_AUTH_GRANTS, "{ not json").await.unwrap();
        assert!(load_snapshot(&db).await.is_empty());
    }

    #[tokio::test]
    async fn seed_loads_persisted_grants_into_the_registry() {
        let db = mem().await;
        persist_snapshot(&db, &sample()).await;
        let asks = AskRegistry::new();
        seed(&db, &asks).await;
        assert_eq!(asks.auto_decision(1, "10", "anything"), Some(Decision::Allow));
        assert_eq!(
            asks.auto_decision(2, "", "Run: npm test"),
            Some(Decision::Allow)
        );
        assert!(asks.auto_decision(2, "", "Run: other").is_none());
    }

    /// Drains one snapshot from the registry's persist channel into the store,
    /// exactly as `spawn`'s consumer loop does — so these end-to-end tests prove
    /// the real wiring, not a hand-built blob.
    async fn drain_once(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<GrantSnapshot>,
        db: &Db,
    ) {
        let snap = rx.recv().await.expect("a grant change was emitted");
        persist_snapshot(db, &snap).await;
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
        let (id, _rx) = asks.request(7, "42", "codex", "Run: cargo build", "cargo build");
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

    /// Acceptance #3: revoking a thread's grants (issue deletion) clears them from
    /// the store, so a later reuse of that thread id starts un-granted.
    #[tokio::test]
    async fn revoking_a_thread_clears_its_persisted_grant() {
        let db = mem().await;
        let asks = AskRegistry::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(tx);
        let (id, _rx) = asks.request(7, "42", "codex", "Run: x", "x");
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
    async fn flush_durably_writes_current_grants_without_the_consumer() {
        let db = mem().await;
        let asks = AskRegistry::new();
        asks.seed_grants(sample());
        // No persist consumer installed — flush must write on its own (this is the
        // durability guarantee for grant-changing commands that await it).
        flush(&db, &asks).await;
        assert_eq!(load_snapshot(&db).await, sample());
        // a revoke then flush leaves only the surviving grant on disk
        asks.revoke_thread(1);
        flush(&db, &asks).await;
        let snap = load_snapshot(&db).await;
        assert!(snap.full.is_empty());
        assert_eq!(snap.always.len(), 1);
    }
}
