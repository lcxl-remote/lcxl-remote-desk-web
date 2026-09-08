use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// One occurrence, including skipped and missed slots, independent of its session.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_schedule_run")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub run_id: String,
    #[sea_orm(indexed)]
    pub schedule_id: String,
    pub owner_user_id: i32,
    /// Canonical source identity encodes schedule + calendar instant or manual key.
    #[sea_orm(unique)]
    pub occurrence_identity: String,
    pub source: String,
    pub scheduled_at: Option<i64>,
    pub requested_at: i64,
    pub schedule_revision: i64,
    pub recovery_epoch: i64,
    pub task_snapshot_json: String,
    pub conversation_id: String,
    pub turn_id: String,
    #[sea_orm(indexed)]
    pub status: String,
    pub start_deadline: i64,
    pub lease_epoch: i64,
    pub lease_owner: Option<String>,
    #[sea_orm(indexed)]
    pub lease_deadline: Option<i64>,
    pub attempt: i32,
    pub failure_accounted: bool,
    pub error_kind: Option<String>,
    pub result_ref: Option<String>,
    pub missed_count: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Original late action receipts were reconciled; this does not resume the task.
    pub receipts_reconciled_at: Option<i64>,
    /// Immutable owner acknowledgement of the original outcome after receipt reconciliation.
    pub outcome_review_json: Option<String>,
    pub cancel_requested_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
