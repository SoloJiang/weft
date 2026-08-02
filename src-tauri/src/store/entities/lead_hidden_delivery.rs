use sea_orm::entity::prelude::*;

/// Durable hidden input handed to a lead engine. The row survives a stopped
/// process and is acknowledged only after the engine emits its first activity
/// for the corresponding hidden turn.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "lead_hidden_delivery")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub thread_id: i32,
    /// Durable source kinds with a stable source id: plan_decision | repo_action.
    pub source_kind: String,
    /// Source row identity when one exists (plan message or repo execution).
    #[sea_orm(default_value = 0)]
    pub source_id: i32,
    /// Stable retry identity. A unique key makes repeated clicks/replays
    /// converge on one agent-bound delivery.
    pub dedupe_key: String,
    /// JSON payload retained verbatim for replay, including its source id.
    pub payload: String,
    /// pending | consumed.
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
