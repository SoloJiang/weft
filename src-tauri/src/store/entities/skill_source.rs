use sea_orm::entity::prelude::*;

/// A git-hosted skill source. Cloned to ~/.weft/skills/sources/<id>/.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "skill_source")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub git_url: String,
    /// Optional branch/tag to track; empty = remote default branch.
    pub git_ref: String,
    /// Unix-secs string of the last successful sync, empty if never.
    pub last_synced: String,
    /// "never" | "ok" | "error:<msg>".
    pub last_status: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
