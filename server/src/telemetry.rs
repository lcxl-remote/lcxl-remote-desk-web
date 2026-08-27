use anyhow::Result;
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::SdkTracerProvider,
};
use std::{path::PathBuf, time::Duration};
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

/// Guards for every non-blocking writer installed by [`init_telemetry`]. The
/// caller MUST keep this alive for the process lifetime: dropping it flushes the
/// buffered lines and stops the writer threads, after which every subsequent log
/// line is silently discarded.
pub struct TelemetryGuards {
    _guards: Vec<WorkerGuard>,
}

/// Wrap a sink so writing to it never blocks the thread that logged.
///
/// **Logging must never be able to stall the service.** Each sink — stdout and
/// the log file — is handed to a dedicated writer thread behind a bounded queue,
/// and the queue is explicitly **lossy**: when the sink stops draining (a hung or
/// full disk, a container log driver that stopped reading, a console that stopped
/// servicing writes for an inactive desktop), records are dropped rather than
/// backpressured onto the caller. Capture, input injection and signaling must
/// keep running while the disk is gone; the log lines are the acceptable loss.
///
/// `lossy(true)` is also the crate default, but it is stated explicitly because
/// the opposite setting would silently convert this into the blocking behaviour
/// this function exists to prevent.
fn non_blocking_writer<W: std::io::Write + Send + 'static>(
    sink: W,
) -> (tracing_appender::non_blocking::NonBlocking, WorkerGuard) {
    tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .finish(sink)
}

pub async fn init_telemetry(
    shared_settings: Arc<SharedSettings>,
    startup_mode: &StartupMode,
) -> Result<Option<TelemetryGuards>> {
    init_telemetry_inner(shared_settings, startup_mode, None).await
}

/// Initialize telemetry for a settings snapshot that deliberately has no
/// writable [`crate::model::settings::SettingsStore`], as is the case for a
/// SessionWorker receiving serialized settings from its daemon.
pub async fn init_telemetry_with_log_dir(
    shared_settings: Arc<SharedSettings>,
    startup_mode: &StartupMode,
    log_dir: PathBuf,
) -> Result<Option<TelemetryGuards>> {
    init_telemetry_inner(shared_settings, startup_mode, Some(log_dir)).await
}

async fn init_telemetry_inner(
    shared_settings: Arc<SharedSettings>,
    startup_mode: &StartupMode,
    log_dir_override: Option<PathBuf>,
) -> Result<Option<TelemetryGuards>> {
    let (mut system_settings, log_settings, log_dir) = {
        let settings_guard = shared_settings.read().await;
        let log_dir =
            log_dir_override.unwrap_or_else(|| settings_guard.paths().log_dir().to_path_buf());
        (
            settings_guard.system.clone(),
            settings_guard.log.clone(),
            log_dir,
        )
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
    // EnvFilter is attached per-layer (not at the Registry root) so the
    // tokio-console `ConsoleLayer` registered further down can bypass it.
    // tokio's spawn / runtime instrumentation events fire at TRACE level on
    // targets like `tokio` and `runtime`; a global EnvFilter at INFO (the
    // default `log_level`) silently drops them before the ConsoleLayer sees
    // them, leaving the console UI with an empty task list. EnvFilter 0.3
    // does not implement Clone, so we rebuild it per layer.
    let make_env_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

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

    // Even in the modes that keep stdout, it is written through a non-blocking
    // writer. Written directly, `fmt` holds the process-wide stdout lock across
    // the `write` syscall, so a consumer that stops reading blocks every thread
    // that logs — the same deadlock shape the headless modes avoid by dropping
    // these layers entirely, but reachable in an interactive run too (a paused
    // `docker logs`, a full pipe, a hung disk behind a redirect).
    let mut guards: Vec<WorkerGuard> = Vec::new();
    let stdout_writer = (!headless_mode).then(|| {
        let (writer, guard) = non_blocking_writer(std::io::stdout());
        guards.push(guard);
        writer
    });

    let stdout_general = stdout_writer.clone().map(|writer| {
        fmt::layer()
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_target(true)
            .with_line_number(true)
            .with_writer(writer)
            .with_filter(filter_fn(|metadata| {
                LevelFilter::from_level(*metadata.level()) != LevelFilter::ERROR
            }))
            .with_filter(make_env_filter())
    });

    let stdout_error = stdout_writer.map(|writer| {
        fmt::layer()
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_target(true)
            .with_line_number(true)
            .with_writer(writer)
            .with_filter(LevelFilter::ERROR)
            .with_filter(make_env_filter())
    });

    let _ = std::fs::create_dir_all(&log_dir);

    // Determine log file name based on startup mode
    let log_file_name = log_file_name_for(startup_mode);

    // File appender
    let file_appender = tracing_appender::rolling::daily(&log_dir, log_file_name);
    let (non_blocking, guard) = non_blocking_writer(file_appender);
    guards.push(guard);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_line_number(true)
        .with_writer(non_blocking)
        .with_filter(make_env_filter());

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

        Some(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(make_env_filter()),
        )
    } else {
        None
    };

    // 4. Combine layers. EnvFilter is intentionally NOT attached at the
    // Registry root — see `make_env_filter` above. The ConsoleLayer below
    // is registered without any filter so tokio's TRACE-level spawn /
    // runtime instrumentation events reach it.
    let registry = Registry::default()
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
                StartupMode::McpStdio => 6672,
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

    // 5. Spawn log cleanup task, but only in a process that owns the log
    //    directory (see `owns_log_cleanup`).
    if owns_log_cleanup(startup_mode) {
        spawn_log_cleanup_task(shared_settings.clone(), log_dir.clone());
    }

    Ok(Some(TelemetryGuards { _guards: guards }))
}

/// Whether a process in this startup mode may prune the shared log directory.
///
/// The sweep deletes every component's rolls, so exactly the long-lived processes
/// that own the directory may run it:
///
/// - `SessionWorker` must not. Each desktop session spawns one, several can be
///   alive at once, and its settings are a *startup snapshot* that never receives
///   the daemon's updates — so a worker that outlived a settings change would keep
///   enforcing a stale retention against the daemon's own logs, and an operator
///   who disabled cleanup would not actually have disabled it.
/// - `McpStdio` must not either: it is a short-lived stdio helper started per
///   client, with the same stale-snapshot problem and no reason to own retention.
pub fn owns_log_cleanup(startup_mode: &StartupMode) -> bool {
    !matches!(
        startup_mode,
        StartupMode::SessionWorker | StartupMode::McpStdio
    )
}

/// Log file name written by the Tauri shell, which has no startup mode of its
/// own but shares the log directory with every other component.
const TAURI_SHELL_LOG_FILE_NAME: &str = "desk-tauri.log";

/// Every rolling log base name this project writes into the resolved host log directory: one
/// per startup mode plus the Tauri shell's.
///
/// The cleanup sweep deletes only rolls of these names, so an unrelated file
/// sharing the directory — a co-located manager's `manager.log`, for instance —
/// is never removed. [`log_file_name_for`] must only return names listed here;
/// a test pins that so the two cannot drift.
const MANAGED_LOG_BASE_NAMES: &[&str] = &[
    "desk-server.log",
    "desk-daemon.log",
    "desk-worker.log",
    "desk-mcp.log",
    TAURI_SHELL_LOG_FILE_NAME,
];

/// Standard log file name for a given startup mode. Kept beside the managed-name list
/// so the rotation appender uses identical naming everywhere.
pub fn log_file_name_for(startup_mode: &StartupMode) -> &'static str {
    match startup_mode {
        StartupMode::ServiceDaemon => "desk-daemon.log",
        StartupMode::SessionWorker => "desk-worker.log",
        StartupMode::McpStdio => "desk-mcp.log",
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
        StartupMode::ServiceDaemon | StartupMode::SessionWorker | StartupMode::McpStdio
    )
}

/// Lightweight tracing init for the Tauri service-shell.
///
/// The full [`init_telemetry`] pulls in OTLP exporters, stdout layers, and the
/// periodic cleanup task — none of which fit a UI shell that has no console
/// (Windows `windows_subsystem = "windows"`) and no need to export traces.
/// This routine sets up just the daily-rolling file appender plus a tracing
/// `Registry`, writing to `desk-tauri.log` under the resolved host log directory so the
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
pub fn init_tauri_shell_telemetry(
    log_level: &str,
    log_dir: &std::path::Path,
) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    let file_appender = tracing_appender::rolling::daily(log_dir, TAURI_SHELL_LOG_FILE_NAME);
    let (non_blocking, guard) = non_blocking_writer(file_appender);
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

/// How long to wait before re-reading the settings while cleanup is disabled.
/// The settings are live, so the task keeps polling instead of exiting: turning
/// cleanup back on takes effect without a restart.
const DISABLED_CLEANUP_RECHECK: Duration = Duration::from_secs(3600);

/// Date suffix the daily rolling appender appends (`desk-server.log.2026-07-28`).
/// The appender rolls on the **UTC** date, so the sweep compares against UTC too.
const LOG_DATE_FORMAT: &str = "%Y-%m-%d";

fn spawn_log_cleanup_task(shared_settings: Arc<SharedSettings>, log_dir: std::path::PathBuf) {
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

            // A zero in either knob means "cleanup off". The interval must be
            // guarded regardless: sleeping zero seconds would spin the loop.
            if interval_hours == 0 || retention_days == 0 {
                tracing::debug!(
                    "Log cleanup is disabled (interval {}h, retention {}d); rolled log files are kept",
                    interval_hours,
                    retention_days
                );
                tokio::time::sleep(DISABLED_CLEANUP_RECHECK).await;
                continue;
            }

            tracing::info!(
                "Starting log cleanup task. Interval: {}h, Retention: {}d, Threshold: {}%",
                interval_hours,
                retention_days,
                threshold_percent
            );

            // Directory scans and unlinks are blocking syscalls, so they stay off
            // the runtime's worker threads.
            let sweep_log_dir = log_dir.clone();
            let swept = tokio::task::spawn_blocking(move || {
                cleanup_logs(
                    &sweep_log_dir,
                    chrono::Utc::now().date_naive(),
                    retention_days,
                    threshold_percent,
                )
            })
            .await;
            match swept {
                Ok(Ok(0)) => {}
                Ok(Ok(n)) => tracing::info!("Log cleanup removed {} expired log file(s)", n),
                Ok(Err(e)) => tracing::error!("Log cleanup error: {}", e),
                Err(e) => tracing::error!("Log cleanup task failed: {}", e),
            }

            tokio::time::sleep(Duration::from_secs(interval_hours as u64 * 3600)).await;
        }
    });
}

/// The roll date of `file_name` if it is one of this project's rolled log files
/// (`<base>.<date>` for a base in [`MANAGED_LOG_BASE_NAMES`]).
///
/// The file currently being written has no date suffix and so yields `None`,
/// which is what keeps the live log out of the sweep.
fn rolled_log_date(file_name: &str) -> Option<chrono::NaiveDate> {
    MANAGED_LOG_BASE_NAMES.iter().find_map(|base| {
        file_name
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|date| chrono::NaiveDate::parse_from_str(date, LOG_DATE_FORMAT).ok())
    })
}

/// Whether a roll may be deleted to reclaim disk space.
///
/// Only files at least a full day older than `today` qualify. A rolling appender
/// decides whether to roll from the time it read *before* it writes, so around
/// UTC midnight it can still hold yesterday's file open while the sweep already
/// computes the new date. Deleting it then would send the next records to an
/// unlinked inode (or a delete-pending handle on Windows) and lose them. Files
/// past the retention window are deleted by age regardless — a non-zero retention
/// keeps that cutoff at least a day back, so the same file is never at risk there.
fn eligible_under_disk_pressure(date: chrono::NaiveDate, today: chrono::NaiveDate) -> bool {
    match today.checked_sub_days(chrono::Days::new(1)) {
        Some(yesterday) => date < yesterday,
        None => false,
    }
}

/// Delete the rolled log files that have aged out of the retention window ending
/// at `today`, then — when a disk threshold is configured — keep deleting the
/// oldest surviving rolls until usage drops back under it. Returns how many files
/// were removed.
///
/// Blocking filesystem work: call it from a blocking context.
fn cleanup_logs(
    dir: &std::path::Path,
    today: chrono::NaiveDate,
    retention_days: u32,
    threshold_percent: u8,
) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let Some(expiration_date) = today.checked_sub_days(chrono::Days::new(retention_days as u64))
    else {
        return Ok(0);
    };

    let mut deleted = 0;
    let mut survivors: Vec<(chrono::NaiveDate, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // The appenders only ever create plain files; never unlink a directory
        // or follow a symlink out of the log directory.
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(date) = rolled_log_date(&file_name) else {
            continue;
        };
        if date < expiration_date {
            tracing::info!("Deleting expired log file: {}", file_name);
            match std::fs::remove_file(entry.path()) {
                Ok(()) => deleted += 1,
                // A file that cannot be unlinked (permissions, a sharing
                // violation on Windows) will be retried next sweep, but silence
                // would let the directory grow while cleanup looks healthy.
                Err(e) => tracing::warn!("Failed to remove log file {}: {}", file_name, e),
            }
        } else if eligible_under_disk_pressure(date, today) {
            // Not expired, and provably not the file any appender still holds
            // open: a candidate should the disk threshold below be exceeded.
            survivors.push((date, entry.path()));
        }
    }

    // Sort by date (oldest first)
    survivors.sort_by_key(|f| f.0);

    // Check disk usage if threshold is set
    if threshold_percent > 0 {
        let mut disks = sysinfo::Disks::new_with_refreshed_list();

        let abs_path = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
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

                for (_, file_path) in survivors {
                    tracing::info!("Deleting log file due to disk space: {:?}", file_path);
                    match std::fs::remove_file(&file_path) {
                        Ok(()) => deleted += 1,
                        Err(e) => {
                            tracing::warn!("Failed to remove log file {:?}: {}", file_path, e)
                        }
                    }

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

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, LOG_DATE_FORMAT).expect("test date")
    }

    /// The sweep can only prune what it recognises, so every name the appenders
    /// use must be listed. A new startup mode that forgets this would silently
    /// leak its logs forever.
    #[test]
    fn every_startup_mode_log_name_is_managed_by_the_cleanup_sweep() {
        for mode in [
            StartupMode::Default,
            StartupMode::Signaling,
            StartupMode::DeskServer,
            StartupMode::ServiceDaemon,
            StartupMode::SessionWorker,
            StartupMode::McpStdio,
        ] {
            let name = log_file_name_for(&mode);
            assert!(
                MANAGED_LOG_BASE_NAMES.contains(&name),
                "{name} is written but never cleaned up"
            );
        }
        assert!(MANAGED_LOG_BASE_NAMES.contains(&TAURI_SHELL_LOG_FILE_NAME));
    }

    /// A rolled file is `<managed base>.<date>`. The live file has no date
    /// suffix, and a file belonging to something else sharing the directory is
    /// not ours to delete.
    #[test]
    fn rolled_log_date_matches_only_this_project_s_rolls() {
        assert_eq!(
            rolled_log_date("desk-server.log.2026-07-28"),
            Some(day("2026-07-28"))
        );
        assert_eq!(
            rolled_log_date("desk-daemon.log.2026-07-28"),
            Some(day("2026-07-28"))
        );
        assert_eq!(
            rolled_log_date("desk-tauri.log.2026-07-28"),
            Some(day("2026-07-28"))
        );
        // The file being written right now, and a co-located manager's logs.
        assert_eq!(rolled_log_date("desk-server.log"), None);
        assert_eq!(rolled_log_date("manager.log.2026-07-28"), None);
        assert_eq!(rolled_log_date("access.log.2026-07-28"), None);
        // Neither a missing separator nor a non-date suffix is a roll.
        assert_eq!(rolled_log_date("desk-server.log2026-07-28"), None);
        assert_eq!(rolled_log_date("desk-server.log.backup"), None);
        assert_eq!(rolled_log_date("desk-server.log.2026-07-28.gz"), None);
    }

    /// The sweep runs against the resolved host log directory and covers every component's
    /// rolls, keeps the retention window and the live files, and leaves foreign
    /// files alone. A zero threshold disables the disk-usage stage, so this
    /// asserts age-based deletion in isolation.
    #[test]
    fn cleanup_removes_only_expired_rolls_of_managed_logs() {
        let dir = tempfile::tempdir().expect("temp dir");
        let files = [
            "desk-server.log",
            "desk-server.log.2026-07-20",
            "desk-server.log.2026-07-27",
            "desk-daemon.log.2026-07-20",
            "desk-worker.log.2026-07-20",
            "desk-mcp.log.2026-07-21",
            "desk-tauri.log.2026-07-20",
            "manager.log.2026-07-20",
            "notes.txt",
        ];
        for name in files {
            std::fs::write(dir.path().join(name), b"x").expect("write file");
        }

        let deleted = cleanup_logs(dir.path(), day("2026-07-28"), 7, 0).expect("cleanup");
        assert_eq!(deleted, 4);

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                // 2026-07-21 is the oldest day inside a 7-day window.
                "desk-mcp.log.2026-07-21",
                "desk-server.log",
                "desk-server.log.2026-07-27",
                "manager.log.2026-07-20",
                "notes.txt",
            ]
        );
    }

    /// A sink that never returns from a write, standing in for a hung disk or a
    /// console that stopped servicing writes for an inactive desktop.
    struct HungSink;

    impl std::io::Write for HungSink {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_secs(3600));
            unreachable!("the test finishes long before this returns")
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Logging must never stall the caller: capture, injection and signaling keep
    /// running even when the log sink is wedged. Records queue and are then
    /// dropped; the thread that logged never waits on the sink.
    ///
    /// The guard is deliberately leaked: dropping it joins the writer thread,
    /// which is parked inside the hung write and would never return.
    #[test]
    fn a_hung_sink_never_blocks_the_thread_that_logs() {
        let (writer, guard) = non_blocking_writer(HungSink);
        std::mem::forget(guard);
        let subscriber = Registry::default().with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(EnvFilter::new("info")),
        );

        let started = std::time::Instant::now();
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..10_000 {
                tracing::info!("record {i} while the disk is gone");
            }
        });
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "logging to a hung sink took {elapsed:?}; the caller was blocked"
        );
    }

    /// The sweep deletes every component's rolls, so only a process that owns the
    /// log directory may run it. A SessionWorker holds a startup snapshot of the
    /// settings that never sees the daemon's updates, so letting it sweep would
    /// make "cleanup disabled" untrue and let a stale worker delete the daemon's
    /// logs.
    #[test]
    fn only_directory_owning_modes_run_the_cleanup_sweep() {
        for mode in [
            StartupMode::Default,
            StartupMode::Signaling,
            StartupMode::DeskServer,
            StartupMode::ServiceDaemon,
        ] {
            assert!(owns_log_cleanup(&mode), "{mode:?} should own cleanup");
        }
        for mode in [StartupMode::SessionWorker, StartupMode::McpStdio] {
            assert!(
                !owns_log_cleanup(&mode),
                "{mode:?} must never sweep the shared log directory"
            );
        }
    }

    /// Disk-pressure deletion must not touch the file an appender may still hold
    /// open across the UTC rollover — only rolls at least a full day old.
    #[test]
    fn disk_pressure_spares_todays_and_yesterdays_rolls() {
        let today = day("2026-07-28");
        assert!(!eligible_under_disk_pressure(day("2026-07-28"), today));
        assert!(!eligible_under_disk_pressure(day("2026-07-27"), today));
        assert!(eligible_under_disk_pressure(day("2026-07-26"), today));
    }

    /// A missing log directory is not an error: the appenders create it lazily,
    /// so an early sweep simply has nothing to do.
    #[test]
    fn cleanup_tolerates_a_missing_log_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("not-created-yet");
        assert_eq!(
            cleanup_logs(&missing, day("2026-07-28"), 7, 0).expect("cleanup"),
            0
        );
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
        assert_eq!(log_file_name_for(&StartupMode::McpStdio), "desk-mcp.log");
    }

    /// Regression guard for the tokio-console "empty task list" bug.
    ///
    /// tokio's spawn / runtime instrumentation (enabled by `--cfg
    /// tokio_unstable` + the `tracing` feature) emits TRACE-level events on
    /// targets like `tokio` and `runtime`. The `console_subscriber::ConsoleLayer`
    /// reads those events to populate its task list. If an `EnvFilter` built
    /// from the default INFO `log_level` is attached at the Registry root, it
    /// drops those TRACE events for *every* layer below it — including the
    /// ConsoleLayer — so tokio-console connects successfully but shows no
    /// tasks.
    ///
    /// `init_telemetry` therefore attaches `EnvFilter` per-layer (stdout,
    /// file, otel) and leaves the ConsoleLayer unfiltered. This test pins the
    /// underlying tracing-subscriber behavior that makes that workaround
    /// necessary: a per-layer EnvFilter at INFO blocks `tokio` TRACE events
    /// from reaching its layer, while a sibling layer with no filter sees
    /// them. If this ever stops being true, `init_telemetry` can be
    /// simplified back to a global filter; until then, removing the
    /// per-layer composition would silently re-break tokio-console.
    #[test]
    fn per_layer_env_filter_lets_unfiltered_sibling_see_tokio_trace_events() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::layer::{Context, Layer};

        #[derive(Clone)]
        struct CountingLayer {
            seen: Arc<Mutex<Vec<(String, tracing::Level)>>>,
        }

        impl<S> Layer<S> for CountingLayer
        where
            S: tracing::Subscriber,
        {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                let meta = event.metadata();
                self.seen
                    .lock()
                    .unwrap()
                    .push((meta.target().to_string(), *meta.level()));
            }
        }

        let filtered_seen: Arc<Mutex<Vec<(String, tracing::Level)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let unfiltered_seen: Arc<Mutex<Vec<(String, tracing::Level)>>> =
            Arc::new(Mutex::new(Vec::new()));

        let filtered = CountingLayer {
            seen: filtered_seen.clone(),
        }
        .with_filter(EnvFilter::new("info"));
        let unfiltered = CountingLayer {
            seen: unfiltered_seen.clone(),
        };

        let subscriber = Registry::default().with(filtered).with(unfiltered);

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "tokio", "spawn event");
            tracing::trace!(target: "runtime", "runtime event");
            tracing::info!(target: "tokio", "info event");
        });

        let filtered_events = filtered_seen.lock().unwrap();
        let unfiltered_events = unfiltered_seen.lock().unwrap();

        // The filtered layer must NOT see TRACE-level events from tokio /
        // runtime targets when EnvFilter is at INFO. INFO-level events still
        // pass.
        assert!(
            !filtered_events
                .iter()
                .any(|(target, level)| (target == "tokio" || target == "runtime")
                    && *level == tracing::Level::TRACE),
            "EnvFilter at 'info' must drop TRACE-level tokio/runtime events; \
             attaching it at the Registry root would silently empty tokio-console's task list. \
             saw: {filtered_events:?}"
        );
        assert!(
            filtered_events
                .iter()
                .any(|(target, level)| target == "tokio" && *level == tracing::Level::INFO),
            "EnvFilter at 'info' should still pass INFO-level events. saw: {filtered_events:?}"
        );

        // The unfiltered layer (ConsoleLayer's role) must receive every
        // event regardless of level.
        assert!(
            unfiltered_events
                .iter()
                .any(|(target, level)| target == "tokio" && *level == tracing::Level::TRACE),
            "Unfiltered sibling layer must receive tokio TRACE events. saw: {unfiltered_events:?}"
        );
        assert!(
            unfiltered_events
                .iter()
                .any(|(target, level)| target == "runtime" && *level == tracing::Level::TRACE),
            "Unfiltered sibling layer must receive runtime TRACE events. saw: {unfiltered_events:?}"
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
    fn headless_modes_exclude_interactive_console_modes() {
        assert!(is_headless_startup_mode(&StartupMode::ServiceDaemon));
        assert!(is_headless_startup_mode(&StartupMode::SessionWorker));
        // stdio carries the MCP protocol, so it must be headless (no stdout log).
        assert!(is_headless_startup_mode(&StartupMode::McpStdio));
        assert!(!is_headless_startup_mode(&StartupMode::Default));
        assert!(!is_headless_startup_mode(&StartupMode::DeskServer));
        assert!(!is_headless_startup_mode(&StartupMode::Signaling));
    }
}
