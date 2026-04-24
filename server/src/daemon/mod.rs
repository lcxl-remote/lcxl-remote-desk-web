pub mod local_api;
pub mod session_monitor;
pub mod signaling_proxy;
pub mod windows_service;
pub mod worker_manager;

use actix_web::web;
use crate::model::settings::{Args, Settings, SharedSettings};
use log::{error, info};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Entry point for `--startup-mode service-daemon`.
///
/// On Windows this first tries to start as a real Windows Service (via SCM).
/// If that fails (not started from SCM), it falls back to an interactive run
/// so developers can test the daemon from a terminal.
pub fn run_service_daemon(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        match windows_service::try_run_as_service() {
            Ok(true) => {
                return Ok(());
            }
            Ok(false) => {
                info!("Not running under SCM — starting interactively");
            }
            Err(e) => {
                error!("Service dispatcher error: {e}");
                return Err(e);
            }
        }
    }

    let system = actix_web::rt::System::new();
    system.block_on(async { run_service_daemon_inner(args, None).await })
}

/// The actual async daemon logic, shared between the interactive path and the
/// Windows Service path (called from `windows_service::run_service()`).
///
/// `shutdown_signal`: when running as a Windows Service the SCM stop handler
/// sends on this channel so that we can reach `shutdown_all()`.  The interactive
/// path passes `None` and waits for Ctrl-C instead.
pub async fn run_service_daemon_inner(
    args: Args,
    shutdown_signal: Option<oneshot::Receiver<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("ServiceDaemon starting");

    let settings = Settings::new(&args).map_err(|e| format!("Failed to load settings: {e}"))?;
    let shared_settings = Arc::new(SharedSettings::from(settings.clone()));
    let shared_settings_data = web::Data::from(shared_settings.clone());

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    info!("Settings loaded — starting worker manager");

    let (worker_mgr, worker_rx) = worker_manager::WorkerManager::new(shared_settings_data.clone());

    let initial_session = get_current_session_id();
    let initial_desktop = get_initial_desktop_name();
    if let Err(e) = worker_mgr
        .start_worker(initial_session, initial_desktop, Vec::new())
        .await
    {
        error!("Failed to start initial worker: {e}");
    }

    let monitor_handle = {
        let worker_mgr = worker_mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = session_monitor::run_session_monitor(worker_mgr).await {
                error!("Session monitor error: {e}");
            }
        })
    };

    let proxy_handle = {
        let settings = shared_settings_data.clone();
        let worker_mgr = worker_mgr.clone();
        actix_web::rt::spawn(async move {
            if let Err(e) =
                signaling_proxy::run_signaling_proxy(settings, worker_mgr, worker_rx).await
            {
                error!("Signaling proxy error: {e}");
            }
        })
    };

    let api_handle = {
        let settings = Arc::clone(&shared_settings);
        tokio::spawn(async move {
            if let Err(e) = local_api::run_local_api(settings).await {
                error!("Local API error: {e}");
            }
        })
    };

    info!("ServiceDaemon running. Press Ctrl+C to stop.");
    match shutdown_signal {
        Some(rx) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { info!("Ctrl-C received"); }
                _ = rx => { info!("SCM shutdown signal received"); }
            }
        }
        None => {
            tokio::signal::ctrl_c().await?;
        }
    }
    info!("ServiceDaemon shutting down…");

    worker_mgr.shutdown_all().await;
    monitor_handle.abort();
    proxy_handle.abort();
    api_handle.abort();

    info!("ServiceDaemon stopped");
    Ok(())
}

#[cfg(target_os = "windows")]
fn get_current_session_id() -> u32 {
    unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() }
}

#[cfg(not(target_os = "windows"))]
fn get_current_session_id() -> u32 {
    0
}

/// Returns the actual active input desktop name at startup so the initial
/// worker is started on the correct desktop (e.g. "Winlogon" at the lock
/// screen rather than always "Default").
fn get_initial_desktop_name() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        match session_monitor::get_current_desktop_name() {
            Ok(name) => Some(name),
            Err(e) => {
                info!("Could not query initial desktop name ({e}), defaulting to 'Default'");
                Some("Default".to_string())
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}
