//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_history_coverage")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub day_bucket: i64,
    pub reason: String,
    pub earliest_recorded_at_ms: i64,
    pub latest_recorded_at_ms: i64,
    pub indexed_through_ms: Option<i64>,
    pub source_count: i64,
    pub revision: i64,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
