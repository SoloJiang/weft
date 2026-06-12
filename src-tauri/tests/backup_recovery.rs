//! Restore round-trip: back the db up, lose the local file, restore from
//! the remote, prove the data is back.

use base64::Engine;
use std::process::Command;
use std::sync::Mutex;
use weft_app_lib::backup::{BackupService, config, recovery_key};
use weft_app_lib::store::Db;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn iso_env_with(home: &std::path::Path, key: [u8; 48]) {
    std::env::set_var("WEFT_HOME", home);
    let b64 = base64::engine::general_purpose::STANDARD.encode(key);
    std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
}

fn make_bare(parent: &std::path::Path) -> String {
    let bare = parent.join("remote.git");
    let s = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("--initial-branch=main")
        .arg(&bare)
        .status()
        .unwrap();
    assert!(s.success());
    format!("file://{}", bare.to_string_lossy())
}

#[tokio::test]
async fn backup_then_restore_roundtrip() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    iso_env_with(&home, [0xEDu8; 48]);

    let url = make_bare(tmp.path());

    {
        let db = Db::open_default().await.unwrap();
        config::save_prefs(
            &db,
            config::UpdatePrefs {
                enabled: true,
                remote_url: url.clone(),
                auto_backup_enabled: false,
                backup_on_exit: false,
            },
        )
        .await
        .unwrap();
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared(
            "INSERT INTO workspace (id, name, slug, created_at) \
             VALUES (1, 'restore-me', 'restore-me', '1234567890')",
        )
        .await
        .unwrap();
        let svc = BackupService::new(db.clone(), home.clone());
        let r = svc.run_now().await.unwrap();
        assert!(matches!(
            r,
            weft_app_lib::backup::RunOutcome::Success { .. }
        ));
    }

    let rk_path = tmp.path().join("rk.json");
    recovery_key::export_to(&rk_path).unwrap();

    // Simulate "new machine": wipe the local db file so restore_from accepts
    // the operation.
    std::fs::remove_file(home.join("weft.db")).unwrap();
    // Also remove WAL/journal sidecars if SQLCipher left any.
    let _ = std::fs::remove_file(home.join("weft.db-wal"));
    let _ = std::fs::remove_file(home.join("weft.db-shm"));

    // restore_from only touches files + Keychain; the Db handle it holds is
    // unused, so a throwaway in-memory db is fine.
    let svc = {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        BackupService::new(db, home.clone())
    };
    svc.restore_from(&url, &rk_path).await.unwrap();

    let db = Db::open_default().await.unwrap();
    use sea_orm::ConnectionTrait;
    let row = db
        .0
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT name FROM workspace WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .expect("row exists");
    let name: String = row.try_get("", "name").unwrap();
    assert_eq!(name, "restore-me");
}

#[tokio::test]
async fn restore_refuses_when_db_exists() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env_with(tmp.path(), [0x12u8; 48]);
    let db = Db::open_default().await.unwrap();
    let svc = BackupService::new(db, tmp.path().to_path_buf());
    let rk = tmp.path().join("rk.json");
    std::fs::write(&rk, b"{}").unwrap();
    let err = svc
        .restore_from("file:///nonexistent", &rk)
        .await
        .err()
        .expect("must error");
    assert!(
        err.to_string().contains("already exists"),
        "got: {err:#}"
    );
}
