//! Durable delegation authority; planning leases belong to agent_session.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_subagent_inbox")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub event_id: String,
    #[sea_orm(unique)]
    pub dedup_key_sha256: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    #[sea_orm(indexed)]
    pub group_id: String,
    #[sea_orm(indexed)]
    pub task_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub state_revision: i64,
    pub event_kind: String,
    pub event_json: String,
    pub parent_input_revision: i64,
    pub parent_control_revision: i64,
    pub ui_read_at_ms: Option<i64>,
    pub notification_attempted_turn_id: Option<String>,
    pub model_notified_at_ms: Option<i64>,
    pub model_notified_turn_id: Option<String>,
    pub model_observed_at_ms: Option<i64>,
    pub observed_tool_call_id: Option<String>,
    pub observed_message_id: Option<String>,
    pub interpreted_at_ms: Option<i64>,
    pub interpreted_turn_id: Option<String>,
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
