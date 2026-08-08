use sea_orm::entity::prelude::*;

/// One AuthorityPolicy revision (issue #172): which writes a scope may
/// auto-materialize, which need a human Gate, and which are denied outright.
/// Append-only per (scope, scope_id): tightening/loosening a policy inserts a
/// NEW row with the next `revision`; the active policy for a scope is its
/// highest-`revision` row with an empty `revoked_at`. Revoking sets
/// `revoked_at` on that row WITHOUT deleting it — `authority::default_policy`
/// (the hard-coded conservative fallback) then applies until a new row is
/// created, so revoking a policy can never leave a scope MORE permissive than
/// before it existed.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "authority_policy")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// "workspace" | "project" — see `authority::PolicyScope`.
    pub scope: String,
    /// The workspace id (or, for a future "project" scope, that scope's id).
    #[sea_orm(default_value = 0)]
    pub scope_id: i32,
    /// Monotonically increasing per (scope, scope_id), stored as a decimal
    /// string — mirrors `plan.created_at`'s OCC-token convention. Starts at
    /// "1"; `authority::default_policy`'s hard-coded fallback is "0", so any
    /// real configured policy always compares as newer.
    pub revision: String,
    /// JSON `authority::PolicyRules`.
    pub rules: String,
    /// "system" | "user" — who created this revision. A revoke keeps the
    /// original `source` (revoking is a separate act, recorded by
    /// `revoked_at`, not a new row).
    pub source: String,
    pub created_at: String,
    /// Unix seconds this row was revoked; "" = still active (subject to the
    /// highest-revision-wins rule above).
    #[sea_orm(default_value = "")]
    pub revoked_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
