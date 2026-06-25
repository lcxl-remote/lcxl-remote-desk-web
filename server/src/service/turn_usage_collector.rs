//! Periodic TURN usage collector for the portable/signal server.
//!
//! Diffs the shared per-connection byte counters against a baseline each tick,
//! resolves each `connection_id` to a `device_code` via the live connection map
//! (falling back to the raw `connection_id`), and folds the delta into the local
//! sqlite hourly rollup. Collect-only: no billing, no owner, no node dimension.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use desk_signal::turn_usage::{
    ConnectionDeviceMap, TurnUsageDelta, truncate_to_hour, upsert_turn_usage,
};
use desk_turn::model::{Statistics, TurnSessionStatistics};
use sea_orm::DatabaseConnection;

/// Hour-aligned UTC timestamp, matching the rollup's `hour_bucket` column type.
type DateTimeUtc = DateTime<Utc>;

/// Default cadence between collection passes.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

pub struct TurnUsageCollector {
    statistics: Arc<RwLock<Statistics>>,
    conn_device_map: Arc<ConnectionDeviceMap>,
    /// Per-connection cumulative counters already persisted; the next pass only
    /// flushes the difference. Node-local and droppable (a restart re-baselines).
    baseline: HashMap<String, TurnSessionStatistics>,
}

impl TurnUsageCollector {
    pub fn new(
        statistics: Arc<RwLock<Statistics>>,
        conn_device_map: Arc<ConnectionDeviceMap>,
    ) -> Self {
        Self {
            statistics,
            conn_device_map,
            baseline: HashMap::new(),
        }
    }

    /// Run forever, flushing on a fixed interval.
    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let db = desk_signal::db::get_db();
            if let Err(e) = self.flush_once(db, chrono::Utc::now()).await {
                log::warn!("TURN usage collector flush failed: {}", e);
            }
        }
    }

    /// One collection pass. Computes per-connection deltas, groups them by
    /// resolved `device_code`, and upserts each into the current hour bucket.
    /// Baseline advances only for connections whose upsert succeeded, so a
    /// failed write is retried (re-added to the delta) next pass.
    async fn flush_once(
        &mut self,
        db: &DatabaseConnection,
        now: DateTimeUtc,
    ) -> Result<(), sea_orm::DbErr> {
        let snapshot = match self.statistics.read() {
            Ok(stats) => stats.snapshot_by_connection(),
            Err(e) => {
                log::warn!("TURN statistics lock poisoned, skipping flush: {}", e);
                return Ok(());
            }
        };

        // Resolve and aggregate per-connection deltas into per-device deltas,
        // remembering which connection_ids contributed so the baseline can
        // advance atomically with a successful device upsert.
        let hour = truncate_to_hour(now);
        let mut per_device: HashMap<String, (TurnUsageDelta, Vec<String>)> = HashMap::new();
        for (conn_id, cur) in &snapshot {
            let delta = delta_since(self.baseline.get(conn_id), cur);
            if delta.is_zero() {
                continue;
            }
            let device_code = self.resolve_device(conn_id).await;
            let entry = per_device
                .entry(device_code)
                .or_insert_with(|| (TurnUsageDelta::default(), Vec::new()));
            entry.0.received_bytes += delta.received_bytes;
            entry.0.sent_bytes += delta.sent_bytes;
            entry.0.received_pkts += delta.received_pkts;
            entry.0.sent_pkts += delta.sent_pkts;
            entry.1.push(conn_id.clone());
        }

        for (device_code, (delta, conn_ids)) in per_device {
            match upsert_turn_usage(db, &device_code, hour, &delta).await {
                Ok(()) => {
                    // Advance the baseline for the contributing connections.
                    for conn_id in conn_ids {
                        if let Some(cur) = snapshot.get(&conn_id) {
                            self.baseline.insert(conn_id, cur.clone());
                        }
                    }
                }
                Err(e) => {
                    // Leave the baseline untouched so this delta is retried.
                    log::warn!("upsert_turn_usage failed for device {}: {}", device_code, e);
                }
            }
        }
        Ok(())
    }

    /// Resolve a `connection_id` to its `device_code`, or fall back to the raw
    /// `connection_id` when no binding exists (unresolved connections still get
    /// collected, just keyed by their id).
    async fn resolve_device(&self, conn_id: &str) -> String {
        self.conn_device_map
            .read()
            .await
            .get(conn_id)
            .cloned()
            .unwrap_or_else(|| conn_id.to_string())
    }
}

/// Signed difference of a current cumulative sample from its baseline. Saturates
/// at zero per field to tolerate a counter reset (e.g. runtime restart).
fn delta_since(
    base: Option<&TurnSessionStatistics>,
    cur: &TurnSessionStatistics,
) -> TurnUsageDelta {
    let (br, bs, brp, bsp) = match base {
        Some(b) => (b.received_bytes, b.send_bytes, b.received_pkts, b.send_pkts),
        None => (0, 0, 0, 0),
    };
    TurnUsageDelta {
        received_bytes: cur.received_bytes.saturating_sub(br) as i64,
        sent_bytes: cur.send_bytes.saturating_sub(bs) as i64,
        received_pkts: cur.received_pkts.saturating_sub(brp) as i64,
        sent_pkts: cur.send_pkts.saturating_sub(bsp) as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal::entity::turn_usage;
    use desk_signal::turn_usage::query_turn_usage;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(turn_usage::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    fn sample(rx: usize, tx: usize) -> TurnSessionStatistics {
        TurnSessionStatistics {
            received_bytes: rx,
            send_bytes: tx,
            received_pkts: 1,
            send_pkts: 1,
            error_pkts: 0,
        }
    }

    fn now() -> DateTimeUtc {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 6, 24, 9, 30, 0).unwrap()
    }

    #[test]
    fn delta_saturates_on_counter_reset() {
        let base = sample(500, 500);
        let cur = sample(100, 100); // lower than baseline (reset)
        let d = delta_since(Some(&base), &cur);
        assert_eq!(d.received_bytes, 0);
        assert_eq!(d.sent_bytes, 0);
    }

    #[tokio::test]
    async fn flush_resolves_device_and_advances_baseline() {
        let db = memory_db().await;
        let stats = Arc::new(RwLock::new(Statistics::default()));
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        map.write().await.insert("conn-1".into(), "dev-1".into());

        // Bind an address and fold some bytes for conn-1.
        {
            let mut s = stats.write().unwrap();
            s.record_binding("127.0.0.1:5000".parse().unwrap(), "conn-1");
            s.record_recv("127.0.0.1:5000".parse().unwrap(), 100);
            s.record_send("127.0.0.1:5000".parse().unwrap(), 40);
        }

        let mut collector = TurnUsageCollector::new(stats.clone(), map.clone());
        collector.flush_once(&db, now()).await.unwrap();

        let rows = query_turn_usage(
            &db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_code, "dev-1");
        assert_eq!(rows[0].received_bytes, 100);
        assert_eq!(rows[0].sent_bytes, 40);

        // Second flush with no new traffic must be a no-op (baseline advanced).
        collector.flush_once(&db, now()).await.unwrap();
        let rows = query_turn_usage(
            &db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(rows[0].received_bytes, 100, "no double-counting");
    }

    #[tokio::test]
    async fn unresolved_connection_falls_back_to_connection_id() {
        let db = memory_db().await;
        let stats = Arc::new(RwLock::new(Statistics::default()));
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());

        {
            let mut s = stats.write().unwrap();
            s.record_binding("127.0.0.1:6000".parse().unwrap(), "conn-x");
            s.record_recv("127.0.0.1:6000".parse().unwrap(), 70);
        }

        let mut collector = TurnUsageCollector::new(stats.clone(), map.clone());
        collector.flush_once(&db, now()).await.unwrap();

        let rows = query_turn_usage(
            &db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_code, "conn-x", "falls back to connection_id");
        assert_eq!(rows[0].received_bytes, 70);
    }
}
