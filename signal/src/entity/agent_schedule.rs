use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Durable owner-scoped schedule; all absolute times are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_schedule")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub schedule_id: String,
    #[sea_orm(indexed)]
    pub owner_user_id: i32,
    pub target_device_id: String,
    pub kind: String,
    #[sea_orm(indexed)]
    pub status: String,
    pub title: String,
    pub prompt: String,
    pub locale: Option<String>,
    pub model_id: Option<i32>,
    pub spec_json: String,
    pub calc_version: String,
    pub revision: i64,
    /// Stable requirement version; scheduler CAS and display edits do not advance it.
    pub task_revision: i64,
    #[sea_orm(indexed)]
    pub next_run_at: Option<i64>,
    pub recurrence_cursor_at: Option<i64>,
    pub grace_seconds: i32,
    pub creation_source: String,
    #[sea_orm(unique)]
    pub creation_identity: String,
    pub creation_payload_sha256: String,
    pub source_conversation_id: Option<String>,
    pub requirement_revision: Option<i64>,
    pub contract_revision: Option<i64>,
    pub authorization_revision: Option<i64>,
    /// Validated shared FailureState, including its recovery epoch and blockers.
    pub failure_state_json: String,
    /// One slot fences calendar runs, manual runs, and pending approval.
    pub active_run_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
