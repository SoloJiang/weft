use sea_orm::entity::prelude::*;

/// An issue's test-case document (0..1 per thread): a markdown tree the lead
/// derives in phase 1.5 (before shaping the technical plan) and the user can
/// edit. `content` is the single source of truth; chat rows only carry a
/// summary. Rendered as a mindmap in the TestPlanPanel.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "test_plan")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub thread_id: i32,
    /// Markdown tree: `#` title + nested unordered lists; leaves are cases.
    pub content: String,
    /// Last writer: "lead" (sentinel) or "user" (panel edit).
    pub source: String,
    /// Unix seconds, same convention as the other stores.
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
