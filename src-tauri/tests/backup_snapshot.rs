//! Integration tests for `Db::snapshot_to`. Default DB is plaintext; the
//! encrypted code path is covered by `db_encryption_lifecycle.rs`.

use std::io::Read;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn iso_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    std::env::remove_var("WEFT_TEST_DB_PASSWORD");
}

#[tokio::test]
async fn snapshot_produces_plaintext_copy_with_same_data() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());

    use sea_orm::ConnectionTrait;
    let db = weft::store::Db::open_default().await.unwrap();
    assert!(!db.encrypted(), "default open must be plaintext");
    db.0.execute_unprepared(
        "INSERT INTO workspace (id, name, slug, created_at) \
         VALUES (1, 'snap-test', 'snap-test', '1234567890')",
    )
    .await
    .unwrap();

    let snap = tmp.path().join("snap.db");
    db.snapshot_to(&snap).await.unwrap();

    assert!(snap.exists());
    let mut header = [0u8; 16];
    let n = std::fs::File::open(&snap)
        .and_then(|mut f| f.read(&mut header))
        .unwrap();
    assert_eq!(n, 16);
    assert_eq!(
        &header[..],
        b"SQLite format 3\0",
        "plaintext snapshot must have the standard SQLite magic"
    );

    let url = format!("sqlite://{}?mode=rw", snap.to_string_lossy());
    let conn = sea_orm::Database::connect(url).await.unwrap();
    let row = conn
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Sqlite,
            "SELECT name FROM workspace WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .expect("row");
    let name: String = row.try_get("", "name").unwrap();
    assert_eq!(name, "snap-test");
}

#[tokio::test]
async fn snapshot_rejects_existing_target() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());
    let db = weft::store::Db::open_default().await.unwrap();
    let target = tmp.path().join("collision.db");
    std::fs::write(&target, b"already here").unwrap();
    let err = db.snapshot_to(&target).await.err().expect("must error");
    assert!(err.to_string().contains("already exists"));
}
