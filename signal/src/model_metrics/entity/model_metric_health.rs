use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_metric_health")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub node_id: String,
    #[sea_orm(indexed)]
    pub reported_at_ms: i64,
    pub state: String,
    pub persisted_at_ms: Option<i64>,
    pub aggregated_at_ms: Option<i64>,
    pub dropped_events: String,
    pub discarded_events: String,
    pub config_revision: Option<String>,
    pub reason: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
