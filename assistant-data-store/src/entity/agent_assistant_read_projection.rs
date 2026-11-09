//! A prepared read is evidence for later native validation, never a send grant.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_read_projection")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub projection_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    pub retrieval_episode_id: String,
    pub kind: String,
    pub logical_call_id: String,
    pub producer_message_id: Option<String>,
    pub tool_call_id: Option<String>,
    #[sea_orm(indexed)]
    pub source_envelope_id: String,
    pub purpose: String,
    #[sea_orm(column_type = "Text")]
    pub proof_json: String,
    #[sea_orm(column_type = "Text")]
    pub result_json: Option<String>,
    pub created_at_ms: i64,
    pub unused_until_ms: i64,
    pub first_consumed_at_ms: Option<i64>,
    pub last_consumed_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
