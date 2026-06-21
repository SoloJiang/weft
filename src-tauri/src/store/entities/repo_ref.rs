use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "repo_ref")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub workspace_id: i32,
    pub name: String,
    pub slug: String,
    pub local_git_path: String,
    pub base_ref: String,
    /// The raw `origin` remote URL captured at add/clone time (empty for a local
    /// repo with no origin). Compared via `git::git_url_key` for workspace-level
    /// dedup so the same remote cloned at two paths isn't added twice.
    #[sea_orm(default_value = "")]
    pub remote_url: String,
    /// Whether `base_ref` was captured as the repo's real DEFAULT branch (true) vs.
    /// a legacy current-branch capture on an upgraded DB (false). The offline
    /// fallback (`git::recorded_base_or_default`) only trusts `base_ref` over the
    /// main/master default chain when this is true — a legacy base_ref (even a
    /// pushed feature branch whose `origin/<base_ref>` resolves) is otherwise
    /// indistinguishable by value from a genuine non-standard default. Existing rows
    /// default to false (their base_ref was the current-branch capture).
    #[sea_orm(default_value = false)]
    pub base_ref_is_default: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
