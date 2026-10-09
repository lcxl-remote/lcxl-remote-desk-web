//! One bounded writer per process, independent of model futures and Actix workers.

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use desk_diagnose_core::model_observability::{
    Attribution, ObservabilitySeam, ObservationContext, ObservationEvent,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::model::model_metrics::{ComponentState, MetricsSettings};

pub const QUEUE_CAPACITY: usize = 1_024;
pub const LOW_PRIORITY_CAPACITY: usize = 128;
const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct AggregateProgress {
    pub applied: u32,
    pub discarded: u32,
    pub last_received_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct WriterHealth {
    pub state: ComponentState,
    pub last_persisted_ms: Option<i64>,
    pub last_aggregated_ms: Option<i64>,
    pub dropped: u64,
    pub discarded: u64,
    pub settings_revision: Option<String>,
    pub reported_at_ms: i64,
    pub reason: Option<&'static str>,
}

#[async_trait]
pub trait MetricsBackend: Send + Sync {
    async fn settings(&self) -> Result<MetricsSettings, ()>;
    async fn persist(&self, events: &[ObservationEvent], now_ms: i64) -> Result<u32, ()>;
    async fn aggregate(&self, now_ms: i64) -> Result<AggregateProgress, ()>;
    async fn cleanup(&self, now_ms: i64) -> Result<(), ()>;
    async fn report(&self, health: &WriterHealth) -> Result<(), ()>;
}

#[derive(Default)]
struct ConfigurationCache {
    loaded: Option<Instant>,
    settings: Option<MetricsSettings>,
}

pub struct Collector {
    critical: mpsc::Sender<ObservationEvent>,
    ordinary: mpsc::Sender<ObservationEvent>,
    cache: RwLock<ConfigurationCache>,
    dropped: AtomicU64,
    storage_failed: AtomicBool,
    stopping: AtomicBool,
    stopped: AtomicBool,
    stop: tokio::sync::Notify,
    done: tokio::sync::Notify,
    ready: tokio::sync::Notify,
}

pub struct Receiver {
    critical: mpsc::Receiver<ObservationEvent>,
    ordinary: mpsc::Receiver<ObservationEvent>,
}

impl Collector {
    pub fn channel() -> (Arc<Self>, Receiver) {
        let (critical, critical_rx) = mpsc::channel(QUEUE_CAPACITY - LOW_PRIORITY_CAPACITY);
        let (ordinary, ordinary_rx) = mpsc::channel(LOW_PRIORITY_CAPACITY);
        (
            Arc::new(Self {
                critical,
                ordinary,
                cache: RwLock::new(ConfigurationCache::default()),
                dropped: AtomicU64::new(0),
                storage_failed: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                stop: tokio::sync::Notify::new(),
                done: tokio::sync::Notify::new(),
                ready: tokio::sync::Notify::new(),
            }),
            Receiver {
                critical: critical_rx,
                ordinary: ordinary_rx,
            },
        )
    }

    pub fn context(
        self: &Arc<Self>,
        call_id: String,
        now_ms: i64,
        attribution: Attribution,
    ) -> Option<ObservationContext> {
        if !self.enabled() || !attribution.is_bounded() {
            return None;
        }
        Some(ObservationContext::new(
            call_id,
            now_ms,
            attribution,
            self.clone(),
        ))
    }

    pub fn enabled(&self) -> bool {
        if self.stopping.load(Ordering::Relaxed) {
            return false;
        }
        let Ok(cache) = self.cache.try_read() else {
            return false;
        };
        cache
            .loaded
            .is_some_and(|loaded| loaded.elapsed() <= CACHE_TTL)
            && cache
                .settings
                .as_ref()
                .is_some_and(|settings| settings.enabled)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn drop_event(&self) {
        let _ = self
            .dropped
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            });
    }

    fn refresh(&self, settings: MetricsSettings) {
        if let Ok(mut cache) = self.cache.try_write() {
            cache.settings = Some(settings);
            cache.loaded = Some(Instant::now());
            self.storage_failed.store(false, Ordering::Relaxed);
        }
    }

    pub fn state(&self) -> ComponentState {
        if self.stopped.load(Ordering::Relaxed) {
            return ComponentState::Unavailable;
        }
        let Ok(cache) = self.cache.try_read() else {
            return ComponentState::CacheExpired;
        };
        if cache.loaded.is_none() {
            return if self.storage_failed.load(Ordering::Relaxed) {
                ComponentState::Unavailable
            } else {
                ComponentState::Initializing
            };
        }
        if cache
            .loaded
            .is_some_and(|loaded| loaded.elapsed() > CACHE_TTL)
        {
            return ComponentState::CacheExpired;
        }
        if cache
            .settings
            .as_ref()
            .is_some_and(|settings| settings.enabled)
        {
            ComponentState::Ready
        } else {
            ComponentState::Disabled
        }
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.stop.notify_one();
        let wait = self.done.notified();
        tokio::pin!(wait);
        wait.as_mut().enable();
        if !self.stopped.load(Ordering::Relaxed) {
            let _ = tokio::time::timeout(Duration::from_secs(7), wait).await;
        }
    }

    fn revision(&self) -> Option<String> {
        self.cache.read().ok().and_then(|cache| {
            cache
                .settings
                .as_ref()
                .map(|settings| settings.revision.clone())
        })
    }
}

impl ObservabilitySeam for Collector {
    fn submit(&self, event: ObservationEvent) {
        if self.stopping.load(Ordering::Relaxed) || !event.is_bounded() {
            self.drop_event();
            return;
        }
        let runtime = matches!(
            &event.payload,
            desk_diagnose_core::model_observability::ObservationPayload::Runtime(_)
        );
        if runtime && !self.enabled() {
            return;
        }
        let queue = if runtime {
            &self.ordinary
        } else {
            &self.critical
        };
        if queue.try_send(event).is_err() {
            self.drop_event();
        } else {
            self.ready.notify_one();
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Storage initialization is supplied as part of the background settings read.
pub async fn run(collector: Arc<Collector>, receiver: Receiver, backend: Arc<dyn MetricsBackend>) {
    use futures_util::FutureExt;
    if std::panic::AssertUnwindSafe(run_inner(collector.clone(), receiver, backend))
        .catch_unwind()
        .await
        .is_err()
    {
        collector.storage_failed.store(true, Ordering::Relaxed);
        collector.stopping.store(true, Ordering::Relaxed);
        collector.stopped.store(true, Ordering::Relaxed);
        collector.done.notify_waiters();
    }
}

async fn run_inner(
    collector: Arc<Collector>,
    mut receiver: Receiver,
    backend: Arc<dyn MetricsBackend>,
) {
    let mut refresh = tokio::time::interval(Duration::from_secs(15));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut cleanup = tokio::time::interval(Duration::from_secs(30));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut batch = Vec::with_capacity(100);
    let mut retries = 0u32;
    let mut retry_started: Option<Instant> = None;
    let mut last_persisted = None;
    let mut last_aggregated = None;
    let mut discarded = 0u64;
    let mut failed = false;
    let mut retry_due: Option<Instant> = None;
    let mut draining: Option<Instant> = None;
    loop {
        if collector.stopping.load(Ordering::Relaxed) && draining.is_none() {
            receiver.critical.close();
            receiver.ordinary.close();
            draining = Some(Instant::now() + Duration::from_secs(5));
        }
        if draining.is_some_and(|deadline| Instant::now() >= deadline)
            || (draining.is_some()
                && batch.is_empty()
                && receiver.critical.is_empty()
                && receiver.ordinary.is_empty())
        {
            break;
        }
        tokio::select! {
            biased;
            _ = collector.stop.notified(), if draining.is_none() => {},
            _ = refresh.tick(), if draining.is_none() => {
                if let Ok(Ok(settings)) = tokio::time::timeout(Duration::from_secs(3), backend.settings()).await {
                    if settings.validate().is_ok() { collector.refresh(settings); }
                    else { collector.storage_failed.store(true, Ordering::Relaxed); failed = true; }
                } else { collector.storage_failed.store(true, Ordering::Relaxed); failed = true; }
            },
            _ = cleanup.tick(), if draining.is_none() => {
                if !matches!(collector.state(), ComponentState::Initializing)
                    && !matches!(tokio::time::timeout(Duration::from_secs(3), backend.cleanup(now_ms())).await, Ok(Ok(()))) {
                    failed = true;
                }
            },
            _ = async {
                if batch.is_empty() {
                    tokio::select! {
                        _ = collector.ready.notified() => {},
                        _ = tick.tick() => {},
                    }
                } else { tick.tick().await; }
            } => {
                if batch.is_empty() {
                    // Reserve a bounded share for operational facts under a
                    // sustained stream of request lifecycle events.
                    for _ in 0..80 { if let Ok(event) = receiver.critical.try_recv() { batch.push(event); } else { break; } }
                    for _ in 0..20 { if let Ok(event) = receiver.ordinary.try_recv() { batch.push(event); } else { break; } }
                    while batch.len() < 100 {
                        match receiver.critical.try_recv().or_else(|_| receiver.ordinary.try_recv()) {
                            Ok(event) => batch.push(event), Err(_) => break,
                        }
                    }
                }
                let now = now_ms();
                if !batch.is_empty() && retry_due.is_none_or(|due| Instant::now() >= due) {
                    let start = *retry_started.get_or_insert_with(Instant::now);
                    match tokio::time::timeout(draining.map_or(Duration::from_secs(3), |deadline| deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(3))), backend.persist(&batch, now)).await {
                        Ok(Ok(dropped)) => {
                            discarded = discarded.saturating_add(u64::from(dropped));
                            last_persisted = Some(now); batch.clear(); retries = 0; retry_started = None; retry_due = None; failed = false;
                        },
                        _ => {
                            retries += 1; failed = true;
                            retry_due = Some(Instant::now() + Duration::from_secs(1u64 << retries.min(3)));
                            if retries >= 3 || start.elapsed() >= Duration::from_secs(30) {
                                discarded = discarded.saturating_add(batch.len() as u64); batch.clear(); retries = 0; retry_started = None; retry_due = None;
                            }
                        },
                    }
                }
                match tokio::time::timeout(draining.map_or(Duration::from_secs(3), |deadline| deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(3))), backend.aggregate(now)).await {
                    Ok(Ok(progress)) => { if let Some(received) = progress.last_received_ms { last_aggregated = Some(received); } discarded = discarded.saturating_add(u64::from(progress.discarded)); },
                    _ => failed = true,
                }
                let state = if failed || collector.dropped() > 0 || discarded > 0 { ComponentState::Degraded } else { collector.state() };
                let health = WriterHealth { state, last_persisted_ms: last_persisted, last_aggregated_ms: last_aggregated,
                    dropped: collector.dropped(), discarded, settings_revision: collector.revision(), reported_at_ms: now,
                    reason: failed.then_some("storage_unavailable") };
                let _ = tokio::time::timeout(draining.map_or(Duration::from_secs(2), |deadline| deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(2))), backend.report(&health)).await;
                // A Notify permit coalesces submissions. Keep draining existing
                // backlog in bounded batches without needing another producer.
                // A failed batch retains its timed backoff instead of spinning.
                if batch.is_empty() && (!receiver.critical.is_empty() || !receiver.ordinary.is_empty()) {
                    collector.ready.notify_one();
                }
            },
        }
    }
    discarded = discarded.saturating_add(batch.len() as u64);
    while receiver.critical.try_recv().is_ok() {
        discarded = discarded.saturating_add(1);
    }
    while receiver.ordinary.try_recv().is_ok() {
        discarded = discarded.saturating_add(1);
    }
    let health = WriterHealth {
        state: ComponentState::Disabled,
        last_persisted_ms: last_persisted,
        last_aggregated_ms: last_aggregated,
        dropped: collector.dropped(),
        discarded,
        settings_revision: collector.revision(),
        reported_at_ms: now_ms(),
        reason: Some("writer_stopped"),
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), backend.report(&health)).await;
    collector.stopped.store(true, Ordering::Relaxed);
    collector.done.notify_waiters();
}

#[cfg(test)]
mod tests;
