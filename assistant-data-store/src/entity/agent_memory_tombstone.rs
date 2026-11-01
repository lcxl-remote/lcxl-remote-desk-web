//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_memory_tombstone")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub memory_id: String,
    pub target_revision: i64,
    pub scope_key: String,

    pub cutoff_input_seq: i64,
    #[sea_orm(indexed)]
    pub operation_id: String,
    #[sea_orm(column_type = "Text")]
    pub source_refs_json: String,
    pub generation: i64,
    pub created_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
