//! Background-only SQLite initialization and process-local observation handles.

use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use desk_diagnose_core::model_observability::{Attribution, ObservationContext, ObservationEvent};
use desk_signal_facade::{
    model::model_metrics::MetricsSettings,
    service::model_metrics::collector::{
        self, AggregateProgress, Collector, MetricsBackend, WriterHealth,
    },
};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, Schema, TransactionTrait,
};

use super::{entity::*, store::Store};

static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();
static STORE: OnceLock<Store> = OnceLock::new();

pub fn initialize(config_dir: &str) {
    let (collector, receiver) = Collector::channel();
    if COLLECTOR.set(collector.clone()).is_err() {
        return;
    }
    let backend = Arc::new(LocalBackend {
        path: PathBuf::from(config_dir).join("model-metrics.sqlite"),
        creating_current_schema: AtomicBool::new(false),
    });
    tokio::spawn(collector::run(collector, receiver, backend));
}

pub async fn shutdown() {
    if let Some(collector) = COLLECTOR.get() {
        collector.shutdown().await;
    }
}

pub fn state() -> desk_signal_facade::model::model_metrics::ComponentState {
    COLLECTOR.get().map_or(
        desk_signal_facade::model::model_metrics::ComponentState::Initializing,
        |collector| collector.state(),
    )
}

pub fn store() -> Option<&'static Store> {
    STORE.get()
}

pub fn context(attribution: Attribution) -> Option<ObservationContext> {
    COLLECTOR.get()?.context(
        uuid::Uuid::new_v4().to_string(),
        chrono::Utc::now().timestamp_millis(),
        attribution,
    )
}

/// Already validated, owned facts only. Submission never awaits the writer.
pub fn submit(event: ObservationEvent) {
    use desk_diagnose_core::model_observability::ObservabilitySeam;
    if let Some(collector) = COLLECTOR.get() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collector.submit(event)));
    }
}

struct LocalBackend {
    path: PathBuf,
    creating_current_schema: AtomicBool,
}

impl LocalBackend {
    async fn connect(&self) -> Result<&'static Store, ()> {
        if let Some(store) = STORE.get() {
            return Ok(store);
        }
        let store = open_current_store(&self.path, &self.creating_current_schema).await?;
        let _ = STORE.set(store);
        STORE.get().ok_or(())
    }
}

async fn open_current_store(
    path: &std::path::Path,
    creating_current_schema: &AtomicBool,
) -> Result<Store, ()> {
    let existing = tokio::fs::try_exists(path).await.map_err(|_| ())?;
    if !existing {
        creating_current_schema.store(true, Ordering::Relaxed);
    }
    let url = format!(
        "sqlite://{}?mode=rwc",
        path.to_string_lossy().replace('\\', "/")
    );
    let mut options = ConnectOptions::new(url);
    options
        .max_connections(2)
        .min_connections(0)
        .connect_timeout(Duration::from_secs(2))
        .acquire_timeout(Duration::from_secs(2))
        .sqlx_logging(false);
    options.map_sqlx_sqlite_opts(|options| {
        options
            .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sea_orm::sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_millis(500))
            .pragma("cache_size", "-2048")
            .pragma("wal_autocheckpoint", "256")
            .pragma("journal_size_limit", "1048576")
    });
    let db = Database::connect(options).await.map_err(|_| ())?;
    // Retry only a file this process began creating. Existing components
    // are never upgraded, rebuilt or read through an older-format adapter.
    if creating_current_schema.load(Ordering::Relaxed) {
        create_schema(&db).await.map_err(|_| ())?;
    }
    let store = Store::new(db, false, "local".into());
    store
        .initialize_settings(chrono::Utc::now().timestamp_millis())
        .await
        .map_err(|_| ())?;
    creating_current_schema.store(false, Ordering::Relaxed);
    Ok(store)
}

pub async fn create_schema(db: &DatabaseConnection) -> Result<(), DbErr> {
    let txn = db.begin().await?;
    let backend = txn.get_database_backend();
    let schema = Schema::new(backend);
    macro_rules! create {
        ($entity:ident) => {
            txn.execute(
                schema
                    .create_table_from_entity($entity::Entity)
                    .if_not_exists(),
            )
            .await?;
            for mut index in schema.create_index_from_entity($entity::Entity) {
                txn.execute(index.if_not_exists()).await?;
            }
        };
    }
    create!(model_metric_event);
    create!(model_metric_compact);
    create!(model_metric_record);
    create!(model_metric_rollup);
    create!(model_metric_settings);
    create!(model_metric_health);
    create!(model_metric_lease);
    macro_rules! index {
        ($name:literal, $entity:ident, $($column:ident),+ $(,)?) => {
            txn.execute(&sea_orm::sea_query::Index::create().if_not_exists()
                .name($name).table($entity::Entity)$(.col($entity::Column::$column))+.to_owned()).await?;
        };
    }
    index!(
        "idx_model_metric_rollup_range",
        model_metric_rollup,
        SeriesKind,
        GranularityMs,
        BucketMs
    );
    index!(
        "idx_model_metric_rollup_model",
        model_metric_rollup,
        ModelId,
        SeriesKind,
        GranularityMs,
        BucketMs
    );
    index!(
        "idx_model_metric_rollup_tool",
        model_metric_rollup,
        Tool,
        SeriesKind,
        GranularityMs,
        BucketMs
    );
    index!(
        "idx_model_metric_rollup_runtime",
        model_metric_rollup,
        RuntimeCategory,
        SeriesKind,
        GranularityMs,
        BucketMs
    );
    index!(
        "idx_model_metric_event_pending",
        model_metric_event,
        Applied,
        Binding,
        Id
    );
    index!(
        "idx_model_metric_event_association",
        model_metric_event,
        AssociationPending,
        NextAssociationAtMs,
        AssociationAlias,
        Id
    );
    index!(
        "idx_model_metric_record_unassociated",
        model_metric_record,
        Kind,
        ReceivedAtMs,
        ObjectId
    );
    index!(
        "idx_model_metric_record_page",
        model_metric_record,
        Kind,
        StartedAtMs,
        ObjectId
    );
    index!(
        "idx_model_metric_record_model",
        model_metric_record,
        ModelId,
        Kind,
        StartedAtMs,
        ObjectId
    );
    index!(
        "idx_model_metric_record_retention",
        model_metric_record,
        RetentionPriority,
        StartedAtMs,
        ObjectId
    );
    index!(
        "idx_model_metric_rollup_cleanup",
        model_metric_rollup,
        BucketMs,
        Id
    );
    index!(
        "idx_model_metric_compact_children",
        model_metric_compact,
        CallId,
        Kind,
        ObjectId
    );
    index!(
        "idx_model_metric_compact_cleanup",
        model_metric_compact,
        StartedAtMs,
        ObjectId
    );
    txn.commit().await
}

#[async_trait::async_trait]
impl MetricsBackend for LocalBackend {
    async fn settings(&self) -> Result<MetricsSettings, ()> {
        self.connect().await?.load_settings().await.map_err(|_| ())
    }
    async fn persist(&self, events: &[ObservationEvent], now: i64) -> Result<u32, ()> {
        MetricsBackend::persist(self.connect().await?, events, now).await
    }
    async fn aggregate(&self, now: i64) -> Result<AggregateProgress, ()> {
        MetricsBackend::aggregate(self.connect().await?, now).await
    }
    async fn cleanup(&self, now: i64) -> Result<(), ()> {
        let store = self.connect().await?;
        MetricsBackend::cleanup(store, now).await?;
        // File allocation includes the WAL; a missing or unreadable sample stays
        // unavailable instead of being replaced by the logical quota charge.
        if let Ok(main) = tokio::fs::metadata(&self.path).await {
            let mut wal_path = self.path.as_os_str().to_owned();
            wal_path.push("-wal");
            let wal = match tokio::fs::metadata(PathBuf::from(wal_path)).await {
                Ok(value) => Some(value.len()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(0),
                Err(_) => None,
            };
            if let Some(bytes) = wal
                .and_then(|wal| main.len().checked_add(wal))
                .and_then(|value| i64::try_from(value).ok())
            {
                let _ = store.record_physical_allocation(bytes, now).await;
            }
        }
        Ok(())
    }
    async fn report(&self, health: &WriterHealth) -> Result<(), ()> {
        MetricsBackend::report(self.connect().await?, health).await
    }
}

#[cfg(test)]
mod tests;
