//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_memory")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub memory_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub scope_key: String,
    pub device_ref: String,

    #[sea_orm(column_type = "Text")]
    pub body: String,
    #[sea_orm(column_type = "Text")]
    pub context: Option<String>,
    pub content_sha256: String,

    pub revision: i64,
    pub state_generation: i64,
    #[sea_orm(indexed)]
    pub status: String,
    pub accepted_input_seq: i64,
    pub observed_at_ms: i64,
    pub updated_at_ms: i64,
    pub valid_from_ms: i64,
    pub valid_until_ms: Option<i64>,
}

impl ActiveModelBehavior for ActiveModel {}
