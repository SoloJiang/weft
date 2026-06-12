//! Scheduler tests. Uses real local bare repos as the "remote" so the full
//! run_now pipeline gets exercised on tick.

use base64::Engine;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use weft_app_lib::backup::{BackupService, config, scheduler};
use weft_app_lib::store::Db;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn iso_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    let raw = [0xBEu8; 48];
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
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
async fn scheduler_fires_at_least_once_when_interval_short() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());
    let db = Db::open_default().await.unwrap();
    let url = make_bare(tmp.path());

    use sea_orm::{ActiveModelTrait, Set};
    let m = config::load(&db).await.unwrap();
    let mut am: weft_app_lib::store::entities::backup_config::ActiveModel = m.into();
    am.enabled = Set(true);
    am.remote_url = Set(url);
    am.auto_backup_enabled = Set(true);
    am.interval_seconds = Set(1);
    am.update(&db.0).await.unwrap();

    let svc = BackupService::new(db.clone(), tmp.path().to_path_buf());
    scheduler::spawn(svc);

    tokio::time::sleep(Duration::from_secs(2)).await;

    let cfg = config::load(&db).await.unwrap();
    assert!(
        cfg.last_backup_at.is_some(),
        "expected at least one successful backup; last_error={:?}",
        cfg.last_error
    );
    assert!(cfg.last_error.is_none(), "last_error = {:?}", cfg.last_error);
}

#[tokio::test]
async fn run_on_exit_no_op_when_disabled() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());
    let db = Db::open_default().await.unwrap();
    let svc = BackupService::new(db.clone(), tmp.path().to_path_buf());
    scheduler::run_on_exit(&svc).await;
    let cfg = config::load(&db).await.unwrap();
    assert!(cfg.last_backup_at.is_none());
    assert!(cfg.last_error.is_none());
}

/// Regression: if Weft was closed during the interval and the next-due time
/// is already in the past on relaunch, the scheduler should fire immediately
/// instead of sleeping `interval` more seconds.
#[tokio::test]
async fn idle_catchup_fires_immediately() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());
    let db = Db::open_default().await.unwrap();
    let url = make_bare(tmp.path());

    use sea_orm::{ActiveModelTrait, Set};
    let m = config::load(&db).await.unwrap();
    let mut am: weft_app_lib::store::entities::backup_config::ActiveModel = m.into();
    am.enabled = Set(true);
    am.remote_url = Set(url);
    am.auto_backup_enabled = Set(true);
    am.interval_seconds = Set(3600);
    am.last_backup_at = Set(Some("1000".into()));
    am.update(&db.0).await.unwrap();

    let svc = BackupService::new(db.clone(), tmp.path().to_path_buf());
    scheduler::spawn(svc);

    tokio::time::sleep(Duration::from_secs(2)).await;

    let cfg = config::load(&db).await.unwrap();
    assert!(cfg.last_backup_at.is_some());
    let last: u64 = cfg.last_backup_at.unwrap().parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(
        now - last < 5,
        "last_backup_at should be recent: now={now} last={last}"
    );
}
