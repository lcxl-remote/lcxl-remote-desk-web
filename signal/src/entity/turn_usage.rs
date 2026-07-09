use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Per-device hourly TURN traffic rollup for the single-node (portable) signal
/// server. This is collect-only telemetry — no billing, no owner/tenant, no
/// node dimension (a portable server is a single process). Connections that
/// cannot be resolved to a device fall back to the raw `connection_id` as the
/// `device_code` key.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "turn_usage_hourly")]
pub struct Model {
    /// Resolved device code, or the raw `connection_id` when unresolved.
    #[sea_orm(primary_key, auto_increment = false)]
    pub device_code: String,
    /// Hour bucket, truncated to the start of the UTC hour.
    #[sea_orm(primary_key, auto_increment = false)]
    pub hour_bucket: DateTimeUtc,
    /// Relayed application data (ChannelData + Send/Data indications). Billable.
    pub relay_received_bytes: i64,
    pub relay_sent_bytes: i64,
    pub relay_received_pkts: i64,
    pub relay_sent_pkts: i64,
    /// STUN + TURN control traffic. Observability only, never billed.
    pub control_received_bytes: i64,
    pub control_sent_bytes: i64,
    pub control_received_pkts: i64,
    pub control_sent_pkts: i64,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
