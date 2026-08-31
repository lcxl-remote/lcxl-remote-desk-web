use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Durable continuation fence; the immutable decision remains in agent_run_event.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_permission_resume")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub permission_id: String,
    pub decision_event_id: String,
    pub run_id: String,
    pub request_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub input_revision: i64,
    /// pending / started / settled / superseded.
    pub state: String,
    pub turn_id: Option<String>,
    pub version: i64,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
