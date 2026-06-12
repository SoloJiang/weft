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
        use sea_orm::ConnectionTrait;
        conn.execute_unprepared("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .await?;

        Migrator::up(&conn, None).await?;
        Ok(Db(conn))
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
