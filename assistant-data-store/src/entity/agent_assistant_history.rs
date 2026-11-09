//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_history")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub source_id: String,
    #[sea_orm(indexed)]
    pub origin_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub device_ref: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    pub native_id: String,
    pub fragment_index: i32,
    pub kind: String,
    pub role: String,
    #[sea_orm(column_type = "Text")]
    pub content: String,
    pub revision: i64,
    pub admitted_generation: i64,
    #[sea_orm(indexed)]
    pub recorded_at_ms: i64,
    pub statement_timezone: String,
    #[sea_orm(indexed)]
    pub native_action_at_ms: Option<i64>,
    pub native_action_from_ms: Option<i64>,
    pub native_action_until_ms: Option<i64>,
    pub time_basis: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub state_json: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub dependencies_json: String,
    #[sea_orm(column_type = "Text")]
    pub retention_proof_json: String,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
    pub admission: String,
    pub deleted_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
