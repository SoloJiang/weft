use sea_orm::entity::prelude::*;

/// Durable ownership and recovery record for one repository action-card
/// execution. `message_id` is unique: one card can materialize at most one
/// invocation, while an exact replay resumes the same token and paths.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "repo_action_execution")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub workspace_id: i32,
    pub thread_id: i32,
    pub message_id: i32,
    pub action_id: String,
    pub action_kind: String,
    /// SHA-1 of the normalized invocation. Clone credentials and raw URLs are
    /// never persisted in this row.
    pub invocation_fingerprint: String,
    /// Random capability used both for compare-and-swap writes and filesystem
    /// ownership markers.
    pub execution_token: String,
    /// pending | materialized | completed | cleanup_pending
    pub status: String,
    pub target_path: String,
    #[sea_orm(default_value = "")]
    pub staging_path: String,
    #[sea_orm(default_value = 0)]
    pub repo_id: i32,
    #[sea_orm(default_value = "")]
    pub repo_name: String,
    /// none | pending | delivered. The execution row doubles as a durable
    /// outbox so a WebView reload after completion cannot lose the lead result.
    #[sea_orm(default_value = "none")]
    pub feedback_state: String,
    /// Canonical credential-free `<weft:repo_action>` JSON payload. Delivery
    /// is retried from this persisted value; clone URLs are never stored.
    #[sea_orm(default_value = "")]
    pub feedback_payload: String,
    /// A delete cascade retains cleanup-pending rows after removing their
    /// parent. True means the target became a registered user repository and
    /// cleanup may remove only its token marker, never the checkout itself.
    #[sea_orm(default_value = false)]
    pub cleanup_preserve_target: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
