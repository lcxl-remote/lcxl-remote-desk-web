//! Single-node TURN usage collection helpers (collect-only, no billing).
//!
//! The portable/signal server folds per-connection TURN byte counters into a
//! per-device hourly rollup stored in the local sqlite database. Connections
//! that cannot be resolved to a device fall back to the raw `connection_id`.

use std::collections::HashMap;

use chrono::Timelike;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::prelude::Expr;
use sea_orm::sea_query::{ExprTrait, OnConflict};
use sea_orm::{ActiveValue::Set, DatabaseConnection, DbErr, EntityTrait};
use tokio::sync::RwLock;

use crate::entity::turn_usage;

/// In-process map from signaling `connection_id` to its resolved `device_code`,
/// populated while a connection is live. The portable server is a single
/// process, so this is purely node-local state with no cross-instance concerns.
pub type ConnectionDeviceMap = RwLock<HashMap<String, String>>;

/// A signed increment to apply to one `(device_code, hour_bucket)` rollup row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnUsageDelta {
    pub received_bytes: i64,
    pub sent_bytes: i64,
    pub received_pkts: i64,
    pub sent_pkts: i64,
}

impl TurnUsageDelta {
    /// Whether this delta carries any traffic (used to skip no-op upserts).
    pub fn is_zero(&self) -> bool {
        self.received_bytes == 0
            && self.sent_bytes == 0
            && self.received_pkts == 0
            && self.sent_pkts == 0
    }
}

/// Truncate a timestamp to the start of its UTC hour.
pub fn truncate_to_hour(ts: DateTimeUtc) -> DateTimeUtc {
    ts.with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(ts)
}

/// Atomically add `delta` to the `(device_code, hour_bucket)` rollup, inserting
/// the row when absent. The `ON CONFLICT DO UPDATE` adds the delta to the
/// existing counters, so repeated flushes accumulate.
pub async fn upsert_turn_usage(
    db: &DatabaseConnection,
    device_code: &str,
    hour_bucket: DateTimeUtc,
    delta: &TurnUsageDelta,
) -> Result<(), DbErr> {
    let now = chrono::Utc::now();
    let model = turn_usage::ActiveModel {
        device_code: Set(device_code.to_string()),
        hour_bucket: Set(hour_bucket),
        received_bytes: Set(delta.received_bytes),
        sent_bytes: Set(delta.sent_bytes),
        received_pkts: Set(delta.received_pkts),
        sent_pkts: Set(delta.sent_pkts),
        updated_at: Set(now),
    };

    turn_usage::Entity::insert(model)
        .on_conflict(
            OnConflict::columns([
                turn_usage::Column::DeviceCode,
                turn_usage::Column::HourBucket,
            ])
            // Unqualified columns in DO UPDATE reference the existing row, so
            // these expressions accumulate the delta into the stored counters.
            .value(
                turn_usage::Column::ReceivedBytes,
                Expr::col(turn_usage::Column::ReceivedBytes).add(delta.received_bytes),
            )
            .value(
                turn_usage::Column::SentBytes,
                Expr::col(turn_usage::Column::SentBytes).add(delta.sent_bytes),
            )
            .value(
                turn_usage::Column::ReceivedPkts,
                Expr::col(turn_usage::Column::ReceivedPkts).add(delta.received_pkts),
            )
            .value(
                turn_usage::Column::SentPkts,
                Expr::col(turn_usage::Column::SentPkts).add(delta.sent_pkts),
            )
            .value(turn_usage::Column::UpdatedAt, now)
            .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}

/// Query per-device hourly rollups whose `hour_bucket` falls in `[from, to)`,
/// ordered by hour then device.
pub async fn query_turn_usage(
    db: &DatabaseConnection,
    from: DateTimeUtc,
    to: DateTimeUtc,
) -> Result<Vec<turn_usage::Model>, DbErr> {
    use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};

    turn_usage::Entity::find()
        .filter(turn_usage::Column::HourBucket.gte(from))
        .filter(turn_usage::Column::HourBucket.lt(to))
        .order_by_asc(turn_usage::Column::HourBucket)
        .order_by_asc(turn_usage::Column::DeviceCode)
        .all(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(turn_usage::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    fn hour(h: u32) -> DateTimeUtc {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 6, 24, h, 0, 0).unwrap()
    }

    #[test]
    fn truncate_drops_sub_hour_components() {
        use chrono::TimeZone;
        let ts = chrono::Utc
            .with_ymd_and_hms(2026, 6, 24, 9, 37, 42)
            .unwrap();
        assert_eq!(truncate_to_hour(ts), hour(9));
    }

    #[tokio::test]
    async fn upsert_accumulates_on_conflict() {
        let db = memory_db().await;
        let delta = TurnUsageDelta {
            received_bytes: 100,
            sent_bytes: 40,
            received_pkts: 2,
            sent_pkts: 1,
        };
        upsert_turn_usage(&db, "dev-1", hour(9), &delta)
            .await
            .unwrap();
        upsert_turn_usage(&db, "dev-1", hour(9), &delta)
            .await
            .unwrap();

        let rows = query_turn_usage(&db, hour(0), hour(23)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].received_bytes, 200);
        assert_eq!(rows[0].sent_bytes, 80);
        assert_eq!(rows[0].received_pkts, 4);
        assert_eq!(rows[0].sent_pkts, 2);
    }

    #[tokio::test]
    async fn distinct_keys_and_range_filter() {
        let db = memory_db().await;
        let d = TurnUsageDelta {
            received_bytes: 10,
            ..Default::default()
        };
        upsert_turn_usage(&db, "dev-1", hour(9), &d).await.unwrap();
        upsert_turn_usage(&db, "dev-2", hour(9), &d).await.unwrap();
        upsert_turn_usage(&db, "dev-1", hour(10), &d).await.unwrap();

        // Range [9,10) keeps only the two hour-9 rows.
        let rows = query_turn_usage(&db, hour(9), hour(10)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.hour_bucket == hour(9)));
    }
}
