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
    /// text | tool | command | proposal | approval | worker_event | meta | settled
    pub kind: String,
    /// kind-shaped JSON, e.g. {"text": "..."} for kind=text
    pub content: String,
    /// streaming | complete | interrupted | error | queued
    pub status: String,
    pub created_at: String,
    /// Delivery-order key: set when a queued row is delivered so reordered
    /// messages appear in send (not creation) order. NULL = use id for ordering.
    pub seq: Option<i64>,
    /// Native rewind anchor, recorded on the user row that opened a turn:
    /// claude = uuid of the turn's last assistant event, codex app-server =
    /// turn id. NULL for rows that predate anchoring or never completed a turn.
    pub native_anchor: Option<String>,
    /// Unix-milliseconds when the agent produced its first observed activity
    /// (any stdout line / app-server event) for the turn this row opened — the
    /// "已被 agent 消费" receipt (issue #94). Set at most once, on a "user" row
    /// with status "complete"; NULL until then and for every other row.
    pub consumed_at: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
