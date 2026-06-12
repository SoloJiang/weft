//! Materializes a snapshot pair (`weft.db` + `.weft-backup-meta.json`) into a
//! staging directory ready for `git add -A`.

use anyhow::Result;
use std::path::Path;

use crate::store::Db;

const META_NAME: &str = ".weft-backup-meta.json";
pub const SNAPSHOT_NAME: &str = "weft.db";

/// Write `staging_dir/weft.db` (overwrite) + `staging_dir/.weft-backup-meta.json`
/// (overwrite). `snapshot_to` insists the target file not exist, so we delete
/// the previous snapshot first. Returns the snapshot byte count.
pub async fn write_snapshot(db: &Db, staging_dir: &Path) -> Result<i64> {
    std::fs::create_dir_all(staging_dir)?;
    let snap = staging_dir.join(SNAPSHOT_NAME);
    if snap.exists() {
        std::fs::remove_file(&snap)?;
    }
    db.snapshot_to(&snap).await?;

    let bytes = std::fs::metadata(&snap)?.len() as i64;
    let meta = serde_json::json!({
        "schema_version": current_schema_version(),
        "snapshot_at": now_unix_secs(),
        "db_bytes": bytes,
        "weft_version": env!("CARGO_PKG_VERSION"),
    });
    std::fs::write(staging_dir.join(META_NAME), serde_json::to_vec_pretty(&meta)?)?;
    Ok(bytes)
}

/// Schema version = number of registered migrations. Bumps by 1 every time we
/// land a new M00NN. Restore uses this to refuse a backup written by a newer
/// Weft.
fn current_schema_version() -> usize {
    use sea_orm_migration::MigratorTrait;
    crate::store::migration::Migrator::migrations().len()
}

fn now_unix_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn iso_env(home: &std::path::Path) {
        std::env::set_var("WEFT_HOME", home);
        let raw = [0x55u8; 48];
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
    }

    #[tokio::test]
    async fn writes_snapshot_and_meta() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        iso_env(tmp.path());
        let db = Db::open_default().await.unwrap();

        let staging = tmp.path().join("staging");
        let bytes = write_snapshot(&db, &staging).await.unwrap();
        assert!(bytes > 0);

        let snap = staging.join(SNAPSHOT_NAME);
        let meta_path = staging.join(META_NAME);
        assert!(snap.exists());
        assert!(meta_path.exists());

        let meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        assert!(meta["schema_version"].as_u64().unwrap() > 0);
        assert_eq!(meta["db_bytes"].as_i64().unwrap(), bytes);
    }

    #[tokio::test]
    async fn overwrites_previous_snapshot() {
        let _g = crate::backup::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        iso_env(tmp.path());
        let db = Db::open_default().await.unwrap();
        let staging = tmp.path().join("staging");
        write_snapshot(&db, &staging).await.unwrap();
        write_snapshot(&db, &staging).await.unwrap();
    }
}
