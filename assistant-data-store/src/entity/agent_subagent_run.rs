//! Durable delegation authority; planning leases belong to agent_session.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_subagent_run")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub task_id: String,
    #[sea_orm(unique)]
    pub child_conversation_id: String,
    #[sea_orm(indexed)]
    pub worker_id: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    #[sea_orm(indexed)]
    pub workbench_id: Option<String>,
    /// A Workbench request holds a Worker slot from first claim until terminal.
    pub workbench_claimed_at_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub group_id: String,
    pub actor_id: String,
    pub device_id: String,
    /// With the root conversation and group, uniquely identifies the creation
    /// (`subagent_creation` + model call id, or `workbench_first_dispatch` +
    /// request id).
    pub origin_kind: String,
    pub origin_ref: String,
    pub creation_arguments_sha256: String,
    pub creation_envelope_json: String,
    #[sea_orm(indexed)]
    pub state: String,
    pub input_revision: i64,
    pub control_revision: i64,
    pub source_epoch: i64,
    pub state_revision: i64,
    pub state_json: String,
    #[sea_orm(indexed)]
    pub next_attempt_at_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub deadline_ms: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
