pub mod entities;
pub mod migration;
pub mod repo;

use migration::Migrator;
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

/// A connected, migrated database handle. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct Db(pub DatabaseConnection);

impl Db {
    /// Connect to a sqlite URL (e.g. "sqlite://<path>?mode=rwc" or
    /// "sqlite::memory:") and run migrations.
    pub async fn connect(url: &str) -> Result<Self, sea_orm::DbErr> {
        let conn = Database::connect(url).await?;
        Migrator::up(&conn, None).await?;
        Ok(Db(conn))
    }

    /// Connect to the on-disk weft db (~/.weft/weft.db).
    pub async fn open_default() -> anyhow::Result<Self> {
        let path = crate::paths::db_path()?;
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        Ok(Self::connect(&url).await?)
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
