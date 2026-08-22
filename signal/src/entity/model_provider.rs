use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Single-row model-provider configuration for the OSS signal central brain.
///
/// The portable signal server is single-node and single-account, so there is
/// exactly one provider config row (id = [`SINGLETON_ID`]). Enum-valued columns
/// (`wire_protocol` / `response_format` / `execution_mode`) are stored as their snake_case wire
/// strings; unsigned runtime limits are stored as signed integers (SQLite has
/// no unsigned type) and projected back to bounded unsigned values by the
/// domain type.
///
/// `api_key` is a server-side secret: it is stored here but is never projected
/// into any public DTO and its rendered forms are redacted (see
/// [`crate::model_provider`]).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "model_provider")]
pub struct Model {
    /// Fixed singleton primary key; always [`SINGLETON_ID`].
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub wire_protocol: Option<String>,
    pub model: Option<String>,
    /// Whether the configured model accepts image content in user messages.
    #[sea_orm(default_value = false)]
    pub supports_image_input: bool,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub profile_schema_version: i32,
    /// Canonical JSON object containing only protocol-approved request options.
    pub request_options: String,
    pub output_limit_field: String,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    pub max_context_bytes: i64,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub response_format: String,
    pub execution_mode: String,
    pub max_same_tool_calls_per_turn: i32,
    pub max_steps_per_turn: i32,
    #[sea_orm(default_value = 120)]
    pub exec_approval_timeout_secs: i32,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The fixed primary key of the singleton provider-config row.
pub const SINGLETON_ID: i32 = 1;
