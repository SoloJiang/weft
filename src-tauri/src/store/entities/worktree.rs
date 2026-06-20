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
    /// Rollback deletes the branch only when this is true, preserving pre-existing branches.
    #[sea_orm(default_value = false)]
    pub created_branch: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
