use sea_orm::entity::prelude::*;

/// One edge in a Lane's (a `direction` row's) upstream dependency set — issue
/// #173's upgrade from `direction.depends_on_direction_id` (a single
/// producer→consumer slot) to a real many-to-many DAG. A consumer
/// (`direction_id`) can own zero to many rows here, each independently in one
/// of three states:
///
/// - `resolved`: a real, materialized upstream Lane. `upstream_direction_id`
///   is that Lane's row id.
/// - `denied`: the consumer named an upstream that was explicitly denied —
///   a permanent, decided fact, not "maybe later". `upstream_direction_id`
///   is `0` ("not applicable"): there is no real producer row this blocks
///   on, only the fact that the reference is dead.
/// - `unresolved`: the consumer's declared upstream could not (yet, or ever)
///   be pinned to exactly one materialized Lane — a typo, an ambiguous
///   duplicate name, a self-reference, or a sibling that simply hasn't been
///   decided yet. `upstream_direction_id` is `0`, same reasoning as `denied`.
///
/// `denied`/`unresolved` deliberately do NOT carry which declared name
/// produced them (the schema has no name column) — matching the legacy
/// column's own sentinel convention (`DENIED_UPSTREAM_SENTINEL` /
/// `UNRESOLVED_UPSTREAM_SENTINEL` carried no name either): the aggregation
/// in `repo::upstream_merge_state` only ever needs to know THAT a consumer
/// has a denied/unresolved edge, not which one, to fail closed.
///
/// Both `resolved` and non-`resolved` rows exist so a single Lane can mix
/// dependency outcomes — e.g. depends on A (resolved), B (denied), C
/// (unresolved) all at once — and the aggregate in `upstream_merge_state`
/// blocks on the worst of the set.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "direction_dependency")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// The consumer Lane (this row is one of ITS upstream edges). Indexed —
    /// every read of a Lane's dependency set filters on this column.
    pub direction_id: i32,
    /// The producer Lane's row id when `state == "resolved"`; `0` ("not
    /// applicable") for `denied`/`unresolved` rows — mirrors
    /// `direction.repo_id`'s own "0 = unset" convention.
    #[sea_orm(default_value = 0)]
    pub upstream_direction_id: i32,
    /// "resolved" | "denied" | "unresolved".
    pub state: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
