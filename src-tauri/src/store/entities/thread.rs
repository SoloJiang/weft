use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "thread")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub workspace_id: i32,
    pub title: String,
    pub slug: String,
    pub kind: String,
    /// The coding CLI driving this thread's lead, stamped at creation.
    pub lead_tool: String,
    /// Optional per-thread command pin overriding the global tool→command map
    /// for this lead. NULL = follow the global override (and any later change).
    /// Set when the user excludes existing sessions from a newly-configured
    /// command override, freezing this lead to its prior command.
    pub lead_command: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
