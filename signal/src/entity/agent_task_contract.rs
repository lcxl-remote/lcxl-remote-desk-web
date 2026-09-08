use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_task_contract")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub contract_id: String,
    #[sea_orm(indexed)]
    pub schedule_id: String,
    #[sea_orm(indexed)]
    pub owner_user_id: i32,
    pub task_revision: i64,
    pub contract_revision: i64,
    #[sea_orm(unique)]
    pub revision_identity: String,
    pub digest_sha256: String,
    #[sea_orm(column_type = "Text")]
    pub canonical_json: String,
    pub created_at: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
