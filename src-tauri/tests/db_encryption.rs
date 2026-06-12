//! 集成测试：覆盖加密库创建、回开、旧明文库归档。
//! 用 WEFT_HOME + WEFT_TEST_DB_KEY_B64 隔离环境；不碰真实 ~/.weft 或 Keychain。

use base64::Engine;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

// integration tests share one process & one env; serialize env mutations.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn set_isolated_env(home: &std::path::Path) {
    std::env::set_var("WEFT_HOME", home);
    let raw = [0x42u8; 48];
    let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
    std::env::set_var("WEFT_TEST_DB_KEY_B64", &b64);
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
async fn open_default_creates_encrypted_db() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_isolated_env(tmp.path());

    let db = weft_app_lib::store::Db::open_default().await.unwrap();

    let p = db_path(tmp.path());
    assert!(p.exists(), "weft.db should be created");
    let header = header_bytes(&p);
    assert_ne!(
        &header[..],
        b"SQLite format 3\0",
        "encrypted db must NOT have plaintext magic"
    );

    // Verify PRAGMA synchronous=NORMAL took effect (guards against silent
    // multi-statement PRAGMA breakage).
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
async fn reopen_with_same_key_reads_existing_data() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_isolated_env(tmp.path());

    use sea_orm::ConnectionTrait;
    let db1 = weft_app_lib::store::Db::open_default().await.unwrap();
    db1.0
        .execute_unprepared(
            "INSERT INTO workspace (id, name, slug, created_at) \
             VALUES (1, 'roundtrip', 'roundtrip', '2026-06-12T00:00:00Z')",
        )
        .await
        .unwrap();
    drop(db1);

    let db2 = weft_app_lib::store::Db::open_default().await.unwrap();
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
async fn legacy_plaintext_is_archived() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_isolated_env(tmp.path());
    let p = db_path(tmp.path());

    std::fs::write(&p, b"SQLite format 3\0PLAINTEXT_PAYLOAD_HERE").unwrap();

    let _db = weft_app_lib::store::Db::open_default().await.unwrap();

    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries
            .iter()
            .any(|n| n.starts_with("weft.db.legacy-plaintext.")),
        "expected legacy archive in {entries:?}"
    );
    let header = header_bytes(&p);
    assert_ne!(&header[..], b"SQLite format 3\0");
}
