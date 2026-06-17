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
    /// The repo's architectural tier as the agent classified it: "frontend" |
    /// "gateway" | "backend", or "" when analysis hasn't run yet. (Column name is
    /// historical — it held the deterministic role before the curator went
    /// agent-only.)
    pub role: String,
    /// JSON array of stack tags (agent-reported).
    pub stack: String,
    pub summary: String,
    /// Legacy columns from the deterministic engine, no longer populated (kept so
    /// upgraded dbs need no destructive migration). Always "[]".
    pub published: String,
    pub deps: String,
    /// "agent" (inferred) | "user" — user edits win over re-analysis.
    pub source: String,
    /// Short HEAD sha when last profiled (empty if unknown).
    pub profiled_commit: String,
    /// JSON array of `profile::AgentRelation` — the agent curator's inferred
    /// cross-repo relations (service-to-service, infra, …). "[]" until analyzed.
    /// Preserved across re-analysis of other fields.
    #[sea_orm(default_value = "[]")]
    pub relations: String,
    /// JSON array of `profile::Component` — monorepo sub-components from the
    /// per-repo deep agent pass, powering the map's "expanded" view. "[]" until
    /// analyzed / for a single-component repo.
    #[sea_orm(default_value = "[]")]
    pub components: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
