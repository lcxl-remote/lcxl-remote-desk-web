//! Indexed immutable output dependencies. Edges pin original provenance only;
//! they contain no body, execute no work and grant no source/model authority.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_model_parent")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub conversation_id: String,
    pub consumer_call_id: String,
    pub parent_call_id: String,
    pub consumer_proof_sha256: String,
    pub parent_proof_sha256: String,
    pub parent_output_envelope_id: String,
    pub parent_output_label_sha256: String,
    pub created_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
