use sea_orm::entity::prelude::*;

/// A human's resolution of one Lane's Gate (issue #172), keyed to the EXACT
/// `authority_policy.revision` it was decided under. `authority::
/// adjudicate_lane` only ever honors an override recorded at the CURRENT
/// policy revision (see `store::repo::get_gate_decision`) — a policy change
/// silently orphans every older row, which is what makes "策略变化也不能静默
/// 沿用旧判定" hold for the Gate-resolution path too, not just the initial
/// materialize path.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "lane_gate_decision")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub direction_id: i32,
    pub policy_revision: String,
    /// "approved" | "denied".
    pub decision: String,
    #[sea_orm(default_value = "")]
    pub reason: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
