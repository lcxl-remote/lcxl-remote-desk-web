use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_goal_open_request")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub request_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub source_message_id: String,
    pub input_revision: i64,
    pub goal_text: String,
    pub target_goal_id: Option<String>,
    pub target_goal_revision: Option<i64>,
    pub previous_completed_goal_id: Option<String>,
    pub limits_json: String,
    pub model_binding_json: String,
    #[sea_orm(indexed)]
    pub status: String,
    pub expires_at: i64,
    pub decided_at: Option<i64>,
    pub decision_event_id: Option<String>,
    pub resulting_goal_id: Option<String>,
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
