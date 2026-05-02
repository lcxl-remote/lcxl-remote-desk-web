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

use crate::model::settings::{SharedSettings, StartupMode};
use crate::version::{SERVER_BUILD_NUMBER, SERVER_COMMIT_HASH};
use std::sync::Arc;
use tracing;

pub async fn init_telemetry(
    shared_settings: Arc<SharedSettings>,
    startup_mode: &StartupMode,
) -> Result<Option<WorkerGuard>> {
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

    // ServiceDaemon and SessionWorker are launched without an
    // interactive console (SCM / `CreateProcessAsUserW` from the
    // daemon). Their stdout is bound to a console that may stop
    // responding when the user session locks (Win+L → conhost stops
    // servicing console requests for inactive desktops); a `WriteFile`
    // to stdout then blocks indefinitely, and because tracing's `fmt`
    // layer holds the global stdio Mutex across the write, every
    // subsequent `log::*!` call from any thread deadlocks in
    // `Mutex::lock_contended` inside `std::io::stdio::write_all`.
    // Skip the stdout layers in those modes — the file appender
    // (and OTel, when enabled) cover all the logging we need from a
    // headless service.
    let headless_mode = is_headless_startup_mode(startup_mode);

    let stdout_general = (!headless_mode).then(|| {
        fmt::layer()
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_target(true)
            .with_line_number(true)
            .with_filter(filter_fn(|metadata| {
                LevelFilter::from_level(*metadata.level()) != LevelFilter::ERROR
            }))
    });

    let stdout_error = (!headless_mode).then(|| {
        fmt::layer()
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_target(true)
            .with_line_number(true)
            .with_filter(LevelFilter::ERROR)
    });

    let log_dir = log_directory();
    let _ = std::fs::create_dir_all(&log_dir);

    // Determine log file name based on startup mode
    let log_file_name = log_file_name_for(startup_mode);

    // File appender
    let file_appender = tracing_appender::rolling::daily(log_dir, log_file_name);
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
    let registry = {
        if log_settings.tokio_console_enabled {
            let port: u16 = match startup_mode {
                StartupMode::ServiceDaemon => 6670,
                StartupMode::SessionWorker => 6671,
                _ => 6669,
            };
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let (console_layer, server) = console_subscriber::ConsoleLayer::builder()
                .server_addr(addr)
                .build();
            std::thread::Builder::new()
                .name("console_subscriber".to_string())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_io()
                        .enable_time()
                        .build()
                        .expect("console subscriber runtime");
                    if let Err(e) = rt.block_on(server.serve()) {
                        log::warn!("tokio-console server failed on {}: {}", addr, e);
                    }
                })
                .expect("spawn console subscriber thread");
            registry.with(Some(console_layer))
        } else {
            registry.with(None::<console_subscriber::ConsoleLayer>)
        }
    };

    registry.init();

    // 5. Spawn log cleanup task
    spawn_log_cleanup_task(shared_settings.clone());

    Ok(Some(guard))
}

/// Returns the canonical log directory used by every component (daemon,
/// embedded server, session worker, Tauri shell). Centralised so the
/// install / uninstall flow can clean every component's logs in one place
/// and so callers can't drift apart on the path layout.
pub fn log_directory() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        let program_data =
            std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
        std::path::PathBuf::from(program_data)
            .join("LCXL Remote Desktop")
            .join("logs")
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::path::PathBuf::from("/var/log/lcxl-remote-desk")
    }
}

/// Standard log file name for a given startup mode. Kept beside `log_directory`
/// so the rotation appender uses identical naming everywhere.
pub fn log_file_name_for(startup_mode: &StartupMode) -> &'static str {
    match startup_mode {
        StartupMode::ServiceDaemon => "desk-daemon.log",
        StartupMode::SessionWorker => "desk-worker.log",
        _ => "desk-server.log",
    }
}

/// Returns true for startup modes that run without an interactive
/// console — `ServiceDaemon` (SCM-spawned) and `SessionWorker`
/// (daemon-spawned via `CreateProcessAsUserW`). In those modes the
/// stdout fmt layer is unsafe to attach: see the comment in
/// [`init_telemetry`] for the lock-screen deadlock it causes.
pub fn is_headless_startup_mode(startup_mode: &StartupMode) -> bool {
    matches!(
        startup_mode,
        StartupMode::ServiceDaemon | StartupMode::SessionWorker
    )
}

/// Lightweight tracing init for the Tauri service-shell.
///
/// The full [`init_telemetry`] pulls in OTLP exporters, stdout layers, and the
/// periodic cleanup task — none of which fit a UI shell that has no console
/// (Windows `windows_subsystem = "windows"`) and no need to export traces.
/// This routine sets up just the daily-rolling file appender plus a tracing
/// `Registry`, writing to `desk-tauri.log` under [`log_directory`] so the
/// daemon's existing cleanup task scrubs them alongside `desk-daemon.log`.
///
/// The returned [`WorkerGuard`] must be kept alive for the lifetime of the
/// Tauri process; dropping it flushes pending log lines to disk and shuts
/// down the non-blocking writer thread.
///
/// Returns `Err` if the global subscriber has already been installed (e.g. a
/// caller mistakenly invoked this from portable mode where the embedded
/// server's `init_telemetry` runs first). Service-shell mode never launches
/// the embedded server in-process, so the conflict is structurally
/// impossible there — but the error is propagated rather than swallowed so
/// integration regressions surface immediately.
pub fn init_tauri_shell_telemetry(log_level: &str) -> Result<WorkerGuard> {
    let log_dir = log_directory();
    std::fs::create_dir_all(&log_dir)?;

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    let file_appender = tracing_appender::rolling::daily(log_dir, "desk-tauri.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_line_number(true)
        .with_writer(non_blocking);

    Registry::default()
        .with(env_filter)
        .with(file_layer)
        .try_init()
        .map_err(|e| anyhow::anyhow!("install tracing subscriber: {e}"))?;

    Ok(guard)
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
        if let Some(date_str) = file_name.strip_prefix("desk-server.log.")
            && let Ok(date) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        {
            if date < expiration_date {
                tracing::info!("Deleting expired log file: {}", file_name);
                let _ = std::fs::remove_file(entry.path());
            } else if date < now {
                // Collect non-expired (but not current) files for potential disk space cleanup
                log_files.push((date, entry.path()));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every component (daemon, embedded server, worker, Tauri shell) must
    /// agree on the log root so the daemon's cleanup task can prune them all.
    #[test]
    fn log_directory_resolves_to_program_data_subtree_on_windows() {
        let dir = log_directory();
        #[cfg(target_os = "windows")]
        {
            assert!(
                dir.ends_with(std::path::Path::new("LCXL Remote Desktop").join("logs")),
                "unexpected log dir: {dir:?}"
            );
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert_eq!(dir, std::path::PathBuf::from("/var/log/lcxl-remote-desk"));
        }
    }

    /// Mode-to-file-name mapping is part of the install-time contract: the
    /// uninstall flow looks for these exact names when wiping logs.
    #[test]
    fn log_file_name_for_each_startup_mode() {
        assert_eq!(
            log_file_name_for(&StartupMode::ServiceDaemon),
            "desk-daemon.log"
        );
        assert_eq!(
            log_file_name_for(&StartupMode::SessionWorker),
            "desk-worker.log"
        );
        assert_eq!(log_file_name_for(&StartupMode::Default), "desk-server.log");
        assert_eq!(
            log_file_name_for(&StartupMode::DeskServer),
            "desk-server.log"
        );
        assert_eq!(
            log_file_name_for(&StartupMode::Signaling),
            "desk-server.log"
        );
    }

    /// `is_headless_startup_mode` controls whether `init_telemetry`
    /// attaches the stdout fmt layer. Misclassifying a mode as
    /// headless silently loses interactive logging; misclassifying a
    /// service mode as non-headless re-introduces the lock-screen
    /// deadlock we hit (worker thread blocked in
    /// `std::io::stdio::write_all` → global stdio Mutex held forever
    /// → every subsequent `log::*!` call deadlocks). Lock the
    /// classification down with an explicit per-mode test.
    #[test]
    fn headless_modes_are_only_service_daemon_and_session_worker() {
        assert!(is_headless_startup_mode(&StartupMode::ServiceDaemon));
        assert!(is_headless_startup_mode(&StartupMode::SessionWorker));
        assert!(!is_headless_startup_mode(&StartupMode::Default));
        assert!(!is_headless_startup_mode(&StartupMode::DeskServer));
        assert!(!is_headless_startup_mode(&StartupMode::Signaling));
    }
}
