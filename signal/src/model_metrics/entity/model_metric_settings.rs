use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_metric_settings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub schema_version: i32,
    pub revision: i64,
    #[sea_orm(column_type = "Text")]
    pub settings_json: String,
    pub available_from_ms: i64,
    pub updated_at_ms: i64,
    pub frozen_before_ms: i64,
    pub coverage_partial: bool,
    pub event_rows: i64,
    pub compact_rows: i64,
    pub detail_rows: i64,
    pub rollup_rows: i64,
    pub storage_used_bytes: i64,
    pub event_cleanup_active: bool,
    pub compact_cleanup_active: bool,
    pub detail_cleanup_active: bool,
    pub rollup_cleanup_active: bool,
    pub storage_cleanup_active: bool,
    pub trimmed_details: i64,
    pub dropped_pending_events: i64,
    pub rollup_trim_before_ms: i64,
    pub physical_allocated_bytes: Option<i64>,
    pub physical_sampled_at_ms: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
