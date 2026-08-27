use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Generic durable action facts for the single-node OSS agent runtime.
///
/// Existing confirmed exec remains in `agent_exec_task`; Computer Use and later
/// action families use this table so their correlation is never disguised as an
/// exec request. The lifecycle columns mirror the shared durable-action contract.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_action_item")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub kind: String,
    #[sea_orm(unique)]
    pub action_request_id: String,
    #[sea_orm(unique)]
    pub exec_request_id: Option<String>,
    pub conversation_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub actor_id: String,
    pub target_device_id: String,
    pub status: String,
    pub owner_node: Option<String>,
    #[sea_orm(unique)]
    pub claim_token: Option<String>,
    pub attempt: i32,
    pub lease_expires_at: Option<DateTimeUtc>,
    pub execution_id: Option<String>,
    pub dispatched_attempt: Option<i32>,
    pub dispatch_intent_at: Option<DateTimeUtc>,
    pub approval_id: Option<String>,
    pub approval_expires_at: Option<DateTimeUtc>,
    pub approved_at: Option<DateTimeUtc>,
    pub draft_hash: String,
    pub policy_revision: i64,
    pub is_side_effecting: bool,
    pub payload_json: String,
    pub payload_schema_version: i32,
    pub result_json: Option<String>,
    pub result_schema_version: Option<i32>,
    pub resolution: Option<String>,
    pub manual_resolved_at: Option<DateTimeUtc>,
    pub cancel_requested_at: Option<DateTimeUtc>,
    pub cancel_requested_by: Option<String>,
    pub cancel_generation: Option<String>,
    #[sea_orm(unique)]
    pub completion_event_id: String,
    pub completion_delivery_state: String,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
