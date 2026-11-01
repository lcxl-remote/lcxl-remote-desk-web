//! Bounded retrieval snapshots and opaque, source-bound page continuations.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_retrieval_query")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub query_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    #[sea_orm(indexed)]
    pub retrieval_episode_id: String,
    pub purpose: String,
    pub kind: String,
    pub request_sha256: String,
    pub scope_sha256: String,
    pub proof_sha256: String,
    #[sea_orm(column_type = "Text")]
    pub proof_json: String,
    pub captured_at_ms: i64,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
