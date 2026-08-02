use sea_orm::entity::prelude::*;

/// Provider terminal-card work that must outlive deletion of its source
/// thread/request. Rows never keep question text; a pending resolved PATCH may
/// temporarily carry its final answer, which is scrubbed with the delivery
/// receipt. The remaining opaque route tombstone guards against late events.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "human_card_terminal_outbox")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Diagnostic identity only; deliberately not a foreign key because this
    /// row must survive human_request/thread/workspace cascades.
    pub request_id: i32,
    /// Inbound stale-reply routing only; also deliberately not a foreign key.
    pub thread_id: i32,
    pub channel: String,
    pub account: String,
    pub owner: String,
    pub message_id: String,
    /// cancelled today; shaped as a lifecycle value so future terminal states
    /// can share the same durable delivery primitive.
    pub terminal_status: String,
    #[sea_orm(default_value = "")]
    pub answer: String,
    pub terminal_revision: i32,
    #[sea_orm(default_value = false)]
    pub delivered: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
