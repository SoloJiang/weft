pub mod encryption;
pub mod entities;
pub mod key;
pub mod migration;
pub mod repo;

use migration::Migrator;
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::io::Read;
use std::path::Path;

const PLAINTEXT_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// A connected, migrated database handle. Cheap to clone (Arc inside).
///
/// The bool records whether this connection opened a SQLCipher file (so
/// snapshot_to / encryption ops can pick the right path). The first field
/// stays as `.0` to keep every existing caller untouched — tons of code does
/// `db.0.execute_unprepared(...)` and migrating that all at once is churn we
/// don't need.
#[derive(Clone)]
pub struct Db(pub DatabaseConnection, pub bool);

impl Db {
    /// Whether this handle is talking to a SQLCipher-encrypted file.
    pub fn encrypted(&self) -> bool {
        self.1
    }

    /// Connect to any sqlite URL without SQLCipher. Used by in-memory tests
    /// and any caller that explicitly wants a plain handle.
    pub async fn connect(url: &str) -> Result<Self, sea_orm::DbErr> {
        let conn = Database::connect(url).await?;
        Migrator::up(&conn, None).await?;
        Ok(Db(conn, false))
    }

    /// Open `~/.weft/weft.db`. Default is plaintext (no encryption). If the
    /// existing file's first 16 bytes do NOT match the plaintext SQLite magic,
    /// we treat it as SQLCipher and require a password from the Keychain.
    /// If no password is stored we surface an explicit error — there is no
    /// in-app unlock prompt; the user manages this from Settings.
    pub async fn open_default() -> anyhow::Result<Self> {
        let path = crate::paths::db_path()?;
        let want_encrypted = detect_encrypted(&path)?;

        if want_encrypted {
            let pwd = crate::store::key::get_password()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "weft.db is encrypted but no password is stored in the keychain. \
                     Set the password from Settings → General → Security, then restart Weft."
                )
            })?;
            open_encrypted(&path, &pwd).await
        } else {
            open_plaintext(&path).await
        }
    }

    /// Export the live database to `target` (must not already exist). The
    /// snapshot mirrors the source encryption: SQLCipher source ⇒ encrypted
    /// snapshot via `sqlcipher_export`, plaintext source ⇒ `VACUUM INTO`. If
    /// anything fails partway, the partial target is removed.
    pub async fn snapshot_to(&self, target: &Path) -> anyhow::Result<()> {
        use sea_orm::ConnectionTrait;

        if target.exists() {
            return Err(anyhow::anyhow!(
                "snapshot target already exists: {}",
                target.display()
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let target_str = target.to_string_lossy().replace('\'', "''");

        let r = if self.1 {
            let pwd = crate::store::key::get_password()?.ok_or_else(|| {
                anyhow::anyhow!("snapshot: encrypted db but no password in keychain")
            })?;
            let key_lit = crate::store::key::pragma_literal(&pwd);
            let attach = format!(
                "ATTACH DATABASE '{}' AS weft_snap KEY {};",
                target_str, key_lit
            );
            async {
                self.0.execute_unprepared(&attach).await?;
                self.0
                    .execute_unprepared("SELECT sqlcipher_export('weft_snap');")
                    .await?;
                self.0
                    .execute_unprepared("DETACH DATABASE weft_snap;")
                    .await?;
                Ok::<(), sea_orm::DbErr>(())
            }
            .await
        } else {
            let stmt = format!("VACUUM INTO '{}';", target_str);
            self.0.execute_unprepared(&stmt).await.map(|_| ())
        };

        if let Err(e) = r {
            if target.exists() {
                let _ = std::fs::remove_file(target);
            }
            return Err(e.into());
        }
        Ok(())
    }
}

/// Sniff the first 16 bytes of `path`. Missing / empty file ⇒ not encrypted
/// (we'll create a fresh plaintext db). Mismatch with the SQLite magic ⇒
/// treat as SQLCipher.
pub(crate) fn detect_encrypted(path: &Path) -> anyhow::Result<bool> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut buf = [0u8; 16];
    let n = f.read(&mut buf)?;
    if n < 16 {
        // empty file from a half-init we'd otherwise crash on — treat plaintext
        return Ok(false);
    }
    Ok(&buf != PLAINTEXT_MAGIC)
}

async fn open_plaintext(path: &Path) -> anyhow::Result<Db> {
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
    let conn = sea_orm::Database::connect(url).await?;
    use sea_orm::ConnectionTrait;
    conn.execute_unprepared("PRAGMA journal_mode=WAL;").await?;
    conn.execute_unprepared("PRAGMA synchronous=NORMAL;")
        .await?;
    Migrator::up(&conn, None).await?;
    Ok(Db(conn, false))
}

async fn open_encrypted(path: &Path, password: &str) -> anyhow::Result<Db> {
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
    let mut opt = sea_orm::ConnectOptions::new(url);
    opt.sqlcipher_key(crate::store::key::pragma_literal(password));
    let conn = sea_orm::Database::connect(opt).await?;
    use sea_orm::ConnectionTrait;
    conn.execute_unprepared("PRAGMA journal_mode=WAL;").await?;
    conn.execute_unprepared("PRAGMA synchronous=NORMAL;")
        .await?;
    Migrator::up(&conn, None).await?;
    Ok(Db(conn, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_and_migrates_in_memory() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared("SELECT id FROM workspace LIMIT 0")
            .await
            .unwrap();
    }

    #[test]
    fn detect_encrypted_handles_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("does-not-exist.db");
        assert!(!detect_encrypted(&p).unwrap());
    }

    #[test]
    fn detect_encrypted_recognises_plaintext_magic() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("plain.db");
        std::fs::write(&p, b"SQLite format 3\0rest-of-file-payload-here").unwrap();
        assert!(!detect_encrypted(&p).unwrap());
    }

    #[test]
    fn detect_encrypted_recognises_cipher_header() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("cipher.db");
        std::fs::write(&p, [0xA1u8; 64]).unwrap();
        assert!(detect_encrypted(&p).unwrap());
    }
}
