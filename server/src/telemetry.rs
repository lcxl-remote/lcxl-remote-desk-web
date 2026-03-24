use anyhow::Result;
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};
use std::time::Duration;
use sysinfo::System;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Layer, Registry,
    filter::{LevelFilter, filter_fn},
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::model::settings::SharedSettings;
use crate::version::{SERVER_BUILD_NUMBER, SERVER_COMMIT_HASH};
use std::sync::Arc;
use tracing;

pub async fn init_telemetry(shared_settings: Arc<SharedSettings>) -> Result<Option<WorkerGuard>> {
    let (mut system_settings, log_settings) = {
        let settings_guard = shared_settings.read().await;
        (settings_guard.system.clone(), settings_guard.log.clone())
    };

    // 1. Create a Resource with Service Info, OS Info, and Custom Tags
    let mut sys = System::new_all();
    sys.refresh_all();

    let os_name = System::name().unwrap_or_else(|| "unknown".to_string());
    let os_version = System::os_version().unwrap_or_else(|| "unknown".to_string());
    let host_name = System::host_name().unwrap_or_else(|| "unknown".to_string());
    let cpu_count = sys.cpus().len().to_string();
    let total_memory = sys.total_memory().to_string();

    let client_id = system_settings.get_or_generate_client_id();

    let resource = Resource::builder()
        .with_attributes(vec![
            KeyValue::new("service.name", "lcxl-remote-desk-server"),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("service.build_number", SERVER_BUILD_NUMBER as i64),
            KeyValue::new("service.commit_hash", SERVER_COMMIT_HASH),
            KeyValue::new("client.id", client_id),
            KeyValue::new("os.name", os_name),
            KeyValue::new("os.version", os_version),
            KeyValue::new("host.name", host_name),
            KeyValue::new("host.cpu_count", cpu_count),
            KeyValue::new("host.total_memory", total_memory),
        ])
        .build();

    // 2. Setup Logging (Stdout + File)
    let log_level = &log_settings.log_level;
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    let stdout_general = fmt::layer()
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_target(true)
        .with_line_number(true)
        .with_filter(filter_fn(|metadata| {
            LevelFilter::from_level(*metadata.level()) != LevelFilter::ERROR
        }));

    let stdout_error = fmt::layer()
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_target(true)
        .with_line_number(true)
        .with_filter(LevelFilter::ERROR);

    // File appender
    let file_appender = tracing_appender::rolling::daily("logs", "desk-server.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_line_number(true)
        .with_writer(non_blocking);

    // 3. Setup OpenTelemetry (Optional based on consent)
    let otel_layer = if system_settings.telemetry_consent == Some(true) {
        // Set global propagator
        global::set_text_map_propagator(TraceContextPropagator::new());

        // Tracer Provider
        let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()?;

        let tracer_provider = SdkTracerProvider::builder()
            .with_batch_exporter(trace_exporter)
            .with_resource(resource.clone())
            .build();

        let tracer = opentelemetry::trace::TracerProvider::tracer(
            &tracer_provider,
            "lcxl-remote-desk-server",
        );

        global::set_tracer_provider(tracer_provider);

        // Metrics Provider
        let metrics_exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .build()?;

        // Assuming PeriodicReader::builder takes only exporter in this version based on error message.
        // And runtime might be optional or omitted?
        let reader = PeriodicReader::builder(metrics_exporter)
            .with_interval(Duration::from_secs(60))
            .build();

        let meter_provider = SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(resource)
            .build();

        global::set_meter_provider(meter_provider);

        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    } else {
        None
    };

    // 4. Combine layers
    let registry = Registry::default()
        .with(env_filter)
        .with(stdout_general)
        .with(stdout_error)
        .with(file_layer)
        .with(otel_layer);

    #[cfg(tokio_unstable)]
    let registry = registry.with(console_subscriber::spawn());

    registry.init();

    // 5. Spawn log cleanup task
    spawn_log_cleanup_task(shared_settings.clone());

    Ok(Some(guard))
}

fn spawn_log_cleanup_task(shared_settings: Arc<SharedSettings>) {
    tokio::spawn(async move {
        loop {
            let (interval_hours, retention_days, threshold_percent) = {
                let settings = shared_settings.read().await;
                (
                    settings.log.log_cleanup_interval_hours,
                    settings.log.log_retention_days,
                    settings.log.log_cleanup_threshold_percent,
                )
            };

            tracing::info!(
                "Starting log cleanup task. Interval: {}h, Retention: {}d, Threshold: {}%",
                interval_hours,
                retention_days,
                threshold_percent
            );

            if let Err(e) = perform_log_cleanup(retention_days, threshold_percent).await {
                tracing::error!("Log cleanup error: {}", e);
            }

            tokio::time::sleep(Duration::from_secs(interval_hours as u64 * 3600)).await;
        }
    });
}

async fn perform_log_cleanup(retention_days: u32, threshold_percent: u8) -> Result<()> {
    let log_dir = "logs";
    let path = std::path::Path::new(log_dir);
    if !path.exists() {
        return Ok(());
    }

    let now = chrono::Local::now().naive_local().date();
    let expiration_date = now - chrono::Duration::days(retention_days as i64);

    let mut log_files = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        // Expected format: desk-server.log.YYYY-MM-DD
        if file_name.starts_with("desk-server.log.") {
            let date_str = &file_name["desk-server.log.".len()..];
            if let Ok(date) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                if date < expiration_date {
                    tracing::info!("Deleting expired log file: {}", file_name);
                    let _ = std::fs::remove_file(entry.path());
                } else if date < now {
                    // Collect non-expired (but not current) files for potential disk space cleanup
                    log_files.push((date, entry.path()));
                }
            }
        }
    }

    // Sort by date (oldest first)
    log_files.sort_by_key(|f| f.0);

    // Check disk usage if threshold is set
    if threshold_percent > 0 {
        let mut disks = sysinfo::Disks::new_with_refreshed_list();

        let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(disk) = disks.iter().find(|d| abs_path.starts_with(d.mount_point())) {
            let used_percent = ((disk.total_space() - disk.available_space()) as f64
                / disk.total_space() as f64
                * 100.0) as u8;

            if used_percent > threshold_percent {
                tracing::warn!(
                    "Disk usage {}% exceeds threshold {}%. Cleaning up more logs...",
                    used_percent,
                    threshold_percent
                );

                for (_, file_path) in log_files {
                    tracing::info!("Deleting log file due to disk space: {:?}", file_path);
                    let _ = std::fs::remove_file(&file_path);

                    // Re-check disk usage
                    disks = sysinfo::Disks::new_with_refreshed_list();
                    if let Some(d) = disks.iter().find(|d| abs_path.starts_with(d.mount_point())) {
                        let current_used = ((d.total_space() - d.available_space()) as f64
                            / d.total_space() as f64
                            * 100.0) as u8;
                        if current_used <= threshold_percent {
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
