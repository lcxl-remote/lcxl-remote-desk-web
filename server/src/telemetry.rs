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

use crate::model::settings::SystemSettings;
use crate::version::{SERVER_BUILD_NUMBER, SERVER_COMMIT_HASH};

pub fn init_telemetry(settings: &SystemSettings) -> Result<Option<WorkerGuard>> {
    // 1. Create a Resource with Service Info, OS Info, and Custom Tags
    let mut sys = System::new_all();
    sys.refresh_all();

    let os_name = System::name().unwrap_or_else(|| "unknown".to_string());
    let os_version = System::os_version().unwrap_or_else(|| "unknown".to_string());
    let host_name = System::host_name().unwrap_or_else(|| "unknown".to_string());
    let cpu_count = sys.cpus().len().to_string();
    let total_memory = sys.total_memory().to_string();

    let client_id = settings.client_id.clone().unwrap_or_default();

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
    let log_level = &settings.log_level;
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
    let otel_layer = if settings.telemetry_consent == Some(true) {
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

    Ok(Some(guard))
}
