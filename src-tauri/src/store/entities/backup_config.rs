use sea_orm::entity::prelude::*;

/// Singleton config row (id = 1) for git-remote backup. Holds both user
/// preferences (`remote_url`, `auto_backup_enabled`, `interval_seconds`,
/// `backup_on_exit`) and the last-run status (`last_backup_*`, `last_error`).
/// Timestamps are unix-seconds-as-string, matching the convention used by
/// the rest of the schema (e.g. workspace.created_at).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "backup_config")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub enabled: bool,
    pub remote_url: String,
    pub auto_backup_enabled: bool,
    pub interval_seconds: i64,
    pub backup_on_exit: bool,
    pub last_backup_at: Option<String>,
    pub last_backup_commit_sha: Option<String>,
    pub last_backup_bytes: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
