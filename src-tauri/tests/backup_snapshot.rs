//! Integration tests for `Db::snapshot_to`. Uses WEFT_HOME + test key env to
//! isolate from the real desktop install and bypass the OS Keychain.

use base64::Engine;
use std::io::Read;
use std::sync::Mutex;

// Integration tests share one process & env; serialize env mutations.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn iso_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    let raw = [0x33u8; 48];
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
    std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
}

#[tokio::test]
async fn snapshot_produces_encrypted_copy_with_same_data() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    iso_env(tmp.path());

    use sea_orm::ConnectionTrait;
    let db = weft_app_lib::store::Db::open_default().await.unwrap();
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
    assert_ne!(
        &header[..],
        b"SQLite format 3\0",
        "snapshot must be encrypted"
    );

    // Re-open the snapshot with the same key and read the row back.
    let url = format!("sqlite://{}?mode=rw", snap.to_string_lossy());
    let mut opt = sea_orm::ConnectOptions::new(url);
    opt.sqlcipher_key(weft_app_lib::store::key::format_for_pragma(
        &weft_app_lib::store::key::get_or_create().unwrap(),
    ));
    let conn = sea_orm::Database::connect(opt).await.unwrap();
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
    let db = weft_app_lib::store::Db::open_default().await.unwrap();
    let target = tmp.path().join("collision.db");
    std::fs::write(&target, b"already here").unwrap();
    let err = db.snapshot_to(&target).await.err().expect("must error");
    assert!(err.to_string().contains("already exists"));
}
