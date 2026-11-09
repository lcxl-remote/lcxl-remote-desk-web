//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_control_receipt")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub operation_id: String,
    pub request_sha256: String,
    pub reason: String,
    pub outcome: String,
    pub accepted_input_seq: i64,
    pub control_generation: i64,
    pub memory_state_generation: i64,
    pub affected_count: i32,
    pub cleanup_pending: bool,
    pub tool_group_id: Option<String>,
    pub tool_group_sha256: Option<String>,

    pub created_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
