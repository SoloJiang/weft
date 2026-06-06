use sea_orm::entity::prelude::*;

/// The curator's map entry for one repo (ARCHITECTURE §4.9). Inferred facts are
/// stored so the dependency graph can be recomputed without re-reading disk;
/// `source` records authority (a user edit outranks inference) and
/// `profiled_commit` lets us flag the profile stale when the repo moves on.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "repo_profile")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub repo_id: i32,
    pub role: String,
    /// JSON array of stack tags.
    pub stack: String,
    pub summary: String,
    /// JSON array of identifiers this repo publishes.
    pub published: String,
    /// JSON array of declared dependency identifiers.
    pub deps: String,
    /// "inferred" | "user" — user edits win over re-profiling.
    pub source: String,
    /// Short HEAD sha when last profiled (empty if unknown).
    pub profiled_commit: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
