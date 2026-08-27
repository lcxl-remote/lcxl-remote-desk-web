use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Append-only ordered facts for one dynamic Device Assistant run.
///
/// Dispatch authority remains in `agent_action_item` / `agent_exec_task`; this
/// table stores correlations and bounded event payloads only.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_run_event")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub event_id: String,
    pub run_id: String,
    pub event_seq: i64,
    pub input_revision: i64,
    pub kind: String,
    pub correlation_id: Option<String>,
    pub input_seq: Option<i64>,
    pub actor_id: Option<String>,
    pub source_envelope_ids_json: String,
    pub result_envelope_ids_json: String,
    pub payload_json: String,
    pub payload_schema_version: i32,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
