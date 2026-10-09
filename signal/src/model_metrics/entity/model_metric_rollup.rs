use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_metric_rollup")]
pub struct Model {
    pub storage_bytes: i64,
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique)]
    pub series_key: String,
    #[sea_orm(indexed)]
    pub bucket_ms: i64,
    pub granularity_ms: i64,
    #[sea_orm(indexed)]
    pub series_kind: String,
    pub object_kind: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub surface: Option<String>,
    pub purpose: Option<String>,
    pub origin: Option<String>,
    pub contract_revision: Option<String>,
    pub tool: Option<String>,
    pub probe: bool,
    pub other: bool,
    pub runtime_category: Option<String>,
    pub runtime_definition: Option<String>,
    #[sea_orm(column_type = "Text")]
    pub dimensions_json: String,
    #[sea_orm(column_type = "Text")]
    pub totals_json: String,
    pub partial: bool,
    pub frozen: bool,
    pub updated_at_ms: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
