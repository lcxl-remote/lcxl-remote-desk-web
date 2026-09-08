use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Durable quota allocation; this row never proves that a provider executed a call.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_task_budget_reservation")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub reservation_id: String,
    #[sea_orm(indexed)]
    pub schedule_id: String,
    #[sea_orm(indexed)]
    pub run_id: String,
    pub owner_user_id: i32,
    pub kind: String,
    /// Exactly one published rule for a tool call; absent for model/run allocations.
    pub rule_id: Option<String>,
    /// Original one-use owner decision consumed for this exact call, if exceptional.
    pub exception_grant_id: Option<String>,
    pub utc_day: i64,
    pub logical_key_sha256: String,
    pub input_sha256: String,
    pub authority_sha256: String,
    pub reserved_units: i64,
    pub charged_units: i64,
    /// reserved | settled | overrun. Unknown outcomes remain fully reserved.
    pub state: String,
    pub receipt_sha256: Option<String>,
    pub version: i64,
    pub created_at: i64,
    pub settled_at: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
