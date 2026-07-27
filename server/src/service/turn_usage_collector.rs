//! Periodic TURN usage collector for the portable/signal server.
//!
//! Diffs the shared per-connection byte counters against a baseline each tick,
//! resolves each `connection_id` to a `device_code` via the live connection map
//! (falling back to the raw `connection_id`), and folds the delta into the local
//! sqlite hourly rollup. Collect-only: no billing, no owner, no node dimension.
//!
//! The counters belong to a TURN runtime, and a settings change replaces that
//! runtime, so the collector follows the supervisor rather than holding one set
//! forever. A holder would keep diffing counters nobody increments any more, and
//! silently report zero usage for the relay that is actually running.
//!
//! Following it takes two signals, because "what is serving" and "whose counters
//! still need collecting" are different questions:
//!
//! - The published runtime says what to account for **now**. It is cleared while
//!   a teardown is under way, so an empty publication means only "not being
//!   advertised" — a close that failed leaves it empty while its runtime keeps
//!   relaying. The collector therefore never releases counters because of it.
//! - The retirement queue says what may be **let go**, one entry per runtime that
//!   actually stopped. That is the only thing that retires a set of counters, and
//!   because it is a queue rather than a published value, two restarts between
//!   two passes still deliver both.
//!
//! Each runtime carries its own baseline for as long as the collector holds it,
//! so a runtime that comes back (the desired state swung back to a runtime whose
//! close failed) resumes where it left off instead of being counted from zero all
//! over again.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use desk_signal::turn_usage::{
    ConnectionDeviceMap, TurnUsageDelta, truncate_to_hour, upsert_turn_usage,
};
use desk_turn::model::{Statistics, TurnApiState, TurnDirectionalCounters, TurnSessionStatistics};
use desk_turn::supervisor::RetiredRuntimes;
use sea_orm::DatabaseConnection;
use tokio::sync::watch;

/// Hour-aligned UTC timestamp, matching the rollup's `hour_bucket` column type.
type DateTimeUtc = DateTime<Utc>;

/// Default cadence between collection passes.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// Where a pass reads its counters from.
pub enum StatisticsSource {
    /// Follow a supervisor. Both halves come from the same one: the runtime it
    /// publishes and the runtimes it retires describe one lifecycle, and pairing
    /// either with another supervisor's would account for neither.
    Supervisor {
        runtime_rx: watch::Receiver<Option<Arc<TurnApiState>>>,
        retired: RetiredRuntimes,
    },
    /// A fixed set of counters that never retires, for tests that drive the
    /// accounting directly.
    Fixed(Arc<RwLock<Statistics>>),
}

impl StatisticsSource {
    /// The counters filling right now, if any are being advertised.
    fn serving(&self) -> Option<Arc<RwLock<Statistics>>> {
        match self {
            Self::Supervisor { runtime_rx, .. } => {
                runtime_rx.borrow().as_ref().map(|s| s.statistics.clone())
            }
            Self::Fixed(stats) => Some(stats.clone()),
        }
    }

    /// Everything that has stopped serving since the last call, oldest first.
    fn take_retired(&mut self) -> Vec<Arc<RwLock<Statistics>>> {
        match self {
            // `try_recv` also ends the iteration once the supervisor is gone,
            // which is the right moment to stop expecting retirements.
            Self::Supervisor { retired, .. } => {
                std::iter::from_fn(|| retired.try_recv().ok()).collect()
            }
            Self::Fixed(_) => Vec::new(),
        }
    }
}

/// One runtime's counters and how much of them has already been written.
struct Accounted {
    statistics: Arc<RwLock<Statistics>>,
    /// Per-connection cumulative counters already persisted; the next pass only
    /// writes the difference. Node-local and droppable (a restart re-baselines).
    baseline: HashMap<String, TurnSessionStatistics>,
    /// Whether the supervisor has said this runtime stopped. Only that makes the
    /// counters final, and only a final set may be let go once written.
    retired: bool,
}

impl Accounted {
    /// Starts with an empty baseline, which is what "nothing has been written for
    /// these counters yet" means — true both for a runtime that just started and
    /// for one the collector is meeting for the first time as it retires.
    fn new(statistics: Arc<RwLock<Statistics>>, retired: bool) -> Self {
        Self {
            statistics,
            baseline: HashMap::new(),
            retired,
        }
    }

    fn is(&self, statistics: &Arc<RwLock<Statistics>>) -> bool {
        Arc::ptr_eq(&self.statistics, statistics)
    }
}

pub struct TurnUsageCollector {
    source: StatisticsSource,
    /// Every runtime with counters still to write, in the order they were first
    /// seen. An entry is kept by identity rather than by which one is current,
    /// because the two signals are read separately: a pass can see a replacement
    /// published before the retirement of the runtime it replaced has arrived.
    /// Keying on identity means the outgoing runtime keeps its baseline through
    /// that gap instead of being dropped and later re-counted from zero.
    accounted: Vec<Accounted>,
    conn_device_map: Arc<ConnectionDeviceMap>,
}

impl TurnUsageCollector {
    pub fn new(source: StatisticsSource, conn_device_map: Arc<ConnectionDeviceMap>) -> Self {
        Self {
            source,
            accounted: Vec::new(),
            conn_device_map,
        }
    }

    /// Run forever, flushing on a fixed interval.
    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.tick(desk_signal::db::get_db(), chrono::Utc::now())
                .await;
        }
    }

    /// One tick: take in what has retired, adopt whatever is serving, and write
    /// what each of them has accumulated.
    async fn tick(&mut self, db: &DatabaseConnection, now: DateTimeUtc) {
        // Step 1: note what has stopped. A runtime already being accounted for
        // keeps the baseline it has; one that came and went between two passes is
        // met for the first time here, with an empty baseline — which is right,
        // since nothing was ever written for it.
        for statistics in self.source.take_retired() {
            match self.accounted.iter_mut().find(|a| a.is(&statistics)) {
                Some(entry) => entry.retired = true,
                None => self.accounted.push(Accounted::new(statistics, true)),
            }
        }

        // Step 2: start on whatever is serving, if it is not already tracked.
        // Nothing is released here: an empty publication means a teardown is
        // under way or has failed, not that any counters are finished.
        if let Some(statistics) = self.source.serving()
            && !self.accounted.iter().any(|a| a.is(&statistics))
        {
            self.accounted.push(Accounted::new(statistics, false));
        }

        // Step 3: write what each of them has accumulated. An entry is let go
        // only once its runtime has stopped *and* everything it holds is stored:
        // either condition alone would drop counters that still matter.
        let mut kept = Vec::new();
        for mut entry in std::mem::take(&mut self.accounted) {
            let settled = Self::flush(db, now, &self.conn_device_map, &mut entry).await;
            if !(entry.retired && settled) {
                kept.push(entry);
            }
        }
        self.accounted = kept;
    }

    /// One collection pass over one runtime's counters. Computes per-connection
    /// deltas, groups them by resolved `device_code`, and upserts each into the
    /// current hour bucket.
    ///
    /// Returns whether everything those counters hold is now persisted. The
    /// baseline advances only for connections whose upsert succeeded, so a failed
    /// write is retried (re-added to the delta) next pass — and answering `false`
    /// is what keeps a retired runtime's counters around long enough for that
    /// retry to have something to read.
    async fn flush(
        db: &DatabaseConnection,
        now: DateTimeUtc,
        conn_device_map: &ConnectionDeviceMap,
        accounted: &mut Accounted,
    ) -> bool {
        let snapshot = match accounted.statistics.read() {
            Ok(stats) => stats.snapshot_by_connection(),
            Err(e) => {
                // Unreadable, so what it holds is unknown rather than nothing.
                log::warn!("TURN statistics lock poisoned, skipping flush: {}", e);
                return false;
            }
        };

        // Resolve and aggregate per-connection deltas into per-device deltas,
        // remembering which connection_ids contributed so the baseline can
        // advance atomically with a successful device upsert.
        let hour = truncate_to_hour(now);
        let mut per_device: HashMap<String, (TurnUsageDelta, Vec<String>)> = HashMap::new();
        for (conn_id, cur) in &snapshot {
            let delta = delta_since(accounted.baseline.get(conn_id), cur);
            if delta.is_zero() {
                continue;
            }
            let device_code = resolve_device(conn_device_map, conn_id).await;
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

        let mut settled = true;
        for (device_code, (delta, conn_ids)) in per_device {
            match upsert_turn_usage(db, &device_code, hour, &delta).await {
                Ok(()) => {
                    // Advance the baseline for the contributing connections.
                    for conn_id in conn_ids {
                        if let Some(cur) = snapshot.get(&conn_id) {
                            accounted.baseline.insert(conn_id, cur.clone());
                        }
                    }
                }
                Err(e) => {
                    // Leave the baseline untouched so this delta is retried.
                    log::warn!("upsert_turn_usage failed for device {}: {}", device_code, e);
                    settled = false;
                }
            }
        }
        settled
    }
}

/// Resolve a `connection_id` to its `device_code`, or fall back to the raw
/// `connection_id` when no binding exists (unresolved connections still get
/// collected, just keyed by their id).
async fn resolve_device(conn_device_map: &ConnectionDeviceMap, conn_id: &str) -> String {
    conn_device_map
        .read()
        .await
        .get(conn_id)
        .cloned()
        .unwrap_or_else(|| conn_id.to_string())
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
        // One connection: each `sqlite::memory:` connection gets a database of
        // its own, so a pool that hands out a second one would answer from an
        // empty schema.
        let mut options = sea_orm::ConnectOptions::new("sqlite::memory:");
        options.max_connections(1);
        let db = Database::connect(options).await.unwrap();
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
    async fn flush(collector: &mut TurnUsageCollector, db: &DatabaseConnection, now: DateTimeUtc) {
        collector.tick(db, now).await;
    }

    /// The supervisor's two write ends, as a test holds them: what is published
    /// as serving, and what is handed over as retired.
    type SupervisorControls = (
        watch::Sender<Option<Arc<TurnApiState>>>,
        tokio::sync::mpsc::UnboundedSender<Arc<RwLock<Statistics>>>,
    );

    /// A collector wired the way production wires it, plus the two handles a test
    /// uses to play supervisor: publish what is serving, and retire what stopped.
    fn supervised(map: Arc<ConnectionDeviceMap>) -> (TurnUsageCollector, SupervisorControls) {
        let (runtime_tx, runtime_rx) = watch::channel(None);
        let (retired_tx, retired) = tokio::sync::mpsc::unbounded_channel();
        let collector = TurnUsageCollector::new(
            StatisticsSource::Supervisor {
                runtime_rx,
                retired,
            },
            map,
        );
        (collector, (runtime_tx, retired_tx))
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
        flush(&mut collector, &db, now()).await;

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
        flush(&mut collector, &db, now()).await;
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
        let (mut collector, (runtime_tx, retired_tx)) = supervised(map.clone());

        // Nothing is serving: no counters to read, and no database work.
        collector.tick(&db, now()).await;

        // A runtime comes up and relays a lot.
        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5000", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        flush(&mut collector, &db, now()).await;
        assert_eq!(total_relay_received(&db).await, 1_000);

        // It is replaced; the fresh runtime's counters start from zero.
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5000", 7);
        retired_tx.send(first.statistics.clone()).unwrap();
        runtime_tx.send_replace(Some(second.clone()));
        flush(&mut collector, &db, now()).await;
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
        let (mut collector, (runtime_tx, retired_tx)) = supervised(map.clone());

        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5100", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(total_relay_received(&db).await, 1_000);

        // Relayed after that pass and before the runtime was replaced.
        record(&first, "conn-1", "127.0.0.1:5100", 40);
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5100", 7);
        retired_tx.send(first.statistics.clone()).unwrap();
        runtime_tx.send_replace(Some(second.clone()));

        collector.tick(&db, now()).await;

        assert_eq!(
            total_relay_received(&db).await,
            1_047,
            "the retiring runtime's last 40 bytes must be collected, not dropped \
             with its counters"
        );

        first.server.close().await.unwrap();
        second.server.close().await.unwrap();
    }

    /// Two restarts inside one flush interval. The middle runtime is never the
    /// published one when anybody looks, so everything it relayed is only
    /// reachable through its retirement — a signal that keeps every entry rather
    /// than the latest.
    #[tokio::test]
    async fn a_runtime_nobody_saw_serving_is_still_collected() {
        let db = memory_db().await;
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        let (mut collector, (runtime_tx, retired_tx)) = supervised(map.clone());

        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5200", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(total_relay_received(&db).await, 1_000);

        // Both restarts happen before the next pass, and each runtime relays
        // something the collector has not written yet.
        record(&first, "conn-1", "127.0.0.1:5200", 40);
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5200", 300);
        retired_tx.send(first.statistics.clone()).unwrap();
        runtime_tx.send_replace(Some(second.clone()));

        let third = loopback_runtime().await;
        record(&third, "conn-1", "127.0.0.1:5200", 7);
        retired_tx.send(second.statistics.clone()).unwrap();
        runtime_tx.send_replace(Some(third.clone()));

        collector.tick(&db, now()).await;

        assert_eq!(
            total_relay_received(&db).await,
            1_347,
            "both replaced runtimes count: the first's tail and everything the \
             second relayed before anyone read it"
        );

        first.server.close().await.unwrap();
        second.server.close().await.unwrap();
        third.server.close().await.unwrap();
    }

    /// The two signals are read one after the other, so a pass can see the
    /// replacement published and only learn on a later pass that the runtime it
    /// replaced has retired.
    ///
    /// The outgoing runtime has to keep its baseline across that gap. Dropping it
    /// on sight of the replacement loses whatever it relayed last, and then the
    /// retirement — arriving to find nothing tracked — reads its counters from
    /// zero and bills its whole lifetime a second time.
    #[tokio::test]
    async fn a_retirement_that_arrives_after_the_replacement_neither_loses_nor_repeats() {
        let db = memory_db().await;
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        let (mut collector, (runtime_tx, retired_tx)) = supervised(map.clone());

        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5500", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(total_relay_received(&db).await, 1_000);

        // The replacement is published; the retirement has not arrived yet.
        record(&first, "conn-1", "127.0.0.1:5500", 40);
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5500", 7);
        runtime_tx.send_replace(Some(second.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(
            total_relay_received(&db).await,
            1_047,
            "the runtime being replaced is still accounted for until it retires"
        );

        // Now it arrives.
        retired_tx.send(first.statistics.clone()).unwrap();
        collector.tick(&db, now()).await;
        assert_eq!(
            total_relay_received(&db).await,
            1_047,
            "a late retirement settles the runtime; it must not re-read it from zero"
        );

        first.server.close().await.unwrap();
        second.server.close().await.unwrap();
    }

    /// A teardown clears the published runtime before closing it, and a close
    /// that fails leaves it cleared while its runtime keeps relaying. The desired
    /// state can then swing back and republish that same runtime.
    ///
    /// Nothing retired, so nothing may be re-counted: treating the empty
    /// publication as the end of a runtime would drop its baseline and bill its
    /// entire lifetime again the moment it reappeared.
    #[tokio::test]
    async fn a_publication_gap_does_not_recount_the_runtime_that_comes_back() {
        let db = memory_db().await;
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        let (mut collector, (runtime_tx, _retired_tx)) = supervised(map.clone());

        let runtime = loopback_runtime().await;
        record(&runtime, "conn-1", "127.0.0.1:5300", 1_000);
        runtime_tx.send_replace(Some(runtime.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(total_relay_received(&db).await, 1_000);

        // The teardown starts, the close fails, and the runtime keeps relaying.
        runtime_tx.send_replace(None);
        record(&runtime, "conn-1", "127.0.0.1:5300", 40);
        collector.tick(&db, now()).await;
        assert_eq!(
            total_relay_received(&db).await,
            1_040,
            "a runtime that is still relaying is still accounted for"
        );

        // The desired state swings back and the same runtime is republished.
        runtime_tx.send_replace(Some(runtime.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(
            total_relay_received(&db).await,
            1_040,
            "the runtime came back, so its history must not be written again"
        );

        runtime.server.close().await.unwrap();
    }

    /// A retired runtime's counters are the only copy of what it relayed last. If
    /// the write fails they have to stay reachable until one succeeds — letting
    /// go on a failed write loses that stretch for good, because the runtime is
    /// closed and nothing will ever report it again.
    #[tokio::test]
    async fn a_retired_runtime_is_held_until_its_last_stretch_is_written() {
        let db = memory_db().await;
        let map: Arc<ConnectionDeviceMap> = Arc::new(ConnectionDeviceMap::default());
        let (mut collector, (runtime_tx, retired_tx)) = supervised(map.clone());

        let first = loopback_runtime().await;
        record(&first, "conn-1", "127.0.0.1:5400", 1_000);
        runtime_tx.send_replace(Some(first.clone()));
        collector.tick(&db, now()).await;
        assert_eq!(total_relay_received(&db).await, 1_000);

        record(&first, "conn-1", "127.0.0.1:5400", 40);
        let second = loopback_runtime().await;
        record(&second, "conn-1", "127.0.0.1:5400", 7);
        retired_tx.send(first.statistics.clone()).unwrap();
        runtime_tx.send_replace(Some(second.clone()));

        // Every write in this pass fails: the rollup table is not there.
        let unwritable = Database::connect("sqlite::memory:").await.unwrap();
        collector.tick(&unwritable, now()).await;

        // Writes work again; the runtime that retired is long closed, so the 40
        // bytes exist nowhere but in the counters the collector kept.
        collector.tick(&db, now()).await;

        assert_eq!(
            total_relay_received(&db).await,
            1_047,
            "the retried write must include the retired runtime's last stretch"
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
        flush(&mut collector, &db, now()).await;

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
