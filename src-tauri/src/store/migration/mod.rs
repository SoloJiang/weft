use crate::store::entities::{
    direction, direction_repo, repo_profile, repo_ref, session, thread, worktree, workspace,
};
use sea_orm::{EntityTrait, Schema};
use sea_orm_migration::prelude::*;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(M0001Init), Box::new(M0002RepoProfile)]
    }
}

pub struct M0001Init;

impl MigrationName for M0001Init {
    fn name(&self) -> &str {
        "m0001_init"
    }
}

impl M0001Init {
    /// Derive a CREATE TABLE statement from an entity, scoped to the backend.
    fn table<E: EntityTrait>(schema: &Schema, e: E) -> TableCreateStatement {
        let mut stmt = schema.create_table_from_entity(e);
        stmt.if_not_exists();
        stmt
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0001Init {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        manager.create_table(Self::table(&schema, workspace::Entity)).await?;
        manager.create_table(Self::table(&schema, repo_ref::Entity)).await?;
        manager.create_table(Self::table(&schema, thread::Entity)).await?;
        manager.create_table(Self::table(&schema, direction::Entity)).await?;
        manager.create_table(Self::table(&schema, direction_repo::Entity)).await?;
        manager.create_table(Self::table(&schema, worktree::Entity)).await?;
        manager.create_table(Self::table(&schema, session::Entity)).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for t in [
            "session", "worktree", "direction_repo", "direction", "thread", "repo_ref", "workspace",
        ] {
            manager
                .drop_table(Table::drop().table(Alias::new(t)).to_owned())
                .await?;
        }
        Ok(())
    }
}

/// Adds the curator's repo-profile table (ARCHITECTURE §4.9).
pub struct M0002RepoProfile;

impl MigrationName for M0002RepoProfile {
    fn name(&self) -> &str {
        "m0002_repo_profile"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0002RepoProfile {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(repo_profile::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("repo_profile")).to_owned())
            .await?;
        Ok(())
    }
}
