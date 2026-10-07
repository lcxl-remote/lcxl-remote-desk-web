//! Durable delegation authority; planning leases belong to agent_session.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_delegation_reservation")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub reservation_id: String,
    #[sea_orm(unique)]
    pub logical_key_sha256: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    #[sea_orm(indexed)]
    pub group_id: String,
    pub conversation_id: String,
    pub task_id: Option<String>,
    pub operation_kind: String,
    pub arguments_sha256: String,
    pub source_epoch: i64,
    pub input_revision: i64,
    pub control_revision: i64,
    pub planning_lease_token: Option<i64>,
    pub reservation_json: String,
    pub actual_json: Option<String>,
    pub provider_receipt_kind: Option<String>,
    #[sea_orm(unique)]
    pub provider_receipt_id: Option<String>,
    pub provider_started_at_ms: Option<i64>,
    /// Original scheduled occurrence quota; independent of the planner lease.
    pub source_schedule_budget_id: Option<String>,
    pub state: String,
    pub version: i64,
    pub created_at: i64,
    pub settled_at: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
