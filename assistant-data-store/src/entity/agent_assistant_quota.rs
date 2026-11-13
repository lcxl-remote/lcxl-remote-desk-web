//! Durable personal-assistant data; execution authority remains in the original task stores.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "agent_assistant_quota")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    #[sea_orm(unique_key = "actor_id")]
    pub actor_id: String,
    pub history_records: i64,
    pub history_body_bytes: i64,
    pub memory_records: i64,
    pub memory_body_bytes: i64,
    pub term_records: i64,
    pub metadata_bytes: i64,
    pub output_gc_completed_at_ms: Option<i64>,
    pub output_gc_archive_id: Option<i64>,

    pub output_gc_next_scan_at_ms: Option<i64>,
    pub revision: i64,
}

impl ActiveModelBehavior for ActiveModel {}
