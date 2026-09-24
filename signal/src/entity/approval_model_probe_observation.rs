use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Last successful three-case review probe for the exact saved configuration.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "approval_model_probe_observation")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub approval_model_provider_id: i32,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub configuration_revision: i64,
    pub tested_at: DateTimeUtc,
    pub reasoning_observed: Option<bool>,
    pub reasoning_tokens: Option<i64>,
    pub stop_reason: Option<String>,
    pub validated_capabilities: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
