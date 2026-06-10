use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "thread")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub workspace_id: i32,
    pub title: String,
    pub slug: String,
    pub kind: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
