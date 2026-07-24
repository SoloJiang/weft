//! Scheduler tests. Uses real local bare repos as the "remote" so the full
//! run_now pipeline gets exercised on tick.
//!
//! Waiting is event-driven: tests run `scheduler::run_loop` on the test
//! runtime and await its tick-completion signal instead of sleeping a fixed
//! wall-clock window. The old fixed 2s sleeps made these tests flaky under
//! machine load (e.g. parallel cargo builds): a tick does real git subprocess
//! work whose latency has no upper bound, so "the backup happened within N
//! seconds of spawn" is not a property the scheduler guarantees.

use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use weft::backup::{config, scheduler, BackupService};
use weft::store::Db;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Upper bound on waiting for the first completed scheduler tick. This is not
/// a timing assertion — the tick is awaited via the completion signal, so a
/// loaded machine just waits longer instead of failing. The cap only bounds
/// "the scheduler never fires", and it stays far below the 3600s interval
/// seeded by `idle_catchup_fires_immediately`, so a regression that sleeps a
/// full interval instead of catching up still fails deterministically.
const TICK_WAIT_CAP: Duration = Duration::from_secs(60);

fn iso_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    std::env::remove_var("WEFT_TEST_DB_PASSWORD");
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

/// Run the scheduler loop until its first completed tick (`run_now` returned
/// and its outcome is recorded in `backup_config`), then abort the loop and
/// return the resulting config row. Aborting keeps the loop from outliving
/// the test and spawning git subprocesses against a deleted tempdir.
async fn first_tick_config(
    svc: BackupService,
    db: &Db,
) -> weft::store::entities::backup_config::Model {
    let (tx, mut rx) = tokio::sync::watch::channel(0u64);
    let loop_task = tokio::spawn(scheduler::run_loop(svc, tx));
    let waited = tokio::time::timeout(TICK_WAIT_CAP, rx.changed()).await;
    loop_task.abort();
    waited
        .expect("scheduler completed no tick within TICK_WAIT_CAP")
        .expect("scheduler loop dropped the tick sender");
    config::load(db).await.unwrap()
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
    let mut am: weft::store::entities::backup_config::ActiveModel = m.into();
    am.enabled = Set(true);
    am.remote_url = Set(url);
    am.auto_backup_enabled = Set(true);
    am.interval_seconds = Set(1);
    am.update(&db.0).await.unwrap();

    let svc = BackupService::new(db.clone(), tmp.path().to_path_buf());
    let cfg = first_tick_config(svc, &db).await;

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
/// instead of sleeping `interval` more seconds. Asserted causally: with
/// `interval_seconds = 3600` and a stale stamp, the first tick must complete
/// within `TICK_WAIT_CAP` (≪ interval) and record a fresh `last_backup_at`
/// over the seeded one — a scheduler that slept a full interval would time
/// out, and one whose tick failed would leave the seeded stamp in place.
#[tokio::test]
async fn idle_catchup_fires_immediately() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());
    let db = Db::open_default().await.unwrap();
    let url = make_bare(tmp.path());

    use sea_orm::{ActiveModelTrait, Set};
    let m = config::load(&db).await.unwrap();
    let mut am: weft::store::entities::backup_config::ActiveModel = m.into();
    am.enabled = Set(true);
    am.remote_url = Set(url);
    am.auto_backup_enabled = Set(true);
    am.interval_seconds = Set(3600);
    am.last_backup_at = Set(Some("1000".into()));
    am.update(&db.0).await.unwrap();

    let svc = BackupService::new(db.clone(), tmp.path().to_path_buf());
    let cfg = first_tick_config(svc, &db).await;

    let last: u64 = cfg
        .last_backup_at
        .as_deref()
        .unwrap_or_default()
        .parse()
        .unwrap_or(0);
    assert!(
        last > 1000,
        "catch-up tick should record a fresh backup over the stale stamp; \
         last_backup_at={:?} last_error={:?}",
        cfg.last_backup_at,
        cfg.last_error
    );
}
