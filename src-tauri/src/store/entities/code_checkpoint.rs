use sea_orm::entity::prelude::*;

/// One pre-turn code checkpoint for a worker session: the shadow-repo commit
/// (`shadow_sha`) capturing the worktree state right BEFORE `lead_message_id`'s
/// turn started, plus the real repo's HEAD at that moment. Recorded at every
/// worker user-turn start (weft-created checkouts only); consumed by code
/// rewind, which restores the worktree to the checkpoint of a chosen message.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "code_checkpoint")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub worktree_id: i32,
    pub session_id: i32,
    /// The user row that opened the snapshotted turn — a code rewind looks the
    /// checkpoint up by the message the user picked.
    pub lead_message_id: i32,
    pub turn_id: i32,
    /// Commit on `refs/heads/s<session_id>` in the worktree's shadow repo
    /// (`<weft_home>/checkpoints/<worktree_id>.git`).
    pub shadow_sha: String,
    /// Real repo HEAD at snapshot time: the rewind resets the lane branch back
    /// to it when the agent committed after the snapshot (never across the
    /// worktree's base_commit).
    pub head_sha: String,
    /// JSON array of nested git repo dirs (relative paths) present at snapshot
    /// time. `git clean -fd` never removes nested repositories, so a restore
    /// deletes exactly the nested repos NOT in this list (created after the
    /// checkpoint) and keeps the ones that were already there.
    #[sea_orm(default_value = "[]")]
    pub nested_repos: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
