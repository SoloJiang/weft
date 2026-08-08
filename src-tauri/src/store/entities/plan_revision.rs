use sea_orm::entity::prelude::*;

/// One immutable snapshot of a thread's dynamic scope proposal (issue #172).
/// `plan` (see `store::entities::plan`) stays the WORKING HEAD a human edits
/// in place; this table is the append-only history behind it — one row per
/// `save_proposal_value` (a lead re-propose) or per `confirm`/`approve_direction`
/// (a human decision), so "which scope revision was this Lane materialized
/// against" is always answerable, not just "what does the plan say right
/// now". Never updated or deleted after insert.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "plan_revision")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub thread_id: i32,
    /// The monotonic OCC token (`planner::proposal_version`'s `{nanos}-{seq}`
    /// shape) this snapshot was taken at — the same value `plan.created_at`
    /// carries at that instant. A re-propose always mints a FRESH version, but
    /// confirm/approve/deny act on the CURRENT version without minting a new
    /// one (only a re-propose changes `plan.created_at`), so more than one row
    /// CAN share a version — e.g. a "lead" row from the propose that
    /// established it, then a "user" row recording the confirm decided
    /// against that same version. Ordered by `id`, not assumed unique.
    pub version: String,
    /// JSON snapshot of the proposal at this revision: `{rationale,
    /// directions:[...]}, same shape as `plan.proposal`.
    pub proposal: String,
    /// "lead" (a fresh `propose_directions`/re-propose) | "user" (a human
    /// confirm/approve/deny/edit) | "system" (a server-driven re-save, e.g.
    /// hint/depends_on normalization carried forward with no human input).
    pub source: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
