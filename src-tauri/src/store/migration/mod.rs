use crate::store::entities::{
    direction, lead_message, plan, repo_profile, repo_ref, session, thread, worktree, workspace,
};
use sea_orm::{EntityTrait, Schema};
use sea_orm_migration::prelude::*;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(M0001Init),
            Box::new(M0002RepoProfile),
            Box::new(M0003Plan),
            Box::new(M0004DirectionStatus),
            Box::new(M0005DirectionRepoReason),
            Box::new(M0006DropDirectionRepo),
            Box::new(M0007LeadMessage),
            Box::new(M0008DirectionMandate),
            Box::new(M0009DropThreadStatus),
        ]
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
        manager.create_table(Self::table(&schema, worktree::Entity)).await?;
        manager.create_table(Self::table(&schema, session::Entity)).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for t in [
            "session", "worktree", "direction", "thread", "repo_ref", "workspace",
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

/// Adds the per-thread plan/proposal table (ARCHITECTURE §4.10).
pub struct M0003Plan;

impl MigrationName for M0003Plan {
    fn name(&self) -> &str {
        "m0003_plan"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0003Plan {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(plan::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("plan")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds the agent/human-driven status column to directions (§4.6).
pub struct M0004DirectionStatus;

impl MigrationName for M0004DirectionStatus {
    fn name(&self) -> &str {
        "m0004_direction_status"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0004DirectionStatus {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // M0001 reflects the current entity, so a FRESH db already has `status`;
        // this migration only matters for dbs created before the column existed.
        // sqlite has no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .add_column(
                        ColumnDef::new(Alias::new("status"))
                            .string()
                            .not_null()
                            .default("queued"),
                    )
                    .to_owned(),
            )
            .await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .drop_column(Alias::new("status"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the single write-repo id + reason columns to directions (scope rework,
/// spec Part 1). M0001 reflects the current entity, so a FRESH db already has
/// both; this only matters for dbs created before the columns existed. sqlite
/// has no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
pub struct M0005DirectionRepoReason;

impl MigrationName for M0005DirectionRepoReason {
    fn name(&self) -> &str {
        "m0005_direction_repo_reason"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0005DirectionRepoReason {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for col in [
            ColumnDef::new(Alias::new("repo_id")).integer().not_null().default(0).to_owned(),
            ColumnDef::new(Alias::new("reason")).string().not_null().default("").to_owned(),
        ] {
            let r = manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("direction"))
                        .add_column(col)
                        .to_owned(),
                )
                .await;
            match r {
                Ok(()) => {}
                Err(e) if e.to_string().to_lowercase().contains("duplicate column") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for c in ["repo_id", "reason"] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("direction"))
                        .drop_column(Alias::new(c))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

/// Drops the now-unused direction_repo table (scope rework: a direction
/// binds a single repo via direction.repo_id). Fresh DBs never created it
/// (M0001 no longer does), so tolerate "no such table".
pub struct M0006DropDirectionRepo;

impl MigrationName for M0006DropDirectionRepo {
    fn name(&self) -> &str {
        "m0006_drop_direction_repo"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0006DropDirectionRepo {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .drop_table(Table::drop().table(Alias::new("direction_repo")).to_owned())
            .await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("no such table") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Irreversible: the table is gone for good. No-op.
        Ok(())
    }
}

/// Adds the worker-mandate column to directions (plan+impl | impl-only). M0001
/// reflects the current entity, so a FRESH db already has it; this only matters
/// for dbs created before the column existed. sqlite has no ADD COLUMN IF NOT
/// EXISTS, so tolerate the duplicate.
pub struct M0008DirectionMandate;

impl MigrationName for M0008DirectionMandate {
    fn name(&self) -> &str {
        "m0008_direction_mandate"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0008DirectionMandate {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .add_column(
                        ColumnDef::new(Alias::new("mandate"))
                            .string()
                            .not_null()
                            .default("plan+impl"),
                    )
                    .to_owned(),
            )
            .await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .drop_column(Alias::new("mandate"))
                    .to_owned(),
            )
            .await
    }
}

/// Drops the vestigial thread.status column: written once at insert ("active"),
/// never read or updated — the workspace board derives a thread's phase from
/// its directions. A FRESH db (M0001 reflects the entity) never has it; only
/// dbs created before the removal do, so tolerate the missing column.
pub struct M0009DropThreadStatus;

impl MigrationName for M0009DropThreadStatus {
    fn name(&self) -> &str {
        "m0009_drop_thread_status"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0009DropThreadStatus {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("thread"))
                    .drop_column(Alias::new("status"))
                    .to_owned(),
            )
            .await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("no such column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Irreversible: the dead column is gone for good. No-op.
        Ok(())
    }
}

/// Adds the chat timeline table for the lead console (and chat-mode workers).
pub struct M0007LeadMessage;

impl MigrationName for M0007LeadMessage {
    fn name(&self) -> &str {
        "m0007_lead_message"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0007LeadMessage {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(lead_message::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_lead_message_thread")
                    .table(Alias::new("lead_message"))
                    .col(Alias::new("thread_id"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("lead_message")).to_owned())
            .await?;
        Ok(())
    }
}
