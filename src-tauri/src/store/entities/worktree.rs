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
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
