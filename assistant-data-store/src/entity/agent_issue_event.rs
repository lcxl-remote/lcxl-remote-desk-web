//! The first observed comment is immutable; edits are metadata only.
use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_issue_event")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub event_key: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub subscription_id: String,
    pub subscription_revision: i64,
    #[sea_orm(column_type = "Text")]
    pub admitted_policy_json: Option<String>,
    pub admitted_policy_sha256: Option<String>,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub event_id: String,
    pub repository_id: i64,
    pub comment_id: i64,
    pub issue_number: Option<i64>,
    pub sender_id: Option<i64>,
    pub automation: bool,
    pub source_created_at_ms: i64,
    pub source_updated_at_ms: i64,
    #[sea_orm(column_type = "Text")]
    pub snapshot_json: Option<String>,
    pub snapshot_sha256: String,
    pub last_observation_sha256: String,
    pub edited_at_ms: Option<i64>,
    #[sea_orm(column_type = "Text")]
    pub issue_header_json: Option<String>,
    pub issue_header_sha256: Option<String>,
    #[sea_orm(indexed)]
    pub work_id: Option<String>,
    pub first_worker_id: Option<String>,
    #[sea_orm(indexed)]
    pub state: String,
    pub reason: String,
    #[sea_orm(indexed)]
    pub request_id: Option<String>,
    pub consumed_at_ms: Option<i64>,
    pub retired_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
impl ActiveModelBehavior for ActiveModel {}
