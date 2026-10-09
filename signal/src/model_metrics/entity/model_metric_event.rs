use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_metric_event")]
pub struct Model {
    pub storage_bytes: i64,
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub event_id: String,
    #[sea_orm(indexed)]
    pub object_id: String,
    #[sea_orm(unique)]
    pub phase_slot: String,
    #[sea_orm(column_type = "Text")]
    pub payload_json: String,
    pub schema_version: i32,
    #[sea_orm(indexed)]
    pub received_at_ms: i64,
    pub occurred_at_ms: i64,
    #[sea_orm(indexed)]
    pub applied: bool,
    pub binding: i32,
    pub association_pending: bool,
    pub association_alias: Option<String>,
    pub requires_operation_start: bool,
    pub next_association_at_ms: i64,
    pub discarded_reason: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
