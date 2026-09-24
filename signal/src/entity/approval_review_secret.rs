use sea_orm::entity::prelude::*;

/// Installation-local audit key. It is never exposed by a model configuration
/// endpoint or included in a review candidate.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "approval_review_secret")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub key_b64: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub const SINGLETON_ID: i32 = 1;
