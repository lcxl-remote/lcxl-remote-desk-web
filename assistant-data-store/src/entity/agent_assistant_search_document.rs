//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_search_document")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub document_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub device_ref: String,
    pub source_kind: String,
    #[sea_orm(indexed)]
    pub source_id: String,
    /// The conversation fragment row of a history document; `None` for memory.
    #[sea_orm(indexed)]
    pub history_id: Option<i64>,
    pub source_revision: i64,
    pub corpus_generation: i64,
    pub origin_id: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    pub conversation_id: String,
    pub fragment_index: i32,
    pub byte_start: i32,
    pub byte_end: i32,
    pub token_count: i32,
    pub policy_revision: i64,
    pub partial: bool,
    #[sea_orm(indexed)]
    pub expires_at_ms: Option<i64>,
}

impl ActiveModelBehavior for ActiveModel {}
