use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Per-model hourly AI gateway token-usage rollup for the single-node (portable)
/// signal server. This is collect-only telemetry — no billing, no subject/tenant,
/// no node dimension (a portable server is a single process). Usage is keyed by
/// model name, the only meaningful dimension when there is a single local owner.
///
/// `input_tokens` is non-cached prompt tokens only; cache reads and writes are
/// tracked separately because they bill at very different rates (mirrors the
/// manager rollup's token classes).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "ai_usage_hourly")]
pub struct Model {
    /// Model name.
    #[sea_orm(primary_key, auto_increment = false)]
    pub model_name: String,
    /// Hour bucket, truncated to the start of the UTC hour.
    #[sea_orm(primary_key, auto_increment = false)]
    pub hour_bucket: DateTimeUtc,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub request_count: i64,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
