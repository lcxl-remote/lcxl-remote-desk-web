use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Independent, signal-owned approval model configuration. The API key never
/// leaves the signal process in a public DTO or a review candidate.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "approval_model_provider")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub enabled: bool,
    pub wire_protocol: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub profile_schema_version: i32,
    pub request_options: String,
    pub output_limit_field: String,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    pub max_context_bytes: i64,
    pub prices_json: Option<String>,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub configuration_revision: i64,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub const SINGLETON_ID: i32 = 1;
