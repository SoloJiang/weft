use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "session")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub direction_id: i32,
    pub repo_id: i32,
    pub tool: String,
    /// Carries the direction/manual-switch provenance into the live worker so
    /// quota failover cannot overwrite an explicit engine selection.
    #[sea_orm(default_value = true)]
    pub engine_pinned: bool,
    /// Optional per-session command pin overriding the global tool→command map.
    /// NULL = follow the global override; set to freeze this worker to its prior
    /// command when the user excludes existing sessions from a later override.
    pub command: Option<String>,
    /// Optional per-session model override (issue #98); see `thread.lead_model`
    /// for the semantics (free-text `--model` value, claude/codex only). Ephemeral
    /// like `command`: a session recreated via `chat_open_worker`'s cold path
    /// (no prior `native_session_id`) starts fresh with no override — the durable
    /// per-task setting is `tool` (mirrored onto `direction.tool`), not `model`.
    pub model: Option<String>,
    pub cwd: String,
    pub native_session_id: Option<String>,
    pub status: String,
    /// Last-known engine meta snapshot (JSON `PersistedMeta`), mirror of
    /// thread.lead_meta for chat-mode workers. Empty = never captured.
    #[sea_orm(default_value = "")]
    pub meta: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
