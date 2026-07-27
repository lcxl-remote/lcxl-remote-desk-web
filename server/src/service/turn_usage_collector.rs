//! Periodic TURN usage collector for the portable/signal server.
//!
//! Diffs the shared per-connection byte counters against a baseline each tick,
//! resolves each `connection_id` to a `device_code` via the live connection map
//! (falling back to the raw `connection_id`), and folds the delta into the local
//! sqlite hourly rollup. Collect-only: no billing, no owner, no node dimension.
//!
//! The counters belong to a TURN runtime, and a settings change replaces that
//! runtime, so the collector resolves them per pass instead of holding one set
//! forever. A holder would keep diffing counters nobody increments any more, and
//! silently report zero usage for the relay that is actually running.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use desk_signal::turn_usage::{
    ConnectionDeviceMap, TurnUsageDelta, truncate_to_hour, upsert_turn_usage,
};
use desk_turn::model::{Statistics, TurnApiState, TurnDirectionalCounters, TurnSessionStatistics};
use sea_orm::DatabaseConnection;
use tokio::sync::watch;

/// Hour-aligned UTC timestamp, matching the rollup's `hour_bucket` column type.
type DateTimeUtc = DateTime<Utc>;

/// Default cadence between collection passes.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// Where a pass reads its counters from, resolved each time.
#[derive(Clone)]
pub enum StatisticsSource {
    /// Follow whatever runtime is serving; `None` while none is.
    Runtime(watch::Receiver<Option<Arc<TurnApiState>>>),
    /// A fixed set of counters, for tests that drive the accounting directly.
    Fixed(Arc<RwLock<Statistics>>),
}

impl StatisticsSource {
    fn resolve(&self) -> Option<Arc<RwLock<Statistics>>> {
        match self {
            Self::Runtime(rx) => rx.borrow().as_ref().map(|s| s.statistics.clone()),
            Self::Fixed(stats) => Some(stats.clone()),
        }
    }
}

pub struct TurnUsageCollector {
    source: StatisticsSource,
    /// The counters the current baseline was measured against, so a runtime swap
    /// is detected by identity rather than guessed at from the values.
    current: Option<Arc<RwLock<Statistics>>>,
    conn_device_map: Arc<ConnectionDeviceMap>,
    /// Per-connection cumulative counters already persisted; the next pass only
    /// flushes the difference. Node-local and droppable (a restart re-baselines).
    baseline: HashMap<String, TurnSessionStatistics>,
}

impl TurnUsageCollector {
    pub fn new(source: StatisticsSource, conn_device_map: Arc<ConnectionDeviceMap>) -> Self {
        Self {
            source,
            current: None,
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
            if let Err(e) = self
                .tick(desk_signal::db::get_db(), chrono::Utc::now())
                .await
            {
                log::warn!("TURN usage collector flush failed: {}", e);
            }
        }
    }

    /// One tick: adopt a replacement runtime if there is one, and account for
    /// whatever is serving now.
    ///
    /// A runtime that has just been replaced is read one last time before its
    /// counters are let go. A fresh runtime starts from zero, so carrying the
    /// retired one's totals forward as the baseline would swallow everything the
    /// new one relays until it exceeded them — and simply dropping them loses the
    /// retired runtime's last stretch, which a settings change or a secret
    /// rotation produces as a matter of course rather than as a failure.
    async fn tick(
        &mut self,
        db: &DatabaseConnection,
        now: DateTimeUtc,
    ) -> Result<(), sea_orm::DbErr> {
        let resolved = self.source.resolve();
        let same = match (&self.current, &resolved) {
            (Some(held), Some(next)) => Arc::ptr_eq(held, next),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(retiring) = self.current.clone() {
                // Measured against the baseline still in place, which is the one
                // describing this runtime. A failure here returns before the
                // baseline is dropped, so the next tick retries the same stretch
                // rather than losing it.
                self.flush_once(db, now, &retiring).await?;
            }
            self.baseline.clear();
            self.current = resolved.clone();
        }
        let Some(statistics) = resolved else {
            // No runtime is serving; there is nothing to account for, and no
            // reason to touch the database.
            return Ok(());
        };
        self.flush_once(db, now, &statistics).await
    }

    /// One collection pass. Computes per-connection deltas, groups them by
    /// resolved `device_code`, and upserts each into the current hour bucket.
    /// Baseline advances only for connections whose upsert succeeded, so a
    /// failed write is retried (re-added to the delta) next pass.
    async fn flush_once(
        &mut self,
        db: &DatabaseConnection,
        now: DateTimeUtc,
        statistics: &Arc<RwLock<Statistics>>,
    ) -> Result<(), sea_orm::DbErr> {
        let snapshot = match statistics.read() {
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
            entry.0.relay_received_bytes += delta.relay_received_bytes;
            entry.0.relay_sent_bytes += delta.relay_sent_bytes;
            entry.0.relay_received_pkts += delta.relay_received_pkts;
            entry.0.relay_sent_pkts += delta.relay_sent_pkts;
            entry.0.control_received_bytes += delta.control_received_bytes;
            entry.0.control_sent_bytes += delta.control_sent_bytes;
            entry.0.control_received_pkts += delta.control_received_pkts;
            entry.0.control_sent_pkts += delta.control_sent_pkts;
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

/// Signed difference of a current cumulative sample from its baseline, per
/// traffic class. Saturates at zero per field to tolerate a counter reset (e.g.
/// runtime restart).
fn delta_since(
    base: Option<&TurnSessionStatistics>,
    cur: &TurnSessionStatistics,
) -> TurnUsageDelta {
    let default = TurnDirectionalCounters::default();
    let (base_relay, base_control) = match base {
        Some(b) => (&b.relay, &b.control),
        None => (&default, &default),
    };
    TurnUsageDelta {
        relay_received_bytes: cur
            .relay
            .received_bytes
            .saturating_sub(base_relay.received_bytes) as i64,
        relay_sent_bytes: cur.relay.send_bytes.saturating_sub(base_relay.send_bytes) as i64,
        relay_received_pkts: cur
            .relay
            .received_pkts
            .saturating_sub(base_relay.received_pkts) as i64,
        relay_sent_pkts: cur.relay.send_pkts.saturating_sub(base_relay.send_pkts) as i64,
        control_received_bytes: cur
            .control
            .received_bytes
            .saturating_sub(base_control.received_bytes) as i64,
        control_sent_bytes: cur
            .control
            .send_bytes
            .saturating_sub(base_control.send_bytes) as i64,
        control_received_pkts: cur
            .control
            .received_pkts
            .saturating_sub(base_control.received_pkts) as i64,
        control_sent_pkts: cur.control.send_pkts.saturating_sub(base_control.send_pkts) as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal::entity::turn_usage;
    use desk_signal::turn_usage::query_turn_usage;
    use desk_turn::model::TurnTrafficClass;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(turn_usage::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    /// A cumulative sample with `rx`/`tx` relay bytes (and a fixed small amount of
    /// control traffic to exercise both dimensions).
    fn sample(rx: usize, tx: usize) -> TurnSessionStatistics {
        TurnSessionStatistics {
            relay: TurnDirectionalCounters {
                received_bytes: rx,
                send_bytes: tx,
                received_pkts: 1,
                send_pkts: 1,
            },
            control: TurnDirectionalCounters {
                received_bytes: 5,
                send_bytes: 5,
                received_pkts: 1,
                send_pkts: 1,
            },
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
        assert_eq!(d.relay_received_bytes, 0);
        assert_eq!(d.relay_sent_bytes, 0);
    }

    /// Drive one pass against the collector's own source, the way `run` does.
    async fn flush(
        collector: &mut TurnUsageCollector,
        db: &DatabaseConnection,
        now: DateTimeUtc,
    ) -> Result<(), sea_orm::DbErr> {
        collector.tick(db, now).await
    }

    #[tokio::test]
    async fn flush_resolves_device_and_advances_baseline() {
        let db = memory_db().await;
        let stats = Arc::new(RwLock::new(Statistics::default()));
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        map.write().await.insert("conn-1".into(), "dev-1".into());

        // Bind an address and fold relay + control bytes for conn-1.
        {
            let mut s = stats.write().unwrap();
            let addr = "127.0.0.1:5000".parse().unwrap();
            s.record_binding(addr, "conn-1");
            s.record_recv(addr, 100, TurnTrafficClass::Relay);
            s.record_send(addr, 40, TurnTrafficClass::Relay);
            s.record_recv(addr, 9, TurnTrafficClass::Control);
        }

        let mut collector =
            TurnUsageCollector::new(StatisticsSource::Fixed(stats.clone()), map.clone());
        flush(&mut collector, &db, now()).await.unwrap();

        let rows = query_turn_usage(
            &db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
            desk_signal::usage_query::Granularity::Hour,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_code, "dev-1");
        assert_eq!(rows[0].relay_received_bytes, 100);
        assert_eq!(rows[0].relay_sent_bytes, 40);
        assert_eq!(rows[0].control_received_bytes, 9);

        // Second flush with no new traffic must be a no-op (baseline advanced).
        flush(&mut collector, &db, now()).await.unwrap();
        let rows = query_turn_usage(
            &db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
            desk_signal::usage_query::Granularity::Hour,
        )
        .await
        .unwrap();
        assert_eq!(rows[0].relay_received_bytes, 100, "no double-counting");
    }

    /// A settings change replaces the runtime, and with it the counters. The
    /// collector must re-baseline on the new instance: keeping the old baseline
    /// would swallow everything the new relay carries until it happened to
    /// exceed the retired runtime's totals.
    #[tokio::test]
    async fn a_replaced_runtime_re_baselines_instead_of_swallowing_its_traffic() {
        let db = memory_db().await;
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        let (runtime_tx, runtime_rx) = watch::channel(None);
        let mut collector =
            TurnUsageCollector::new(StatisticsSource::Runtime(runtime_rx), map.clone());

        // Nothing is serving: no counters to read, and no database work.
        assert!(collector.tick(&db, now()).await.is_ok());

        // A runtime comes up and relays a lot.
        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5000", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        flush(&mut collector, &db, now()).await.unwrap();
        assert_eq!(total_relay_received(&db).await, 1_000);

        // It is replaced; the fresh runtime's counters start from zero.
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5000", 7);
        runtime_tx.send_replace(Some(second.clone()));
        flush(&mut collector, &db, now()).await.unwrap();
        assert_eq!(
            total_relay_received(&db).await,
            1_007,
            "the new runtime's traffic must be added, not compared away"
        );

        first.server.close().await.unwrap();
        second.server.close().await.unwrap();
    }

    /// Replacing a runtime is a settings change or a secret rotation, not a
    /// crash. Whatever the outgoing one relayed after its last pass is real
    /// traffic, and it only reaches the hourly bucket if it is read before its
    /// counters are let go for the successor's.
    #[tokio::test]
    async fn the_traffic_a_retiring_runtime_relayed_last_is_still_collected() {
        let db = memory_db().await;
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        let (runtime_tx, runtime_rx) = watch::channel(None);
        let mut collector =
            TurnUsageCollector::new(StatisticsSource::Runtime(runtime_rx), map.clone());

        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5100", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        collector.tick(&db, now()).await.unwrap();
        assert_eq!(total_relay_received(&db).await, 1_000);

        // Relayed after that pass and before the runtime was replaced.
        record(&first, "conn-1", "127.0.0.1:5100", 40);
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5100", 7);
        runtime_tx.send_replace(Some(second.clone()));

        collector.tick(&db, now()).await.unwrap();

        assert_eq!(
            total_relay_received(&db).await,
            1_047,
            "the retiring runtime's last 40 bytes must be collected, not dropped \
             with its counters"
        );

        first.server.close().await.unwrap();
        second.server.close().await.unwrap();
    }

    /// A real TURN runtime on an ephemeral loopback port; the collector reads
    /// counters off `TurnApiState`, so a stand-in would not exercise the path.
    async fn loopback_runtime() -> Arc<desk_turn::model::TurnApiState> {
        use desk_turn::model::{TurnInterface, TurnTransport};
        let settings = desk_turn::model::TurnSettings {
            interfaces: vec![TurnInterface {
                transport: TurnTransport::UDP,
                listen: "127.0.0.1:0".into(),
                external: "127.0.0.1:3478".into(),
            }],
            ..Default::default()
        };
        let statistics = Arc::new(RwLock::new(Statistics::default()));
        let auth = Arc::new(crate::model::turn::TurnAuthHandler::new(
            settings.clone(),
            actix_web::web::Data::new(desk_signal::model::SharedConnectionMap::from(
                std::collections::BTreeMap::new(),
            )),
            statistics.clone(),
        ));
        desk_turn::service::startup_turn_server(settings, auth, statistics)
            .await
            .expect("a loopback TURN runtime should start")
    }

    /// Fold `bytes` of relayed traffic for `conn_id` into a runtime's counters.
    fn record(
        runtime: &Arc<desk_turn::model::TurnApiState>,
        conn_id: &str,
        addr: &str,
        bytes: usize,
    ) {
        let addr = addr.parse().unwrap();
        let mut stats = runtime.statistics.write().unwrap();
        stats.record_binding(addr, conn_id);
        stats.record_recv(addr, bytes, TurnTrafficClass::Relay);
    }

    async fn total_relay_received(db: &DatabaseConnection) -> i64 {
        query_turn_usage(
            db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
            desk_signal::usage_query::Granularity::Hour,
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.relay_received_bytes)
        .sum()
    }

    #[tokio::test]
    async fn unresolved_connection_falls_back_to_connection_id() {
        let db = memory_db().await;
        let stats = Arc::new(RwLock::new(Statistics::default()));
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());

        {
            let mut s = stats.write().unwrap();
            let addr = "127.0.0.1:6000".parse().unwrap();
            s.record_binding(addr, "conn-x");
            s.record_recv(addr, 70, TurnTrafficClass::Relay);
        }

        let mut collector =
            TurnUsageCollector::new(StatisticsSource::Fixed(stats.clone()), map.clone());
        flush(&mut collector, &db, now()).await.unwrap();

        let rows = query_turn_usage(
            &db,
            now() - chrono::Duration::hours(1),
            now() + chrono::Duration::hours(1),
            desk_signal::usage_query::Granularity::Hour,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_code, "conn-x", "falls back to connection_id");
        assert_eq!(rows[0].relay_received_bytes, 70);
    }
}
