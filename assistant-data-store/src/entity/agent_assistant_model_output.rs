//! Immutable provenance metadata; no model response body or execution authority.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_model_output")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub device_ref: String,
    #[sea_orm(unique)]
    pub physical_call_id: String,
    pub output_envelope_id: String,
    pub output_digest_sha256: String,
    pub output_label_sha256: String,
    pub actor_input_seq: i64,
    pub input_revision: i64,
    pub input_recorded_at_ms: i64,
    pub statement_timezone: String,
    pub completed_at_ms: i64,
    /// The proof with its growing lists replaced by item ordinal ranges.
    #[sea_orm(column_type = "Text")]
    pub proof_json: String,
    /// Parent-use fields derived from the complete proof when it was accepted.
    #[sea_orm(column_type = "Text")]
    pub parent_summary_json: String,
}

impl ActiveModelBehavior for ActiveModel {}
