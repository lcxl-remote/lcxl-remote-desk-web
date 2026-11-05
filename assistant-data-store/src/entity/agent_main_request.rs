use sea_orm::entity::prelude::*;

/// Intake is durable before the request joins the native assistant session.
#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "agent_main_request")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub request_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    #[sea_orm(indexed)]
    pub actor_id: String,
    pub client_message_id: String,
    pub submission_sha256: String,
    #[sea_orm(indexed)]
    pub state: String,
    pub failure: Option<String>,
    /// Removed when the first native claim commits or the request is discarded.
    pub pending_input_json: Option<String>,
    /// The authenticated connection that submitted this input, for live output.
    pub reply_connection_id: Option<String>,
    pub coordinated: bool,
    pub first_dispatch_json: Option<String>,
    pub first_worker_request_id: Option<String>,
    pub coordination_attempts: i32,
    pub retry_at_ms: Option<i64>,
    pub input_revision: Option<i64>,
    pub accepted_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ActiveModelBehavior for ActiveModel {}
