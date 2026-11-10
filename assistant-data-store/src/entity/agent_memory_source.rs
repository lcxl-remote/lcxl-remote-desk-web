//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_memory_source")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub memory_id: String,
    pub memory_revision: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub origin_id: i64,
    pub role: String,
    #[sea_orm(column_type = "Text")]
    pub body: String,
    /// Original retained byte count remains after natural body cleanup.
    pub retained_bytes: i32,
    pub body_cleared_at_ms: Option<i64>,
    #[sea_orm(column_type = "Text")]
    pub dependencies_json: String,
    #[sea_orm(column_type = "Text")]
    pub known_sources_json: String,
    #[sea_orm(column_type = "Text")]
    pub forget_sources_json: String,
    #[sea_orm(column_type = "Text")]
    pub retention_proof_json: String,
    #[sea_orm(column_type = "Text")]
    pub policy_json: String,
    #[sea_orm(column_type = "Text")]
    pub source_device_json: Option<String>,
    pub observed_at_ms: i64,
    pub statement_timezone: String,
    pub revoked_at_ms: Option<i64>,
    /// Original content-policy deadline; natural body retention is revision-local.
    pub expires_at_ms: Option<i64>,
}

impl ActiveModelBehavior for ActiveModel {}

impl Model {
    /// The model-visible source id formatted from this row's primary key.
    pub fn source_id(&self) -> String {
        desk_agent_protocol::ai_assistant::source_ref::SourceRef::memory(self.id).to_string()
    }
}
