use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_approval_review")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub candidate_id: String,
    #[sea_orm(indexed)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub delegation_id: String,
    /// Revision of the reviewer configuration used for this model call.
    pub model_config_revision: i64,
    pub source_kind: String,
    pub source_id: String,
    pub action_sha256: String,
    /// Keyed digest of the transient approval context; never store a second
    /// copy of the user's messages, exact inputs or attachment contents.
    pub context_hmac_sha256: String,
    pub decision_json: Option<String>,
    #[sea_orm(indexed)]
    pub status: String,
    pub lease_epoch: i64,
    pub lease_owner: Option<String>,
    pub lease_deadline: Option<i64>,
    pub reserved_tokens: i64,
    pub reserved_cost_micros: i64,
    pub expires_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
