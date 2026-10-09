use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_metric_compact")]
pub struct Model {
    pub storage_bytes: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub object_id: String,
    #[sea_orm(indexed)]
    pub call_id: Option<String>,
    #[sea_orm(indexed)]
    pub kind: String,
    #[sea_orm(indexed)]
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    #[sea_orm(column_type = "Text")]
    pub snapshot_json: String,
    #[sea_orm(column_type = "Text")]
    pub contribution_json: String,
    pub state_revision: i64,
    pub detail_trimmed: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
