//! End-to-end backup check: configure → run_now → assert the bare remote
//! actually grew a commit and the backup_config row was updated.

use base64::Engine;
use std::process::Command;
use std::sync::Mutex;
use weft_app_lib::backup::{BackupService, config};
use weft_app_lib::store::Db;

// Integration tests share one process; serialize env mutations.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn iso_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    let raw = [0xCDu8; 48];
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
    std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
}

#[tokio::test]
async fn end_to_end_backup_creates_commit_in_bare_remote() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());

    let bare = tmp.path().join("remote.git");
    let status = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("--initial-branch=main")
        .arg(&bare)
        .status()
        .expect("git init --bare");
    assert!(status.success());
    let url = format!("file://{}", bare.to_string_lossy());

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

    let svc = BackupService::new(db.clone(), tmp.path().to_path_buf());
    let r = svc.run_now().await.unwrap();
    assert!(matches!(
        r,
        weft_app_lib::backup::RunOutcome::Success { .. }
    ));

    let out = Command::new("git")
        .current_dir(&bare)
        .arg("log")
        .arg("--oneline")
        .output()
        .unwrap();
    assert!(out.status.success(), "git log failed: {:?}", out);
    let log = String::from_utf8(out.stdout).unwrap();
    assert!(log.contains("snapshot"), "log = {log}");

    let cfg = config::load(&db).await.unwrap();
    assert!(cfg.last_backup_at.is_some());
    assert!(cfg.last_backup_commit_sha.is_some());
    assert!(cfg.last_error.is_none());
}
