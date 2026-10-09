use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_metric_record")]
pub struct Model {
    pub storage_bytes: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub object_id: String,
    #[sea_orm(indexed)]
    pub call_id: Option<String>,
    #[sea_orm(indexed)]
    pub kind: String,
    #[sea_orm(indexed)]
    pub provider_id: String,
    #[sea_orm(indexed)]
    pub model_id: String,
    pub purpose: String,
    pub surface: String,
    pub origin: String,
    pub contract_revision: String,
    pub tool: Option<String>,
    pub outcome: String,
    pub output: Option<String>,
    pub duration_ms: Option<i64>,
    pub first_content_ms: Option<i64>,
    pub permission: Option<String>,
    pub dispatched: Option<bool>,
    #[sea_orm(indexed)]
    pub retention_priority: i32,
    pub issue: Option<String>,
    #[sea_orm(indexed)]
    pub started_at_ms: Option<i64>,
    pub received_at_ms: i64,
    pub correction_group_observed: bool,
    pub updated_at_ms: i64,
    #[sea_orm(column_type = "Text")]
    pub snapshot_json: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
