//! Physical assistant-data uses are committed with the native model receipt.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_data_authorization")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "authorization_id")]
    pub authorization_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub physical_call_id: String,
    #[sea_orm(indexed)]
    pub consumer_conversation_id: String,
    #[sea_orm(indexed)]
    pub projection_id: String,
    pub purpose: String,
    #[sea_orm(column_type = "Text")]
    pub proof_json: String,
    pub authorized_at_ms: i64,
    pub expires_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
}

impl ActiveModelBehavior for ActiveModel {}
