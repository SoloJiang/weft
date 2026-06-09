use sea_orm::entity::prelude::*;

/// One row in a chat timeline (the lead console; chat-mode workers reuse it via
/// `session_id`). `content` is kind-shaped JSON; `session_id` is NULL for lead
/// messages, whose timeline is keyed by `thread_id` alone.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "lead_message")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub thread_id: i32,
    pub session_id: Option<i32>,
    pub turn_id: i32,
    /// user | assistant | system
    pub role: String,
    /// text | tool | command | proposal | approval | worker_event | meta
    pub kind: String,
    /// kind-shaped JSON, e.g. {"text": "..."} for kind=text
    pub content: String,
    /// streaming | complete | interrupted | error | queued
    pub status: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
