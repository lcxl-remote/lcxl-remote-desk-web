//! Canonical source authority, shared by both hosts.
use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_issue_subscription")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub subscription_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub revision: i64,
    pub enabled: bool,
    #[sea_orm(column_type = "Text")]
    pub policy_json: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
impl ActiveModelBehavior for ActiveModel {}
