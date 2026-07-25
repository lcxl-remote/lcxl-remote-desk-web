use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Durable execution task owned by the single-node OSS signal agent runtime.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_exec_task")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub exec_request_id: String,
    #[sea_orm(unique)]
    pub execution_generation: String,
    pub conversation_id: String,
    pub tool_call_id: String,
    pub target_connection_id: String,
    pub status: String,
    pub disposition_json: Option<String>,
    pub result_text: Option<String>,
    #[sea_orm(unique)]
    pub event_id: String,
    pub delivery_state: String,
    pub deadline: DateTimeUtc,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
