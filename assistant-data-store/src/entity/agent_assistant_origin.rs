//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_origin")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
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
    pub accepted_input_seq: i64,
    pub kind: String,
    pub original_sha256: String,
    #[sea_orm(default_value = false)]
    pub history_admitted: bool,
    pub deletion_generation: i64,
    pub deleted_at_ms: Option<i64>,
    #[sea_orm(column_type = "Text")]
    pub policy_json: String,

    pub created_at_ms: i64,
    pub statement_timezone: String,
}

impl ActiveModelBehavior for ActiveModel {}
