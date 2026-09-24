use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_goal_run")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub goal_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    #[sea_orm(indexed)]
    pub status: String,
    pub state_json: String,
    pub state_version: i64,
    pub goal_revision: i64,
    pub input_revision: i64,
    pub slice_seq: i32,
    pub lease_epoch: i64,
    pub lease_owner: Option<String>,
    #[sea_orm(indexed)]
    pub lease_deadline: Option<i64>,
    #[sea_orm(indexed)]
    pub next_attempt_at: Option<i64>,
    pub absolute_deadline: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
