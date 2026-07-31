use crate::store::entities::{
    app_setting, backup_config, code_checkpoint, direction, im_route, lead_message, plan,
    pull_request, repo_profile, repo_ref, session, skill_enable, skill_source, test_plan, thread,
    workspace, worktree,
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
            Box::new(M0010AppSetting),
            Box::new(M0011ThreadLeadTool),
            Box::new(M0012DropRepoDefaultTool),
            Box::new(M0013SkillSource),
            Box::new(M0014SkillEnable),
            Box::new(M0015ImRoute),
            Box::new(M0016BackupConfig),
            Box::new(M0017SessionStatusReset),
            Box::new(M0018DirectionTargetBranch),
            Box::new(M0019ThreadLeadCommand),
            Box::new(M0020SessionCommand),
            Box::new(M0021RepoRemoteUrl),
            Box::new(M0022RepoProfileRelations),
            Box::new(M0023RepoProfileComponents),
            Box::new(M0024DirectionBaseBranch),
            Box::new(M0025WorktreeCreatedBranch),
            Box::new(M0026WorktreeCreatedCheckout),
            Box::new(M0027RepoRefBaseRefIsDefault),
            Box::new(M0028WorktreeBaseCommit),
            Box::new(M0029GatewayToBackend),
            Box::new(M0030AnalysisState),
            Box::new(M0031RepoCategoryDomains),
            Box::new(M0032LeadMessageSeq),
            Box::new(M0033RepoLayerRank),
            Box::new(M0034SessionMetaSnapshot),
            Box::new(M0035TestPlan),
            Box::new(M0036LeadMessageNativeAnchor),
            Box::new(M0037CodeCheckpoint),
            Box::new(M0038CodeCheckpointNestedRepos),
            Box::new(M0039CodeCheckpointIndexTree),
            Box::new(M0040LeadMessageConsumedAt),
            Box::new(M0041LeadMessageThreadKindIdx),
            Box::new(M0042ThreadLeadModel),
            Box::new(M0043SessionModel),
            Box::new(M0044EngineRoutingPin),
            Box::new(M0045PullRequest),
            Box::new(M0046DirectionUpstream),
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
        manager
            .create_table(Self::table(&schema, workspace::Entity))
            .await?;
        manager
            .create_table(Self::table(&schema, repo_ref::Entity))
            .await?;
        manager
            .create_table(Self::table(&schema, thread::Entity))
            .await?;
        manager
            .create_table(Self::table(&schema, direction::Entity))
            .await?;
        manager
            .create_table(Self::table(&schema, worktree::Entity))
            .await?;
        manager
            .create_table(Self::table(&schema, session::Entity))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for t in [
            "session",
            "worktree",
            "direction",
            "thread",
            "repo_ref",
            "workspace",
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
            ColumnDef::new(Alias::new("repo_id"))
                .integer()
                .not_null()
                .default(0)
                .to_owned(),
            ColumnDef::new(Alias::new("reason"))
                .string()
                .not_null()
                .default("")
                .to_owned(),
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

/// Adds the global key-value settings table (default-tool selection).
pub struct M0010AppSetting;

impl MigrationName for M0010AppSetting {
    fn name(&self) -> &str {
        "m0010_app_setting"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0010AppSetting {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(app_setting::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("app_setting")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds thread.lead_tool (the CLI driving the thread's lead), stamped at
/// creation. Existing threads were always claude-led, so backfill "claude".
/// M0001 reflects the current entity, so a FRESH db already has the column;
/// sqlite has no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
pub struct M0011ThreadLeadTool;

impl MigrationName for M0011ThreadLeadTool {
    fn name(&self) -> &str {
        "m0011_thread_lead_tool"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0011ThreadLeadTool {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("thread"))
                    .add_column(
                        ColumnDef::new(Alias::new("lead_tool"))
                            .string()
                            .not_null()
                            .default("claude"),
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
                    .table(Alias::new("thread"))
                    .drop_column(Alias::new("lead_tool"))
                    .to_owned(),
            )
            .await
    }
}

/// Drops the dead repo_ref.default_tool column: written once at registration
/// ("claude"), never read — tool selection is now app_setting + per-card. A
/// FRESH db (M0001 reflects the entity) never has it, so tolerate the miss.
pub struct M0012DropRepoDefaultTool;

impl MigrationName for M0012DropRepoDefaultTool {
    fn name(&self) -> &str {
        "m0012_drop_repo_default_tool"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0012DropRepoDefaultTool {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_ref"))
                    .drop_column(Alias::new("default_tool"))
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

/// Adds the skill_source table (git-hosted skill sources).
pub struct M0013SkillSource;
impl MigrationName for M0013SkillSource {
    fn name(&self) -> &str {
        "m0013_skill_source"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0013SkillSource {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(skill_source::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("skill_source")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds the skill_enable table (per-skill, per-scope enablement).
pub struct M0014SkillEnable;
impl MigrationName for M0014SkillEnable {
    fn name(&self) -> &str {
        "m0014_skill_enable"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0014SkillEnable {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(skill_enable::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("skill_enable")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds the im_route table — issue ↔ IM thread binding (spec §6, M2).
pub struct M0015ImRoute;
impl MigrationName for M0015ImRoute {
    fn name(&self) -> &str {
        "m0015_im_route"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0015ImRoute {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(im_route::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        // Composite unique: same Feishu thread can't bind to two issues.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_im_route_thread_ref")
                    .table(Alias::new("im_route"))
                    .col(Alias::new("channel"))
                    .col(Alias::new("chat_id"))
                    .col(Alias::new("im_thread_ref"))
                    .unique()
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("im_route")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds backup_config — singleton config for git-remote backup.
pub struct M0016BackupConfig;
impl MigrationName for M0016BackupConfig {
    fn name(&self) -> &str {
        "m0016_backup_config"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0016BackupConfig {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(backup_config::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("backup_config")).to_owned())
            .await?;
        Ok(())
    }
}

/// Reconciles the legacy session.status high-water-mark. Before honest activity
/// status, `status` was set to "running" on attach and never reset to idle, so
/// every pre-upgrade worker row reads "running"/"starting" whether or not its
/// turn finished. The boot revive sweep resumes orphaned "running" rows, so
/// without this one-time reset the first launch after upgrade would resume and
/// nudge every old idle/review worker. Reset them to "idle" once; from here on
/// the engine writes status honestly at turn boundaries.
pub struct M0017SessionStatusReset;
impl MigrationName for M0017SessionStatusReset {
    fn name(&self) -> &str {
        "m0017_session_status_reset"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0017SessionStatusReset {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        crate::store::repo::reset_stale_running_sessions(manager.get_connection())
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Data reconcile only — nothing to reverse.
        Ok(())
    }
}

/// Per-task (direction) target branch for the diff panel's "vs target" mode.
/// Empty = use the repo's default branch (repo_ref.base_ref). Tolerate a
/// duplicate column so re-running against a hand-patched db is a no-op.
pub struct M0018DirectionTargetBranch;
impl MigrationName for M0018DirectionTargetBranch {
    fn name(&self) -> &str {
        "m0018_direction_target_branch"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0018DirectionTargetBranch {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .add_column(
                        ColumnDef::new(Alias::new("target_branch"))
                            .string()
                            .not_null()
                            .default(""),
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
                    .drop_column(Alias::new("target_branch"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the nullable thread.lead_command pin (per-lead command override for the
/// coding-agent alias feature). NULL = follow the global tool→command map. M0001
/// reflects the current entity, so a FRESH db already has the column; sqlite has
/// no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
pub struct M0019ThreadLeadCommand;
impl MigrationName for M0019ThreadLeadCommand {
    fn name(&self) -> &str {
        "m0019_thread_lead_command"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0019ThreadLeadCommand {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("thread"))
                    .add_column(ColumnDef::new(Alias::new("lead_command")).string().null())
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
                    .table(Alias::new("thread"))
                    .drop_column(Alias::new("lead_command"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the nullable session.command pin (per-worker command override). Same
/// semantics and duplicate tolerance as M0019.
pub struct M0020SessionCommand;
impl MigrationName for M0020SessionCommand {
    fn name(&self) -> &str {
        "m0020_session_command"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0020SessionCommand {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("session"))
                    .add_column(ColumnDef::new(Alias::new("command")).string().null())
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
                    .table(Alias::new("session"))
                    .drop_column(Alias::new("command"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the captured `origin` remote URL to repo_ref, for workspace-level git
/// dedup. M0001 reflects the current entity, so a FRESH db already has it; this
/// only matters for older dbs, and sqlite has no ADD COLUMN IF NOT EXISTS so the
/// duplicate is tolerated.
pub struct M0021RepoRemoteUrl;
impl MigrationName for M0021RepoRemoteUrl {
    fn name(&self) -> &str {
        "m0021_repo_remote_url"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0021RepoRemoteUrl {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_ref"))
                    .add_column(
                        ColumnDef::new(Alias::new("remote_url"))
                            .string()
                            .not_null()
                            .default(""),
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
                    .table(Alias::new("repo_ref"))
                    .drop_column(Alias::new("remote_url"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the agent curator's inferred cross-repo relations (JSON) to repo_profile.
/// M0002 reflects the current entity, so a FRESH db already has it; this only
/// matters for older dbs, and sqlite has no ADD COLUMN IF NOT EXISTS so the
/// duplicate is tolerated.
pub struct M0022RepoProfileRelations;
impl MigrationName for M0022RepoProfileRelations {
    fn name(&self) -> &str {
        "m0022_repo_profile_relations"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0022RepoProfileRelations {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("relations"))
                            .string()
                            .not_null()
                            .default("[]"),
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
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("relations"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the per-repo deep agent pass's monorepo sub-components (JSON) to
/// repo_profile, powering the repo map's "expanded" view. M0002 reflects the
/// current entity, so a FRESH db already has it; this only matters for older
/// dbs, and sqlite has no ADD COLUMN IF NOT EXISTS so the duplicate is tolerated.
pub struct M0023RepoProfileComponents;
impl MigrationName for M0023RepoProfileComponents {
    fn name(&self) -> &str {
        "m0023_repo_profile_components"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0023RepoProfileComponents {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("components"))
                            .string()
                            .not_null()
                            .default("[]"),
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
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("components"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds direction.base_branch: the ref a worktree branches off (empty = repo
/// default). M0001 reflects the current entity, so a FRESH db already has it;
/// sqlite has no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
pub struct M0024DirectionBaseBranch;
impl MigrationName for M0024DirectionBaseBranch {
    fn name(&self) -> &str {
        "m0024_direction_base_branch"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0024DirectionBaseBranch {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .add_column(
                        ColumnDef::new(Alias::new("base_branch"))
                            .string()
                            .not_null()
                            .default(""),
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
                    .drop_column(Alias::new("base_branch"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds worktree.created_branch: whether Weft created this branch (vs. checking
/// out a pre-existing one). Rollback deletes the branch only when true, so a
/// pre-existing branch reused by the fallback is never deleted on rollback.
/// Existing rows default to false (safe: rollback won't delete their branch).
/// M0001 reflects the current entity, so a FRESH db already has it; sqlite has
/// no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
/// Adds worktree.created_branch: whether Weft created this worktree's branch (via
/// `git worktree add -b`) vs. reusing a pre-existing branch. Thread/repo cascade
/// cleanup only deletes the branch when this is true — a reused branch must survive.
/// Existing rows default to TRUE: every pre-this-change worktree had its branch
/// created by Weft (the old materialize path always `worktree add -b`'d), so
/// zero-accumulation still tears those legacy branches down on teardown. sqlite has no
/// ADD COLUMN IF NOT EXISTS, so tolerate the duplicate (M0001 reflects the current
/// entity, so a FRESH db already has the column).
pub struct M0025WorktreeCreatedBranch;
impl MigrationName for M0025WorktreeCreatedBranch {
    fn name(&self) -> &str {
        "m0025_worktree_created_branch"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0025WorktreeCreatedBranch {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("worktree"))
                    .add_column(
                        ColumnDef::new(Alias::new("created_branch"))
                            .boolean()
                            .not_null()
                            .default(true),
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
                    .table(Alias::new("worktree"))
                    .drop_column(Alias::new("created_branch"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds worktree.created_checkout: whether Weft created this worktree directory
/// (vs. reusing a pre-existing path). Rollback and cascade cleanup only call
/// `git worktree remove` when this is true — a reused pre-existing path must
/// survive rollback. Existing rows default to true (they ARE genuine Weft
/// checkouts, safe to remove on teardown). M0001 reflects the current entity,
/// so a FRESH db already has it; sqlite has no ADD COLUMN IF NOT EXISTS, so
/// tolerate the duplicate.
pub struct M0026WorktreeCreatedCheckout;
impl MigrationName for M0026WorktreeCreatedCheckout {
    fn name(&self) -> &str {
        "m0026_worktree_created_checkout"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0026WorktreeCreatedCheckout {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("worktree"))
                    .add_column(
                        ColumnDef::new(Alias::new("created_checkout"))
                            .boolean()
                            .not_null()
                            .default(true),
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
                    .table(Alias::new("worktree"))
                    .drop_column(Alias::new("created_checkout"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds repo_ref.base_ref_is_default: whether `base_ref` was captured as the repo's
/// real default branch (true) vs. a legacy current-branch capture on an upgraded DB
/// (false). The offline fallback (`recorded_base_or_default`) only trusts `base_ref`
/// over the default chain when this is true — a legacy base_ref (even a pushed
/// feature branch whose `origin/<base_ref>` resolves) is indistinguishable from a
/// genuine non-standard default by value alone, so the marker is the only signal.
/// Existing/legacy rows default to FALSE (their base_ref was the current-branch
/// capture, not a vetted default). M0001 reflects the current entity, so a FRESH db
/// already has it; sqlite has no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
pub struct M0027RepoRefBaseRefIsDefault;
impl MigrationName for M0027RepoRefBaseRefIsDefault {
    fn name(&self) -> &str {
        "m0027_repo_ref_base_ref_is_default"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0027RepoRefBaseRefIsDefault {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_ref"))
                    .add_column(
                        ColumnDef::new(Alias::new("base_ref_is_default"))
                            .boolean()
                            .not_null()
                            .default(false),
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
                    .table(Alias::new("repo_ref"))
                    .drop_column(Alias::new("base_ref_is_default"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds worktree.base_commit: the COMMIT the work branch was forked from at create time
/// (the resolved base's tip on the `worktree add -b <branch> <resolved>` success path).
/// Reuse-time validation checks the work branch DESCENDS from this STABLE commit rather
/// than re-resolving a moving base NAME — so a base that advanced (or a lane forked from a
/// local ref while origin later diverged) is not false-rejected, while a branch externally
/// reset onto an unrelated base is still caught. Empty = legacy/reuse/fallback row (skip
/// validation). M0001 reflects the current entity, so a FRESH db already has it; sqlite has
/// no ADD COLUMN IF NOT EXISTS, so tolerate the duplicate.
pub struct M0028WorktreeBaseCommit;
impl MigrationName for M0028WorktreeBaseCommit {
    fn name(&self) -> &str {
        "m0028_worktree_base_commit"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0028WorktreeBaseCommit {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("worktree"))
                    .add_column(
                        ColumnDef::new(Alias::new("base_commit"))
                            .string()
                            .not_null()
                            .default(""),
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
                    .table(Alias::new("worktree"))
                    .drop_column(Alias::new("base_commit"))
                    .to_owned(),
            )
            .await
    }
}

/// Rewrite any `repo_profile` rows that still carry the old "gateway" tier value
/// (removed from the model in B-T1). Both the top-level `role` column and
/// per-component `tier` fields inside the `components` JSON blob are updated.
/// `down` is a no-op — the merge is irreversible.
pub struct M0029GatewayToBackend;
impl MigrationName for M0029GatewayToBackend {
    fn name(&self) -> &str {
        "m0029_gateway_to_backend"
    }
}

/// Pure helper: rewrite any component tiers that equal "gateway" to "backend".
/// Returns the input string unchanged on any parse error so a malformed blob
/// never aborts the migration.
pub(crate) fn gateway_components_to_backend(components_json: &str) -> String {
    let mut comps: Vec<crate::profile::Component> = match serde_json::from_str(components_json) {
        Ok(v) => v,
        Err(_) => return components_json.to_string(),
    };
    for c in &mut comps {
        if c.tier == "gateway" {
            c.tier = "backend".to_string();
        }
    }
    match serde_json::to_string(&comps) {
        Ok(s) => s,
        Err(_) => components_json.to_string(),
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0029GatewayToBackend {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        use sea_orm::{ConnectionTrait, Statement};
        let db = manager.get_connection();
        let backend = db.get_database_backend();

        // 1) Rewrite the top-level role column.
        db.execute(Statement::from_string(
            backend,
            "UPDATE repo_profile SET role = 'backend' WHERE role = 'gateway'".to_owned(),
        ))
        .await?;

        // 2) Rewrite gateway tiers embedded in the components JSON array.
        // Use raw SELECT of only `id` + `components` — the entity model would
        // SELECT every column including `analysis_state`/`category`/`domains`
        // that do not yet exist when M0029 runs (M0030/M0031 add them later).
        let rows = db
            .query_all(Statement::from_string(
                backend,
                "SELECT id, components FROM repo_profile".to_owned(),
            ))
            .await?;
        for row in rows {
            let id: i32 = match row.try_get("", "id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let components: String = match row.try_get("", "components") {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !components.contains("\"gateway\"") {
                continue;
            }
            let fixed = gateway_components_to_backend(&components);
            if fixed == components {
                continue;
            }
            // Propagate a write failure rather than swallowing it: if this UPDATE
            // errors, the migration must fail so it is NOT recorded as applied and
            // retries on the next startup. Discarding the error would leave these
            // component blobs permanently on the removed "gateway" tier.
            db.execute(Statement::from_sql_and_values(
                backend,
                "UPDATE repo_profile SET components = ? WHERE id = ?",
                [fixed.into(), id.into()],
            ))
            .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Data merge is irreversible.
        Ok(())
    }
}

/// Adds `analysis_state` (TEXT NOT NULL DEFAULT 'idle') and `analysis_error`
/// (TEXT NULL) to `repo_profile` so run-state survives process restarts.
/// A fresh db already has these (M0002 reflects the current entity); sqlite has
/// no ADD COLUMN IF NOT EXISTS, so the duplicate is tolerated.
pub struct M0030AnalysisState;
impl MigrationName for M0030AnalysisState {
    fn name(&self) -> &str {
        "m0030_analysis_state"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0030AnalysisState {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r1 = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("analysis_state"))
                            .string()
                            .not_null()
                            .default("idle"),
                    )
                    .to_owned(),
            )
            .await;
        match r1 {
            Ok(()) => {}
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => {}
            Err(e) => return Err(e),
        }
        let r2 = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("analysis_error"))
                            .string()
                            .null(),
                    )
                    .to_owned(),
            )
            .await;
        match r2 {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("analysis_state"))
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("analysis_error"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds `category` (TEXT NOT NULL DEFAULT '') and `domains` (TEXT NOT NULL DEFAULT '[]')
/// to `repo_profile`. A fresh db already has these (M0002 reflects the current entity);
/// sqlite has no ADD COLUMN IF NOT EXISTS, so the duplicate is tolerated.
pub struct M0031RepoCategoryDomains;
impl MigrationName for M0031RepoCategoryDomains {
    fn name(&self) -> &str {
        "m0031_repo_category_domains"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0031RepoCategoryDomains {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r1 = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("category"))
                            .string()
                            .not_null()
                            .default(""),
                    )
                    .to_owned(),
            )
            .await;
        match r1 {
            Ok(()) => {}
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => {}
            Err(e) => return Err(e),
        }
        let r2 = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("domains"))
                            .string()
                            .not_null()
                            .default("[]"),
                    )
                    .to_owned(),
            )
            .await;
        match r2 {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("category"))
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("domains"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds nullable `seq` (BIGINT) to `lead_message`. When a queued row is
/// delivered, `assign_delivery_seq` sets seq to max(COALESCE(seq,id)) + 1 so
/// reordered-then-delivered rows appear in send order, not creation order.
/// Existing rows stay NULL and sort by id (COALESCE(seq, id) = id).
pub struct M0032LeadMessageSeq;
impl MigrationName for M0032LeadMessageSeq {
    fn name(&self) -> &str {
        "m0032_lead_message_seq"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0032LeadMessageSeq {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("lead_message"))
                    .add_column(ColumnDef::new(Alias::new("seq")).big_integer().null())
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
                    .table(Alias::new("lead_message"))
                    .drop_column(Alias::new("seq"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds `layer` (TEXT NOT NULL DEFAULT '') and `layer_rank` (INTEGER NOT NULL
/// DEFAULT 0) to `repo_profile`. The cross-repo curator pass assigns these so the
/// repo map can stack repos into agent-named architectural bands. A fresh db
/// already has them (M0002 reflects the entity); sqlite has no ADD COLUMN IF NOT
/// EXISTS, so the duplicate is tolerated.
pub struct M0033RepoLayerRank;
impl MigrationName for M0033RepoLayerRank {
    fn name(&self) -> &str {
        "m0033_repo_layer_rank"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0033RepoLayerRank {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r1 = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("layer"))
                            .string()
                            .not_null()
                            .default(""),
                    )
                    .to_owned(),
            )
            .await;
        match r1 {
            Ok(()) => {}
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => {}
            Err(e) => return Err(e),
        }
        let r2 = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .add_column(
                        ColumnDef::new(Alias::new("layer_rank"))
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await;
        match r2 {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("layer"))
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("repo_profile"))
                    .drop_column(Alias::new("layer_rank"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds thread.lead_meta and session.meta: the engine's last-known meta snapshot
/// (JSON — context tokens, window, model, MCP servers, tools), written at
/// init/turn-end and read back on engine (re)creation so the Session panel
/// survives an app relaunch instead of blanking until the next turn. A fresh db
/// already has both (M0001 reflects the entities); sqlite has no ADD COLUMN IF
/// NOT EXISTS, so the duplicate is tolerated.
pub struct M0034SessionMetaSnapshot;
impl MigrationName for M0034SessionMetaSnapshot {
    fn name(&self) -> &str {
        "m0034_session_meta_snapshot"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0034SessionMetaSnapshot {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (table, column) in [("thread", "lead_meta"), ("session", "meta")] {
            let r = manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .add_column(
                            ColumnDef::new(Alias::new(column))
                                .string()
                                .not_null()
                                .default(""),
                        )
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
        for (table, column) in [("thread", "lead_meta"), ("session", "meta")] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .drop_column(Alias::new(column))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

/// Creates the test_plan table: an issue's 0..1 test-case document (markdown
/// tree), derived by the lead in phase 1.5 and editable by the user. The
/// UNIQUE thread_id enforces the 0..1 binding at the schema level.
pub struct M0035TestPlan;
impl MigrationName for M0035TestPlan {
    fn name(&self) -> &str {
        "m0035_test_plan"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0035TestPlan {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(test_plan::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("test_plan")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds nullable `native_anchor` (TEXT) to `lead_message`. The engine records
/// it on the user row that opened a turn: claude = the turn's last assistant
/// event uuid, codex app-server = the turn id. Conversation rewind cuts the
/// native session at the anchor of the nearest user row before the target. A
/// fresh db already has it (M0007 reflects the entity); sqlite has no ADD
/// COLUMN IF NOT EXISTS, so the duplicate is tolerated.
pub struct M0036LeadMessageNativeAnchor;
impl MigrationName for M0036LeadMessageNativeAnchor {
    fn name(&self) -> &str {
        "m0036_lead_message_native_anchor"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0036LeadMessageNativeAnchor {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("lead_message"))
                    .add_column(ColumnDef::new(Alias::new("native_anchor")).string().null())
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
                    .table(Alias::new("lead_message"))
                    .drop_column(Alias::new("native_anchor"))
                    .to_owned(),
            )
            .await
    }
}

/// Creates the code_checkpoint table: one row per pre-turn code checkpoint
/// (shadow-repo commit + real HEAD) recorded at a worker's user-turn start,
/// consumed by code rewind. A fresh db already has it (the entity drives
/// create_table_from_entity); IF NOT EXISTS tolerates the duplicate like
/// M0036's column-add does.
pub struct M0037CodeCheckpoint;
impl MigrationName for M0037CodeCheckpoint {
    fn name(&self) -> &str {
        "m0037_code_checkpoint"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0037CodeCheckpoint {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(code_checkpoint::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("code_checkpoint")).to_owned())
            .await?;
        Ok(())
    }
}

/// Adds `nested_repos` (TEXT, default '[]') to `code_checkpoint`: the snapshot
/// manifest of nested git repo dirs, so a restore can delete exactly the
/// nested repos created AFTER the checkpoint (git clean -fd never touches
/// nested repos). Duplicate tolerated (fresh DBs reflect the entity already).
pub struct M0038CodeCheckpointNestedRepos;
impl MigrationName for M0038CodeCheckpointNestedRepos {
    fn name(&self) -> &str {
        "m0038_code_checkpoint_nested_repos"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0038CodeCheckpointNestedRepos {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("code_checkpoint"))
                    .add_column(
                        ColumnDef::new(Alias::new("nested_repos"))
                            .string()
                            .not_null()
                            .default("[]"),
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
                    .table(Alias::new("code_checkpoint"))
                    .drop_column(Alias::new("nested_repos"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds `index_tree` (TEXT, default '') to `code_checkpoint`: the tree of the
/// real repo's index at snapshot time, so a restore can put the user's staged
/// state back instead of resetting the index to HEAD. Duplicate tolerated
/// (fresh DBs reflect the entity already).
pub struct M0039CodeCheckpointIndexTree;
impl MigrationName for M0039CodeCheckpointIndexTree {
    fn name(&self) -> &str {
        "m0039_code_checkpoint_index_tree"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0039CodeCheckpointIndexTree {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("code_checkpoint"))
                    .add_column(
                        ColumnDef::new(Alias::new("index_tree"))
                            .string()
                            .not_null()
                            .default(""),
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
                    .table(Alias::new("code_checkpoint"))
                    .drop_column(Alias::new("index_tree"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds nullable `consumed_at` (BIGINT, unix-millis) to `lead_message`. The
/// engine stamps it on the "user" row that opened a turn the first time the
/// agent produces ANY observed activity for that turn — the "已被 agent 消费"
/// delivery receipt (issue #94), distinct from `status` (which already tracks
/// queued/delivered/error/interrupted). A fresh db already has it (M0007
/// reflects the entity); sqlite has no ADD COLUMN IF NOT EXISTS, so the
/// duplicate is tolerated like M0032/M0036's column-adds.
pub struct M0040LeadMessageConsumedAt;
impl MigrationName for M0040LeadMessageConsumedAt {
    fn name(&self) -> &str {
        "m0040_lead_message_consumed_at"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0040LeadMessageConsumedAt {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("lead_message"))
                    .add_column(ColumnDef::new(Alias::new("consumed_at")).big_integer().null())
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
                    .table(Alias::new("lead_message"))
                    .drop_column(Alias::new("consumed_at"))
                    .to_owned(),
            )
            .await
    }
}

/// Composite index for the single-row `lead_message` lookups the stall sweep
/// (`lead_chat::revive`) repeats on every pass. They all shape up as
/// `thread_id = ? AND kind = ? [AND session_id …]`, but M0007 only ever
/// indexed `thread_id`, so each one had to walk that thread's ENTIRE message
/// history — which on a long-lived thread is the whole chat — to return one
/// row. Three callers ride this: `repo::last_turn_freeze_recovery_secs` (the
/// turn-freeze grace window, per thread AND per stalled direction) plus the
/// older `repo::lead_native_id` / `repo::lead_status` meta reads that already
/// ran twice per thread per sweep.
///
/// `(thread_id, kind, session_id, id)` serves all of them: equality on the
/// leading columns, with `id` last so `ORDER BY id DESC LIMIT 1` reads
/// straight off the index instead of sorting. `lead_native_id`/`lead_status`
/// use the `(thread_id, kind)` prefix. `idx_lead_message_thread` stays — the
/// timeline reads that filter on `thread_id` alone are still served by the
/// narrower index, and dropping it is a separate, riskier change.
///
/// Additive and idempotent: `if_not_exists` matches M0007's own index
/// creation, so re-runs and fresh databases behave the same. Pure
/// performance — no column, no data, no behavior change.
pub struct M0041LeadMessageThreadKindIdx;
impl MigrationName for M0041LeadMessageThreadKindIdx {
    fn name(&self) -> &str {
        "m0041_lead_message_thread_kind_idx"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0041LeadMessageThreadKindIdx {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_lead_message_thread_kind_session")
                    .table(Alias::new("lead_message"))
                    .col(Alias::new("thread_id"))
                    .col(Alias::new("kind"))
                    .col(Alias::new("session_id"))
                    .col(Alias::new("id"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_lead_message_thread_kind_session")
                    .table(Alias::new("lead_message"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the nullable thread.lead_model override (issue #98: model selection in
/// the UI). NULL = follow the CLI's own configured default. M0001 reflects the
/// current entity, so a fresh db already has the column; sqlite has no ADD
/// COLUMN IF NOT EXISTS, so the duplicate is tolerated like M0019/M0020.
pub struct M0042ThreadLeadModel;
impl MigrationName for M0042ThreadLeadModel {
    fn name(&self) -> &str {
        "m0042_thread_lead_model"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0042ThreadLeadModel {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("thread"))
                    .add_column(ColumnDef::new(Alias::new("lead_model")).string().null())
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
                    .table(Alias::new("thread"))
                    .drop_column(Alias::new("lead_model"))
                    .to_owned(),
            )
            .await
    }
}

/// Adds the nullable session.model override. Same semantics/duplicate
/// tolerance as M0042, scoped to chat-mode workers.
pub struct M0043SessionModel;
impl MigrationName for M0043SessionModel {
    fn name(&self) -> &str {
        "m0043_session_model"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0043SessionModel {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let r = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("session"))
                    .add_column(ColumnDef::new(Alias::new("model")).string().null())
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
                    .table(Alias::new("session"))
                    .drop_column(Alias::new("model"))
                    .to_owned(),
            )
            .await
    }
}

/// Records whether a tool identity came from a user choice rather than the
/// global/default routing policy. Existing rows intentionally migrate as
/// pinned: an upgrade must never redirect a currently known task. Legacy
/// curator rows remain pinned as well because older versions did not persist
/// enough provenance to distinguish a default from an explicit engine choice.
pub struct M0044EngineRoutingPin;
impl MigrationName for M0044EngineRoutingPin {
    fn name(&self) -> &str {
        "m0044_engine_routing_pin"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0044EngineRoutingPin {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (table, column) in [
            ("thread", "engine_pinned"),
            ("direction", "engine_pinned"),
            ("session", "engine_pinned"),
        ] {
            let result = manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .add_column(
                            ColumnDef::new(Alias::new(column))
                                .boolean()
                                .not_null()
                                .default(true),
                        )
                        .to_owned(),
                )
                .await;
            match result {
                Ok(()) => {}
                Err(err) if err.to_string().to_lowercase().contains("duplicate column") => {}
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (table, column) in [
            ("session", "engine_pinned"),
            ("direction", "engine_pinned"),
            ("thread", "engine_pinned"),
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table))
                        .drop_column(Alias::new(column))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

/// Issue #110 T1: the tracked-PR/MR entity — a real DB row (repo, task,
/// host-normalized state) so "what is this PR/MR waiting on" is a store fact
/// the background monitor (`crate::host::monitor`) can read and update, not
/// something that only lives in an agent's turn or a chat session's memory.
/// One task may declare ANOTHER task as its upstream, so a cross-repo change
/// set can be merged in dependency order: the producer's PR lands before the
/// consumer's is considered mergeable at all.
///
/// A single column, not a join table, and deliberately so. It expresses one
/// upstream per task, which is what a producer→consumer pair needs, and this
/// is the minimum that answers whether ordered cross-repo delivery is worth
/// building out. A real topological sequencer wants many-to-many and will need
/// its own table — see the module docs on `host::judge::UpstreamStatus`.
///
/// `0` means "no upstream", matching the `direction.repo_id` convention rather
/// than introducing a nullable column with different emptiness semantics.
/// Existing rows migrate to 0: an upgrade must never invent a dependency that
/// would block a task the user can merge today.
pub struct M0046DirectionUpstream;
impl MigrationName for M0046DirectionUpstream {
    fn name(&self) -> &str {
        "m0046_direction_upstream"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0046DirectionUpstream {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let result = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .add_column(
                        ColumnDef::new(Alias::new("depends_on_direction_id"))
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(err) if err.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("direction"))
                    .drop_column(Alias::new("depends_on_direction_id"))
                    .to_owned(),
            )
            .await
    }
}

pub struct M0045PullRequest;
impl MigrationName for M0045PullRequest {
    fn name(&self) -> &str {
        "m0045_pull_request"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0045PullRequest {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut stmt = schema.create_table_from_entity(pull_request::Entity);
        stmt.if_not_exists();
        manager.create_table(stmt).await?;
        // Belt-and-suspenders alongside `repo::register_pull_request`'s
        // application-level find-then-upsert: guarantees the natural key
        // (host_kind, host_owner, host_repo, number) can never duplicate at
        // the DB level even under a race the app layer doesn't catch.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_pull_request_natural_key")
                    .table(Alias::new("pull_request"))
                    .col(Alias::new("host_kind"))
                    .col(Alias::new("host_owner"))
                    .col(Alias::new("host_repo"))
                    .col(Alias::new("number"))
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("pull_request")).to_owned())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{gateway_components_to_backend, M0044EngineRoutingPin, M0045PullRequest, M0046DirectionUpstream};

    #[test]
    fn gateway_tier_rewritten_to_backend() {
        let input = r#"[{"name":"api","path":"services/api","tier":"gateway","summary":"","deps":[]}]"#;
        let output = gateway_components_to_backend(input);
        assert!(
            !output.contains("\"gateway\""),
            "gateway should be gone: {output}"
        );
        assert!(
            output.contains("\"backend\""),
            "backend should appear: {output}"
        );
    }

    #[test]
    fn non_gateway_tier_untouched() {
        let input = r#"[{"name":"web","path":"apps/web","tier":"frontend","summary":"","deps":[]}]"#;
        let output = gateway_components_to_backend(input);
        // Tier must not be rewritten; name/path must survive round-trip.
        assert!(output.contains("\"frontend\""), "frontend tier must survive: {output}");
        assert!(!output.contains("\"backend\""), "backend must not appear: {output}");
        assert!(output.contains("\"web\""), "name must survive: {output}");
    }

    #[test]
    fn malformed_json_returned_unchanged() {
        let input = "not valid json {{";
        let output = gateway_components_to_backend(input);
        assert_eq!(output, input);
    }

    #[test]
    fn idempotent_already_backend() {
        let input =
            r#"[{"name":"api","path":"services/api","tier":"backend","summary":"","deps":[]}]"#;
        let output = gateway_components_to_backend(input);
        // Tier must stay backend; the round-trip is expected to add the new `domains` field.
        assert!(output.contains("\"backend\""), "backend tier must survive: {output}");
        assert!(!output.contains("\"gateway\""), "gateway must not appear: {output}");
    }

    #[test]
    fn mixed_tiers_only_gateway_rewritten() {
        let input = r#"[{"name":"a","path":"a","tier":"gateway","summary":"","deps":[]},{"name":"b","path":"b","tier":"frontend","summary":"","deps":[]}]"#;
        let output = gateway_components_to_backend(input);
        assert!(
            !output.contains("\"gateway\""),
            "no gateway should remain: {output}"
        );
        assert!(
            output.contains("\"frontend\""),
            "frontend should survive: {output}"
        );
    }

    /// M0031: category and domains columns are present after migration and default correctly.
    #[tokio::test]
    async fn m0031_category_domains_columns_added() {
        use crate::store::Db;
        use crate::store::repo::{add_repo_ref, create_workspace, get_repo_profile, upsert_repo_profile};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        // Both columns must exist with their defaults.
        assert_eq!(p.category, "", "category column must exist and default to empty");
        assert_eq!(p.domains, "[]", "domains column must exist and default to '[]'");
    }

    /// M0033: layer and layer_rank columns are present after migration and default correctly.
    #[tokio::test]
    async fn m0033_layer_rank_columns_added() {
        use crate::store::Db;
        use crate::store::repo::{add_repo_ref, create_workspace, get_repo_profile, upsert_repo_profile};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        // Both columns must exist with their defaults.
        assert_eq!(p.layer, "", "layer column must exist and default to empty");
        assert_eq!(p.layer_rank, 0, "layer_rank column must exist and default to 0");
    }

    /// M0034: thread.lead_meta and session.meta are present after migration and
    /// default to empty (never captured).
    #[tokio::test]
    async fn m0034_session_meta_snapshot_columns_added() {
        use crate::store::Db;
        use crate::store::repo::{
            add_repo_ref, create_direction, create_session, create_thread, create_workspace,
        };

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        assert_eq!(t.lead_meta, "", "thread.lead_meta must exist and default to empty");
        let r = add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        let d = create_direction(&db, t.id, "dir", "claude", r.id, "why", "plan+impl", "")
            .await
            .unwrap();
        let s = create_session(&db, d.id, r.id, "claude", "/tmp/cwd").await.unwrap();
        assert_eq!(s.meta, "", "session.meta must exist and default to empty");
    }

    /// M0036: lead_message.native_anchor is present after migration and defaults to NULL.
    #[tokio::test]
    async fn m0036_native_anchor_column_added() {
        use crate::store::Db;
        use crate::store::repo::{create_thread, create_workspace, insert_lead_message};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        let m = insert_lead_message(&db, t.id, None, 1, "user", "text", r#"{"text":"hi"}"#, "complete")
            .await
            .unwrap();
        // Selecting the row back requires the column to exist.
        assert_eq!(m.native_anchor, None, "native_anchor must exist and default to NULL");
    }

    /// M0040: lead_message.consumed_at is present after migration and defaults to NULL.
    #[tokio::test]
    async fn m0040_consumed_at_column_added() {
        use crate::store::Db;
        use crate::store::repo::{create_thread, create_workspace, insert_lead_message};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        let m = insert_lead_message(&db, t.id, None, 1, "user", "text", r#"{"text":"hi"}"#, "complete")
            .await
            .unwrap();
        // Selecting the row back requires the column to exist.
        assert_eq!(m.consumed_at, None, "consumed_at must exist and default to NULL");
    }

    /// M0041: the composite index exists AND SQLite actually plans the stall
    /// sweep's lookups through it. Asserting the query PLAN, not just
    /// `sqlite_master`, is the point: an index that exists but doesn't match
    /// the query's column order would still leave the sweep doing a full
    /// per-thread scan, and only the plan can tell those apart. Covers both
    /// shapes — the freeze marker (`kind` + `session_id`) and the older
    /// `lead_native_id`/`lead_status` meta reads that use the leading prefix.
    #[tokio::test]
    async fn m0041_thread_kind_index_backs_the_sweep_lookups() {
        use crate::store::Db;
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let plan_for = |sql: &'static str| {
            let db = db.0.clone();
            async move {
                db.query_all(Statement::from_string(
                    sea_orm::DatabaseBackend::Sqlite,
                    format!("EXPLAIN QUERY PLAN {sql}"),
                ))
                .await
                .unwrap()
                .iter()
                .filter_map(|r| r.try_get::<String>("", "detail").ok())
                .collect::<Vec<_>>()
                .join(" | ")
            }
        };

        // The freeze-marker lookup (repo::last_turn_freeze_recovery_secs),
        // worker form: all three equality columns plus the ORDER BY.
        let marker = plan_for(
            "SELECT * FROM lead_message WHERE thread_id = 1 AND kind = 'turn_freeze_recovered' \
             AND session_id = 2 ORDER BY id DESC LIMIT 1",
        )
        .await;
        assert!(
            marker.contains("idx_lead_message_thread_kind_session"),
            "marker lookup must use the composite index, got: {marker}"
        );
        // …and it must not fall back to sorting, which is the whole reason `id`
        // is the trailing column.
        assert!(
            !marker.contains("TEMP B-TREE"),
            "ORDER BY id DESC should read off the index, got: {marker}"
        );

        // The lead form keys `session_id IS NULL` instead.
        let lead_marker = plan_for(
            "SELECT * FROM lead_message WHERE thread_id = 1 AND kind = 'turn_freeze_recovered' \
             AND session_id IS NULL ORDER BY id DESC LIMIT 1",
        )
        .await;
        assert!(
            lead_marker.contains("idx_lead_message_thread_kind_session"),
            "lead marker lookup must use the composite index, got: {lead_marker}"
        );

        // The pre-existing per-sweep meta reads ride the (thread_id, kind) prefix.
        let meta =
            plan_for("SELECT * FROM lead_message WHERE thread_id = 1 AND kind = 'meta' LIMIT 1")
                .await;
        assert!(
            meta.contains("idx_lead_message_thread_kind_session"),
            "meta lookup must use the composite index prefix, got: {meta}"
        );
    }

    /// M0044: legacy curator rows stay conservatively pinned. The old schema
    /// has no provenance to distinguish a default engine from a manual pick,
    /// so migration must not opt any existing row into automatic fail-over.
    #[tokio::test]
    async fn m0044_legacy_curator_is_pinned_and_existing_values_survive_rerun() {
        use sea_orm::{ConnectionTrait, Database, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        for sql in [
            "CREATE TABLE thread (id INTEGER PRIMARY KEY, kind TEXT NOT NULL)",
            "CREATE TABLE direction (id INTEGER PRIMARY KEY)",
            "CREATE TABLE session (id INTEGER PRIMARY KEY)",
            "INSERT INTO thread (id, kind) VALUES (1, 'curator'), (2, 'feature')",
            "INSERT INTO direction (id) VALUES (1)",
            "INSERT INTO session (id) VALUES (1)",
        ] {
            db.execute(Statement::from_string(backend, sql.to_owned()))
                .await
                .unwrap();
        }

        M0044EngineRoutingPin
            .up(&SchemaManager::new(&db))
            .await
            .unwrap();

        for sql in [
            "SELECT engine_pinned FROM thread WHERE id = 1",
            "SELECT engine_pinned FROM thread WHERE id = 2",
            "SELECT engine_pinned FROM direction WHERE id = 1",
            "SELECT engine_pinned FROM session WHERE id = 1",
        ] {
            let row = db
                .query_one(Statement::from_string(backend, sql.to_owned()))
                .await
                .unwrap()
                .unwrap();
            let pinned: bool = row.try_get("", "engine_pinned").unwrap();
            assert!(pinned, "legacy rows must migrate as pinned: {sql}");
        }

        db.execute(Statement::from_string(
            backend,
            "UPDATE thread SET engine_pinned = 0 WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            backend,
            "UPDATE direction SET engine_pinned = 0 WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            backend,
            "UPDATE session SET engine_pinned = 0 WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap();

        // A retry against an already-upgraded database must leave explicit
        // values untouched, including the curator row.
        M0044EngineRoutingPin
            .up(&SchemaManager::new(&db))
            .await
            .unwrap();

        for sql in [
            "SELECT engine_pinned FROM thread WHERE id = 1",
            "SELECT engine_pinned FROM direction WHERE id = 1",
            "SELECT engine_pinned FROM session WHERE id = 1",
        ] {
            let row = db
                .query_one(Statement::from_string(backend, sql.to_owned()))
                .await
                .unwrap()
                .unwrap();
            let pinned: bool = row.try_get("", "engine_pinned").unwrap();
            assert!(!pinned, "an existing engine_pinned value must survive: {sql}");
        }
    }

    /// M0045 (issue #110 adversarial review P2): the natural-key unique
    /// index actually exists and is enforced AT THE DB LEVEL — not just by
    /// `repo::register_pull_request`'s application-level find-then-upsert,
    /// which a raw insert bypasses entirely. Without this, deleting the
    /// migration's `.unique()` call by accident would silently stop being
    /// caught by anything: the app-level upsert still LOOKS correct in every
    /// test that goes through it, since it never races itself.
    #[tokio::test]
    async fn m0045_natural_key_unique_index_rejects_a_raw_duplicate_insert() {
        use sea_orm::{ConnectionTrait, Database, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        M0045PullRequest.up(&SchemaManager::new(&db)).await.unwrap();

        let insert_sql = |number: i32| -> String {
            format!(
                "INSERT INTO pull_request \
                 (thread_id, direction_id, repo_id, host_kind, host_base, host_owner, host_repo, \
                  number, url, title, head_sha, base_ref, lifecycle, ci_status, review_status, \
                  conflict_status, merge_readiness, last_checked_at, last_error, probe_fail_count, \
                  created_at) \
                 VALUES (1, 1, 1, 'github', 'github.com', 'acme', 'widgets', {number}, \
                  '', '', '', '', 'open', '', '', '', '', '', '', 0, '1')"
            )
        };

        // First insert succeeds.
        db.execute(Statement::from_string(backend, insert_sql(1)))
            .await
            .unwrap();

        // A RAW second insert with the SAME natural key (host_kind,
        // host_owner, host_repo, number) — bypassing
        // `register_pull_request`'s find-then-upsert entirely — must be
        // rejected by the index itself.
        let dup = db.execute(Statement::from_string(backend, insert_sql(1))).await;
        assert!(
            dup.is_err(),
            "a raw duplicate insert on the natural key must be rejected by idx_pull_request_natural_key"
        );

        // A genuinely different number is unaffected — the index is scoped
        // to the whole natural key, not falsely global on e.g. just the host.
        let distinct = db.execute(Statement::from_string(backend, insert_sql(2))).await;
        assert!(distinct.is_ok(), "a different PR number must still insert cleanly");
    }

    /// M0046 (issue #110 T4, Codex review PR #159 migration/mod.rs:2036): the
    /// planner/store tests otherwise use a freshly-migrated database whose earliest
    /// migration already creates `direction` WITH `depends_on_direction_id` present —
    /// they can never detect a regression in THIS migration's actual upgrade path.
    /// This starts from a pre-M0046 `direction` table (no such column at all, mirroring
    /// what a real user's existing database looks like right before upgrading), and
    /// verifies: the column gets added, EXISTING rows default to `0` (never inventing a
    /// dependency that would block a task the user could already merge — the migration's
    /// own doc comment's promise), an explicit non-zero value set afterward is a normal
    /// column value, and a RERUN (the "duplicate column" catch in `up()`) is safe and
    /// does not clobber it.
    #[tokio::test]
    async fn m0046_direction_upstream_column_defaults_existing_rows_to_zero_and_reruns_safely() {
        use sea_orm::{ConnectionTrait, Database, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        // Pre-M0046 shape: a `direction` table that predates this migration entirely —
        // no `depends_on_direction_id` column at all, only what an upgrade needs to
        // exercise (ALTER TABLE ADD COLUMN does not care about sibling columns).
        db.execute(Statement::from_string(
            backend,
            "CREATE TABLE direction (id INTEGER PRIMARY KEY, name TEXT NOT NULL)".to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            backend,
            "INSERT INTO direction (id, name) VALUES (1, 'producer'), (2, 'consumer')".to_owned(),
        ))
        .await
        .unwrap();

        M0046DirectionUpstream.up(&SchemaManager::new(&db)).await.unwrap();

        for id in [1, 2] {
            let row = db
                .query_one(Statement::from_string(
                    backend,
                    format!("SELECT depends_on_direction_id FROM direction WHERE id = {id}"),
                ))
                .await
                .unwrap()
                .unwrap();
            let value: i32 = row.try_get("", "depends_on_direction_id").unwrap();
            assert_eq!(
                value, 0,
                "an existing pre-M0046 row must default to 0 (no upstream) — never invent a \
                 dependency that would block a task the user could already merge"
            );
        }

        // An explicit edge, set the way `repo::set_direction_upstream` would.
        db.execute(Statement::from_string(
            backend,
            "UPDATE direction SET depends_on_direction_id = 1 WHERE id = 2".to_owned(),
        ))
        .await
        .unwrap();

        // Re-running the migration (the "duplicate column" catch in `up()`) must succeed,
        // not error — and must not reset the explicit value back to the column default.
        M0046DirectionUpstream.up(&SchemaManager::new(&db)).await.unwrap();

        let row = db
            .query_one(Statement::from_string(
                backend,
                "SELECT depends_on_direction_id FROM direction WHERE id = 2".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        let value: i32 = row.try_get("", "depends_on_direction_id").unwrap();
        assert_eq!(value, 1, "a rerun must not clobber an already-recorded upstream edge");
    }

    /// M0037: code_checkpoint exists after migration and round-trips a row.
    #[tokio::test]
    async fn m0037_code_checkpoint_table_created() {
        use crate::store::repo::{code_checkpoint_for, insert_code_checkpoint};
        use crate::store::Db;

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let c = insert_code_checkpoint(&db, 1, 7, 100, 1, "shadow-sha", "head-sha", "[\"gen\"]", "idx-1")
            .await
            .unwrap();
        let found = code_checkpoint_for(&db, 1, 100).await.unwrap().unwrap();
        assert_eq!(found.id, c.id);
        assert_eq!(found.shadow_sha, "shadow-sha");
        assert_eq!(found.head_sha, "head-sha");
        // m0038: the nested-repos manifest column exists and round-trips.
        assert_eq!(found.nested_repos, "[\"gen\"]");
        // m0039: the staged-index tree column exists and round-trips.
        assert_eq!(found.index_tree, "idx-1");
    }

    /// M0029: raw-query path rewrites gateway components without touching
    /// columns that do not yet exist (analysis_state / category / domains).
    /// Simulates the mid-migration state where only legacy columns are present.
    #[tokio::test]
    async fn m0029_raw_path_rewrites_gateway_no_new_columns() {
        use sea_orm::{ConnectionTrait, Database, Statement};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        // Create a minimal repo_profile table with only legacy columns (no
        // analysis_state / category / domains).
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE repo_profile (id INTEGER PRIMARY KEY, role TEXT NOT NULL, \
             components TEXT NOT NULL DEFAULT '[]')"
                .to_owned(),
        ))
        .await
        .unwrap();

        let gateway_blob =
            r#"[{"name":"api","path":"services/api","tier":"gateway","summary":"","deps":[]}]"#;
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO repo_profile (id, role, components) VALUES (1, 'gateway', ?)",
            [gateway_blob.into()],
        ))
        .await
        .unwrap();

        // Run the same raw-query logic used by M0029::up.
        let backend = db.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            "UPDATE repo_profile SET role = 'backend' WHERE role = 'gateway'".to_owned(),
        ))
        .await
        .unwrap();

        let rows = db
            .query_all(Statement::from_string(
                backend,
                "SELECT id, components FROM repo_profile".to_owned(),
            ))
            .await
            .unwrap();
        for row in rows {
            let id: i32 = row.try_get("", "id").unwrap();
            let components: String = row.try_get("", "components").unwrap();
            if !components.contains("\"gateway\"") {
                continue;
            }
            let fixed = super::gateway_components_to_backend(&components);
            if fixed == components {
                continue;
            }
            db.execute(Statement::from_sql_and_values(
                backend,
                "UPDATE repo_profile SET components = ? WHERE id = ?",
                [fixed.into(), id.into()],
            ))
            .await
            .unwrap();
        }

        // Verify: role and component tier both converted, no new columns touched.
        let result = db
            .query_all(Statement::from_string(
                backend,
                "SELECT role, components FROM repo_profile WHERE id = 1".to_owned(),
            ))
            .await
            .unwrap();
        let r = result.first().unwrap();
        let role: String = r.try_get("", "role").unwrap();
        let components: String = r.try_get("", "components").unwrap();
        assert_eq!(role, "backend", "role must be rewritten to backend");
        assert!(
            !components.contains("\"gateway\""),
            "gateway tier must be gone: {components}"
        );
        assert!(
            components.contains("\"backend\""),
            "backend tier must appear: {components}"
        );
    }
}
