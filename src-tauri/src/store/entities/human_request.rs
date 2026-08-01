use sea_orm::entity::prelude::*;

/// Durable agent-to-human question. This is intentionally narrow: permission
/// prompts remain ephemeral in `AskRegistry`, while only free-text questions
/// survive an app restart.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "human_request")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub workspace_id: i32,
    pub thread_id: i32,
    /// Numeric direction id, or 0 for the lead.
    #[sea_orm(default_value = 0)]
    pub direction_id: i32,
    /// Exact bus address (direction id string or "lead").
    pub direction_scope: String,
    /// Source turn when known; 0 for bus clients that do not expose one.
    #[sea_orm(default_value = 0)]
    pub turn_id: i32,
    pub question: String,
    /// open | resolved | cancelled | superseded
    #[sea_orm(default_value = "open")]
    pub status: String,
    #[sea_orm(default_value = "")]
    pub answer: String,
    /// OCC token incremented on every lifecycle transition.
    #[sea_orm(default_value = 1)]
    pub revision: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
