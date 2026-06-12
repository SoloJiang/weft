//! Git-remote backup of the local SQLCipher database.
//!
//! - `config`: singleton backup_config repo
//! - `snapshot`: writes `weft.db` + meta json to a staging dir
//! - `git_remote`: shells out to the system `git` CLI
//! - `recovery_key`: Recovery Key file format
//! - `scheduler`: hourly tick + on-exit hook
//!
//! Design: `DESIGN-2026-06-12-local-db-backup.md`.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::store::Db;

pub mod config;
pub mod git_remote;
pub mod recovery_key;
pub mod scheduler;
pub mod snapshot;

// Shared env-mutation lock for in-process backup tests. Every submodule that
// pokes `WEFT_HOME` / `WEFT_TEST_DB_KEY_B64` must take this same mutex, or
// parallel tests will trample each other mid-`open_default`.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// App-level backup handle. Held in Tauri state; scheduler and commands both
/// share the same instance so they cannot race.
#[derive(Clone)]
pub struct BackupService {
    db: Db,
    home: PathBuf,
    lock: Arc<Mutex<()>>,
}

#[derive(Debug)]
pub enum RunOutcome {
    Disabled,
    Success { commit_sha: String, bytes: i64 },
}

impl BackupService {
    pub fn new(db: Db, home: PathBuf) -> Self {
        Self {
            db,
            home,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Trigger one backup. Failures are recorded into `backup_config.last_error`
    /// and surfaced as `Err`; we never panic. Serialized by `lock` so a
    /// scheduler tick can't collide with a manual `run_now`.
    pub async fn run_now(&self) -> Result<RunOutcome> {
        let _guard = self.lock.lock().await;
        let cfg = config::load(&self.db).await?;
        if !cfg.enabled || cfg.remote_url.is_empty() {
            return Ok(RunOutcome::Disabled);
        }

        let result = self.do_backup(&cfg.remote_url).await;
        match &result {
            Ok((sha, bytes)) => {
                config::record_success(
                    &self.db,
                    config::BackupOutcome {
                        commit_sha: sha.clone(),
                        bytes: *bytes,
                    },
                )
                .await?;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let _ = config::record_failure(&self.db, &msg).await;
            }
        }
        let (sha, bytes) = result?;
        Ok(RunOutcome::Success {
            commit_sha: sha,
            bytes,
        })
    }

    async fn do_backup(&self, remote_url: &str) -> Result<(String, i64)> {
        git_remote::ensure_git_available()?;
        let staging = self.staging_dir(remote_url);
        git_remote::ensure_clone(&staging, remote_url)?;

        let bytes = snapshot::write_snapshot(&self.db, &staging).await?;

        let msg = format!("snapshot at {}", unix_now());
        let report = git_remote::commit_and_push(&staging, &msg)?;
        // `bytes_pushed` is a rough wd-bytes sum; fall back to snapshot size
        // if for some reason it came back zero.
        Ok((report.commit_sha, report.bytes_pushed.max(bytes)))
    }

    /// Deterministic per-remote staging path under `<home>/backup/<sha1[..8]>`.
    /// Same URL → same dir, so repeat backups reuse the clone instead of
    /// re-cloning each tick.
    pub fn staging_dir(&self, remote_url: &str) -> PathBuf {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(remote_url.as_bytes());
        let digest = hex::encode(h.finalize());
        self.home.join("backup").join(&digest[..8])
    }

    /// Pull a backup snapshot down from `remote_url`, validate it against this
    /// build's schema, and lay `weft.db` back into `<home>/weft.db`. Also
    /// writes the key in `recovery_key_path` back into the Keychain so the
    /// next `Db::open_default` can decrypt it.
    ///
    /// Safety: refuses to run when `<home>/weft.db` already exists — restore
    /// is a destructive operation; the caller must move/delete the old db
    /// deliberately. The caller is expected to prompt the user to restart
    /// Weft after this succeeds.
    pub async fn restore_from(
        &self,
        remote_url: &str,
        recovery_key_path: &std::path::Path,
    ) -> Result<()> {
        let db_path = self.home.join("weft.db");
        if db_path.exists() {
            return Err(anyhow::anyhow!(
                "refusing to restore: {} already exists; remove it first",
                db_path.display()
            ));
        }

        // Writes back to Keychain (or no-ops under the test env bypass).
        let _imported = recovery_key::import_from(recovery_key_path)?;

        let tmp = self.home.join("backup-restore-tmp");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)?;
        }
        git_remote::clone_to(&tmp, remote_url)?;

        let meta_path = tmp.join(".weft-backup-meta.json");
        let meta_bytes = std::fs::read(&meta_path)
            .map_err(|e| anyhow::anyhow!("read backup meta {}: {e}", meta_path.display()))?;
        let meta: serde_json::Value = serde_json::from_slice(&meta_bytes)?;
        let backup_version = meta["schema_version"].as_u64().unwrap_or(0) as usize;
        use sea_orm_migration::MigratorTrait;
        let current_version = crate::store::migration::Migrator::migrations().len();
        if backup_version > current_version {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(anyhow::anyhow!(
                "backup schema {backup_version} is newer than this Weft ({current_version}); upgrade Weft first"
            ));
        }

        let snap = tmp.join("weft.db");
        if !snap.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(anyhow::anyhow!(
                "backup repo missing weft.db: {}",
                snap.display()
            ));
        }
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&snap, &db_path)?;
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(())
    }
}

fn unix_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn iso_env(home: &std::path::Path) {
        std::env::set_var("WEFT_HOME", home);
        let raw = [0xA1u8; 48];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
    }

    #[tokio::test]
    async fn run_now_returns_disabled_when_unconfigured() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        iso_env(tmp.path());
        let db = Db::open_default().await.unwrap();
        let svc = BackupService::new(db, tmp.path().to_path_buf());
        let r = svc.run_now().await.unwrap();
        assert!(matches!(r, RunOutcome::Disabled));
    }

    #[tokio::test]
    async fn staging_dir_is_deterministic_per_url() {
        let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        iso_env(tmp.path());
        let db = Db::open_default().await.unwrap();
        let svc = BackupService::new(db, tmp.path().to_path_buf());
        let a = svc.staging_dir("git@host:r.git");
        let b = svc.staging_dir("git@host:r.git");
        let c = svc.staging_dir("git@host:other.git");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with(tmp.path().join("backup")));
    }
}
