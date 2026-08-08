use sea_orm::entity::prelude::*;

/// One append-only ledger row (issue #174 R1-04): the minimal Evidence
/// account that connects a write's basis, its verification result, and the
/// revision it was observed against. Diff/checks/PR/review/AuthorityPolicy
/// facts are currently scattered across `pull_request`, in-memory check
/// caches, and derived readiness reasoning — this table is the one place a
/// user (or a later automated reconciler) can enumerate "what did we know,
/// from where, as of which commit" for one Lane.
///
/// Append-only: a fact is never edited in place. A newer observation for the
/// same identity (`direction_id`, `kind`, `source`, `source_ref`) is inserted
/// as a new row, and the previous latest non-superseded row for that identity
/// is stamped with this row's id in `superseded_by` — see
/// `repo::append_evidence`. This is what lets a drift row survive a later
/// CLI-reported "success": the drift row itself is never overwritten, only
/// superseded by whatever comes next, so the history stays traceable.
///
/// `source_ref` and `summary`/`payload` are bounded pointers/text, never raw
/// unbounded log capture — see `repo::truncate_bounded` and
/// `repo::redact_secrets`, both applied at every write site before a row is
/// persisted.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "evidence")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// The owning issue/thread. Always set — every evidence row belongs to
    /// exactly one thread even when it is issue-level (`direction_id == 0`).
    pub thread_id: i32,
    /// The Lane this fact is about. `0` = issue-level (no single Lane owns
    /// it — mirrors `pull_request.direction_id`'s "0 = unset/unbound"
    /// convention). Deliberately NOT a foreign key: a Lane (direction) can be
    /// deleted while its historical evidence remains (see the deletion-policy
    /// tests in `store::repo::tests`) — a dangling `direction_id` is expected,
    /// not a data-integrity failure.
    #[sea_orm(default_value = 0)]
    pub direction_id: i32,
    /// "code" | "verification" | "interface" | "host" | "execution" |
    /// "decision" | "handoff" — the minimal Evidence type list from issue
    /// #174. See `store::repo::EVIDENCE_KIND_*`.
    pub kind: String,
    /// What collected this fact, e.g. "check_flight" | "host_monitor" |
    /// "reconciliation" | "planner". Free text but drawn from a small
    /// producer set — see `store::repo::EVIDENCE_SOURCE_*`.
    pub source: String,
    /// A bounded pointer to the origin, never the origin's full content: a PR
    /// URL, a repo name, a command name, a message id. Combined with `kind`
    /// and `source`, this is the append/supersede identity — see
    /// `repo::append_evidence`.
    #[sea_orm(default_value = "")]
    pub source_ref: String,
    /// Unix seconds (string, same convention as the rest of this store) this
    /// fact was actually observed — not when the row was inserted (though
    /// today those are the same instant for every write site).
    pub observed_at: String,
    /// The git HEAD sha or plan OCC version this observation is anchored to.
    /// "" when unknown/not applicable (e.g. a decision evidence row before
    /// #172's real `AuthorityPolicy` revision exists). See
    /// `readiness::evidence_freshness`, which stale-checks revision-anchored
    /// kinds against this column.
    #[sea_orm(default_value = "")]
    pub revision: String,
    /// The plan's OCC version (`plan.created_at`) in effect when a decision
    /// evidence row was recorded. "" for every other kind. Issue #172 will
    /// replace this with a real `AuthorityPolicy` revision; this column's
    /// shape is deliberately left compatible with that migration (a plain
    /// opaque revision string) rather than inventing a richer structure now.
    #[sea_orm(default_value = "")]
    pub policy_revision: String,
    /// Bounded, redacted human-readable summary (≤ ~4KB after
    /// `repo::truncate_bounded`).
    #[sea_orm(default_value = "")]
    pub summary: String,
    /// Bounded, redacted JSON payload. For `execution` kind:
    /// `{"declared": ..., "observed": ..., "result": "matched|drifted|unknown"}`.
    /// Never raw unbounded stdout/stderr — see `repo::redact_secrets` and
    /// `repo::truncate_bounded`, applied before this column is set.
    #[sea_orm(default_value = "")]
    pub payload: String,
    /// "ok" | "unknown" — whether THIS observation attempt succeeded. A
    /// failed host probe or Git signature sample writes "unknown" here rather
    /// than being silently skipped or recorded as if it were a clean read (the
    /// acceptance criterion: a failed pull must never be read back as
    /// success).
    #[sea_orm(default_value = "ok")]
    pub collection_state: String,
    /// `0` = still the latest fact for its (direction_id, kind, source,
    /// source_ref) identity. Non-zero = the id of the row that superseded
    /// this one. Set in the SAME transaction as the superseding insert — see
    /// `repo::append_evidence`.
    #[sea_orm(default_value = 0)]
    pub superseded_by: i32,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
