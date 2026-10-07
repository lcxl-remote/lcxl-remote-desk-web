//! Durable delegation authority; planning leases belong to agent_session.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_delegation_group")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub group_id: String,
    #[sea_orm(unique)]
    pub source_key_sha256: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub device_id: String,
    #[sea_orm(indexed)]
    pub source_goal_id: Option<String>,
    #[sea_orm(indexed)]
    pub source_schedule_id: Option<String>,
    pub source_occurrence_id: Option<String>,
    pub source_admission: String,
    pub source_epoch: i64,
    pub parent_input_revision: i64,
    pub parent_control_revision: i64,
    pub parent_active: bool,
    pub state_json: String,
    pub creation_envelope_json: String,
    pub model_binding_json: String,
    /// Content expiry is separate from budget/native reconciliation retention.
    pub content_redacted_at_ms: Option<i64>,
    pub version: i64,
    #[sea_orm(indexed)]
    pub deadline_ms: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
