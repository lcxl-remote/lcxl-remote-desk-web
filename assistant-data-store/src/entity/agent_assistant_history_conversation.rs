//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_history_conversation")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub conversation_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub device_ref: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    pub parent_conversation_id: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub first_recorded_at_ms: i64,
    #[sea_orm(indexed)]
    pub last_recorded_at_ms: i64,
    pub deletion_generation: i64,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
