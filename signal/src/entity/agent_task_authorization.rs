use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_task_authorization")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub authorization_id: String,
    #[sea_orm(indexed)]
    pub schedule_id: String,
    #[sea_orm(indexed)]
    pub owner_user_id: i32,
    pub task_revision: i64,
    pub contract_revision: i64,
    pub contract_sha256: String,
    pub authorization_revision: i64,
    #[sea_orm(unique)]
    pub revision_identity: String,
    #[sea_orm(unique)]
    pub publication_identity: String,
    pub publication_payload_sha256: String,
    pub rehearsal_run_id: String,
    #[sea_orm(column_type = "Text")]
    pub rehearsal_evidence_json: String,
    pub approved_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub revoked_reason: Option<String>,
    pub version: i64,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
