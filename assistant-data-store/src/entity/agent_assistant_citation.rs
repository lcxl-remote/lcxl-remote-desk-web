//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_citation")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub answer_message_id: String,
    pub answer_revision: i64,
    #[sea_orm(indexed)]
    pub answer_conversation_id: String,
    pub output_envelope_id: String,
    pub answer_sha256: String,
    #[sea_orm(column_type = "Text")]
    pub citations_json: String,
    pub incomplete: bool,
    pub created_at_ms: i64,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
