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
    /// "backend", or "" when analysis hasn't run yet. (Column name is historical —
    /// it held the deterministic role before the curator went agent-only.)
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
    /// Live analysis run-state, persisted so a repo "running" at shutdown can be
    /// resumed (not silently dropped to idle). "idle" | "running" | "failed".
    #[sea_orm(default_value = "idle")]
    pub analysis_state: String,
    /// Error from the last failed analysis (NULL unless analysis_state == "failed").
    pub analysis_error: Option<String>,
    /// Role category within the tier (free-text, agent-assigned): e.g. backend →
    /// gateway/biz/core/common/idl/support; frontend → app/sdk/web. "" until classified.
    #[sea_orm(default_value = "")]
    pub category: String,
    /// JSON array of owned feature domains (agent-assigned). "[]" until classified.
    #[sea_orm(default_value = "[]")]
    pub domains: String,
    /// Architectural layer label, assigned by the cross-repo curator pass (which sees
    /// the whole workspace, so the labels are consistent across repos). Free text the
    /// agent names itself, e.g. "Foundation"/"Core 核心"/"Client". "" until classified.
    /// Drives the repo map's stacked-band layout; this is the band's header text.
    #[sea_orm(default_value = "")]
    pub layer: String,
    /// Vertical rank of `layer`: higher = closer to the user (client/top), 0 = most
    /// foundational (bottom). The agent reuses the same label+rank for every repo in a
    /// layer so the map can aggregate them into one band. 0 until classified.
    #[sea_orm(default_value = "0")]
    pub layer_rank: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
