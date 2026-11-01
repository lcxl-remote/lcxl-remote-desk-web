//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_device_binding")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub binding_id: String,
    pub instance_id: String,
    pub audience: String,
    pub device_name: Option<String>,
    pub first_seen_at_ms: i64,
    pub last_seen_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
