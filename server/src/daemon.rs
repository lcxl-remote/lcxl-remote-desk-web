pub mod bitrate_controller;
pub mod codec_negotiation;
pub mod command_blocklist;
pub mod command_templates;
pub mod exec_approval;
pub mod exec_capacity;
pub mod exec_ledger;
pub mod local_access_control;
pub mod local_access_control_transport;
pub mod local_api;
pub mod manager_credential_scope;
pub mod manager_link_gate;
pub mod manager_link_state;
pub mod pc_manager;
#[cfg(target_os = "windows")]
pub mod pipe_security;
pub mod remote_access;
pub mod session_approval;
pub mod session_monitor;
pub mod signaling_proxy;
pub mod signaling_router;
pub mod support_link_state;
pub mod tauri_ipc;
pub mod virtual_display;
pub mod windows_service;
pub mod worker_manager;

use crate::daemon::pc_manager::PcRegistry;
use crate::host_control::HostControlHub;
use crate::model::settings::{Args, Settings, SharedSettings};
use actix_web::web;
use desk_utils::host_data_paths::HostDataPaths;
use log::{error, info, warn};
use std::{sync::Arc, time::Duration};
use tauri_ipc::TauriIpcBridge;
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
    let settings = Settings::new(&args).map_err(|e| format!("Failed to load settings: {e}"))?;
    let paths = settings.paths().clone();
    let shared_settings = Arc::new(SharedSettings::from(settings.clone()));
    let shared_settings_data = web::Data::from(shared_settings.clone());
    // The host's authoritative security policy and single durable commit path
    // for it and the locale. Built before anything can change the settings.
    let settings_coordinator = Arc::new(
        crate::model::settings_coordinator::SettingsCoordinator::from_settings(
            shared_settings.clone(),
        )
        .await,
    );

    // Initialize telemetry for ServiceDaemon mode
    let _guard =
        crate::telemetry::init_telemetry(shared_settings.clone(), &args.startup_mode).await?;

    info!("ServiceDaemon starting");
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Initialize the signal database (same SQLite used by the Default/Signaling modes).
    // Required because open_signaling_handle calls get_or_create_device_code() which
    // calls get_db() and panics if the DB has not been initialized.
    let signal_db_dir = paths.signal_db_dir().to_string_lossy().to_string();
    let signal_db = desk_signal::db::init_db(&signal_db_dir)
        .await
        .map_err(|e| format!("Failed to init signal DB: {e}"))?;
    info!("Signal database initialized at {signal_db_dir}");
    let exec_ledger = open_exec_ledger(&paths).await?;
    // Age-based retention cleanup for the local usage rollups (collect-only
    // telemetry, no billing coupling). One task per process; the delete is idempotent.
    tokio::spawn(desk_signal::usage_retention::run_retention_cleanup_loop(
        signal_db.clone(),
    ));

    // Ensure local_signaling_token exists so the signaling proxy can authenticate
    // as a desk node with the local HTTP server on SERVICE_API_PORT.
    {
        let mut s = shared_settings.write().await;
        if s.system.local_signaling_token.is_none() {
            let token = uuid::Uuid::new_v4().to_string();
            info!("Generated new local_signaling_token for ServiceDaemon");
            s.system.local_signaling_token = Some(token);
            if let Err(e) = s.save() {
                error!("Failed to save local_signaling_token: {e}");
            }
        }
    }

    info!("Settings loaded — building host-control hub and HTTP API");

    // Aggregator hub: routes between worker forwarders and the Tauri shell.
    let host_control_hub = Arc::new(HostControlHub::new_aggregator());
    host_control_hub
        .host_activity()
        .set_indicator_enabled(settings.system.host_access_indicator_enabled);
    initialize_remote_access(&paths, &host_control_hub);

    // Host→manager link status, shared between the signaling proxy (records a fatal
    // device-quota rejection, parks until manual retry) and the local API (surfaces
    // it + triggers reconnect).
    let manager_link_state = Arc::new(manager_link_state::ManagerLinkState::new());

    // On-demand temporary-support lifecycle, shared between the signaling proxy
    // (drives the support upstream) and the local API (start / stop / status).
    let support_link_state = Arc::new(support_link_state::SupportLinkState::new());

    // Shared "should the manager link be connected" gate, shared between the
    // signaling proxy (tears the manager / support upstream down on disable) and
    // the local API (settings controllers flip it when the host toggles the
    // manager connection). Initial value derived from the persisted settings.
    let manager_link_gate = {
        let s = shared_settings.read().await;
        Arc::new(manager_link_gate::ManagerLinkGate::new(
            signaling_proxy::manager_link_should_connect(
                &s.system.manager_url,
                &s.system.manager_api_token,
                s.system.manager_enabled,
            ),
        ))
    };

    // The bridge is now a tiny holder for `tauri_is_admin` + `tauri_login_token`.
    // The host control endpoint owns `/ws/tauri_ipc` and `/ws/host_upstream` and
    // refreshes `tauri_login_token` on every Tauri ws connect.
    let tauri_bridge = TauriIpcBridge::new();

    // Spawn local_api first so /ws/host_upstream is reachable before any
    // forwarder starts trying to connect.
    let (api_ready_tx, api_ready_rx) = oneshot::channel::<()>();
    let api_handle = {
        let settings = Arc::clone(&shared_settings);
        let bridge = Arc::clone(&tauri_bridge);
        let hub = Arc::clone(&host_control_hub);
        let link_state = Arc::clone(&manager_link_state);
        let support_state = Arc::clone(&support_link_state);
        let link_gate = Arc::clone(&manager_link_gate);
        let coordinator = Arc::clone(&settings_coordinator);
        tokio::spawn(async move {
            if let Err(e) = local_api::run_local_api(
                settings,
                coordinator,
                bridge,
                hub,
                link_state,
                support_state,
                link_gate,
                Some(api_ready_tx),
            )
            .await
            {
                error!("Local API error: {e}");
            }
        })
    };

    // Wait for the HTTP server to bind. Failure here means the daemon never
    // becomes useful, so log a warning and continue — the worker forwarder
    // retry-loop will eventually surface the problem.
    match tokio::time::timeout(Duration::from_secs(10), api_ready_rx).await {
        Ok(Ok(())) => info!("Local API bound; spawning workers"),
        Ok(Err(_)) => {
            warn!("Local API ready signal dropped before workers spawned — proceeding anyway")
        }
        Err(_) => warn!(
            "Local API did not signal ready within 10s — proceeding anyway (forwarders may retry)"
        ),
    }

    // Daemon-wide per-`connection_id` PeerConnection registry.
    // Shared between `WorkerManager` (so the media-pipe receiver task
    // can look up `video_track`s) and `signaling_proxy` (so the
    // `RouterContext` referenced by every signaling endpoint sees the
    // same PCs).
    let pc_registry = PcRegistry::new();
    pc_registry.set_host_activity(host_control_hub.host_activity());
    pc_registry.set_host_control_hub(&host_control_hub);

    let (worker_mgr, worker_rx) =
        worker_manager::WorkerManager::new(shared_settings_data.clone(), pc_registry.clone());
    worker_mgr.bind_remote_access_gate(host_control_hub.remote_access_gate());
    settings_coordinator.bind_worker_manager(worker_mgr.clone());
    pc_registry.spawn_system_audio_policy_enforcer(
        settings_coordinator.subscribe_policy(),
        worker_mgr.clone(),
    );

    // Allow the per-connection file-transfer writer task to push a
    // `FileTransferSendFailed` back to the worker on a daemon-side
    // `dc.send` failure. Installed here so the OnceCell sees a real
    // `WorkerManager` for the lifetime of the daemon; tests that
    // never reach this entry point keep observing `None` and stick
    // to the legacy log-and-drop behaviour.
    pc_registry.set_worker_manager(worker_mgr.clone());

    let initial_session = get_current_session_id();
    let initial_desktop = get_initial_desktop_name();
    if let Err(e) = worker_mgr
        .start_worker(initial_session, initial_desktop)
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

    // Service-daemon mode only: construct the virtual display
    // supervisor and hand it to the router (via signaling_proxy). v5
    // lazy lifecycle: attach is NOT performed at daemon startup any
    // more. The router calls `ensure_attached` on the first
    // `RequestRemoteAccess` whose desk settings enable the virtual display,
    // and `cleanup_pc` calls `apply(false)` when the last PC drops.
    // This keeps the IDD invisible to the local user while there is
    // no remote session — so a window dragged onto an extended
    // monitor is impossible when nobody is connected.
    let virtual_display_supervisor = {
        let provider = desk_virtual_display::lifecycle_provider();
        let supervisor = virtual_display::new_arc(provider, worker_mgr.clone());
        // Inject the router-side desired-state computer. The closure
        // captures `settings` and `pc_registry` Arc clones but
        // intentionally does NOT capture the supervisor itself; the
        // supervisor invokes the closure with `active` it has
        // already taken from `is_active()`, avoiding a self-reference
        // and lock cycle.
        {
            let settings = shared_settings_data.clone().into_inner();
            let pc_registry = pc_registry.clone();
            let computer: virtual_display::DesiredComputerFn = Arc::new(move |active: bool| {
                let settings = settings.clone();
                let pc_registry = pc_registry.clone();
                Box::pin(async move {
                    signaling_router::compute_desired_with_active(&settings, &pc_registry, active)
                        .await
                })
            });
            supervisor.set_desired_computer(computer).await;
        }
        Some(supervisor)
    };
    if let Some(coordinator) = host_control_hub.remote_access_coordinator() {
        coordinator.attach_runtime(
            pc_registry.clone(),
            worker_mgr.clone(),
            virtual_display_supervisor.clone(),
            Arc::downgrade(&host_control_hub),
        );
    }

    let proxy_handle = {
        let settings = shared_settings_data.clone();
        let worker_mgr = worker_mgr.clone();
        let host_control_hub = Arc::clone(&host_control_hub);
        let pc_registry = pc_registry.clone();
        let virtual_display = virtual_display_supervisor.clone();
        let manager_link_state = Arc::clone(&manager_link_state);
        let support_link_state = Arc::clone(&support_link_state);
        let manager_link_gate = Arc::clone(&manager_link_gate);
        let exec_ledger = exec_ledger.clone();
        let coordinator = Arc::clone(&settings_coordinator);
        actix_web::rt::spawn(async move {
            if let Err(e) = signaling_proxy::run_signaling_proxy(
                settings,
                coordinator,
                worker_mgr,
                host_control_hub,
                worker_rx,
                pc_registry,
                virtual_display,
                manager_link_state,
                support_link_state,
                manager_link_gate,
                None,
                exec_ledger,
            )
            .await
            {
                error!("Signaling proxy error: {e}");
            }
        })
    };

    // Watchdog kicks the worker if it stops sending IPC traffic.
    // Settings can disable it at runtime (debug aid: a hung worker
    // stays alive for stack capture instead of being respawned).
    let watchdog_handle = worker_mgr.spawn_heartbeat_watchdog();

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

    if let Some(supervisor) = virtual_display_supervisor.as_ref() {
        supervisor.shutdown().await;
    }
    worker_mgr.shutdown_all().await;
    monitor_handle.abort();
    proxy_handle.abort();
    watchdog_handle.abort();
    api_handle.abort();

    info!("ServiceDaemon stopped");
    Ok(())
}

#[cfg(target_os = "windows")]
fn get_current_session_id() -> u32 {
    session_monitor::get_active_session_id()
}

#[cfg(not(target_os = "windows"))]
fn get_current_session_id() -> u32 {
    0
}

/// How long an execution's stored output is kept before it ages out. The row that
/// proves the execution happened is kept for ever; only the output goes.
const EXEC_RESULT_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// How often the result sweep runs. Results age out on a day scale, so an hourly
/// pass is ample and costs nothing.
const EXEC_RESULT_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Open this host's durable exec ledger next to its config file.
///
/// Common to every host form: the ledger is what makes "this dispatch already
/// ran here" answerable across a crash, so a host that failed to open one must
/// not start rather than silently execute unrecorded.
async fn open_exec_ledger(
    paths: &HostDataPaths,
) -> Result<Arc<crate::daemon::exec_ledger::ExecLedger>, String> {
    let dir = paths.exec_ledger_dir().to_string_lossy().to_string();
    let ledger = crate::daemon::exec_ledger::ExecLedger::open(&dir)
        .await
        .map_err(|e| format!("Failed to open the exec ledger in {dir}: {e}"))?;
    info!("Exec ledger initialized at {dir}");

    // Anything still marked in flight belongs to a process that is gone. Settle it
    // as indeterminate now: the host genuinely cannot say whether those commands
    // ran, and leaving them looking in-progress would tell a manager "still
    // working on it" for ever.
    match ledger.abandon_in_flight().await {
        Ok(lost) if !lost.is_empty() => {
            for row in &lost {
                warn!(
                    "[exec-ledger] lost track of execution {} (task {}) across a restart; \
                     recorded as indeterminate, containment {:?}",
                    row.execution_generation, row.task_id, row.containment_identity
                );
            }
        }
        Ok(_) => {}
        Err(e) => return Err(format!("Failed to settle abandoned executions: {e}")),
    }

    let ledger = Arc::new(ledger);
    // Age out stored results on a long cycle. Only the results go; the rows are
    // permanent, because a row is the proof its generation was already accepted.
    {
        let ledger = ledger.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(EXEC_RESULT_SWEEP_INTERVAL);
            loop {
                ticker.tick().await;
                match ledger
                    .forget_results_older_than(EXEC_RESULT_RETENTION)
                    .await
                {
                    Ok(n) if n > 0 => info!("[exec-ledger] aged out {n} stored result(s)"),
                    Ok(_) => {}
                    Err(e) => warn!("[exec-ledger] result sweep failed: {e}"),
                }
            }
        });
    }
    Ok(ledger)
}

/// Single-process entry into the daemon-worker pipeline, used by
/// [`crate::run_with_hub`] for both `StartupMode::Default` (portable) and
/// `StartupMode::DeskServer` (headless desk node). Replaces the legacy
/// `start_desk_session` path so that even in single-process modes the
/// WebRTC PeerConnection lives in the daemon-side code, not in a
/// `DeskSession`-coupled `capture` task.
///
/// Compared with [`run_service_daemon_inner`]:
/// - **No HTTP local API** — the caller's `actix-web` server already binds
///   the listener; in Default mode it also serves `/api/desk/signaling`
///   for browser connections, while in DeskServer mode the local
///   signaling endpoint is intentionally absent (the proxy's local-WS
///   client also skips itself in DeskServer mode — see
///   [`signaling_proxy::run_signaling_proxy`]).
/// - **No `CreateProcessAsUserW`** — `WorkerManager::start_inprocess_worker`
///   spawns the worker as a same-process `actix_web::rt::spawn` task and
///   wires it via in-process tokio mpsc transports.
/// - **Same `signaling_proxy`** — the proxy's signaling-WS clients (local
///   loopback when applicable; remote signaling / manager when configured)
///   feed the daemon-held `PcRegistry` through the shared
///   `SignalingRouter`, so the inbound SDP/ICE path is identical to the
///   ServiceDaemon mode.
/// - **No session monitor / heartbeat watchdog** — single-process modes
///   cannot swap workers on UAC (single process owns the capture session),
///   so there is nothing for the monitor to detect; and watchdog-driven
///   restarts would tear down the actix-rt System the same task is
///   running on.
#[allow(clippy::too_many_arguments)]
pub async fn start_inprocess_daemon(
    args: crate::model::settings::Args,
    settings: web::Data<SharedSettings>,
    settings_coordinator: Arc<crate::model::settings_coordinator::SettingsCoordinator>,
    host_control_hub: Arc<crate::host_control::HostControlHub>,
    own_turn_endpoints: crate::daemon::pc_manager::OwnTurnEndpoints,
    manager_link_state: Arc<crate::daemon::manager_link_state::ManagerLinkState>,
    support_link_state: Arc<crate::daemon::support_link_state::SupportLinkState>,
    manager_link_gate: Arc<crate::daemon::manager_link_gate::ManagerLinkGate>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Starting in-process daemon (portable mode)");
    host_control_hub
        .host_activity()
        .set_indicator_enabled(settings.read().await.system.host_access_indicator_enabled);
    let paths = settings.read().await.paths().clone();
    initialize_remote_access(&paths, &host_control_hub);

    let exec_ledger = open_exec_ledger(&paths).await?;

    // Endpoints of this node's own bundled TURN, read from the running runtime
    // on each use (empty when no embedded TURN is serving) so the PC manager
    // never relays through itself, and follows the relay if it moves.
    let pc_registry = PcRegistry::new().with_own_turn_endpoints(own_turn_endpoints);
    pc_registry.set_host_activity(host_control_hub.host_activity());
    pc_registry.set_host_control_hub(&host_control_hub);
    let (worker_mgr, worker_rx) =
        worker_manager::WorkerManager::new(settings.clone(), pc_registry.clone());
    worker_mgr.bind_remote_access_gate(host_control_hub.remote_access_gate());
    settings_coordinator.bind_worker_manager(worker_mgr.clone());
    pc_registry.spawn_system_audio_policy_enforcer(
        settings_coordinator.subscribe_policy(),
        worker_mgr.clone(),
    );
    if let Some(coordinator) = host_control_hub.remote_access_coordinator() {
        coordinator.attach_runtime(
            pc_registry.clone(),
            worker_mgr.clone(),
            None,
            Arc::downgrade(&host_control_hub),
        );
    }

    // See ServiceDaemon entry above: install the WorkerManager handle
    // so the file-transfer writer task can report `dc.send` failures
    // back to the worker via FileTransferSendFailed.
    pc_registry.set_worker_manager(worker_mgr.clone());

    let session_id = get_current_session_id();
    let initial_desktop = get_initial_desktop_name();
    let computer_use_broker =
        Arc::new(crate::worker::agent::computer_use_broker::ComputerUseBroker::new());
    let device_id = settings
        .read()
        .await
        .system
        .get_client_id()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    computer_use_broker.start_browser_extension_bridge(
        paths.data_root(),
        device_id,
        session_id.to_string(),
    )?;

    // Spawn the in-process worker: same WorkerSession::run_with_transports
    // entry point as the ServiceDaemon path; only difference is the
    // transport flavour (mpsc instead of named-pipe-framed).
    worker_mgr
        .start_inprocess_worker(
            args,
            session_id,
            initial_desktop,
            Arc::clone(&host_control_hub),
            Arc::clone(&computer_use_broker),
        )
        .await?;

    // Same signaling proxy as ServiceDaemon mode: drains worker_rx, routes
    // inbound signaling against the shared PcRegistry, and connects to the
    // local + remote signaling endpoints (per the proxy's Default-mode
    // branch — see `signaling_proxy::run_signaling_proxy`).
    let proxy_settings = settings.clone();
    let proxy_hub = Arc::clone(&host_control_hub);
    let proxy_registry = pc_registry.clone();
    // In-process mode never owns a virtual display supervisor — the
    // router replies with FEATURE_UNAVAILABLE for ChangeDisplaySettings.
    actix_web::rt::spawn(async move {
        if let Err(e) = signaling_proxy::run_signaling_proxy(
            proxy_settings,
            settings_coordinator,
            worker_mgr,
            proxy_hub,
            worker_rx,
            proxy_registry,
            None,
            manager_link_state,
            support_link_state,
            manager_link_gate,
            Some(computer_use_broker),
            exec_ledger,
        )
        .await
        {
            error!("In-process signaling proxy error: {e}");
        }
    });

    Ok(())
}

fn initialize_remote_access(paths: &HostDataPaths, host_control_hub: &HostControlHub) {
    let store =
        remote_access::RemoteAccessStateStore::new(paths.remote_access_state_file().to_path_buf());
    let state = store.load_or_initialize();
    host_control_hub
        .remote_access_gate()
        .initialize_from_store(state.clone());
    host_control_hub
        .host_activity()
        .set_remote_access_status((&state).into());
    let coordinator = Arc::new(remote_access::RemoteAccessCoordinator::new(
        store.clone(),
        host_control_hub.remote_access_gate(),
        host_control_hub.host_activity(),
    ));
    let local_access_service = Arc::new(local_access_control::LocalAccessControlService::new(
        coordinator.clone(),
        Arc::new(local_access_control::ElevatedPeerAuthenticator),
    ));
    let local_access_endpoint = local_access_control_transport::endpoint_for_paths(paths);
    actix_web::rt::spawn(async move {
        if let Err(error) =
            local_access_control_transport::serve(local_access_endpoint, local_access_service).await
        {
            error!("LocalAccessControl transport stopped: {error:#}");
        }
    });
    if host_control_hub
        .install_remote_access_coordinator(coordinator)
        .is_err()
    {
        warn!("Remote-access coordinator was already initialized");
    }
    info!(
        "Remote access state loaded: mode={:?}, version={}, path={}",
        state.mode,
        state.state_version,
        store.path().display()
    );
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
