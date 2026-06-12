//! Singleton backup_config row repo.

use anyhow::Result;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

use crate::store::Db;
use crate::store::entities::backup_config;

const SINGLETON_ID: i32 = 1;
const DEFAULT_INTERVAL_SECONDS: i64 = 3600;

fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Load the singleton row, materializing a default (disabled) row on first read.
pub async fn load(db: &Db) -> Result<backup_config::Model> {
    if let Some(m) = backup_config::Entity::find_by_id(SINGLETON_ID)
        .one(&db.0)
        .await?
    {
        return Ok(m);
    }
    let n = now();
    let m = backup_config::ActiveModel {
        id: Set(SINGLETON_ID),
        enabled: Set(false),
        remote_url: Set(String::new()),
        auto_backup_enabled: Set(true),
        interval_seconds: Set(DEFAULT_INTERVAL_SECONDS),
        backup_on_exit: Set(true),
        last_backup_at: Set(None),
        last_backup_commit_sha: Set(None),
        last_backup_bytes: Set(None),
        last_error: Set(None),
        created_at: Set(n.clone()),
        updated_at: Set(n),
    }
    .insert(&db.0)
    .await?;
    Ok(m)
}

#[derive(Debug, Clone)]
pub struct UpdatePrefs {
    pub enabled: bool,
    pub remote_url: String,
    pub auto_backup_enabled: bool,
    pub backup_on_exit: bool,
}

/// Update user-facing prefs only; preserves last_* status columns.
pub async fn save_prefs(db: &Db, p: UpdatePrefs) -> Result<backup_config::Model> {
    let cur = load(db).await?;
    let mut am: backup_config::ActiveModel = cur.into();
    am.enabled = Set(p.enabled);
    am.remote_url = Set(p.remote_url);
    am.auto_backup_enabled = Set(p.auto_backup_enabled);
    am.backup_on_exit = Set(p.backup_on_exit);
    am.updated_at = Set(now());
    Ok(am.update(&db.0).await?)
}

#[derive(Debug, Clone)]
pub struct BackupOutcome {
    pub commit_sha: String,
    pub bytes: i64,
}

pub async fn record_success(db: &Db, o: BackupOutcome) -> Result<()> {
    let cur = load(db).await?;
    let mut am: backup_config::ActiveModel = cur.into();
    am.last_backup_at = Set(Some(now()));
    am.last_backup_commit_sha = Set(Some(o.commit_sha));
    am.last_backup_bytes = Set(Some(o.bytes));
    am.last_error = Set(None);
    am.updated_at = Set(now());
    am.update(&db.0).await?;
    Ok(())
}

pub async fn record_failure(db: &Db, error_msg: &str) -> Result<()> {
    let cur = load(db).await?;
    let mut am: backup_config::ActiveModel = cur.into();
    am.last_error = Set(Some(error_msg.to_string()));
    am.updated_at = Set(now());
    am.update(&db.0).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_creates_default_singleton() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let m = load(&db).await.unwrap();
        assert_eq!(m.id, SINGLETON_ID);
        assert!(!m.enabled);
        assert_eq!(m.interval_seconds, DEFAULT_INTERVAL_SECONDS);
        assert!(m.auto_backup_enabled);
        assert!(m.backup_on_exit);
        assert!(m.last_backup_at.is_none());
    }

    #[tokio::test]
    async fn load_is_idempotent() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let a = load(&db).await.unwrap();
        let b = load(&db).await.unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(a.created_at, b.created_at);
    }

    #[tokio::test]
    async fn save_prefs_preserves_last_state() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        record_success(
            &db,
            BackupOutcome {
                commit_sha: "abc1234".into(),
                bytes: 42,
            },
        )
        .await
        .unwrap();
        save_prefs(
            &db,
            UpdatePrefs {
                enabled: true,
                remote_url: "git@host:r.git".into(),
                auto_backup_enabled: true,
                backup_on_exit: false,
            },
        )
        .await
        .unwrap();
        let m = load(&db).await.unwrap();
        assert!(m.enabled);
        assert_eq!(m.remote_url, "git@host:r.git");
        assert!(!m.backup_on_exit);
        assert_eq!(m.last_backup_commit_sha.as_deref(), Some("abc1234"));
        assert_eq!(m.last_backup_bytes, Some(42));
    }

    #[tokio::test]
    async fn record_failure_sets_error_keeps_last_success() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        record_success(
            &db,
            BackupOutcome {
                commit_sha: "sha1".into(),
                bytes: 10,
            },
        )
        .await
        .unwrap();
        record_failure(&db, "network").await.unwrap();
        let m = load(&db).await.unwrap();
        assert_eq!(m.last_error.as_deref(), Some("network"));
        assert_eq!(m.last_backup_commit_sha.as_deref(), Some("sha1"));
    }
}
