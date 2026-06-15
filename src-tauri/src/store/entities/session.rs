use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "session")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub direction_id: i32,
    pub repo_id: i32,
    pub tool: String,
    /// Optional per-session command pin overriding the global tool→command map.
    /// NULL = follow the global override; set to freeze this worker to its prior
    /// command when the user excludes existing sessions from a later override.
    pub command: Option<String>,
    pub cwd: String,
    pub native_session_id: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
