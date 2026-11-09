//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_memory_revision")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub memory_id: String,
    pub revision: i64,
    pub state_generation: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub producer_call_id: String,
    pub tool_call_id: String,
    pub logical_mutation_id: String,
    pub write_outcome: String,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    #[sea_orm(column_type = "Text")]
    pub context: Option<String>,

    pub status: String,
    pub accepted_input_seq: i64,
    #[sea_orm(column_type = "Text")]
    pub dependencies_json: String,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
}

impl ActiveModelBehavior for ActiveModel {}
