//! One durable commit per original assistant tool invocation.
//! The receipt contains locators and fingerprints, never copied conversation text.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_memory_commit")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub commit_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub instance_id: Option<String>,
    pub source_call_id: String,
    pub request_sha256: String,
    pub memory_state_generation: i64,
    #[sea_orm(column_type = "Text")]
    pub result_json: String,
    pub result_sha256: String,
    pub created_at_ms: i64,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
