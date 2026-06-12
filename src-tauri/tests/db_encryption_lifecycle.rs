//! Lifecycle tests for DB encryption: plaintext → enable → change → disable.
//! Uses the `WEFT_TEST_DB_PASSWORD` env bypass so the OS Keychain is never
//! touched; serializes against sibling integration tests through ENV_LOCK.

use std::io::Read;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn fresh_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    std::env::set_var("WEFT_TEST_DB_PASSWORD", "test-db-password");
}

fn header(p: &std::path::Path) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let n = std::fs::File::open(p)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap();
    assert_eq!(n, 16);
    buf
}

async fn seed_marker(db: &weft::store::Db, marker: &str) {
    use sea_orm::ConnectionTrait;
    db.0.execute_unprepared(&format!(
        "INSERT INTO workspace (id, name, slug, created_at) \
         VALUES (1, '{marker}', '{marker}', '2026-06-12T00:00:00Z')"
    ))
    .await
    .unwrap();
}

async fn read_marker(db: &weft::store::Db) -> String {
    use sea_orm::ConnectionTrait;
    let row = db
        .0
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT name FROM workspace WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .expect("row");
    row.try_get("", "name").unwrap()
}

#[tokio::test]
async fn full_lifecycle_plain_enable_change_disable() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    fresh_env(tmp.path());
    let path = weft::paths::db_path().unwrap();

    // 1. Fresh open is plaintext.
    let db = weft::store::Db::open_default().await.unwrap();
    assert!(!db.encrypted());
    seed_marker(&db, "lifecycle").await;
    drop(db);
    assert_eq!(&header(&path)[..], b"SQLite format 3\0");

    // 2. Enable encryption with password "first".
    weft::store::encryption::enable(&path, "first").await.unwrap();
    assert_ne!(&header(&path)[..], b"SQLite format 3\0");

    // Reopen: password is now in the env-bypass; data must round-trip.
    let db = weft::store::Db::open_default().await.unwrap();
    assert!(db.encrypted());
    assert_eq!(read_marker(&db).await, "lifecycle");
    drop(db);

    // 3. Change the password.
    weft::store::encryption::change_password(&path, "first", "second")
        .await
        .unwrap();
    let db = weft::store::Db::open_default().await.unwrap();
    assert!(db.encrypted());
    assert_eq!(read_marker(&db).await, "lifecycle");
    drop(db);

    // 4. Disable: file goes back to plaintext magic; env-bypass is cleared.
    weft::store::encryption::disable(&path, "second").await.unwrap();
    assert_eq!(&header(&path)[..], b"SQLite format 3\0");
    assert!(std::env::var("WEFT_TEST_DB_PASSWORD").is_err());
    let db = weft::store::Db::open_default().await.unwrap();
    assert!(!db.encrypted());
    assert_eq!(read_marker(&db).await, "lifecycle");
}

#[tokio::test]
async fn enable_rejects_empty_password() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    fresh_env(tmp.path());
    let path = weft::paths::db_path().unwrap();
    weft::store::Db::open_default().await.unwrap();
    let err = weft::store::encryption::enable(&path, "")
        .await
        .err()
        .expect("empty password must fail");
    assert!(err.to_string().contains("password"));
}

#[tokio::test]
async fn change_password_with_wrong_old_fails() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    fresh_env(tmp.path());
    let path = weft::paths::db_path().unwrap();
    weft::store::Db::open_default().await.unwrap();
    weft::store::encryption::enable(&path, "right").await.unwrap();
    let err = weft::store::encryption::change_password(&path, "WRONG", "new")
        .await
        .err()
        .expect("wrong password must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.to_ascii_lowercase().contains("password")
            || msg.to_ascii_lowercase().contains("file is not"),
        "unexpected error: {msg}"
    );
    // Cleanup: leaving WEFT_TEST_DB_PASSWORD set could break later tests.
    std::env::remove_var("WEFT_TEST_DB_PASSWORD");
}

#[tokio::test]
async fn disable_with_wrong_password_fails() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    fresh_env(tmp.path());
    let path = weft::paths::db_path().unwrap();
    weft::store::Db::open_default().await.unwrap();
    weft::store::encryption::enable(&path, "real").await.unwrap();
    let err = weft::store::encryption::disable(&path, "wrong")
        .await
        .err()
        .expect("wrong password must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.to_ascii_lowercase().contains("password")
            || msg.to_ascii_lowercase().contains("file is not"),
        "unexpected error: {msg}"
    );
    std::env::remove_var("WEFT_TEST_DB_PASSWORD");
}
