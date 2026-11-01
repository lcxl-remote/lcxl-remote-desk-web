//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_source_use")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub physical_call_id: String,
    #[sea_orm(indexed)]
    pub consumer_conversation_id: String,
    #[sea_orm(indexed)]
    pub source_id: String,
    pub source_revision: i64,
    pub purpose: String,
    pub authorization_id: String,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
}

impl ActiveModelBehavior for ActiveModel {}
