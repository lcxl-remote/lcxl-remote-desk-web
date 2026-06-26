use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Single-row model-provider configuration for the OSS signal central brain.
///
/// The portable signal server is single-node and single-account, so there is
/// exactly one provider config row (id = [`SINGLETON_ID`]). Enum-valued columns
/// (`response_format` / `execution_mode`) are stored as their snake_case wire
/// strings; `max_context_bytes` is stored as a signed integer (SQLite has no
/// unsigned type) and projected back to an unsigned byte count by the domain
/// type.
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
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub max_context_bytes: Option<i64>,
    pub response_format: String,
    pub execution_mode: String,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The fixed primary key of the singleton provider-config row.
pub const SINGLETON_ID: i32 = 1;
