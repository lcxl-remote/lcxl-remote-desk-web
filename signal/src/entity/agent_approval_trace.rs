//! Private readable content is independent of the content-free audit ledger.
use sea_orm::entity::prelude::*;
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_approval_trace")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    #[sea_orm(indexed)]
    pub owner_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub device_id: String,
    pub root_conversation_id: Option<String>,
    pub session_kind: String,
    pub source_kind: String,
    pub source_id: String,
    pub lease_epoch: i64,
    pub model_revision: i64,
    pub model_name: Option<String>,
    pub protocol: Option<String>,
    pub output_limit: Option<i64>,
    pub stop_reason: Option<String>,
    pub usage_json: Option<String>,
    pub failure_reason: Option<String>,
    pub recording_state: String,
    pub captured_bytes: i64,
    pub created_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub expires_at_ms: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
