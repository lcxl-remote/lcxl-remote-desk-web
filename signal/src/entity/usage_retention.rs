use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Single-row usage-retention configuration for the OSS signal server.
///
/// The portable signal server is single-node and single-account (it never runs
/// multi-instance — that is a manager-only concern), so there is exactly one
/// retention config row (id = [`SINGLETON_ID`]). It controls how many days of
/// `turn_usage_hourly` / `ai_usage_hourly` rollups are kept before the cleanup
/// loop deletes them. The signal rollups are collect-only telemetry with no
/// billing coupling, so cleanup deletes purely by age.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "usage_retention")]
pub struct Model {
    /// Fixed singleton primary key; always [`SINGLETON_ID`].
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    /// Retention window for TURN traffic rollups, in days.
    pub turn_days: i32,
    /// Retention window for AI token rollups, in days.
    pub ai_days: i32,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// The fixed primary key of the singleton retention-config row.
pub const SINGLETON_ID: i32 = 1;
