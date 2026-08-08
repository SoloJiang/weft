use crate::store::entities::{
    app_setting, authority_policy, backup_config, code_checkpoint, direction, direction_dependency,
    evidence, human_card_terminal_outbox, human_request, im_route, lane_gate_decision,
    lead_hidden_delivery, lead_message, plan, plan_revision, pull_request, repo_action_execution,
    repo_profile, repo_ref, session, skill_enable, skill_source, test_plan, thread, workspace,
    worktree,
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
            Box::new(M0047PullRequestThreadStatus),
            Box::new(M0048HumanRequest),
            Box::new(M0049HumanRequestSourceMessage),
            Box::new(M0050HumanRequestImRoutes),
            Box::new(M0051HumanCardTerminalOutbox),
            Box::new(M0052RepoActionExecution),
            Box::new(M0053LeadHiddenDelivery),
            Box::new(M0054DirectionDependency),
            Box::new(M0055Evidence),
            Box::new(M0056AuthorityPolicy),
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

/// Composite index for single-row `lead_message` lookups shaped as
/// `thread_id = ? AND kind = ? [AND session_id …]`. M0007 only indexed
/// `thread_id`, so each lookup otherwise walks an entire long-lived timeline.
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

/// Issue #110: `pull_request.thread_status` — the review-discussion-thread
/// axis (`crate::host::ThreadStatus`, JSON-serialized like the CI/review/
/// conflict columns beside it). Until this column existed, "are there
/// unresolved review threads" was not merely unchecked but unrepresentable,
/// so the auto-merge gate could authorize a merge over an open review round.
///
/// Existing rows migrate to `""`, and that emptiness is load-bearing rather
/// than incidental: `host::gate::parse_threads` maps it to
/// `ThreadStatus::Unknown`, which blocks. An upgraded install therefore
/// auto-merges NOTHING until `host::monitor`'s next successful sweep has
/// actually read each row's threads — the opposite of the default a
/// `NOT NULL DEFAULT 'all_resolved'` would have quietly created, which would
/// have granted every pre-existing row a clean bill of health it never
/// earned, at exactly the moment the checking code first shipped.
pub struct M0047PullRequestThreadStatus;
impl MigrationName for M0047PullRequestThreadStatus {
    fn name(&self) -> &str {
        "m0047_pull_request_thread_status"
    }
}
#[async_trait::async_trait]
impl MigrationTrait for M0047PullRequestThreadStatus {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let result = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("pull_request"))
                    .add_column(
                        ColumnDef::new(Alias::new("thread_status"))
                            .text()
                            .not_null()
                            .default(""),
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
                    .table(Alias::new("pull_request"))
                    .drop_column(Alias::new("thread_status"))
                    .to_owned(),
            )
            .await
    }
}

/// Durable free-text human questions. Permission prompts deliberately stay out
/// of this table because their tool-call transport cannot survive restart.
pub struct M0048HumanRequest;
impl MigrationName for M0048HumanRequest {
    fn name(&self) -> &str {
        "m0048_human_request"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0048HumanRequest {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut statement = schema.create_table_from_entity(human_request::Entity);
        statement.if_not_exists();
        manager.create_table(statement).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_human_request_workspace_status")
                    .table(Alias::new("human_request"))
                    .col(Alias::new("workspace_id"))
                    .col(Alias::new("status"))
                    .col(Alias::new("created_at"))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_human_request_scope_status")
                    .table(Alias::new("human_request"))
                    .col(Alias::new("thread_id"))
                    .col(Alias::new("direction_scope"))
                    .col(Alias::new("status"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("human_request")).to_owned())
            .await
    }
}

/// Add an exact lead_message anchor for rewind-safe durable-question
/// cancellation. New databases already receive the column from M0048's entity
/// schema; duplicate-column tolerance keeps reruns and partially upgraded DBs
/// safe.
pub struct M0049HumanRequestSourceMessage;
impl MigrationName for M0049HumanRequestSourceMessage {
    fn name(&self) -> &str {
        "m0049_human_request_source_message"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0049HumanRequestSourceMessage {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in ["source_message_id", "source_session_id"] {
            let result = manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("human_request"))
                        .add_column(
                            ColumnDef::new(Alias::new(column))
                                .integer()
                                .not_null()
                                .default(0),
                        )
                        .to_owned(),
                )
                .await;
            match result {
                Ok(()) => {}
                Err(error)
                    if error.to_string().to_lowercase().contains("duplicate column") => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in ["source_session_id", "source_message_id"] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("human_request"))
                        .drop_column(Alias::new(column))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

/// Persist every provider message id used for a durable human-question card,
/// so replies to pre-restart and replayed cards remain tied to the request.
pub struct M0050HumanRequestImRoutes;
impl MigrationName for M0050HumanRequestImRoutes {
    fn name(&self) -> &str {
        "m0050_human_request_im_routes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0050HumanRequestImRoutes {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let result = manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("human_request"))
                    .add_column(
                        ColumnDef::new(Alias::new("im_routes"))
                            .text()
                            .not_null()
                            .default("[]"),
                    )
                    .to_owned(),
            )
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.to_string().to_lowercase().contains("duplicate column") => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("human_request"))
                    .drop_column(Alias::new("im_routes"))
                    .to_owned(),
            )
            .await
    }
}

/// Durable provider PATCH work must not disappear with the source question.
/// There is intentionally no FK: thread/workspace deletion removes the source
/// content while a pending row retains only the answer needed for its final
/// provider PATCH; its delivery receipt scrubs that answer and leaves an
/// opaque route tombstone.
pub struct M0051HumanCardTerminalOutbox;
impl MigrationName for M0051HumanCardTerminalOutbox {
    fn name(&self) -> &str {
        "m0051_human_card_terminal_outbox"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0051HumanCardTerminalOutbox {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut statement = schema.create_table_from_entity(human_card_terminal_outbox::Entity);
        statement.if_not_exists();
        manager.create_table(statement).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_human_card_terminal_route")
                    .table(Alias::new("human_card_terminal_outbox"))
                    .col(Alias::new("channel"))
                    .col(Alias::new("account"))
                    .col(Alias::new("owner"))
                    .col(Alias::new("message_id"))
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_human_card_terminal_pending")
                    .table(Alias::new("human_card_terminal_outbox"))
                    .col(Alias::new("channel"))
                    .col(Alias::new("account"))
                    .col(Alias::new("owner"))
                    .col(Alias::new("delivered"))
                    .col(Alias::new("id"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("human_card_terminal_outbox"))
                    .to_owned(),
            )
            .await
    }
}

/// A repository action card owns exactly one durable execution. The unique
/// message index is the cross-process admission gate; the token index makes
/// filesystem markers unambiguous during recovery.
pub struct M0052RepoActionExecution;
impl MigrationName for M0052RepoActionExecution {
    fn name(&self) -> &str {
        "m0052_repo_action_execution"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0052RepoActionExecution {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut statement = schema.create_table_from_entity(repo_action_execution::Entity);
        statement.if_not_exists();
        manager.create_table(statement).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_repo_action_execution_message")
                    .table(Alias::new("repo_action_execution"))
                    .col(Alias::new("message_id"))
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_repo_action_execution_token")
                    .table(Alias::new("repo_action_execution"))
                    .col(Alias::new("execution_token"))
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("repo_action_execution"))
                    .to_owned(),
            )
            .await
    }
}

/// Durable hidden lead input. Source rows deliberately remain unreferenced so
/// a stopped/crashed engine can replay a plan decision or repo feedback after
/// the originating card/repository has been cleaned up.
pub struct M0053LeadHiddenDelivery;
impl MigrationName for M0053LeadHiddenDelivery {
    fn name(&self) -> &str {
        "m0053_lead_hidden_delivery"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0053LeadHiddenDelivery {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut statement = schema.create_table_from_entity(lead_hidden_delivery::Entity);
        statement.if_not_exists();
        manager.create_table(statement).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_lead_hidden_delivery_dedupe")
                    .table(Alias::new("lead_hidden_delivery"))
                    .col(Alias::new("dedupe_key"))
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_lead_hidden_delivery_pending")
                    .table(Alias::new("lead_hidden_delivery"))
                    .col(Alias::new("thread_id"))
                    .col(Alias::new("state"))
                    .col(Alias::new("id"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("lead_hidden_delivery"))
                    .to_owned(),
            )
            .await
    }
}

/// Issue #173 (R1-03): lifts `direction.depends_on_direction_id` — a single
/// producer→consumer slot — into a real many-to-many DAG. Each consumer
/// Lane can now own zero to many rows here instead of exactly one implicit
/// edge; see `direction_dependency::Model`'s doc for the row shape and
/// `repo::set_direction_upstreams` for the sole writer.
///
/// `depends_on_direction_id` is NOT dropped: the ADR (issue #173 comment)
/// keeps it as a maintained, fail-closed MIRROR column so every OLD reader
/// (and a rollback of this feature) keeps working off the exact same
/// semantics it always has — `0` = no upstream, a positive id = one
/// resolved edge, `-1`/`-2` = the existing denied/unresolved sentinels.
/// `set_direction_upstreams` is the only writer of both the new table and
/// the mirror going forward, so the two can never drift once this migration
/// completes.
///
/// The one-time data lift below runs only when `direction_dependency` is
/// still empty (a fresh upgrade); a rerun of `up()` — this migration's own
/// "duplicate column"-style tolerance, matching `M0046DirectionUpstream` —
/// finds the table non-empty and skips the lift, so it can never double the
/// same rows. Lift mapping, straight from the legacy sentinel convention:
/// `0` → no row; a positive id → one `resolved` edge; `-1`
/// (`DENIED_UPSTREAM_SENTINEL`) → one `denied` edge; `-2`
/// (`UNRESOLVED_UPSTREAM_SENTINEL`) → one `unresolved` edge. A `denied`/
/// `unresolved` row's `upstream_direction_id` is `0` ("not applicable") —
/// the legacy column never recorded WHICH name produced the sentinel
/// either, so there is nothing more to lift.
///
/// Uniqueness on `(direction_id, upstream_direction_id)` for `resolved`
/// edges is enforced at the application layer (`set_direction_upstreams`),
/// not by a DB index: sea-query's cross-backend `Index` builder has no
/// portable way to scope a unique index to `state = 'resolved'` only (a
/// SQLite partial index would work but is not expressible through the
/// shared migration API this codebase's other tables use), so this follows
/// the same precedent as `register_pull_request`'s app-level
/// find-then-upsert rather than adding a raw-SQL one-off here.
pub struct M0054DirectionDependency;
impl MigrationName for M0054DirectionDependency {
    fn name(&self) -> &str {
        "m0054_direction_dependency"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0054DirectionDependency {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        use sea_orm::{ConnectionTrait, Statement};

        let schema = Schema::new(manager.get_database_backend());
        let mut statement = schema.create_table_from_entity(direction_dependency::Entity);
        statement.if_not_exists();
        manager.create_table(statement).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_direction_dependency_direction")
                    .table(Alias::new("direction_dependency"))
                    .col(Alias::new("direction_id"))
                    .to_owned(),
            )
            .await?;

        let db = manager.get_connection();
        let backend = db.get_database_backend();

        // Rerun guard: a fresh table lifts once; a rerun (this migration's own
        // "duplicate column"-style tolerance) sees existing rows and skips —
        // never double-inserts the same legacy edge.
        let already_lifted = db
            .query_one(Statement::from_string(
                backend,
                "SELECT id FROM direction_dependency LIMIT 1".to_owned(),
            ))
            .await?
            .is_some();
        if already_lifted {
            return Ok(());
        }

        let rows = db
            .query_all(Statement::from_string(
                backend,
                "SELECT id, depends_on_direction_id FROM direction".to_owned(),
            ))
            .await?;
        for row in rows {
            let direction_id: i32 = match row.try_get("", "id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let legacy: i32 = match row.try_get("", "depends_on_direction_id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let (upstream_direction_id, state): (i32, &str) = match legacy {
                0 => continue, // no upstream declared — no row to lift.
                // DENIED_UPSTREAM_SENTINEL (repo.rs) — kept as a literal here since
                // migrations intentionally do not depend on `store::repo`.
                -1 => (0, "denied"),
                // UNRESOLVED_UPSTREAM_SENTINEL (repo.rs).
                -2 => (0, "unresolved"),
                positive => (positive, "resolved"),
            };
            db.execute(Statement::from_sql_and_values(
                backend,
                "INSERT INTO direction_dependency (direction_id, upstream_direction_id, state, created_at) VALUES (?, ?, ?, ?)",
                [
                    direction_id.into(),
                    upstream_direction_id.into(),
                    state.into(),
                    crate::store::repo::now_unix().into(),
                ],
            ))
            .await?;
        }
        Ok(())
    }

    /// Drops the table only. `depends_on_direction_id` is untouched — it is a
    /// maintained mirror, never solely populated by this migration, so a
    /// rollback of the new table leaves every existing reader (which only
    /// ever looked at the legacy column) working exactly as it did before
    /// this feature shipped.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("direction_dependency"))
                    .to_owned(),
            )
            .await
    }
}

/// The minimal Evidence ledger (issue #174 R1-04): append-only rows tying a
/// write's basis, verification, and revision together per Lane. See
/// `store::entities::evidence` for the column-by-column rationale and
/// `store::repo::append_evidence` for the append/supersede write path.
pub struct M0055Evidence;
impl MigrationName for M0055Evidence {
    fn name(&self) -> &str {
        "m0055_evidence"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0055Evidence {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());
        let mut statement = schema.create_table_from_entity(evidence::Entity);
        statement.if_not_exists();
        manager.create_table(statement).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_evidence_thread")
                    .table(Alias::new("evidence"))
                    .col(Alias::new("thread_id"))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_evidence_direction_kind")
                    .table(Alias::new("evidence"))
                    .col(Alias::new("direction_id"))
                    .col(Alias::new("kind"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("evidence")).to_owned())
            .await
    }
}

/// Issue #172: versioned dynamic scope + AuthorityPolicy judgment. Three
/// tables:
///
/// - `plan_revision`: the append-only scope history behind `plan`'s working
///   head (see that entity's own doc).
/// - `authority_policy`: the append-only AuthorityPolicy log (see that
///   entity's own doc) — active row = highest `revision` per (scope,
///   scope_id) with an empty `revoked_at`.
/// - `lane_gate_decision`: a human's per-Lane Gate resolution, keyed to the
///   exact policy revision it was decided under.
pub struct M0056AuthorityPolicy;
impl MigrationName for M0056AuthorityPolicy {
    fn name(&self) -> &str {
        "m0056_authority_policy"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for M0056AuthorityPolicy {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let schema = Schema::new(manager.get_database_backend());

        let mut plan_revision_stmt = schema.create_table_from_entity(plan_revision::Entity);
        plan_revision_stmt.if_not_exists();
        manager.create_table(plan_revision_stmt).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_plan_revision_thread")
                    .table(Alias::new("plan_revision"))
                    .col(Alias::new("thread_id"))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_plan_revision_thread_version")
                    .table(Alias::new("plan_revision"))
                    .col(Alias::new("thread_id"))
                    .col(Alias::new("version"))
                    .to_owned(),
            )
            .await?;

        let mut authority_policy_stmt = schema.create_table_from_entity(authority_policy::Entity);
        authority_policy_stmt.if_not_exists();
        manager.create_table(authority_policy_stmt).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_authority_policy_scope")
                    .table(Alias::new("authority_policy"))
                    .col(Alias::new("scope"))
                    .col(Alias::new("scope_id"))
                    .col(Alias::new("revoked_at"))
                    .to_owned(),
            )
            .await?;

        let mut lane_gate_decision_stmt = schema.create_table_from_entity(lane_gate_decision::Entity);
        lane_gate_decision_stmt.if_not_exists();
        manager.create_table(lane_gate_decision_stmt).await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_lane_gate_decision_direction_revision")
                    .table(Alias::new("lane_gate_decision"))
                    .col(Alias::new("direction_id"))
                    .col(Alias::new("policy_revision"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Alias::new("lane_gate_decision")).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Alias::new("authority_policy")).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Alias::new("plan_revision")).to_owned())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        gateway_components_to_backend, M0044EngineRoutingPin, M0045PullRequest,
        M0046DirectionUpstream, M0047PullRequestThreadStatus, M0048HumanRequest,
        M0049HumanRequestSourceMessage, M0050HumanRequestImRoutes,
        M0051HumanCardTerminalOutbox, M0052RepoActionExecution, M0053LeadHiddenDelivery,
        M0054DirectionDependency, M0055Evidence, M0056AuthorityPolicy,
    };

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

    /// M0041: the composite index exists AND SQLite actually plans the
    /// sweep's lookups through it. Asserting the query PLAN, not just
    /// `sqlite_master`, is the point: an index that exists but doesn't match
    /// the query's column order would still leave the sweep doing a full
    /// per-thread scan, and only the plan can tell those apart. Covers both
    /// shapes — session-scoped card lookups (`kind` + `session_id`) and the
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

        // A session-scoped timeline-kind lookup: all three equality columns
        // plus the ORDER BY.
        let marker = plan_for(
            "SELECT * FROM lead_message WHERE thread_id = 1 AND kind = 'action_card' \
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

        // The lead-scoped form keys `session_id IS NULL` instead.
        let lead_marker = plan_for(
            "SELECT * FROM lead_message WHERE thread_id = 1 AND kind = 'action_card' \
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

    #[tokio::test]
    async fn m0048_human_request_table_indexes_and_defaults_survive_rerun() {
        use crate::store::repo;
        use crate::store::Db;
        use sea_orm::{ConnectionTrait, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "Issue", "feature", "codex")
            .await
            .unwrap();
        let request = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            "lead",
            0,
            7,
            0,
            0,
            "Ship it?",
        )
        .await
        .unwrap();
        assert_eq!(request.status, "open");
        assert_eq!(request.revision, 1);
        assert!(request.answer.is_empty());

        // Migrations are expected to be safely repeatable in hand-repaired or
        // partially-upgraded databases.
        M0048HumanRequest
            .up(&SchemaManager::new(&db.0))
            .await
            .unwrap();
        M0049HumanRequestSourceMessage
            .up(&SchemaManager::new(&db.0))
            .await
            .unwrap();
        M0050HumanRequestImRoutes
            .up(&SchemaManager::new(&db.0))
            .await
            .unwrap();

        let indexes = db
            .0
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'human_request'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        assert!(indexes.contains("idx_human_request_workspace_status"));
        assert!(indexes.contains("idx_human_request_scope_status"));
        let columns = db
            .0
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "PRAGMA table_info(human_request)".to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        assert!(columns.contains("source_message_id"));
        assert!(columns.contains("source_session_id"));
        assert!(columns.contains("im_routes"));

        let restored = repo::get_human_request(&db, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.status, "open");
        assert_eq!(restored.source_message_id, 0);
        assert_eq!(restored.source_session_id, 0);
        assert_eq!(restored.im_routes, "[]");
    }

    #[tokio::test]
    async fn m0051_terminal_outbox_is_rerunnable_and_has_no_source_foreign_key() {
        use crate::store::repo::{self, HumanRequestImRoute, HUMAN_REQUEST_CANCELLED};
        use crate::store::Db;
        use sea_orm::{ConnectionTrait, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        M0051HumanCardTerminalOutbox
            .up(&SchemaManager::new(&db.0))
            .await
            .unwrap();
        let route = HumanRequestImRoute {
            channel: "feishu".to_string(),
            account: "cli_test".to_string(),
            owner: "ou_owner".to_string(),
            message_id: "om_orphan_safe".to_string(),
            terminal_revision: 0,
        };
        repo::queue_human_card_terminal_outbox(
            &db,
            999,
            42,
            &route,
            HUMAN_REQUEST_CANCELLED,
            "",
            2,
        )
        .await
        .unwrap();

        let rows = repo::list_pending_human_card_terminal_outbox(
            &db,
            &route.channel,
            &route.account,
            &route.owner,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, 999);
        assert_eq!(rows[0].thread_id, 42);
        let indexes = db
            .0
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'index' AND \
                 tbl_name = 'human_card_terminal_outbox'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        assert!(indexes.contains("idx_human_card_terminal_route"));
        assert!(indexes.contains("idx_human_card_terminal_pending"));
    }

    #[tokio::test]
    async fn m0052_repo_action_execution_is_rerunnable_with_unique_identity_indexes() {
        use crate::store::Db;
        use sea_orm::{ConnectionTrait, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let manager = SchemaManager::new(&db.0);
        M0052RepoActionExecution.up(&manager).await.unwrap();
        M0052RepoActionExecution.up(&manager).await.unwrap();

        let columns =
            db.0.query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "PRAGMA table_info(repo_action_execution)".to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        for column in [
            "workspace_id",
            "thread_id",
            "message_id",
            "action_id",
            "action_kind",
            "invocation_fingerprint",
            "execution_token",
            "status",
            "target_path",
            "staging_path",
            "repo_id",
            "repo_name",
        ] {
            assert!(columns.contains(column), "missing {column}");
        }
        let indexes =
            db.0.query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'index' AND \
                 tbl_name = 'repo_action_execution'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        assert!(indexes.contains("idx_repo_action_execution_message"));
        assert!(indexes.contains("idx_repo_action_execution_token"));
    }

    #[tokio::test]
    async fn m0053_hidden_delivery_is_rerunnable_and_deduped() {
        use crate::store::repo;
        use crate::store::Db;
        use sea_orm::{ConnectionTrait, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let manager = SchemaManager::new(&db.0);
        M0053LeadHiddenDelivery.up(&manager).await.unwrap();
        M0053LeadHiddenDelivery.up(&manager).await.unwrap();
        let workspace = repo::create_workspace(&db, "m0053").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let first = repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "plan_decision",
            7,
            "plan_decision:7",
            r#"{"tool":"plan_decision","message_id":7}"#,
        )
        .await
        .unwrap();
        let second = repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "plan_decision",
            7,
            "plan_decision:7",
            r#"{"tool":"plan_decision","message_id":7}"#,
        )
        .await
        .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(
            repo::list_pending_lead_hidden_deliveries(&db, Some(thread.id))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "test_cases_updated",
            8,
            "test_cases_updated:8",
            r#"{"tool":"test_cases_updated"}"#,
        )
        .await
        .is_err());
        let columns = db
            .0
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "PRAGMA table_info(lead_hidden_delivery)".to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        assert!(columns.contains("dedupe_key"));
        assert!(columns.contains("state"));
    }

    #[tokio::test]
    async fn m0055_evidence_is_rerunnable_with_expected_columns_and_indexes() {
        use crate::store::Db;
        use sea_orm::{ConnectionTrait, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let manager = SchemaManager::new(&db.0);
        M0055Evidence.up(&manager).await.unwrap();
        M0055Evidence.up(&manager).await.unwrap();

        let columns = db
            .0
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "PRAGMA table_info(evidence)".to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        for column in [
            "thread_id",
            "direction_id",
            "kind",
            "source",
            "source_ref",
            "observed_at",
            "revision",
            "policy_revision",
            "summary",
            "payload",
            "collection_state",
            "superseded_by",
        ] {
            assert!(columns.contains(column), "missing {column}");
        }
        let indexes = db
            .0
            .query_all(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'index' AND \
                 tbl_name = 'evidence'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .into_iter()
            .filter_map(|row| row.try_get::<String>("", "name").ok())
            .collect::<std::collections::HashSet<_>>();
        assert!(indexes.contains("idx_evidence_thread"));
        assert!(indexes.contains("idx_evidence_direction_kind"));
    }

    /// M0047 (issue #110): the same upgrade-path coverage M0046 gets, for the
    /// review-thread column — and the assertion that matters most is about
    /// what an EXISTING row gets, not that the column appeared.
    ///
    /// A row written before this migration has never had its review threads
    /// read. It must come out `""`, which `host::gate::parse_threads` maps to
    /// `ThreadStatus::Unknown`, which the auto-merge gate refuses. Any other
    /// default — most temptingly a serialized `all_resolved`, which would
    /// have made every existing row keep flowing through the gate unchanged —
    /// would hand a clean bill of health to precisely the rows nobody has
    /// ever checked, at the exact moment the checking code first ships.
    #[tokio::test]
    async fn m0047_thread_status_defaults_existing_rows_to_unknown_not_to_a_clear_value() {
        use crate::host::{gate, ThreadStatus};
        use sea_orm::{ConnectionTrait, Database, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        // Pre-M0047 shape: a `pull_request` table with the three sibling
        // status columns but no `thread_status` at all.
        db.execute(Statement::from_string(
            backend,
            "CREATE TABLE pull_request (id INTEGER PRIMARY KEY, ci_status TEXT NOT NULL DEFAULT '', \
             review_status TEXT NOT NULL DEFAULT '', conflict_status TEXT NOT NULL DEFAULT '')"
                .to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            backend,
            "INSERT INTO pull_request (id, ci_status, review_status, conflict_status) \
             VALUES (1, '{\"state\":\"passing\"}', '{\"state\":\"approved\"}', '{\"state\":\"clean\"}')"
                .to_owned(),
        ))
        .await
        .unwrap();

        M0047PullRequestThreadStatus.up(&SchemaManager::new(&db)).await.unwrap();

        let read = |db: &sea_orm::DatabaseConnection| {
            let stmt = Statement::from_string(
                backend,
                "SELECT thread_status FROM pull_request WHERE id = 1".to_owned(),
            );
            let db = db.clone();
            async move {
                let row = db.query_one(stmt).await.unwrap().unwrap();
                row.try_get::<String>("", "thread_status").unwrap()
            }
        };

        let stored = read(&db).await;
        assert_eq!(stored, "", "an existing pre-M0047 row carries no thread reading at all");
        assert!(
            matches!(gate::parse_threads(&stored), ThreadStatus::Unknown { .. }),
            "the migration default must decode to Unknown — a row nobody has checked must not \
             read as clear, got {:?}",
            gate::parse_threads(&stored)
        );

        // A real reading, written the way `apply_pull_request_snapshot` does.
        let all_resolved = serde_json::to_string(&ThreadStatus::AllResolved).unwrap();
        db.execute(Statement::from_string(
            backend,
            format!("UPDATE pull_request SET thread_status = '{all_resolved}' WHERE id = 1"),
        ))
        .await
        .unwrap();

        // A rerun (the "duplicate column" catch in `up()`) must succeed and
        // must not reset an already-recorded reading back to the default.
        M0047PullRequestThreadStatus.up(&SchemaManager::new(&db)).await.unwrap();
        assert_eq!(
            gate::parse_threads(&read(&db).await),
            ThreadStatus::AllResolved,
            "a rerun must not clobber a reading the monitor already took"
        );
    }

    /// M0037: code_checkpoint exists after migration and round-trips a row.
    #[tokio::test]
    async fn m0037_code_checkpoint_table_created() {
        use crate::store::repo::{
            code_checkpoint_for, create_direction, create_session, create_thread,
            insert_code_checkpoint, insert_lead_message, record_worktree,
        };
        use crate::store::Db;

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = crate::store::repo::create_workspace(&db, "m0037").await.unwrap();
        let repo = crate::store::repo::add_repo_ref(
            &db,
            workspace.id,
            "repo",
            "/tmp/m0037-repo",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let direction = create_direction(
            &db,
            thread.id,
            "task",
            "codex",
            repo.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session = create_session(&db, direction.id, repo.id, "codex", "/tmp/m0037-session")
            .await
            .unwrap();
        let worktree = record_worktree(
            &db,
            repo.id,
            direction.id,
            "m0037",
            "/tmp/m0037-worktree",
            true,
            true,
            "",
        )
        .await
        .unwrap();
        let message = insert_lead_message(
            &db,
            thread.id,
            Some(session.id),
            1,
            "user",
            "text",
            "{}",
            "complete",
        )
        .await
        .unwrap();
        let c = insert_code_checkpoint(
            &db,
            worktree.id,
            session.id,
            message.id,
            message.turn_id,
            "shadow-sha",
            "head-sha",
            "[\"gen\"]",
            "idx-1",
        )
            .await
            .unwrap();
        let found = code_checkpoint_for(&db, worktree.id, message.id)
            .await
            .unwrap()
            .unwrap();
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

    /// M0054 (issue #173): the DAG upgrade's own upgrade-path test — mirrors
    /// `m0046_direction_upstream_column_defaults_existing_rows_to_zero_and_reruns_safely`'s
    /// shape. Starts from a pre-M0054 `direction` table (the legacy single-slot
    /// column already present, no `direction_dependency` table at all — exactly
    /// what a real user's database looks like right before upgrading, including
    /// rows already sitting on the `-1`/`-2` sentinels) and verifies: every legacy
    /// value lifts to the right edge row, `0` lifts to nothing, and a RERUN does
    /// not double the lifted rows.
    #[tokio::test]
    async fn m0054_direction_dependency_lifts_legacy_column_values_and_reruns_safely() {
        use sea_orm::{ConnectionTrait, Database, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        // Pre-M0054 shape: `direction` already carries the legacy single-slot
        // column (M0046 shipped long before this migration) but no
        // `direction_dependency` table exists yet.
        db.execute(Statement::from_string(
            backend,
            "CREATE TABLE direction (id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
             depends_on_direction_id INTEGER NOT NULL DEFAULT 0)"
                .to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            backend,
            "INSERT INTO direction (id, name, depends_on_direction_id) VALUES \
             (1, 'no-upstream', 0), \
             (2, 'producer', 0), \
             (3, 'resolved-consumer', 2), \
             (4, 'denied-consumer', -1), \
             (5, 'unresolved-consumer', -2)"
                .to_owned(),
        ))
        .await
        .unwrap();

        M0054DirectionDependency
            .up(&SchemaManager::new(&db))
            .await
            .unwrap();

        async fn edges_for(db: &sea_orm::DatabaseConnection, direction_id: i32) -> Vec<(i32, String)> {
            let backend = db.get_database_backend();
            db.query_all(Statement::from_string(
                backend,
                format!(
                    "SELECT upstream_direction_id, state FROM direction_dependency \
                     WHERE direction_id = {direction_id} ORDER BY id"
                ),
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                let upstream: i32 = row.try_get("", "upstream_direction_id").unwrap();
                let state: String = row.try_get("", "state").unwrap();
                (upstream, state)
            })
            .collect()
        }

        assert_eq!(
            edges_for(&db, 1).await,
            Vec::<(i32, String)>::new(),
            "0 lifts to no rows — never invent a dependency"
        );
        assert_eq!(
            edges_for(&db, 3).await,
            vec![(2, "resolved".to_string())],
            "a positive legacy value lifts to one resolved edge naming that upstream"
        );
        assert_eq!(
            edges_for(&db, 4).await,
            vec![(0, "denied".to_string())],
            "-1 (DENIED_UPSTREAM_SENTINEL) lifts to one denied edge, upstream 0 (not applicable)"
        );
        assert_eq!(
            edges_for(&db, 5).await,
            vec![(0, "unresolved".to_string())],
            "-2 (UNRESOLVED_UPSTREAM_SENTINEL) lifts to one unresolved edge"
        );

        // Rerun must not double the lifted rows (this migration's own
        // "duplicate column"-style tolerance, matching M0046's rerun test).
        M0054DirectionDependency
            .up(&SchemaManager::new(&db))
            .await
            .unwrap();
        assert_eq!(
            edges_for(&db, 3).await,
            vec![(2, "resolved".to_string())],
            "a rerun must not duplicate the already-lifted edge"
        );
        assert_eq!(
            edges_for(&db, 4).await.len(),
            1,
            "a rerun must not duplicate the already-lifted denied edge"
        );
    }

    /// M0056: `plan_revision`, `authority_policy`, and `lane_gate_decision` are
    /// created from scratch (a fresh DB has none of the three), a row can be
    /// written+read through each entity's real accessors end to end, and a
    /// rerun of `up` tolerates the already-existing tables/indexes (mirrors
    /// every other migration's own `if_not_exists` rerun tolerance test).
    #[tokio::test]
    async fn m0056_authority_policy_tables_created_and_rerun_safe() {
        use crate::store::repo;
        use crate::store::Db;
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "issue", "claude")
            .await
            .unwrap();

        // plan_revision: insert + read back through the real accessor.
        let rev = repo::insert_plan_revision(&db, thread.id, "1-0", "{}", "lead")
            .await
            .unwrap();
        assert_eq!(rev.thread_id, thread.id);
        let latest = repo::latest_plan_revision(&db, thread.id).await.unwrap();
        assert_eq!(latest.map(|r| r.id), Some(rev.id));

        // authority_policy: create + read back as the active row.
        let policy = repo::create_authority_policy(&db, "workspace", ws.id, "{}", "user")
            .await
            .unwrap();
        assert_eq!(policy.revision, "1");
        let active = repo::get_active_authority_policy(&db, "workspace", ws.id)
            .await
            .unwrap();
        assert_eq!(active.map(|p| p.id), Some(policy.id));

        // lane_gate_decision: needs a real direction row (FK-free, but the
        // accessor's own contract is "one row per direction+policy_revision").
        let dir = repo::create_direction(
            &db,
            thread.id,
            "lane-a",
            "claude",
            repo_ref.id,
            "because",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        repo::record_gate_decision(&db, dir.id, &policy.revision, "approved", "looked fine")
            .await
            .unwrap();
        let gate = repo::get_gate_decision(&db, dir.id, &policy.revision)
            .await
            .unwrap();
        assert_eq!(gate.map(|g| g.decision), Some("approved".to_string()));

        // Rerun tolerance: `up` again must not fail on already-existing tables/indexes.
        M0056AuthorityPolicy
            .up(&SchemaManager::new(&db.0))
            .await
            .unwrap();
        // The pre-rerun rows must survive untouched.
        let still_active = repo::get_active_authority_policy(&db, "workspace", ws.id)
            .await
            .unwrap();
        assert_eq!(still_active.map(|p| p.id), Some(policy.id));
    }

    /// M0056 `down` drops all three tables it created, leaving nothing behind
    /// — the rollback-compatibility bar every migration in this file carries.
    #[tokio::test]
    async fn m0056_authority_policy_down_drops_every_table() {
        use sea_orm::{ConnectionTrait, Database, Statement};
        use sea_orm_migration::{MigrationTrait, SchemaManager};

        let db = Database::connect("sqlite::memory:").await.unwrap();
        let manager = SchemaManager::new(&db);
        M0056AuthorityPolicy.up(&manager).await.unwrap();
        M0056AuthorityPolicy.down(&manager).await.unwrap();

        let backend = db.get_database_backend();
        for table in ["plan_revision", "authority_policy", "lane_gate_decision"] {
            let rows = db
                .query_all(Statement::from_string(
                    backend,
                    format!(
                        "SELECT name FROM sqlite_master WHERE type='table' AND name='{table}'"
                    ),
                ))
                .await
                .unwrap();
            assert!(rows.is_empty(), "{table} must not exist after down()");
        }
    }
}
