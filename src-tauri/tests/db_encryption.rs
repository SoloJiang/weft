//! Integration tests for the default open path:
//! - fresh open creates a plaintext DB
//! - reopen reads the same data back
//! - if the existing file is encrypted and the password env-bypass is set,
//!   `open_default` opens it with SQLCipher
//!
//! Full lifecycle (enable / change / disable) is covered by
//! `db_encryption_lifecycle.rs`.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn set_plain_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    std::env::remove_var("WEFT_TEST_DB_PASSWORD");
}

fn db_path(home: &std::path::Path) -> PathBuf {
    home.join("weft.db")
}

fn header_bytes(p: &std::path::Path) -> Vec<u8> {
    let mut buf = [0u8; 16];
    let n = std::fs::File::open(p)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap();
    buf[..n].to_vec()
}

#[tokio::test]
async fn open_default_creates_plaintext_db() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_plain_env(tmp.path());

    let db = weft::store::Db::open_default().await.unwrap();
    assert!(!db.encrypted(), "default open must be plaintext");

    let p = db_path(tmp.path());
    assert!(p.exists());
    let header = header_bytes(&p);
    assert_eq!(
        &header[..],
        b"SQLite format 3\0",
        "fresh DB must be plaintext SQLite"
    );

    use sea_orm::ConnectionTrait;
    let row = db
        .0
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "PRAGMA synchronous;".to_owned(),
        ))
        .await
        .unwrap()
        .expect("pragma returns row");
    let sync: i32 = row
        .try_get("", "synchronous")
        .unwrap_or_else(|_| row.try_get_by_index(0).unwrap());
    assert_eq!(sync, 1, "synchronous should be NORMAL (1)");
}

#[tokio::test]
async fn reopen_reads_existing_data() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_plain_env(tmp.path());

    use sea_orm::ConnectionTrait;
    let db1 = weft::store::Db::open_default().await.unwrap();
    db1.0
        .execute_unprepared(
            "INSERT INTO workspace (id, name, slug, created_at) \
             VALUES (1, 'roundtrip', 'roundtrip', '2026-06-12T00:00:00Z')",
        )
        .await
        .unwrap();
    drop(db1);

    let db2 = weft::store::Db::open_default().await.unwrap();
    let r = db2
        .0
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT name FROM workspace WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .expect("row exists");
    let name: String = r.try_get("", "name").unwrap();
    assert_eq!(name, "roundtrip");
}

#[tokio::test]
async fn open_default_errors_when_encrypted_but_no_password() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_plain_env(tmp.path());

    // Fake an encrypted file by writing non-SQLite magic bytes.
    let p = db_path(tmp.path());
    std::fs::write(&p, [0x99u8; 32]).unwrap();
    assert!(weft::store::Db::open_default().await.is_err());
}
