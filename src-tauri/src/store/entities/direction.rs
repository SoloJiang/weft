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
    /// LEGACY single-slot cross-repo ordering edge (producer → consumer). `0` = no upstream,
    /// same convention as `repo_id`; `-1`/`-2` are the denied/unresolved sentinels (see
    /// `store::repo::DENIED_UPSTREAM_SENTINEL` / `UNRESOLVED_UPSTREAM_SENTINEL`).
    ///
    /// Issue #173 (R1-03) upgraded this to a real many-to-many DAG (see
    /// `store::entities::direction_dependency`): a Lane's real dependency set is the
    /// `direction_dependency` rows keyed by this direction's id, not this column. This column
    /// SURVIVES as a maintained, fail-closed MIRROR — the sole writer is
    /// `store::repo::set_direction_upstreams` — so any old reader of this column, and a
    /// rollback of the DAG feature, keep working off the exact same sentinel convention they
    /// always have. Never write it directly; go through `set_direction_upstreams` (or its
    /// single-edge compat wrapper `set_direction_upstream`).
    #[sea_orm(default_value = 0)]
    pub depends_on_direction_id: i32,
    /// Explicit engine selections are pins. Legacy rows stay pinned; a new
    /// route-derived direction is explicitly created unpinned.
    #[sea_orm(default_value = true)]
    pub engine_pinned: bool,
    /// Worker mandate, assigned with the role: "plan+impl" (plan its own
    /// direction first, then build) or "impl-only" (fully scoped — build
    /// straight away). The brief renders per-mandate.
    #[sea_orm(default_value = "plan+impl")]
    pub mandate: String,
    /// Diff-panel "vs target" comparison branch (per-task). Empty = use the
    /// repo's default branch (repo_ref.base_ref). The diff compares the task's
    /// changes against the merge-base with `origin/<target_branch>`.
    #[sea_orm(default_value = "")]
    pub target_branch: String,
    /// The ref the work branch is created OFF at materialize time (the base).
    /// Empty = the repo's default branch, resolved live from origin/HEAD. Distinct
    /// from `target_branch` (diff-comparison only); fixed once the worktree exists.
    #[sea_orm(default_value = "")]
    pub base_branch: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
