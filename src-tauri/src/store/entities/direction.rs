use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "direction")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub thread_id: i32,
    pub name: String,
    pub slug: String,
    pub tool: String,
    pub branch: String,
    /// Agent/human-driven lifecycle: queued | planning | working | review | done.
    /// Reversible; weft never forces it (an open ask overlays Needs-you in the UI).
    #[sea_orm(default_value = "queued")]
    pub status: String,
    /// The one repo this direction writes (scope rework, spec Part 1). FK into
    /// repo_ref. 0 = unset (shouldn't happen for a confirmed write direction).
    #[sea_orm(default_value = 0)]
    pub repo_id: i32,
    /// Why this repo must change — the lead's required justification, surfaced
    /// in Needs-you and kept for audit.
    #[sea_orm(default_value = "")]
    pub reason: String,
    /// Worker mandate, assigned with the role: "plan+impl" (plan its own
    /// direction first, then build) or "impl-only" (fully scoped — build
    /// straight away). The brief renders per-mandate.
    #[sea_orm(default_value = "plan+impl")]
    pub mandate: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
