use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Durable multi-turn agent session for the single-node OSS signal runtime.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_session")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub state_json: String,
    pub version: i64,
    pub lease_token: i64,
    pub lease_deadline: Option<DateTimeUtc>,
    /// Presentation sequence/cache, independent of the running turn's CAS.
    pub snapshot_seq: Option<i64>,
    pub snapshot_fingerprint: Option<String>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
