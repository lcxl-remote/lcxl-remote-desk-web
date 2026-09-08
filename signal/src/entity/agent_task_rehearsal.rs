use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Explicit interactive rehearsal reservation, never proof of successful execution.

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_task_rehearsal")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub rehearsal_id: String,
    #[sea_orm(indexed)]
    pub schedule_id: String,
    #[sea_orm(indexed)]
    pub owner_user_id: i32,
    pub task_revision: i64,
    pub target_device_id: String,
    /// Immutable requirement snapshot; later task edits do not rewrite a rehearsal.
    pub prompt: String,
    pub prompt_sha256: String,
    pub locale: Option<String>,
    pub model_id: Option<i32>,
    #[sea_orm(unique)]
    pub client_conversation_id: String,
    #[sea_orm(unique)]
    pub conversation_id: String,
    /// Pending records have not started an assistant turn or acquired authority.
    pub status: String,
    #[sea_orm(unique)]
    pub creation_identity: String,
    pub creation_payload_sha256: String,
    /// Admission time, not evidence that a tool or model completed.
    pub started_at: Option<i64>,
    /// A frozen reference to the answered session, not a permission approval.
    pub finished_at: Option<i64>,
    pub completed_session_version: Option<i64>,
    pub completed_session_sha256: Option<String>,
    pub answer_message_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
