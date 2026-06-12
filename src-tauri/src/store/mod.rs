pub mod entities;
pub mod key;
pub mod legacy;
pub mod migration;
pub mod repo;

use migration::Migrator;
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

/// A connected, migrated database handle. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct Db(pub DatabaseConnection);

impl Db {
    /// Connect to any sqlite URL (e.g. "sqlite://<path>?mode=rwc" or
    /// "sqlite::memory:") without attaching a SQLCipher key. Kept for the
    /// in-memory tests and any caller that wants a plain sqlite handle.
    pub async fn connect(url: &str) -> Result<Self, sea_orm::DbErr> {
        let conn = Database::connect(url).await?;
        Migrator::up(&conn, None).await?;
        Ok(Db(conn))
    }

    /// Open `~/.weft/weft.db` as an encrypted SQLCipher database. The key is
    /// taken from (or minted into) the OS Keychain. Any pre-existing plaintext
    /// db is renamed aside — we do not migrate its data.
    pub async fn open_default() -> anyhow::Result<Self> {
        let path = crate::paths::db_path()?;
        crate::store::legacy::archive_if_plaintext(&path)?;

        let key = crate::store::key::get_or_create()?;
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());

        let mut opt = sea_orm::ConnectOptions::new(url);
        opt.sqlcipher_key(crate::store::key::format_for_pragma(&key));

        let conn = sea_orm::Database::connect(opt).await?;

        // SQLCipher-recommended pragmas; must run after the key is registered
        // (otherwise the connection is still locked and these statements fail).
        // Split into separate calls — sqlx-sqlite's execute path only runs the
        // first statement in a multi-statement string and silently drops the
        // rest, which would land `synchronous=NORMAL` quietly broken.
        use sea_orm::ConnectionTrait;
        conn.execute_unprepared("PRAGMA journal_mode=WAL;").await?;
        conn.execute_unprepared("PRAGMA synchronous=NORMAL;").await?;

        Migrator::up(&conn, None).await?;
        Ok(Db(conn))
    }

    /// Export the live SQLCipher database to `target`, encrypted with the same
    /// key. Uses SQLCipher's `sqlcipher_export` (ATTACH … KEY …; SELECT
    /// sqlcipher_export(…); DETACH …) to produce a transactionally consistent
    /// copy — safer than a raw file copy under WAL. Refuses to overwrite an
    /// existing file; if the export fails partway, the partial target is
    /// removed so the caller never sees a corrupt file.
    pub async fn snapshot_to(&self, target: &std::path::Path) -> anyhow::Result<()> {
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

        let key = crate::store::key::get_or_create()?;
        let key_lit = crate::store::key::format_for_pragma(&key);
        // Defensive escape: SQL string-literal-style single-quote doubling for
        // the file path. Normal paths don't contain single quotes; this guards
        // the edge.
        let target_str = target.to_string_lossy().replace('\'', "''");

        let attach = format!(
            "ATTACH DATABASE '{}' AS weft_snap KEY {};",
            target_str, key_lit
        );

        let r = async {
            self.0.execute_unprepared(&attach).await?;
            self.0
                .execute_unprepared("SELECT sqlcipher_export('weft_snap');")
                .await?;
            self.0
                .execute_unprepared("DETACH DATABASE weft_snap;")
                .await?;
            Ok::<(), sea_orm::DbErr>(())
        }
        .await;

        if let Err(e) = r {
            if target.exists() {
                let _ = std::fs::remove_file(target);
            }
            return Err(e.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connects_and_migrates_in_memory() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        // a table from the migration must exist
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared("SELECT id FROM workspace LIMIT 0")
            .await
            .unwrap();
    }
}
