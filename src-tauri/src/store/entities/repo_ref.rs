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
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
