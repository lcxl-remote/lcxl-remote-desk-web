use super::*;
use desk_diagnose_core::model_observability::{
    CallSnapshot, ConfigurationScope, EVENT_SCHEMA_VERSION, ObservationPayload, ObservationPhase,
    Origin, OutputOutcome, Protocol, Purpose, RequestOutcome, RuntimeSnapshot, Surface,
    runtime::{RuntimeDefinition, RuntimeLabels},
};
use std::sync::Mutex;

fn event(index: usize, runtime: bool) -> ObservationEvent {
    let id = format!("collector-test-{index}");
    ObservationEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        event_id: format!("{id}.terminal"),
        object_id: id,
        call_id: None,
        phase: ObservationPhase::Terminal,
        sequence: 0,
        started_at_ms: 1_000,
        occurred_at_ms: 1_100,
        attribution: Attribution {
            provider_id: "provider".into(),
            model_id: "model".into(),
            model_name: "Test model".into(),
            configuration_revision: "1".into(),
            contract_revision: "1".into(),
            purpose: Purpose::Agent,
            surface: Surface::Assistant,
            origin: Origin::User,
            configuration_scope: ConfigurationScope::Local,
            protocol: Protocol::OpenAiChatCompletions,
        },
        relation: None,
        payload: if runtime {
            ObservationPayload::Runtime(RuntimeSnapshot {
                definition: RuntimeDefinition::ActionLogPersistFailure,
                labels: RuntimeLabels::default(),
                value: 1,
                duration_ms: None,
                quantities: Default::default(),
            })
        } else {
            ObservationPayload::Call(CallSnapshot {
                outcome: RequestOutcome::Returned,
                output: OutputOutcome::Accepted,
                ..Default::default()
            })
        },
    }
}

#[derive(Default)]
struct Backend {
    persist_mode: AtomicU64,
    settings_failed: AtomicBool,
    persist_calls: AtomicU64,
    batches: Mutex<Vec<Vec<ObservationEvent>>>,
    health: Mutex<Vec<WriterHealth>>,
}

#[async_trait]
impl MetricsBackend for Backend {
    async fn settings(&self) -> Result<MetricsSettings, ()> {
        if self.settings_failed.load(Ordering::Relaxed) {
            Err(())
        } else {
            Ok(MetricsSettings::defaults(false))
        }
    }
    async fn persist(&self, events: &[ObservationEvent], _now: i64) -> Result<u32, ()> {
        self.persist_calls.fetch_add(1, Ordering::Relaxed);
        match self.persist_mode.load(Ordering::Relaxed) {
            1 => Err(()),
            2 => std::future::pending().await,
            3 => panic!("isolated observation worker failure"),
            _ => {
                self.batches.lock().unwrap().push(events.to_vec());
                Ok(0)
            }
        }
    }
    async fn aggregate(&self, now: i64) -> Result<AggregateProgress, ()> {
        Ok(AggregateProgress {
            last_received_ms: Some(now),
            ..Default::default()
        })
    }
    async fn cleanup(&self, _now: i64) -> Result<(), ()> {
        Ok(())
    }
    async fn report(&self, health: &WriterHealth) -> Result<(), ()> {
        self.health.lock().unwrap().push(health.clone());
        Ok(())
    }
}

async fn yield_worker() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn configuration_failure_does_not_enable_capture() {
    let (collector, _receiver) = Collector::channel();
    assert!(!collector.enabled());
    collector.refresh(MetricsSettings::defaults(false));
    assert!(collector.enabled());
    let mut disabled = MetricsSettings::defaults(false);
    disabled.enabled = false;
    collector.refresh(disabled);
    assert!(!collector.enabled());
    collector.cache.write().unwrap().loaded = Some(Instant::now() - Duration::from_secs(61));
    assert_eq!(collector.state(), ComponentState::CacheExpired);
    assert!(!collector.enabled());
}

#[test]
fn full_queues_drop_only_observations_and_keep_a_separate_runtime_share() {
    let (collector, receiver) = Collector::channel();
    collector.refresh(MetricsSettings::defaults(false));
    for index in 0..QUEUE_CAPACITY {
        collector.submit(event(index, false));
    }
    for index in 0..LOW_PRIORITY_CAPACITY + 1 {
        collector.submit(event(10_000 + index, true));
    }
    assert_eq!(
        receiver.critical.len(),
        QUEUE_CAPACITY - LOW_PRIORITY_CAPACITY
    );
    assert_eq!(receiver.ordinary.len(), LOW_PRIORITY_CAPACITY);
    assert_eq!(collector.dropped(), LOW_PRIORITY_CAPACITY as u64 + 1);
    assert!(collector.enabled());
}

#[tokio::test(start_paused = true)]
async fn a_burst_drains_without_one_second_per_batch_and_runtime_facts_are_not_starved() {
    let (collector, receiver) = Collector::channel();
    let backend = Arc::new(Backend::default());
    let worker = tokio::spawn(run(collector.clone(), receiver, backend.clone()));
    yield_worker().await;
    assert!(collector.enabled());
    let started = Instant::now();
    for index in 0..600 {
        collector.submit(event(index, false));
    }
    for index in 0..120 {
        collector.submit(event(1_000 + index, true));
    }
    yield_worker().await;
    {
        let batches = backend.batches.lock().unwrap();
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 720);
        assert!(batches.iter().all(|batch| batch.len() <= 100));
        assert!(
            batches[0]
                .iter()
                .any(|event| matches!(event.payload, ObservationPayload::Runtime(_)))
        );
        let ids: std::collections::BTreeSet<_> = batches
            .iter()
            .flatten()
            .map(|event| &event.event_id)
            .collect();
        assert_eq!(ids.len(), 720);
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(collector.dropped(), 0);
    collector.shutdown().await;
    worker.await.unwrap();
    assert_eq!(collector.state(), ComponentState::Unavailable);
}

#[tokio::test(start_paused = true)]
async fn failed_batches_have_bounded_retries_and_later_calls_can_be_observed() {
    let (collector, receiver) = Collector::channel();
    let backend = Arc::new(Backend::default());
    backend.persist_mode.store(1, Ordering::Relaxed);
    let worker = tokio::spawn(run(collector.clone(), receiver, backend.clone()));
    yield_worker().await;
    for index in 0..10 {
        collector.submit(event(index, false));
    }
    yield_worker().await;
    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(1)).await;
        yield_worker().await;
    }
    assert_eq!(backend.persist_calls.load(Ordering::Relaxed), 3);
    assert!(backend.batches.lock().unwrap().is_empty());
    assert!(
        backend
            .health
            .lock()
            .unwrap()
            .iter()
            .any(|health| health.discarded == 10)
    );
    backend.persist_mode.store(0, Ordering::Relaxed);
    collector.submit(event(100, false));
    yield_worker().await;
    assert_eq!(
        backend
            .batches
            .lock()
            .unwrap()
            .iter()
            .map(Vec::len)
            .sum::<usize>(),
        1
    );
    assert_eq!(backend.persist_calls.load(Ordering::Relaxed), 4);
    collector.shutdown().await;
    worker.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_hung_writer_cannot_hold_the_caller_or_shutdown_indefinitely() {
    let (collector, receiver) = Collector::channel();
    let backend = Arc::new(Backend::default());
    backend.persist_mode.store(2, Ordering::Relaxed);
    let worker = tokio::spawn(run(collector.clone(), receiver, backend.clone()));
    yield_worker().await;
    collector.submit(event(0, false));
    yield_worker().await;
    assert_eq!(backend.persist_calls.load(Ordering::Relaxed), 1);
    let business = async {
        collector.submit(event(1, false));
        "original model result"
    };
    assert_eq!(business.await, "original model result");
    let started = Instant::now();
    collector.shutdown().await;
    assert!(started.elapsed() <= Duration::from_secs(7));
    // The caller's shutdown bound does not depend on an in-flight observation
    // transaction reaching its timeout. The detached worker still has a bound.
    tokio::time::timeout(Duration::from_secs(4), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(backend.health.lock().unwrap().last().unwrap().discarded >= 2);
}

#[tokio::test(start_paused = true)]
async fn an_observation_worker_panic_stops_capture_without_panicking_the_caller() {
    let (collector, receiver) = Collector::channel();
    let backend = Arc::new(Backend::default());
    backend.persist_mode.store(3, Ordering::Relaxed);
    let worker = tokio::spawn(run(collector.clone(), receiver, backend));
    yield_worker().await;
    collector.submit(event(0, false));
    worker.await.unwrap();
    assert_eq!(collector.state(), ComponentState::Unavailable);
    assert!(!collector.enabled());
    collector.submit(event(1, false));
    assert_eq!(collector.dropped(), 1);
    collector.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_configuration_read_does_not_reenable_an_expired_cache() {
    let (collector, receiver) = Collector::channel();
    let backend = Arc::new(Backend::default());
    backend.settings_failed.store(true, Ordering::Relaxed);
    let worker = tokio::spawn(run(collector.clone(), receiver, backend.clone()));
    yield_worker().await;
    assert_eq!(collector.state(), ComponentState::Unavailable);
    assert!(!collector.enabled());
    backend.settings_failed.store(false, Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(15)).await;
    yield_worker().await;
    assert!(collector.enabled());
    backend.settings_failed.store(true, Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(61)).await;
    yield_worker().await;
    assert_eq!(collector.state(), ComponentState::CacheExpired);
    assert!(!collector.enabled());
    collector.shutdown().await;
    worker.await.unwrap();
}
