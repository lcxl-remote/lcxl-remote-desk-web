//! A frozen artifact and its one original-issue delivery, independent of devices.

use sea_orm::entity::prelude::*;

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_issue_reply")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "artifact_id")]
    pub artifact_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    #[sea_orm(indexed)]
    pub root_conversation_id: String,
    #[sea_orm(indexed)]
    pub request_id: String,
    pub execution_conversation_id: String,
    pub input_revision: i64,
    #[sea_orm(column_type = "Text")]
    pub artifact_json: Option<String>,
    pub artifact_sha256: String,
    #[sea_orm(column_type = "Text")]
    pub receipt_json: String,
    pub retired_at_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub content_expires_at_ms: Option<i64>,
    #[sea_orm(indexed)]
    pub state: String,
    pub reason: Option<String>,
    pub owner_node: Option<String>,
    pub claim_token: Option<String>,
    pub attempt: i32,
    pub lease_expires_at_ms: Option<i64>,
    /// Never replaced after the dispatch intent. Late receipts use this token.
    pub dispatch_token: Option<String>,
    pub dispatched_at_ms: Option<i64>,
    pub dispatched_authorization_revision: Option<i64>,
    pub comment_id: Option<i64>,
    pub reconciliation_page: i64,
    #[sea_orm(indexed)]
    pub next_attempt_at_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
