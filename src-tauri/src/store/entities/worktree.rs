use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "worktree")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub repo_id: i32,
    pub direction_id: i32,
    pub branch: String,
    pub path: String,
    pub created_at: String,
    /// Whether Weft created this branch (as opposed to checking out a pre-existing one).
    /// Rollback/cascade cleanup deletes the branch only when this is true, preserving
    /// pre-existing branches. Existing rows default to true: every pre-this-change
    /// worktree had its branch created by Weft, so its branch is torn down on teardown.
    #[sea_orm(default_value = true)]
    pub created_branch: bool,
    /// Whether Weft created this checkout directory (vs. reusing a pre-existing worktree path).
    /// Rollback and cleanup remove the worktree directory only when this is true. Existing rows
    /// default to true (they are genuine Weft checkouts; safe to remove on teardown).
    #[sea_orm(default_value = true)]
    pub created_checkout: bool,
    /// The COMMIT this worktree's work branch was forked from at create time (the resolved
    /// base's tip, captured only on the `worktree add -b <branch> <resolved>` success path).
    /// Reuse-time validation checks the work branch still DESCENDS from this stable commit —
    /// not a re-resolved (moving) base NAME — so an advanced base or a local-fork/diverged-origin
    /// lane is not false-rejected, while an externally reset branch is still caught. Empty for
    /// legacy/reuse/fallback rows, which SKIP that validation.
    #[sea_orm(default_value = "")]
    pub base_commit: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
