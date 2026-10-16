//! Physical reviewer calls; independent from the final permission outcome.
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_approval_attempt")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub call_id: String,
    #[sea_orm(indexed)]
    pub candidate_id: String,
    pub attempt_index: i64,
    #[sea_orm(indexed)]
    pub delegation_id: String,
    pub delegation_reservation_id: Option<String>,
    pub provider_receipt_kind: Option<String>,
    pub provider_receipt_id: Option<String>,
    pub snapshot_json: String,
    pub response_complete: bool,
    pub format_issue_json: Option<String>,
    #[sea_orm(indexed)]
    pub usage_settlement_state: Option<String>,
    pub usage_reconcile_at_ms: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
