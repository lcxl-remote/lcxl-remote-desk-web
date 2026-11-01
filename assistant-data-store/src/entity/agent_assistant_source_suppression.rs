//! Exact model-purpose suppression; original owner-visible chat is preserved.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_source_suppression")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub source_id: String,
    pub source_revision: i64,
    pub origin_id: String,
    pub content_sha256: String,
    pub cutoff_input_seq: i64,
    pub control_generation: i64,
    pub created_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
