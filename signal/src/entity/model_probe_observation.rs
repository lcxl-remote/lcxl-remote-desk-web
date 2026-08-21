use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Last successful validation observation for the OSS singleton model profile.
/// It is descriptive telemetry only and never controls request construction.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_probe_observation")]
pub struct Model {
    /// Fixed singleton id and FK to `model_provider.id`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub model_provider_id: i32,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub tested_at: DateTimeUtc,
    pub reasoning_observed: Option<bool>,
    pub reasoning_tokens: Option<i64>,
    pub stop_reason: Option<String>,
    /// Canonical JSON object, not a declaration of runtime capabilities.
    pub validated_capabilities: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
