use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_capability_dispatch_outbox")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub dispatch_id: String,
    #[sea_orm(unique)]
    pub call_id: String,
    #[sea_orm(unique)]
    pub work_id: i64,
    #[sea_orm(unique)]
    pub reservation_id: String,
    pub generation: i64,
    pub state: String,
    pub payload_json: String,
    pub payload_schema_version: i32,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
