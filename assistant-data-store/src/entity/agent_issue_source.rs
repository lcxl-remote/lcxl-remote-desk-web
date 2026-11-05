//! Durable polling ownership and the last complete source watermark.
use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_issue_source")]
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
    pub first_enabled_at_ms: i64,
    pub watermark_ms: i64,
    #[sea_orm(column_type = "Text")]
    pub scan_json: Option<String>,
    pub scan_sha256: Option<String>,
    pub lease_id: Option<String>,
    pub lease_until_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub next_poll_at_ms: i64,
    pub last_error: Option<String>,
    pub updated_at_ms: i64,
}
impl ActiveModelBehavior for ActiveModel {}
