//! Canonical source authority, shared by both hosts.
use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_issue_admission")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "authorization_id")]
    pub authorization_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub subscription_id: String,
    pub subscription_revision: i64,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    #[sea_orm(indexed)]
    pub request_id: String,
    pub execution_root_id: String,
    pub event_id: String,
    #[sea_orm(column_type = "Text")]
    pub snapshot_json: Option<String>,
    pub stopped_at_ms: Option<i64>,
    pub retired_at_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub content_expires_at_ms: Option<i64>,
    #[sea_orm(column_type = "Text")]
    pub authorization_change_json: Option<String>,
    pub created_at_ms: i64,
}
impl ActiveModelBehavior for ActiveModel {}
