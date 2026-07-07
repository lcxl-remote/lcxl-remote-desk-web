use super::manager_link_state::ManagerLinkState;
use super::support_link_state::SupportLinkState;
use super::pc_manager::PcRegistry;
use super::signaling_router::{self, RouterContext};
use super::virtual_display::VirtualDisplaySupervisor;
use super::worker_manager::{WorkerManager, WorkerMessageReceiver};
use crate::diagnose::DiagnoseOrchestrator;
use crate::diagnose::collector::AgentContextCollector;
use crate::diagnose::redaction::RegexRedactor;
use crate::host_control::HostControlHub;
use crate::model::settings::{SharedSettings, StartupMode};
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::audit_sink::{LogAuditSink, RemoteAuditSink};
use actix_web::web;
use awc::{Client, Connector};
use desk_agent_protocol::audit::AuditSink;
use desk_agent_protocol::authz::{AuthorizationBlock, AuthorizedControlPayload};
use desk_ipc_protocol::message::{
    ERROR_CODE_MEDIA_TRANSPORT_STUCK, VirtualDisplayModeOutcome, WorkerToService,
};
use desk_signal_facade::model::{
    signal::{RemoteDeskTypeEnum, SignalingModel, SignalingResponseState, SignalingType},
    version::VersionInfo,
    virtual_display::ChangeDisplaySettingsPayload,
};
use desk_utils::error::DeskErrorCode;
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

pub async fn run_signaling_proxy(
    settings: web::Data<SharedSettings>,
    worker_mgr: WorkerManager,
    host_control_hub: Arc<HostControlHub>,
    mut worker_rx: WorkerMessageReceiver,
    pc_registry: PcRegistry,
    virtual_display: Option<Arc<VirtualDisplaySupervisor>>,
    // Shared host→manager link status: records a fatal registration rejection and
    // carries the manual-retry signal. The same handle is exposed to the host REST
    // API so the UI can show the rejection and trigger a reconnect.
    manager_link_state: Arc<ManagerLinkState>,
    // On-demand temporary-support lifecycle: the host REST API flips it active to
    // request a support session; the support loop below drives a dedicated Support
    // upstream from it. The same handle rides into the router context so the
    // inbound `SupportCodeIssued` handler can record the code + arm its TTL.
    support_link_state: Arc<SupportLinkState>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Signaling proxy starting");

    let (outbound_tx, _seed_rx) = broadcast::channel::<String>(128);

    // Operator command templates (built-in baseline ∪ manager-synced) and the
    // agentic exec coordinator are shared by the agentic diagnose runtime (exec
    // classify + approval/result waits), the router (ResolveExec routing), and the
    // worker-message loop (ExecResult delivery), so all reference the same state.
    let command_templates = Arc::new(crate::daemon::command_templates::CommandTemplateCache::new());
    let command_blocklist =
        Arc::new(crate::daemon::command_blocklist::CommandBlocklistCache::new());
    let agentic_exec = Arc::new(crate::daemon::agentic_exec::AgenticExecCoordinator::new());

    // The diagnose orchestrator runs daemon-side wherever an in-process worker
    // can collect locally (Default / DeskServer); ServiceDaemon leaves it `None`.
    // AI diagnosis is orchestrated by the central signaling brain, so this host
    // only serves the remote-collect edge path: the central server pushes a
    // `CollectRequest` and the orchestrator gathers evidence through the in-process
    // agent and scrubs it with the regex redactor before streaming it back. The
    // `EdgeReadInvoker` serves the central read-tool path against the same agent.
    let (diagnose_orchestrator, remote_read) = match settings.read().await.args.startup_mode {
        StartupMode::ServiceDaemon => (None, None),
        _ => {
            let audit: Arc<dyn AuditSink> = Arc::new(LogAuditSink);
            let agent = Arc::new(
                LocalDeviceAgent::with_settings(settings.clone().into_inner()).with_audit(audit),
            );
            let collector = Arc::new(AgentContextCollector::new(
                agent.clone(),
                settings.clone().into_inner(),
            ));
            // The orchestrator only serves `collect_for_remote`: it runs the
            // collect + fail-closed redact phases and never dials a model
            // (diagnosis is orchestrated centrally).
            let orchestrator = Arc::new(DiagnoseOrchestrator::new(
                collector,
                Arc::new(RegexRedactor::new()),
            ));
            // Serves a central read-tool call (§8.3) against the same in-process
            // agent, redacting fail-closed.
            let edge_read = Arc::new(crate::diagnose::remote_read::EdgeReadInvoker::new(
                agent,
                Arc::new(RegexRedactor::new()),
                settings.clone().into_inner(),
            ));
            (Some(orchestrator), Some(edge_read))
        }
    };

    // Choose the audit sink once: report to the manager (DB persistence) when a
    // manager is configured, otherwise log locally. `RemoteAuditSink` still logs
    // too, so a fleet host keeps a local trail.
    let manager_configured = {
        let s = settings.read().await;
        s.system
            .manager_url
            .as_ref()
            .map(|u| !u.is_empty())
            .unwrap_or(false)
    };
    let audit_sink: Arc<dyn AuditSink> = if manager_configured {
        Arc::new(RemoteAuditSink::new(outbound_tx.clone()))
    } else {
        Arc::new(LogAuditSink)
    };

    // The daemon constructs `pc_registry` once in `daemon::mod` and shares
    // it with both `WorkerManager` (for the media-pipe receiver) and the
    // signaling proxy (for inbound SDP/ICE handlers). Using a single
    // registry across all signaling endpoints (local / remote signaling /
    // remote manager) means the same PC handles inbound messages
    // regardless of which WS surfaced them.
    let router_ctx = RouterContext {
        pc_registry: pc_registry.clone(),
        outbound_tx: outbound_tx.clone(),
        settings: settings.clone(),
        host_control_hub: host_control_hub.clone(),
        worker_mgr: worker_mgr.clone(),
        // Some(...) only in service-daemon mode; in-process and
        // desk-server modes leave this None so the router replies
        // with FEATURE_UNAVAILABLE for every inbound
        // ChangeDisplaySettings.
        virtual_display: virtual_display.clone(),
        diagnose_orchestrator: diagnose_orchestrator.clone(),
        remote_read: remote_read.clone(),
        // Confirmed execution is available wherever an in-process worker can
        // execute (Default / DeskServer), gated like the diagnose orchestrator.
        exec_supported: diagnose_orchestrator.is_some(),
        exec_approvals: Arc::new(crate::daemon::exec_approval::PendingApprovalStore::new()),
        agentic_exec: agentic_exec.clone(),
        session_approvals: Arc::new(crate::daemon::session_approval::SessionApprovalStore::new()),
        command_templates: command_templates.clone(),
        command_blocklist: command_blocklist.clone(),
        // Audit sink: in fleet mode (a manager is configured) report events to
        // the manager for DB persistence; otherwise keep the local log sink.
        audit: audit_sink.clone(),
        diagnose_tasks: Default::default(),
        // Per-call trusted-central authorization is injected by the inbound
        // dispatcher; the shared base context carries none.
        inbound_authz: None,
        // Per-call restriction flag: set by the inbound dispatcher when the frame
        // arrived on the support upstream. The shared base context is unrestricted.
        inbound_restricted: false,
        // Fleet exec correlation set, shared with the worker-message loop below so
        // a worker `ExecResult` for an in-flight fleet attempt is relayed to the
        // manager as a `EdgeExecResult`.
        edge_exec_pending: Default::default(),
        // On-demand temporary-support lifecycle, shared with the support loop.
        support_link_state: support_link_state.clone(),
    };

    let local_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (port, enable_ipv6, local_token, startup_mode) = {
                    let s = settings.read().await;
                    (
                        s.system.port,
                        s.system.enable_ipv6,
                        s.system.local_signaling_token.clone().unwrap_or_default(),
                        s.args.startup_mode.clone(),
                    )
                };

                // In Default mode connect to the embedded server port; in
                // ServiceDaemon mode connect to the daemon's own HTTP server.
                // All other modes have no local signaling endpoint.
                let effective_port = match startup_mode {
                    StartupMode::Default => port,
                    StartupMode::ServiceDaemon => crate::daemon::local_api::SERVICE_API_PORT,
                    _ => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        continue;
                    }
                };

                if local_token.is_empty() {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                // The local proxy should keep using loopback even when the daemon
                // API is exposed on all interfaces.
                let local_url = if enable_ipv6 && startup_mode != StartupMode::ServiceDaemon {
                    format!("ws://[::1]:{effective_port}/api/desk/signaling")
                } else {
                    format!("ws://127.0.0.1:{effective_port}/api/desk/signaling")
                };

                let rx = outbound_tx.subscribe();
                // The local loopback is `TrustedCentral` in Default mode but is NOT
                // the manager link, so fatal device-quota rejection is disabled here.
                let _ = maintain_proxy_connection(
                    settings.clone(),
                    &router_ctx,
                    local_url,
                    local_token,
                    rx,
                    local_loopback_source(&startup_mode),
                    false,
                    None,
                )
                .await;

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    let remote_sig_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (signaling_url, signaling_token) = {
                    let s = settings.read().await;
                    (
                        s.system.signaling_url.clone(),
                        s.system.signaling_token.clone(),
                    )
                };

                if let (Some(url), Some(token)) = (signaling_url, signaling_token)
                    && !url.is_empty()
                    && !token.is_empty()
                {
                    let rx = outbound_tx.subscribe();
                    // A bare remote-signaling relay is not the manager link; fatal
                    // device-quota rejection does not apply.
                    let _ = maintain_proxy_connection(
                        settings.clone(),
                        &router_ctx,
                        url,
                        token,
                        rx,
                        InboundSignalingSource::RemoteSignaling,
                        false,
                        None,
                    )
                    .await;
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    let remote_mgr_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
        let manager_link_state = manager_link_state.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (manager_url, manager_api_token) = {
                    let s = settings.read().await;
                    (
                        s.system.manager_url.clone(),
                        s.system.manager_api_token.clone(),
                    )
                };

                if let (Some(url), Some(token)) = (manager_url, manager_api_token)
                    && !url.is_empty()
                    && !token.is_empty()
                {
                    let rx = outbound_tx.subscribe();
                    // The manager link is the only one that enforces fatal
                    // device-quota rejection (`remote_mgr_handle` is the sole manager
                    // injection point).
                    let outcome = maintain_proxy_connection(
                        settings.clone(),
                        &router_ctx,
                        url,
                        token,
                        rx,
                        InboundSignalingSource::TrustedCentral,
                        true,
                        Some(manager_link_state.clone()),
                    )
                    .await;

                    if let Ok(ProxyConnectionOutcome::FatalReject { .. }) = outcome {
                        // Stop the 5s auto-reconnect storm: retrying changes nothing
                        // until the user frees a device slot from a control end. Park
                        // until a manual retry is requested, then reconnect at once
                        // (no long backoff).
                        manager_link_state.await_retry().await;
                        manager_link_state.clear().await;
                        continue;
                    }
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    // On-demand temporary-support upstream. Unlike the three always-on upstreams
    // this one parks until a local user requests a support code (the host REST API
    // flips `support_link_state` active), then opens a single dedicated `Support`
    // link to the manager. It serves exactly one session — until the upstream
    // closes, the local user ends support, or the code's TTL expires — then
    // force-tears any restricted PCs the supporter established and parks again.
    let support_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
        let support_link_state = support_link_state.clone();
        actix_web::rt::spawn(async move {
            loop {
                support_link_state.wait_for_start().await;

                let (manager_url, manager_api_token) = {
                    let s = settings.read().await;
                    (
                        s.system.manager_url.clone(),
                        s.system.manager_api_token.clone(),
                    )
                };

                if let (Some(url), Some(token)) = (manager_url, manager_api_token)
                    && !url.is_empty()
                    && !token.is_empty()
                {
                    let rx = outbound_tx.subscribe();
                    // Serve one support session: whichever finishes first wins —
                    // the upstream connection ending, or a stop (manual "end
                    // support" / TTL expiry) flipping the state inactive.
                    tokio::select! {
                        _ = maintain_proxy_connection(
                            settings.clone(),
                            &router_ctx,
                            url,
                            token,
                            rx,
                            InboundSignalingSource::Support,
                            false,
                            None,
                        ) => {}
                        _ = support_link_state.wait_for_stop() => {}
                    }
                    // End the supporter's session physically, not just at the
                    // signaling layer, by closing every restricted PC.
                    crate::daemon::pc_manager::cleanup_restricted_connections(
                        &router_ctx.pc_registry,
                        &router_ctx.worker_mgr,
                        router_ctx.virtual_display.as_ref(),
                        "support_session_ended",
                    )
                    .await;
                } else {
                    warn!("[support] start requested but the manager link is not configured");
                }
                // Reset for the next session (idempotent if already stopped).
                support_link_state.finish().await;
            }
        })
    };

    while let Some(msg) = worker_rx.recv().await {
        // Every IPC message — heartbeat, signaling, desktop change —
        // counts as a sign of life for the watchdog. Updating before
        // the match keeps the bookkeeping in one place and avoids the
        // watchdog firing on a worker that's actively talking but
        // hasn't happened to send a Heartbeat in the last interval.
        worker_mgr.note_heartbeat().await;

        match msg {
            WorkerToService::Ready => {
                info!("[SignalingProxy] Worker is Ready");
            }
            WorkerToService::Capabilities(caps) => {
                info!(
                    "[SignalingProxy] Worker reported capabilities: video={:?}, audio={:?}, \
                     desktop={:?}, has_tauri={}, is_admin={}",
                    caps.video_codecs,
                    caps.audio_codecs,
                    caps.desktop_name,
                    caps.has_tauri,
                    caps.is_admin,
                );
                worker_mgr.set_worker_capabilities(caps);
                // Keep-PC: every Capabilities arrival is the
                // signal the worker is ready to accept media work.
                // Re-issue cached `StartMedia` + `ForceKeyframe` for
                // every PC that already negotiated an offer; the
                // first IDR clears each PC's `media_paused` flag in
                // place. For the very first Capabilities (no PCs yet,
                // no cached offers) this is a no-op.
                pc_registry.resume_active_media(&worker_mgr).await;
                // Virtual display reattach: tell the supervisor a
                // worker is alive, so a freshly-spawned worker that
                // arrives mid-session gets AttachVirtualDisplay
                // without polling. The supervisor decides whether
                // to actually send (no-op unless Attaching /
                // Attached).
                if let Some(supervisor) = virtual_display.as_ref() {
                    supervisor.on_worker_capabilities().await;
                }
            }
            // Worker-emitted typed error response. Catches every
            // `service::signaling::DeskSession::send_error` call (terminal
            // permission denied, manager file errors, fallthrough
            // `_ => UNKNOWN_SIGNALING_TYPE`, ...) regardless of the
            // originating `SignalingType`. Daemon rebuilds the matching
            // outbound `SignalingModel::error(...)` and broadcasts to the
            // WS sinks so the browser sees the error response on its
            // pending request.
            WorkerToService::SignalingError(payload) => {
                let response_state = SignalingResponseState {
                    error_code: payload.error_code,
                    message: payload.error_message.clone(),
                };
                match SignalingModel::new_response::<()>(
                    &payload.request_id,
                    payload.signaling_type,
                    None,
                    Some(payload.connection_id.clone()),
                    None,
                    response_state,
                ) {
                    Ok(model) => match serde_json::to_string(&model) {
                        Ok(text) => {
                            let _ = outbound_tx.send(text);
                        }
                        Err(e) => warn!(
                            "[SignalingProxy] Failed to serialise SignalingError response \
                             for {} (request_id={}, type={:?}): {e}",
                            payload.connection_id, payload.request_id, payload.signaling_type,
                        ),
                    },
                    Err(e) => warn!(
                        "[SignalingProxy] Failed to build SignalingError response model \
                         for {} (request_id={}, type={:?}): {e}",
                        payload.connection_id, payload.request_id, payload.signaling_type,
                    ),
                }
            }
            WorkerToService::Heartbeat(hb) => {
                log::trace!(
                    "[SignalingProxy] Heartbeat: connections={}, ts={}",
                    hb.active_connections,
                    hb.timestamp_ms
                );
            }
            WorkerToService::DesktopChanged(payload) => {
                // Portable / Default mode: the "worker" is an in-process
                // task and we can't cross window-stations from a single
                // process anyway. The worker still spawns desktop_monitor
                // (it doesn't know its own topology) so the event arrives,
                // but acting on it would mean calling `start_worker` —
                // i.e. `CreateProcessAsUserW` — from a non-SYSTEM
                // context, which fails or worse, half-succeeds. Skip.
                if worker_mgr.is_inprocess() {
                    debug!(
                        "[SignalingProxy] In-process worker reported desktop drift -> '{}'; \
                         no-op in portable mode (single process cannot cross window stations)",
                        payload.name
                    );
                    continue;
                }
                info!(
                    "[SignalingProxy] Worker reported desktop drift -> '{}'; restarting worker \
                     (keep-PC: browser PC stays up across the swap)",
                    payload.name
                );
                let worker_mgr = worker_mgr.clone();
                let new_desktop = payload.name.clone();
                // Run the switch on a separate task so the message loop
                // keeps draining (notify_desktop_switch awaits worker
                // mailbox flushes; bridge_loop pushes more messages while
                // we wait). notify_desktop_switch pauses every
                // PC and tells the dying worker to shut down its
                // encoders, then start_worker spawns the replacement.
                // The new worker's `Capabilities` arrival above triggers
                // `pc_registry.resume_active_media` to re-issue
                // StartMedia + ForceKeyframe; the first IDR per PC
                // clears its `media_paused` flag and the browser sees
                // the desktop swap as a brief frame freeze.
                actix_web::rt::spawn(async move {
                    worker_mgr.notify_desktop_switch().await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let session_id = crate::daemon::session_monitor::get_active_session_id();
                    if let Err(e) = worker_mgr
                        .start_worker(session_id, Some(new_desktop.clone()))
                        .await
                    {
                        error!(
                            "[SignalingProxy] Failed to start worker for desktop '{}': {}",
                            new_desktop, e
                        );
                    }
                });
            }
            WorkerToService::Error(err) => {
                error!(
                    "[SignalingProxy] Worker error: code={}, msg={}, recoverable={}, \
                     connection_id={:?}",
                    err.code, err.message, err.recoverable, err.connection_id
                );
                // MediaTransportStuck self-heal: I-frame send timed out
                // on the worker side. The daemon (not the worker) owns
                // recovery — issue StopMedia +
                // StartMedia + ForceKeyframe so the encoder pipeline
                // is rebuilt and a fresh IDR clears the paused flag.
                if err.code == ERROR_CODE_MEDIA_TRANSPORT_STUCK {
                    if let Some(connection_id) = err.connection_id.clone() {
                        let registry = pc_registry.clone();
                        let worker_mgr = worker_mgr.clone();
                        actix_web::rt::spawn(async move {
                            registry.reset_media_for(&connection_id, &worker_mgr).await;
                        });
                    } else {
                        warn!(
                            "[SignalingProxy] MediaTransportStuck without connection_id; \
                             cannot scope reset — leaving stream paused"
                        );
                    }
                }
            }
            // Cursor sync: worker emits CursorData when its
            // capture loop sees a cursor shape / position update;
            // daemon writes the JSON bytes to the matching browser's
            // `cursor_sync_event` DC. Lookup happens in
            // `pc_manager::write_cursor_data` (silent-drop on
            // unknown connection / no DC).
            WorkerToService::CursorData(payload) => {
                crate::daemon::pc_manager::write_cursor_data(&pc_registry, payload).await;
            }
            // Clipboard write-back: worker emits
            // ClipboardRead when its polling task observes a local
            // clipboard change (or in response to ClipboardRequest);
            // daemon writes the JSON to the matching browser's
            // `clipboard_event` DC. Permission gate
            // (`accept_clipboard_sync`) lives in
            // `pc_manager::write_clipboard_data` so the worker stays
            // ignorant of per-connection accept state.
            WorkerToService::ClipboardRead(payload) => {
                crate::daemon::pc_manager::write_clipboard_data(&pc_registry, payload).await;
            }
            // Typed-IPC migration batch 1: replaces the legacy
            // `WorkerToService::SignalingMessage` reverse path for
            // private-screen state changes. Daemon constructs the
            // outbound `SignalingType::PrivateScreenStateChanged`
            // model (matching the wire shape the browser already
            // expects) and broadcasts it through the same outbound
            // channel the SignalingMessage path used. Build failures
            // are non-fatal — log + drop, no panic on the bus.
            WorkerToService::PrivateScreenStateChanged(payload) => {
                match SignalingModel::new_request(
                    SignalingType::PrivateScreenStateChanged,
                    Some(payload.connection_id.clone()),
                    Some(&payload.data),
                ) {
                    Ok(model) => match serde_json::to_string(&model) {
                        Ok(text) => {
                            let _ = outbound_tx.send(text);
                        }
                        Err(e) => warn!(
                            "[SignalingProxy] Failed to serialise PrivateScreenStateChanged \
                             for {}: {e}",
                            payload.connection_id
                        ),
                    },
                    Err(e) => warn!(
                        "[SignalingProxy] Failed to build PrivateScreenStateChanged model \
                         for {}: {e}",
                        payload.connection_id
                    ),
                }
            }
            // Batch 2 of the typed-IPC migration — manager plane
            // responses. Each `Manager*Response` rebuilds the
            // matching outbound `SignalingType::Manager*` response
            // model and writes it to the connection's WS sink. The
            // daemon owns the SignalingResponseState (always
            // `success` here — the worker only ships responses for
            // requests it handled successfully; failures still ride
            // the `Error` enum).
            WorkerToService::ManagerSystemInfoResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "ManagerSystemInfo",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::ManagerSystemInfo,
                    Some(&payload.info),
                );
            }
            WorkerToService::ManagerFileListResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "ManagerFileList",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::ManagerFileList,
                    Some(&payload.response),
                );
            }
            WorkerToService::ManagerFileDeleteResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "ManagerFileDelete",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::ManagerFileDelete,
                    Option::<&()>::None,
                );
            }
            WorkerToService::ManagerQuerySettingsResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "ManagerQuerySettings",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::ManagerQuerySettings,
                    Some(&payload.settings),
                );
            }
            WorkerToService::ManagerUpdateSettingsResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "ManagerUpdateSettings",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::ManagerUpdateSettings,
                    Option::<&()>::None,
                );
            }
            // Batch 3 of the typed-IPC migration — terminal plane.
            // Each `Terminal*` variant rebuilds the matching outbound
            // `SignalingType::*` model and writes it onto the
            // outbound channel for the WS sinks to ship to the
            // browser. `TerminalStarted` is a `success_response`
            // (StartTerminal correlation); `TerminalClosed` and
            // `ReplyFromTerminal` are server-initiated `new_request`
            // notifications (no `request_id` correlation);
            // `ListTerminalResponse` is a `success_response` for
            // `ListTerminal`.
            WorkerToService::TerminalStarted(payload) => {
                // Terminal session traffic always carries a
                // `from_connection_id` (open_terminal_session mints
                // one per terminal WS); wrap it in `Some` so the
                // shared helper signature stays unified with
                // manager-plane responses where it's `Option`.
                let to = Some(payload.connection_id);
                send_manager_response(
                    &outbound_tx,
                    "TerminalStarted",
                    &payload.request_id,
                    &to,
                    SignalingType::TerminalStarted,
                    Option::<&()>::None,
                );
            }
            WorkerToService::TerminalClosed(payload) => {
                send_terminal_notification(
                    &outbound_tx,
                    "TerminalClosed",
                    &payload.connection_id,
                    SignalingType::TerminalClosed,
                    Option::<&()>::None,
                );
            }
            WorkerToService::ReplyFromTerminal(payload) => {
                send_terminal_notification(
                    &outbound_tx,
                    "ReplyFromTerminal",
                    &payload.connection_id,
                    SignalingType::ReplyFromTerminal,
                    Some(&payload.data),
                );
            }
            WorkerToService::ListTerminalResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "ListTerminal",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::ListTerminal,
                    Some(&payload.terminals),
                );
            }
            // Virtual display attach result: drive the supervisor's
            // state machine via the worker's confirmation that it
            // resolved (or failed to resolve) the PnP instance id we
            // forwarded. Not a browser-facing message — the supervisor
            // is the only consumer; the browser learns about success /
            // failure indirectly through is_active() gating on the
            // next ChangeDisplaySettings request.
            WorkerToService::VirtualDisplayAttachResult(payload) => {
                dispatch_attach_result(payload, virtual_display.as_ref()).await;
            }
            // Virtual display response: rebuild the matching outbound
            // ChangeDisplaySettings model and write it onto the
            // browser's signaling WS. Applied -> success response with
            // the mode the driver actually applied (may have been
            // snapped). Failed -> SignalingModel::error with
            // INVALID_STATE + the controller's error message.
            WorkerToService::VirtualDisplayMode(payload) => {
                let connection_id_debug = payload.connection_id.clone();
                let request_id_debug = payload.request_id.clone();
                match build_virtual_display_response(payload, virtual_display.as_ref()) {
                    Ok(model) => match serde_json::to_string(&model) {
                        Ok(text) => {
                            let _ = outbound_tx.send(text);
                        }
                        Err(e) => warn!(
                            "[SignalingProxy] Failed to serialise VirtualDisplayMode response \
                             for {connection_id_debug} (request_id={request_id_debug}): {e}"
                        ),
                    },
                    Err(e) => warn!(
                        "[SignalingProxy] Failed to build VirtualDisplayMode response model \
                         for {connection_id_debug} (request_id={request_id_debug}): {e}"
                    ),
                }
            }
            // Route to the supervisor; the driver loop and op_id gate
            // live there. Routing is a no-op when this is a non-
            // service-daemon mode that does not own a supervisor
            // (Default/DeskServer pass `virtual_display = None`).
            WorkerToService::ExclusiveResult(payload) => {
                if let Some(supervisor) = virtual_display.as_ref() {
                    supervisor.on_exclusive_result(payload).await;
                } else {
                    log::debug!(
                        "[SignalingProxy] ExclusiveResult arrived but no supervisor in this mode; \
                         op_id={} direction={:?}",
                        payload.op_id,
                        payload.direction,
                    );
                }
            }
            // AI agent reply: rebuild the outbound
            // `SignalingType::AgentResponse` model carrying the
            // `AgentOutcome` verbatim as signaling_data and write it onto
            // the control end's signaling WS. Capability-level errors live
            // inside the `AgentOutcome::Err` (the response state stays a
            // transport-level success), so the control-end UI receives the
            // full structured `AgentError`. Mirrors the
            // manager-plane response rebuild.
            WorkerToService::AgentResponse(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "AgentResponse",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::AgentResponse,
                    Some(&payload.outcome),
                );
            }
            // AI exec result: rebuild the outbound `SignalingType::ExecResult`
            // as a notification-style frame (`response_state = None`) carrying
            // the `ExecResultPayload` verbatim, correlated to the suggested
            // command by `exec_request_id`. Execution failures live inside the
            // payload's `AgentOutcome::Err`, not the transport.
            WorkerToService::ExecResult(payload) => {
                // Audit the completion (single-machine log sink). The summary is
                // content-free (exit code / error kind only), never stdout.
                {
                    use desk_agent_protocol::AgentOutcome;
                    let (success, summary, redactions) = match &payload.result.outcome {
                        AgentOutcome::Ok(desk_agent_protocol::OperationOutput::Exec(o)) => (
                            o.exit_code == 0,
                            format!("exit {}", o.exit_code),
                            o.redactions.len() as i32,
                        ),
                        AgentOutcome::Ok(_) => (true, "ok".to_string(), 0),
                        AgentOutcome::Err(e) => (false, format!("{:?}", e.kind), 0),
                    };
                    router_ctx
                        .audit
                        .record(
                            desk_agent_protocol::audit::AuditEvent::command_completed(
                                uuid::Uuid::new_v4().to_string(),
                                chrono::Utc::now().to_rfc3339(),
                                &payload.result.exec_request_id.0,
                                success,
                                summary,
                                redactions,
                                0,
                            )
                            // The worker echoed back the originating ConfirmExec
                            // frame request_id (the manager's ledger key) so this
                            // completion is attributed to the real operator.
                            .with_task_id(payload.audit_source_request_id.as_deref()),
                        )
                        .await;
                }
                // Agentic exec correlation: if the model-initiated loop is awaiting
                // this result (keyed by `exec_request_id`), hand it to the awaiting
                // runner and suppress the browser-bound frame — the loop feeds the
                // result back to the model instead.
                if router_ctx.agentic_exec.deliver_result(
                    &payload.result.exec_request_id.0,
                    payload.result.outcome.clone(),
                ) {
                    continue;
                }
                // Fleet exec correlation: if this result is for an in-flight
                // fleet attempt, relay it to the manager as a `EdgeExecResult`
                // (`Executed`) instead of an `ExecResult(609)` toward a browser.
                let is_fleet = router_ctx
                    .edge_exec_pending
                    .lock()
                    .map(|mut p| p.remove(&payload.request_id))
                    .unwrap_or(false);
                if is_fleet {
                    signaling_router::send_edge_exec_result(
                        &outbound_tx,
                        &payload.request_id,
                        desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed {
                            outcome: payload.result.outcome.clone(),
                        },
                    );
                    continue;
                }
                match serde_json::to_value(&payload.result) {
                    Ok(value) => {
                        let frame = SignalingModel::new(
                            &payload.request_id,
                            SignalingType::ExecResult,
                            None,
                            payload.connection_id.clone(),
                            Some(value),
                            None,
                        );
                        match serde_json::to_string(&frame) {
                            Ok(text) => {
                                let _ = outbound_tx.send(text);
                            }
                            Err(e) => warn!(
                                "[SignalingProxy] Failed to serialise ExecResult frame for \
                                 {:?}: {e} (request_id={})",
                                payload.connection_id, payload.request_id,
                            ),
                        }
                    }
                    Err(e) => warn!(
                        "[SignalingProxy] Failed to serialise ExecResultPayload for {:?}: {e} \
                         (request_id={})",
                        payload.connection_id, payload.request_id,
                    ),
                }
            }
        }
    }

    local_handle.abort();
    remote_sig_handle.abort();
    remote_mgr_handle.abort();
    support_handle.abort();

    info!("Signaling proxy stopped");
    Ok(())
}

/// Dispatch a [`WorkerToService::VirtualDisplayAttachResult`] to the
/// supervisor if it exists; in non-service-daemon modes
/// (`virtual_display = None`) production routes never produce this
/// variant, so a stray reply is either a test fixture or a logic bug —
/// drop it with a warning rather than panic. Extracted from the proxy
/// match arm to keep the routing logic unit-testable without spinning
/// up the full proxy task / outbound channel infrastructure.
async fn dispatch_attach_result(
    payload: desk_ipc_protocol::message::VirtualDisplayAttachResultPayload,
    virtual_display: Option<
        &std::sync::Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>,
    >,
) {
    match virtual_display {
        Some(supervisor) => {
            supervisor.on_worker_attach_result(payload).await;
        }
        None => {
            warn!(
                "[SignalingProxy] VirtualDisplayAttachResult arrived while supervisor \
                 disabled (non-service-daemon mode?); dropping instance_id={}",
                payload.instance_id,
            );
        }
    }
}

/// Helper: build the outbound `SignalingModel` for a
/// `WorkerToService::VirtualDisplayMode` response. Applied →
/// success response carrying the mode the driver actually applied
/// (which may have been snapped to a nearby supported configuration);
/// Failed → `SignalingModel::error(INVALID_STATE, reason)`.
///
/// On a successful `Applied` outcome we also update the supervisor's
/// `last_known_refresh_hz` cache — this is the daemon's authoritative
/// source for the refresh-hz fallback when the auto-resolution browser
/// hook sends `refresh_hz=0`. Stray responses in non-service-daemon
/// mode (`supervisor=None`) are tolerated and the cache simply does
/// not update.
///
/// Kept as a free function so the routing logic can be unit-tested
/// without spinning up a signaling-proxy task. The call site in the
/// proxy loop only deals with the serialisation + outbound broadcast.
fn build_virtual_display_response(
    payload: desk_ipc_protocol::message::VirtualDisplayModeResponsePayload,
    supervisor: Option<&std::sync::Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
) -> Result<SignalingModel, desk_signal_facade::error::DeskSignalFacadeError> {
    let connection_id = Some(payload.connection_id);
    match payload.outcome {
        VirtualDisplayModeOutcome::Applied(data) => {
            if let Some(supervisor) = supervisor {
                // Cache the full mode so the router's idempotent
                // short-circuit can compare exact (width, height,
                // refresh_hz) on the next inbound 205. `record_applied_mode`
                // silently drops any update with a zero component, so a
                // malformed worker echo cannot poison the cache.
                supervisor.record_applied_mode(data.width, data.height, data.refresh_hz);
            }
            let response = ChangeDisplaySettingsPayload {
                width: data.width,
                height: data.height,
                refresh_hz: data.refresh_hz,
                auto: false,
            };
            SignalingModel::success_response(
                &payload.request_id,
                SignalingType::ChangeDisplaySettings,
                None,
                connection_id,
                Some(&response),
            )
        }
        VirtualDisplayModeOutcome::Failed(reason) => SignalingModel::error(
            &payload.request_id,
            SignalingType::ChangeDisplaySettings,
            None,
            connection_id,
            DeskErrorCode::INVALID_STATE,
            &reason,
        ),
    }
}

/// Helper for batch 2 of the typed-IPC migration: rebuild the
/// outbound `Manager*` response `SignalingModel` (with the
/// `request_id` echoed for correlation) and broadcast it to the
/// browser via `outbound_tx`. Build / serialise failures are
/// non-fatal — log + drop, no panic on the bus.
///
/// `from_connection_id` is left `None` (the daemon is the responder
/// here, not a peer browser); `to_connection_id` is `Option<String>`
/// because manager-plane / `ListTerminal` requests can be HTTP-API-
/// triggered without an originating browser PC — in that case the
/// signal/manager server matches the response by `request_id` alone
/// (see `signal-facade::model::connection::request_callback_map`).
fn send_manager_response<T>(
    outbound_tx: &broadcast::Sender<String>,
    type_name: &'static str,
    request_id: &str,
    connection_id: &Option<String>,
    signaling_type: SignalingType,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    match SignalingModel::success_response(
        request_id,
        signaling_type,
        None,
        connection_id.clone(),
        data,
    ) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => warn!(
                "[SignalingProxy] Failed to serialise {type_name} response for {connection_id:?}: \
                 {e} (request_id={request_id})"
            ),
        },
        Err(e) => warn!(
            "[SignalingProxy] Failed to build {type_name} response model for {connection_id:?}: \
             {e} (request_id={request_id})"
        ),
    }
}

/// Helper for batch 3 of the typed-IPC migration: build a
/// server-initiated `new_request` `SignalingModel` (no `request_id`
/// correlation — the daemon mints a fresh one inside `new_request`)
/// for terminal-plane notifications (`ReplyFromTerminal`,
/// `TerminalClosed`) and broadcast it to the browser via
/// `outbound_tx`. Build / serialise failures are non-fatal —
/// log + drop, no panic on the bus. Mirrors the shape
/// `service::terminal` used to construct directly when worker still
/// owned the WS path.
fn send_terminal_notification<T>(
    outbound_tx: &broadcast::Sender<String>,
    type_name: &'static str,
    connection_id: &str,
    signaling_type: SignalingType,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    match SignalingModel::new_request(signaling_type, Some(connection_id.to_string()), data) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => warn!(
                "[SignalingProxy] Failed to serialise {type_name} notification for \
                 {connection_id}: {e}"
            ),
        },
        Err(e) => warn!(
            "[SignalingProxy] Failed to build {type_name} notification model for \
             {connection_id}: {e}"
        ),
    }
}

/// Outcome of handling one inbound signaling frame on a proxy link.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InboundOutcome {
    /// Keep the connection running.
    Continue,
    /// A fatal registration rejection arrived on the manager link: stop the
    /// connection and its auto-reconnect loop. Carries the error code and message.
    FatalReject { error_code: i32, message: String },
}

/// Outcome of one `maintain_proxy_connection` lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProxyConnectionOutcome {
    /// The connection ended normally (closed / errored); the caller may reconnect.
    Closed,
    /// The manager fatally rejected registration; the caller must NOT auto-reconnect
    /// until a manual retry is requested.
    FatalReject { error_code: i32, message: String },
}

/// Whether an inbound `Error(-1)` frame is a fatal registration rejection the host
/// must stop reconnecting on. The fatal set is exactly the device-quota codes
/// (`DEVICE_QUOTA_EXCEEDED` / `DEVICE_CLIENT_ID_REQUIRED`); any other error code is
/// transient and handled normally.
fn fatal_registration_reject(model: &SignalingModel) -> Option<(i32, String)> {
    if model.signaling_type != SignalingType::Error {
        return None;
    }
    let state = model.response_state.as_ref()?;
    let code = state.error_code;
    if code == DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code()
        || code == DeskErrorCode::DEVICE_CLIENT_ID_REQUIRED.code()
    {
        let message = state
            .message
            .clone()
            .unwrap_or_else(|| "device registration rejected".to_string());
        Some((code, message))
    } else {
        None
    }
}

/// Outbound Support-isolation filter for one upstream's egress.
///
/// A frame destined to a restricted support-origin connection may leave ONLY on
/// the dedicated support upstream; every other frame — including all
/// `to_connection_id = None` daemon→central frames (`AiAuditEvent`,
/// `CollectResponse`, `RemoteToolResponse`, `EdgeExecResult`, `Manager*Response`,
/// …) — may leave only on the trusted/relay upstreams and must never reach the
/// support link. This is the core secrecy boundary: the support upstream carries
/// a connection handle to a semi-trusted supporter, so leaking a manager response
/// or an exec result onto it would expose privileged state.
///
/// Only `to_connection_id` is parsed (a lightweight projection); a malformed
/// frame or one without a target is treated as non-support, which is fail-closed
/// for the support link (it never receives such a frame).
async fn egress_permitted(
    msg: &str,
    is_support_upstream: bool,
    restricted_connections: &tokio::sync::RwLock<std::collections::HashSet<String>>,
) -> bool {
    #[derive(serde::Deserialize)]
    struct EgressRoute {
        to_connection_id: Option<String>,
    }
    let to = serde_json::from_str::<EgressRoute>(msg)
        .ok()
        .and_then(|r| r.to_connection_id);
    let is_support_dest = match to.as_deref() {
        Some(c) => restricted_connections.read().await.contains(c),
        None => false,
    };
    if is_support_upstream {
        is_support_dest
    } else {
        !is_support_dest
    }
}

async fn maintain_proxy_connection(
    settings: web::Data<SharedSettings>,
    router_ctx: &RouterContext,
    signaling_url: String,
    auth_token: String,
    mut outbound_rx: broadcast::Receiver<String>,
    source: InboundSignalingSource,
    // True only on the manager link (`remote_mgr_handle`); a fatal device-quota
    // `Error` then stops auto-reconnect. Never set for the local loopback (which is
    // also `TrustedCentral` in Default mode) or a bare remote-signaling relay.
    fatal_quota_reject_enabled: bool,
    // Records the fatal rejection for the host UI when the link is the manager link.
    manager_link_state: Option<Arc<ManagerLinkState>>,
) -> Result<ProxyConnectionOutcome, Box<dyn std::error::Error>> {
    let display_name = {
        let s = settings.read().await;
        s.desk.display_name.clone()
    };
    let display_name = display_name.or_else(sysinfo::System::host_name);

    let client_id = {
        let s = settings.read().await;
        s.system.get_client_id().map_err(|e| format!("{e}"))?
    };

    // The three trusted/relay links register as a normal `Server` connection;
    // the dedicated support upstream registers as `Support` so the central brain
    // resolves it to a restricted, temp-code session (no device / presence
    // registration) rather than the host's main connection.
    let remote_desk_type = if source.is_restricted() {
        RemoteDeskTypeEnum::Support
    } else {
        RemoteDeskTypeEnum::Server
    };

    let mut version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        crate::version::SERVER_BUILD_NUMBER,
        crate::version::SERVER_COMMIT_HASH.to_string(),
        remote_desk_type,
        display_name,
        Some(client_id),
    );
    version_info.token = Some(auth_token);
    if !crate::version::SERVER_REPOSITORY_URL.is_empty() {
        version_info.repository_url = Some(crate::version::SERVER_REPOSITORY_URL.to_string());
    }
    let version_query = serde_urlencoded::to_string(&version_info)
        .map_err(|e| format!("Failed to encode version info: {e}"))?;

    let mut root_store = RootCertStore::empty();
    for cert in load_native_certs().expect("could not load platform certs") {
        root_store.add(cert).unwrap();
    }
    let client = Client::builder()
        .connector(
            Connector::new()
                .timeout(Duration::from_secs(10))
                .rustls_0_23(Arc::new(
                    ClientConfig::builder()
                        .with_root_certificates(Arc::new(root_store))
                        .with_no_client_auth(),
                )),
        )
        .finish();

    let url_clean = signaling_url.trim().trim_matches(|c: char| c.is_control());
    let connect_url = if url_clean.contains('?') {
        format!("{url_clean}&{version_query}")
    } else {
        format!("{url_clean}?{version_query}")
    };

    info!("[Proxy] Connecting to: {signaling_url}");
    debug!("[Proxy] Full URL: {connect_url}");

    let (_resp, framed) = client
        .ws(&connect_url)
        .connect()
        .await
        .map_err(|e| format!("WebSocket connect failed: {e:?}"))?;

    info!("[Proxy] Connected to {signaling_url}");

    // A successful (re)connection clears any prior fatal rejection so the host UI
    // stops showing the blocked state once registration goes through.
    if let Some(state) = manager_link_state.as_ref() {
        state.clear().await;
    }

    let (mut sink, mut stream) = framed.split();

    // Outbound Support-isolation state. `is_support_upstream` is true only for the
    // dedicated support link; `restricted_connections` is the shared registry
    // projection of which browser connections are restricted support sessions.
    // The single `outbound_tx` broadcast still fans every frame to all upstreams;
    // `egress_permitted` then keeps a support-destined frame on the support link
    // only and every other frame (including all `to_connection_id = None`
    // daemon→central frames) off it. When no support upstream is live the set stays
    // empty, so the trusted links forward everything exactly as before.
    let is_support_upstream = source.is_restricted();
    let restricted_connections = router_ctx.pc_registry.restricted_connections_handle();

    loop {
        tokio::select! {
            ws_msg = stream.next() => {
                match ws_msg {
                    Some(Ok(frame)) => {
                        match frame {
                            awc::ws::Frame::Text(text) => {
                                let text_str = match std::str::from_utf8(&text) {
                                    Ok(s) => s.to_string(),
                                    Err(e) => {
                                        error!("[Proxy] Invalid UTF-8 from WS: {e}");
                                        continue;
                                    }
                                };
                                match handle_inbound_signaling_text(
                                    text_str,
                                    router_ctx,
                                    source,
                                    fatal_quota_reject_enabled,
                                )
                                .await
                                {
                                    InboundOutcome::Continue => {}
                                    InboundOutcome::FatalReject { error_code, message } => {
                                        warn!(
                                            "[Proxy] Manager rejected registration (code {error_code}): \
                                             {message}; stopping auto-reconnect until manual retry"
                                        );
                                        if let Some(state) = manager_link_state.as_ref() {
                                            state.record_fatal(error_code, message.clone()).await;
                                        }
                                        let _ = sink.send(awc::ws::Message::Close(None)).await;
                                        return Ok(ProxyConnectionOutcome::FatalReject {
                                            error_code,
                                            message,
                                        });
                                    }
                                }
                            }
                            awc::ws::Frame::Ping(data) => {
                                let _ = sink.send(awc::ws::Message::Pong(data)).await;
                            }
                            awc::ws::Frame::Close(reason) => {
                                warn!("[Proxy] WS close frame: {reason:?}");
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => {
                        error!("[Proxy] WS error: {e}");
                        break;
                    }
                    None => {
                        warn!("[Proxy] WS stream closed");
                        break;
                    }
                }
            }

            outbound = outbound_rx.recv() => {
                match outbound {
                    Ok(msg) => {
                        if !egress_permitted(&msg, is_support_upstream, &restricted_connections).await {
                            // Dropped by the Support-isolation filter: either a
                            // support-destined frame on a trusted upstream, or a
                            // non-support frame (including every None-target
                            // daemon→central frame) on the support upstream.
                            continue;
                        }
                        if let Err(e) = sink.send(awc::ws::Message::Text(msg.into())).await {
                            error!("[Proxy] WS send error: {e}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[Proxy] Outbound channel lagged by {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("[Proxy] Outbound broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }

    info!("[Proxy] Connection to {signaling_url} ended");
    Ok(ProxyConnectionOutcome::Closed)
}

/// Which upstream link an inbound signaling frame arrived on. This is the
/// daemon-side notion of "where did this frame come from", distinct from the
/// central-side `AuthContext` ("how did this connection authenticate"). Only the
/// `TrustedCentral` link is a trusted policy-decision upstream that may inject an
/// [`AuthorizedControlPayload`]; the local and remote-signaling links carry bare
/// payloads gated by local config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundSignalingSource {
    /// The in-process / loopback signaling link (single-machine and the
    /// service-daemon's own API). No fleet PDP.
    Local,
    /// A bare remote signaling relay link (WebRTC signaling only). This link is
    /// NOT trusted as a central brain and must never be promoted to inject
    /// authorization — doing so would let any relay signaling server gain
    /// central-level injection rights.
    RemoteSignaling,
    /// The trusted central-brain link — the only authorization-injecting
    /// upstream. Covers both the enterprise manager and an OSS signal acting as
    /// the central brain; the edge classifies a link as trusted-central only
    /// from the connection's authentication result (the central credential
    /// slot), never from a bare relay.
    TrustedCentral,
    /// A dedicated temporary-support upstream (see [`RemoteDeskTypeEnum::Support`]).
    /// The host opens it on demand to serve one semi-trusted supporter and holds
    /// every session that arrives on it fail-closed: inbound frames are restricted
    /// to the establishment / control-plane allowlist, and outbound frames destined
    /// to a support-origin connection egress ONLY here (never any privileged
    /// daemon→central frame). It is never a policy-decision upstream — it cannot
    /// inject authorization and is treated like a bare relay for every trust check.
    Support,
}

impl InboundSignalingSource {
    /// Whether frames arriving on this link belong to a restricted
    /// temporary-support session. Only the dedicated [`Support`] upstream is
    /// restricted; the loopback / relay / trusted-central links are not. Drives
    /// both the router's inbound fail-closed allowlist and the outbound
    /// Support-isolation filter.
    ///
    /// [`Support`]: InboundSignalingSource::Support
    pub fn is_restricted(self) -> bool {
        matches!(self, InboundSignalingSource::Support)
    }
}

/// Classify the daemon's local loopback signaling link by startup mode.
///
/// In portable `Default` mode the loopback reaches the **embedded signal acting
/// as the central brain** — same process, single machine, authenticated by the
/// local token — so the link is trusted-central: that signal pushes evidence
/// collection (`CollectRequest`) and wrapped AI frames over it, which the edge
/// must accept. In `ServiceDaemon` mode the loopback is the daemon's own internal
/// API, not a central brain (the real central is remote, reached through the
/// central credential slot), so it stays a plain `Local` link with no PDP.
fn local_loopback_source(mode: &StartupMode) -> InboundSignalingSource {
    match mode {
        StartupMode::Default => InboundSignalingSource::TrustedCentral,
        _ => InboundSignalingSource::Local,
    }
}

/// Outcome of the source-gated authorization check for one inbound frame.
enum AuthzGateOutcome {
    /// Forward this (possibly unwrapped) model to the router, carrying the
    /// validated authorization block when the frame arrived wrapped from the
    /// manager link.
    Pass(SignalingModel, Option<AuthorizationBlock>),
    /// Drop the frame; the string explains why (for logging).
    Drop(String),
}

/// True for the control-end AI frames that may carry an authorization wrapper.
fn is_ai_control_frame(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::AgentRequest | SignalingType::Diagnose | SignalingType::ConfirmExec
    )
}

/// Source-gate an inbound AI frame against the authorization wrapper rules
/// (security model D11/D20):
///
/// - Non-AI frames pass through untouched.
/// - A wrapper (`AuthorizedControlPayload`) is only legitimate from the
///   `TrustedCentral` link; on any other source it is dropped (a non-central
///   upstream must never inject authorization).
/// - On the `TrustedCentral` link a wrapper is validated against the frame
///   (`request_id`), this daemon's audience, and expiry; on success the inner
///   payload is unwrapped and forwarded. The carried decision is consumed by the
///   enforcement step (the policy-injection stage); here the mechanism only
///   validates and unwraps.
/// - A bare payload passes through to local-config gating.
fn gate_authz_frame(
    model: SignalingModel,
    source: InboundSignalingSource,
    expected_audience: &str,
    now_rfc3339: &str,
) -> AuthzGateOutcome {
    if !is_ai_control_frame(model.signaling_type) {
        return AuthzGateOutcome::Pass(model, None);
    }

    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        // The trusted-central link always wraps AI control frames (its PDP
        // authorizes and wraps every one), so a bare AI control frame from the
        // trusted-central source is illegitimate — forged or a relay fault — and
        // is dropped rather than falling through to the local default scope,
        // which would bypass the central policy. Local / remote-signaling links
        // have no PDP and pass bare frames through to local-config gating.
        if source == InboundSignalingSource::TrustedCentral {
            return AuthzGateOutcome::Drop(
                "bare AI control frame from trusted-central source (authorization wrapper required)"
                    .to_string(),
            );
        }
        return AuthzGateOutcome::Pass(model, None);
    }

    if source != InboundSignalingSource::TrustedCentral {
        return AuthzGateOutcome::Drop(format!(
            "AI frame carried an authz wrapper from non-central source {source:?}"
        ));
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return AuthzGateOutcome::Drop("wrapper frame had no data".to_string()),
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => return AuthzGateOutcome::Drop(format!("malformed authz wrapper: {e}")),
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return AuthzGateOutcome::Drop(format!("authz wrapper rejected: {e:?}"));
    }

    // Validated: forward the inner payload as a bare frame plus the validated
    // authorization block, which the router threads into the AI handlers
    // (scope / max_risk / orchestrator grants) to enforce the central decision.
    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    AuthzGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// True for the trusted-central plumbing frames that drive an evidence
/// collection or a remote read-tool call. Unlike the AI control frames these may
/// arrive either bare or wrapped, so they get the optional-wrapper gate rather
/// than [`gate_authz_frame`]'s require-wrapper rule.
fn is_central_plumbing_frame(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::CollectRequest | SignalingType::RemoteToolRequest
    )
}

/// Optional-wrapper gate for trusted-central plumbing (`CollectRequest` /
/// `RemoteToolRequest`). The caller has already confirmed the trusted-central
/// source. These frames may arrive either:
///
/// - **bare** — the legacy / enterprise-manager path emits the raw payload; the
///   trusted-central link authentication is the trust anchor, so a bare frame
///   passes through to the router unchanged; or
/// - **wrapped** in an [`AuthorizedControlPayload`] — an OSS signal central brain
///   stamps and wraps every frame. A wrapper is validated against the frame's
///   `request_id`, this daemon's audience, and expiry (replay / misroute
///   defense-in-depth), then unwrapped to its inner payload for the router.
///
/// A wrapper that fails validation is dropped (no denied-result protocol exists
/// for these read-only frames; the central reaper times the pending entry out).
fn gate_optional_central_wrapper(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> AuthzGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        return AuthzGateOutcome::Pass(model, None);
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return AuthzGateOutcome::Drop("wrapper frame had no data".to_string()),
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => return AuthzGateOutcome::Drop(format!("malformed authz wrapper: {e}")),
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return AuthzGateOutcome::Drop(format!("authz wrapper rejected: {e:?}"));
    }

    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    AuthzGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// Outcome of the dedicated `EdgeExecRequest` authorization gate. Unlike the
/// generic [`gate_authz_frame`] (which drops a frame whose wrapper fails to
/// validate), a fleet request from the trusted-central link that fails
/// validation is answered with a synthesized denied result so the central
/// pending entry resolves rather than hanging. Only a frame that cannot be
/// correlated at all (no `request_id`) is dropped outright.
#[derive(Debug)]
enum FleetExecGateOutcome {
    /// Validated: the unwrapped frame (data = inner `ExecPlan`) plus the
    /// validated authorization block to thread into the router handler.
    Pass(SignalingModel, AuthorizationBlock),
    /// Trusted source but the request is unauthorized / malformed; answer the
    /// central brain with a `RejectedBeforeDispatch` carrying `reason`.
    Denied { request_id: String, reason: String },
    /// Uncorrelatable garbage; drop silently (no result can be attributed).
    Drop(String),
}

/// Dedicated authorization gate for `EdgeExecRequest` (central → daemon). The
/// caller has already confirmed the trusted-central source. Validates the
/// `AuthorizedControlPayload<ExecPlan>` wrapper; on success unwraps the inner
/// plan and returns the validated authorization block.
fn gate_fleet_exec_frame(
    model: SignalingModel,
    expected_audience: &str,
    now_rfc3339: &str,
) -> FleetExecGateOutcome {
    let request_id = model.request_id.clone();
    if request_id.is_empty() {
        return FleetExecGateOutcome::Drop(
            "EdgeExecRequest without request_id (cannot correlate a result)".to_string(),
        );
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => {
            return FleetExecGateOutcome::Denied {
                request_id,
                reason: "pep_rejected:authz:missing_payload".to_string(),
            };
        }
    };
    let wrapper: AuthorizedControlPayload<serde_json::Value> = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return FleetExecGateOutcome::Denied {
                request_id,
                reason: format!("pep_rejected:authz:malformed_wrapper:{e}"),
            };
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&request_id, expected_audience, now_rfc3339)
    {
        return FleetExecGateOutcome::Denied {
            request_id,
            reason: format!("pep_rejected:authz:{e:?}"),
        };
    }

    let unwrapped = SignalingModel::new(
        &request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    FleetExecGateOutcome::Pass(unwrapped, wrapper.authz)
}

/// Inbound-text dispatcher pulled out of `maintain_proxy_connection`
/// so the parse / route sequence is reusable for tests and the
/// per-frame logic stays out of the WS select loop.
///
/// Parses the inbound text once, applies source-gated authorization wrapper
/// handling ([`gate_authz_frame`]), and hands the model to
/// [`signaling_router::route`]. The router exhaustively dispatches:
/// PC / SDP / ICE types are handled inline, worker-bound types ride
/// dedicated `ServiceToWorker::*` typed IPC variants, and
/// daemon-emitted notifications are trace-logged + dropped.
async fn handle_inbound_signaling_text(
    text_str: String,
    router_ctx: &RouterContext,
    source: InboundSignalingSource,
    fatal_quota_reject_enabled: bool,
) -> InboundOutcome {
    let parsed = match serde_json::from_str::<SignalingModel>(&text_str) {
        Ok(m) => m,
        Err(e) => {
            warn!("[Proxy] Dropping malformed signaling text: {e}");
            return InboundOutcome::Continue;
        }
    };

    // Manager-link registration rejection: an `Error(-1)` carrying a device-quota
    // fatal code. Intercept before the router (which swallows `Error` frames) so the
    // caller can stop the auto-reconnect storm. Only honoured on the manager link;
    // the same code on any other link is treated as transient (just dropped below).
    if fatal_quota_reject_enabled
        && let Some((error_code, message)) = fatal_registration_reject(&parsed)
    {
        return InboundOutcome::FatalReject {
            error_code,
            message,
        };
    }

    // Source gate: `CommandTemplateSync`, `CommandBlocklistSync`,
    // `CollectRequest`, `EdgeExecRequest`, and `RemoteToolRequest` are trusted
    // central→daemon plumbing. Accept them only from the trusted-central link; a
    // Local / remote-signaling origin (no trusted PDP) must never inject operator
    // templates, weaken the command blocklist, drive an evidence collection,
    // dispatch a sealed execution plan, or drive a remote read. For the blocklist
    // this is critical: a forged sync with a higher revision and a thinned rule
    // set would otherwise wipe the daemon's floor and fail-open.
    if matches!(
        parsed.signaling_type,
        SignalingType::CommandTemplateSync
            | SignalingType::CommandBlocklistSync
            | SignalingType::CollectRequest
            | SignalingType::EdgeExecRequest
            | SignalingType::RemoteToolRequest
    ) && source != InboundSignalingSource::TrustedCentral
    {
        warn!(
            "[Proxy] Dropping {:?} from non-central source {source:?}",
            parsed.signaling_type
        );
        return InboundOutcome::Continue;
    }

    let expected_audience = {
        let s = router_ctx.settings.read().await;
        s.system.get_client_id().unwrap_or_default()
    };
    let now = chrono::Utc::now().to_rfc3339();

    // `EdgeExecRequest` uses a dedicated authorization gate: a trusted-but-
    // invalid request is answered with a synthesized denied result (so the
    // central pending entry resolves) rather than silently dropped.
    if parsed.signaling_type == SignalingType::EdgeExecRequest {
        match gate_fleet_exec_frame(parsed, &expected_audience, &now) {
            FleetExecGateOutcome::Pass(unwrapped, block) => {
                let effective_ctx = RouterContext {
                    inbound_authz: Some(block),
                    ..router_ctx.clone()
                };
                if let Err(e) = signaling_router::route(&unwrapped, &effective_ctx).await {
                    warn!("[Proxy] router handler failed for EdgeExecRequest: {e}");
                }
            }
            FleetExecGateOutcome::Denied { request_id, reason } => {
                warn!("[Proxy] EdgeExecRequest denied ({reason}); replying denied result");
                signaling_router::send_edge_exec_result(
                    &router_ctx.outbound_tx,
                    &request_id,
                    desk_agent_protocol::edge_exec::EdgeExecDisposition::RejectedBeforeDispatch {
                        reason,
                    },
                );
            }
            FleetExecGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping EdgeExecRequest: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    // `CollectRequest` / `RemoteToolRequest` carry an optional authorization
    // wrapper: bare from the enterprise-manager path, wrapped from an OSS signal
    // central brain. Validate-and-unwrap when wrapped, pass through when bare.
    // The router handlers parse the inner payload either way, so unwrapping here
    // keeps them untouched.
    if is_central_plumbing_frame(parsed.signaling_type) {
        match gate_optional_central_wrapper(parsed, &expected_audience, &now) {
            AuthzGateOutcome::Pass(unwrapped, authz) => {
                let effective_ctx;
                let ctx_ref = match authz {
                    Some(block) => {
                        effective_ctx = RouterContext {
                            inbound_authz: Some(block),
                            ..router_ctx.clone()
                        };
                        &effective_ctx
                    }
                    None => router_ctx,
                };
                if let Err(e) = signaling_router::route(&unwrapped, ctx_ref).await {
                    warn!(
                        "[Proxy] router handler failed for {:?}: {e}",
                        unwrapped.signaling_type
                    );
                }
            }
            AuthzGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping central plumbing frame: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    let (parsed, authz) = match gate_authz_frame(parsed, source, &expected_audience, &now) {
        AuthzGateOutcome::Pass(m, authz) => (m, authz),
        AuthzGateOutcome::Drop(reason) => {
            warn!("[Proxy] Dropping AI frame: {reason}");
            return InboundOutcome::Continue;
        }
    };

    // A validated central authorization — and/or the support-session restriction
    // flag — rides into the handlers via a per-call clone of the router context
    // (cheap: the context is Arc-backed). This keeps `route()` and the AI handler
    // signatures untouched. A support upstream can only ever reach this general
    // path (its frames are dropped by the trusted-central source gate above before
    // the AI / plumbing / fleet-exec branches), so tagging restriction here covers
    // every frame a restricted session can deliver.
    let restricted = source.is_restricted();
    let effective_ctx;
    let ctx_ref = if authz.is_some() || restricted {
        effective_ctx = RouterContext {
            inbound_authz: authz,
            inbound_restricted: restricted,
            ..router_ctx.clone()
        };
        &effective_ctx
    } else {
        router_ctx
    };

    if let Err(e) = signaling_router::route(&parsed, ctx_ref).await {
        warn!(
            "[Proxy] router handler failed for {:?}: {e}; dropping unrouted message",
            parsed.signaling_type,
        );
    }

    InboundOutcome::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pc_manager::PcRegistry;
    use crate::host_control::HostControlHub;
    use crate::model::settings::{Settings, SharedSettings};
    use desk_signal_facade::model::signal::{SignalingModel, SignalingType};

    #[test]
    fn local_loopback_is_trusted_central_only_in_portable_default() {
        // Portable Default mode: the loopback reaches the embedded central-brain
        // signal, so the link is trusted-central (it pushes CollectRequest /
        // wrapped AI frames the edge must accept).
        assert_eq!(
            local_loopback_source(&StartupMode::Default),
            InboundSignalingSource::TrustedCentral
        );
        // ServiceDaemon mode: the loopback is the daemon's own internal API, not a
        // central brain — it stays a plain Local link (the real central is remote
        // through the central credential slot, never the loopback).
        assert_eq!(
            local_loopback_source(&StartupMode::ServiceDaemon),
            InboundSignalingSource::Local
        );
    }

    #[test]
    fn only_the_support_upstream_is_restricted() {
        assert!(InboundSignalingSource::Support.is_restricted());
        assert!(!InboundSignalingSource::Local.is_restricted());
        assert!(!InboundSignalingSource::RemoteSignaling.is_restricted());
        assert!(!InboundSignalingSource::TrustedCentral.is_restricted());
    }

    fn frame_to(to: Option<&str>) -> String {
        let model = SignalingModel::new(
            "req-1",
            SignalingType::Init,
            None,
            to.map(|s| s.to_string()),
            None,
            None,
        );
        serde_json::to_string(&model).expect("serialize frame")
    }

    /// The outbound Support-isolation filter keeps a support-destined frame on the
    /// support upstream only, and every other frame (normal target, unknown
    /// target, or no target) off it and on the trusted upstreams. This is the
    /// secrecy boundary that a fourth support upstream depends on.
    #[tokio::test]
    async fn egress_isolates_support_destined_frames() {
        use std::collections::HashSet;
        let restricted = tokio::sync::RwLock::new(HashSet::from(["conn-support".to_string()]));

        let to_support = frame_to(Some("conn-support"));
        let to_normal = frame_to(Some("conn-owner"));
        let to_unknown = frame_to(Some("conn-ghost"));
        let to_none = frame_to(None);

        // Support upstream: ONLY frames destined to a support-origin connection.
        assert!(egress_permitted(&to_support, true, &restricted).await);
        assert!(!egress_permitted(&to_normal, true, &restricted).await);
        assert!(!egress_permitted(&to_unknown, true, &restricted).await);
        assert!(!egress_permitted(&to_none, true, &restricted).await);

        // Trusted upstream: everything EXCEPT support-destined frames.
        assert!(!egress_permitted(&to_support, false, &restricted).await);
        assert!(egress_permitted(&to_normal, false, &restricted).await);
        assert!(egress_permitted(&to_unknown, false, &restricted).await);
        assert!(egress_permitted(&to_none, false, &restricted).await);
    }

    /// Daemon→central frames carry `to_connection_id = None`. Table-driven guard
    /// that none of them ever egress on the support upstream (they would leak
    /// privileged manager responses / exec results / audit to a semi-trusted
    /// supporter) and all of them still egress on a trusted upstream.
    #[tokio::test]
    async fn egress_keeps_none_target_daemon_frames_off_support_upstream() {
        use std::collections::HashSet;
        let restricted = tokio::sync::RwLock::new(HashSet::from(["conn-support".to_string()]));
        for st in [
            SignalingType::AiAuditEvent,
            SignalingType::CollectResponse,
            SignalingType::RemoteToolResponse,
            SignalingType::EdgeExecResult,
            SignalingType::ManagerSystemInfo,
            SignalingType::ManagerQuerySettings,
        ] {
            let frame = serde_json::to_string(&SignalingModel::new(
                "req-1", st, None, None, None, None,
            ))
            .expect("serialize frame");
            assert!(
                !egress_permitted(&frame, true, &restricted).await,
                "{st:?} must not egress on the support upstream"
            );
            assert!(
                egress_permitted(&frame, false, &restricted).await,
                "{st:?} must egress on a trusted upstream"
            );
        }
    }

    /// A malformed outbound frame is treated as non-support: dropped on the
    /// support upstream (fail-closed — it never receives an unparseable frame)
    /// and forwarded on the trusted upstreams (parity with the prior flood).
    #[tokio::test]
    async fn egress_treats_malformed_frame_as_non_support() {
        use std::collections::HashSet;
        let restricted = tokio::sync::RwLock::new(HashSet::from(["conn-support".to_string()]));
        let junk = "not json";
        assert!(!egress_permitted(junk, true, &restricted).await);
        assert!(egress_permitted(junk, false, &restricted).await);
    }

    fn make_router_ctx() -> (RouterContext, broadcast::Sender<String>) {
        let (outbound_tx, _) = broadcast::channel::<String>(16);
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _rx) = WorkerManager::new(settings.clone(), pc_registry.clone());
        let ctx = RouterContext {
            pc_registry,
            outbound_tx: outbound_tx.clone(),
            settings,
            host_control_hub: Arc::new(HostControlHub::new_local()),
            worker_mgr,
            virtual_display: None,
            diagnose_orchestrator: None,
            remote_read: None,
            exec_supported: false,
            exec_approvals: Arc::new(crate::daemon::exec_approval::PendingApprovalStore::new()),
            agentic_exec: Arc::new(crate::daemon::agentic_exec::AgenticExecCoordinator::new()),
            session_approvals: Arc::new(
                crate::daemon::session_approval::SessionApprovalStore::new(),
            ),
            command_templates: Arc::new(
                crate::daemon::command_templates::CommandTemplateCache::new(),
            ),
            command_blocklist: Arc::new(
                crate::daemon::command_blocklist::CommandBlocklistCache::new(),
            ),
            audit: Arc::new(LogAuditSink),
            diagnose_tasks: Default::default(),
            inbound_authz: None,
            inbound_restricted: false,
            edge_exec_pending: Default::default(),
            support_link_state: Arc::new(
                crate::daemon::support_link_state::SupportLinkState::new(),
            ),
        };
        (ctx, outbound_tx)
    }

    /// Worker-bound signaling without `from_connection_id` is dropped
    /// inside `route()` (the per-type helper logs and returns Ok). The
    /// dispatcher therefore returns cleanly with no IPC send. Pinning
    /// this guards against a regression where `route()` would surface
    /// a missing-id case as a `RouterError` and noisily warn-spam.
    #[tokio::test]
    async fn drops_worker_bound_message_without_from_connection_id() {
        let (router_ctx, _out_tx) = make_router_ctx();

        let model = SignalingModel::new(
            "req-1",
            SignalingType::EnablePrivateScreen,
            None,
            None,
            None,
            None,
        );
        let text = serde_json::to_string(&model).unwrap();
        handle_inbound_signaling_text(text, &router_ctx, InboundSignalingSource::Local, false)
            .await;
    }

    /// Malformed JSON arriving on the WS is dropped with a warning
    /// rather than crashing the proxy loop.
    #[tokio::test]
    async fn drops_malformed_json() {
        let (router_ctx, _out_tx) = make_router_ctx();
        handle_inbound_signaling_text(
            "{ this is not valid json".to_string(),
            &router_ctx,
            InboundSignalingSource::Local,
            false,
        )
        .await;
    }

    fn error_frame(code: i32, msg: &str) -> String {
        let model = SignalingModel::error(
            "manager-handshake",
            SignalingType::Error,
            None,
            Some("conn-1".to_string()),
            DeskErrorCode::new(code),
            msg,
        )
        .unwrap();
        serde_json::to_string(&model).unwrap()
    }

    /// `fatal_registration_reject` recognises exactly the device-quota fatal codes
    /// on an `Error` frame and nothing else.
    #[test]
    fn fatal_registration_reject_matches_only_quota_codes() {
        let quota = serde_json::from_str::<SignalingModel>(&error_frame(
            DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code(),
            "full",
        ))
        .unwrap();
        assert_eq!(
            fatal_registration_reject(&quota),
            Some((
                DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code(),
                "full".to_string()
            ))
        );
        let missing = serde_json::from_str::<SignalingModel>(&error_frame(
            DeskErrorCode::DEVICE_CLIENT_ID_REQUIRED.code(),
            "no id",
        ))
        .unwrap();
        assert!(fatal_registration_reject(&missing).is_some());

        // A different error code is not fatal.
        let other = serde_json::from_str::<SignalingModel>(&error_frame(
            DeskErrorCode::PERMISSION_ERROR.code(),
            "denied",
        ))
        .unwrap();
        assert_eq!(fatal_registration_reject(&other), None);

        // A non-Error frame is never fatal.
        let normal = SignalingModel::new("r", SignalingType::RequestRemote, None, None, None, None);
        assert_eq!(fatal_registration_reject(&normal), None);
    }

    /// On the manager link (flag enabled) a device-quota `Error` frame yields a
    /// `FatalReject` outcome; with the flag disabled (loopback / relay) the same
    /// frame is treated as transient and the loop continues.
    #[tokio::test]
    async fn quota_error_is_fatal_only_on_manager_link() {
        let (router_ctx, _out_tx) = make_router_ctx();
        let text = error_frame(DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code(), "full");

        let enabled = handle_inbound_signaling_text(
            text.clone(),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            true,
        )
        .await;
        assert_eq!(
            enabled,
            InboundOutcome::FatalReject {
                error_code: DeskErrorCode::DEVICE_QUOTA_EXCEEDED.code(),
                message: "full".to_string(),
            }
        );

        // Same frame, flag disabled (e.g. Default-mode loopback which is also
        // TrustedCentral): not fatal.
        let disabled = handle_inbound_signaling_text(
            text,
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(disabled, InboundOutcome::Continue);
    }

    /// Daemon-owned RequestRemote without `from_connection_id` does
    /// not crash the dispatcher — the router's `handle_request_remote`
    /// returns the per-handler error which we log and return.
    #[tokio::test]
    async fn handles_router_error_without_panic() {
        let (router_ctx, _out_tx) = make_router_ctx();

        let model = SignalingModel::new(
            "req-2",
            SignalingType::RequestRemote,
            None, // missing from_connection_id triggers handler error
            None,
            None,
            None,
        );
        let text = serde_json::to_string(&model).unwrap();
        handle_inbound_signaling_text(text, &router_ctx, InboundSignalingSource::Local, false)
            .await;
    }

    /// Worker-bound signaling with `from_connection_id` reaches the
    /// typed `send_to_worker` path. Without an active worker the call
    /// errors inside `route()` (logged), but the dispatcher must still
    /// return cleanly. The successful-forward case is covered by
    /// per-variant round-trip tests in `desk-ipc-protocol`.
    #[tokio::test]
    async fn worker_owned_with_from_connection_id_does_not_panic() {
        let (router_ctx, _out_tx) = make_router_ctx();

        let model = SignalingModel::new(
            "req-3",
            SignalingType::EnablePrivateScreen,
            Some("conn-x".to_string()),
            None,
            None,
            None,
        );
        let text = serde_json::to_string(&model).unwrap();
        handle_inbound_signaling_text(text, &router_ctx, InboundSignalingSource::Local, false)
            .await;
    }

    fn command_template_sync_text() -> String {
        use desk_agent_protocol::command_template::{
            COMMAND_TEMPLATE_SYNC_VERSION, CommandTemplateSyncPayload, SyncedCommandTemplate,
        };
        use desk_agent_protocol::exec::ExecEffect;
        let payload = CommandTemplateSyncPayload {
            version: COMMAND_TEMPLATE_SYNC_VERSION,
            templates: vec![SyncedCommandTemplate {
                template_id: "get_disk".into(),
                argv: vec!["Get-Disk".into()],
                effect: ExecEffect::ReadOnly,
            }],
            command_template_revision: Some(1),
        };
        let model = SignalingModel::new(
            "rs",
            SignalingType::CommandTemplateSync,
            None,
            None,
            Some(serde_json::to_value(payload).unwrap()),
            None,
        );
        serde_json::to_string(&model).unwrap()
    }

    /// A `CommandTemplateSync` from a non-central source is dropped by the source
    /// gate (the operator-template cache stays empty); from the trusted-central
    /// link it is applied. This is the forged-sync rejection guarantee.
    #[tokio::test]
    async fn command_template_sync_is_accepted_only_from_trusted_central_source() {
        let (router_ctx, _out_tx) = make_router_ctx();

        // Local source: dropped.
        handle_inbound_signaling_text(
            command_template_sync_text(),
            &router_ctx,
            InboundSignalingSource::Local,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 0);

        // Remote-signaling source: dropped (a bare relay is never trusted-central).
        handle_inbound_signaling_text(
            command_template_sync_text(),
            &router_ctx,
            InboundSignalingSource::RemoteSignaling,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 0);

        // Trusted-central source: applied.
        handle_inbound_signaling_text(
            command_template_sync_text(),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 1);
    }

    /// A current daemon accepts a v1 payload (from an old manager during a rolling
    /// upgrade — no revision, applied as `None`) and ignores a payload whose
    /// version is outside the supported range (a future version reaching this
    /// older daemon), leaving the cache untouched.
    #[tokio::test]
    async fn command_template_sync_accepts_v1_and_ignores_unknown_version() {
        use desk_agent_protocol::command_template::{
            CommandTemplateSyncPayload, SyncedCommandTemplate,
        };
        use desk_agent_protocol::exec::ExecEffect;
        let (router_ctx, _out_tx) = make_router_ctx();

        let make_text = |version: u16, revision: Option<i64>| {
            let payload = CommandTemplateSyncPayload {
                version,
                templates: vec![SyncedCommandTemplate {
                    template_id: "get_disk".into(),
                    argv: vec!["Get-Disk".into()],
                    effect: ExecEffect::ReadOnly,
                }],
                command_template_revision: revision,
            };
            let model = SignalingModel::new(
                "rs",
                SignalingType::CommandTemplateSync,
                None,
                None,
                Some(serde_json::to_value(payload).unwrap()),
                None,
            );
            serde_json::to_string(&model).unwrap()
        };

        // v1 (no revision) from trusted central: applied; cache revision stays None.
        handle_inbound_signaling_text(
            make_text(1, None),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 1);
        assert_eq!(router_ctx.command_templates.revision(), None);

        // An unsupported future version is ignored — the cache keeps the v1 apply.
        handle_inbound_signaling_text(
            make_text(99, Some(5)),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 1);
        assert_eq!(router_ctx.command_templates.revision(), None);
    }

    // ====== Source-gated authorization wrapper ======

    use desk_agent_protocol::authz::{
        AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthzActor, AuthzDevice,
    };
    use desk_agent_protocol::diagnose::DiagnoseRequestData;
    use desk_agent_protocol::{AgentScope, ExecutionMode, RiskLevel};

    fn block(request_id: &str, audience: &str) -> AuthorizationBlock {
        AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            scope: AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            orchestrator_grants: vec!["ai.diagnose".to_string()],
            max_risk: RiskLevel::Low,
            actor: AuthzActor { user_id: Some(1) },
            device: AuthzDevice { device_id: Some(2) },
            request_id: request_id.to_string(),
            session_id: None,
            expires_at: Some("2999-01-01T00:00:00Z".to_string()),
            issuer: "manager".to_string(),
            audience: audience.to_string(),
            signature: None,
        }
    }

    fn wrapped_diagnose_model(request_id: &str, audience: &str) -> SignalingModel {
        let wrapper = AuthorizedControlPayload {
            inner: DiagnoseRequestData {
                question: "why slow?".to_string(),
                ..Default::default()
            },
            authz: block(request_id, audience),
        };
        SignalingModel::new(
            request_id,
            SignalingType::Diagnose,
            Some("browser-conn".to_string()),
            Some("server-conn".to_string()),
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        )
    }

    fn bare_diagnose_model(request_id: &str) -> SignalingModel {
        let inner = DiagnoseRequestData {
            question: "why slow?".to_string(),
            ..Default::default()
        };
        SignalingModel::new(
            request_id,
            SignalingType::Diagnose,
            Some("browser-conn".to_string()),
            Some("server-conn".to_string()),
            Some(serde_json::to_value(&inner).unwrap()),
            None,
        )
    }

    const NOW: &str = "2026-06-14T00:00:00Z";

    #[test]
    fn non_ai_frame_passes_through_any_source() {
        let model = SignalingModel::new(
            "r",
            SignalingType::Offer,
            Some("c".to_string()),
            None,
            None,
            None,
        );
        assert!(matches!(
            gate_authz_frame(model, InboundSignalingSource::TrustedCentral, "dev", NOW),
            AuthzGateOutcome::Pass(_, _)
        ));
    }

    #[test]
    fn bare_ai_frame_passes_through_local() {
        let model = bare_diagnose_model("r1");
        assert!(matches!(
            gate_authz_frame(model, InboundSignalingSource::Local, "dev", NOW),
            AuthzGateOutcome::Pass(_, _)
        ));
    }

    #[test]
    fn bare_ai_frame_from_trusted_central_is_dropped() {
        // The central brain always wraps AI control frames, so a bare one on the
        // trusted-central link is illegitimate and must be dropped rather than
        // falling through to the local default scope (which would bypass central
        // policy).
        let model = bare_diagnose_model("r1");
        assert!(matches!(
            gate_authz_frame(model, InboundSignalingSource::TrustedCentral, "dev", NOW),
            AuthzGateOutcome::Drop(_)
        ));
    }

    #[test]
    fn bare_ai_frame_passes_through_remote_signaling() {
        // Remote-signaling links have no PDP; bare frames still pass to local
        // gating (no regression for non-central relays).
        let model = bare_diagnose_model("r1");
        assert!(matches!(
            gate_authz_frame(model, InboundSignalingSource::RemoteSignaling, "dev", NOW),
            AuthzGateOutcome::Pass(_, _)
        ));
    }

    #[test]
    fn wrapper_from_non_central_source_is_dropped() {
        for source in [
            InboundSignalingSource::Local,
            InboundSignalingSource::RemoteSignaling,
        ] {
            let model = wrapped_diagnose_model("r1", "dev-1");
            assert!(
                matches!(
                    gate_authz_frame(model, source, "dev-1", NOW),
                    AuthzGateOutcome::Drop(_)
                ),
                "wrapper from {source:?} must be dropped"
            );
        }
    }

    // A ConfirmExec carrying the operator-promoted copilot command, wrapped by the
    // central brain exactly as `control_authorizer::build_wrapper_outcome` emits
    // it. Source-gating it proves the terminal copilot exec path is reachable on
    // the same trusted-central links as diagnose exec, and unreachable elsewhere.
    fn wrapped_confirm_exec_model(request_id: &str, audience: &str) -> SignalingModel {
        use desk_agent_protocol::exec::ConfirmExecData;
        use desk_agent_protocol::{AgentOperation, ExecInput, ExecTarget, OperationInput};
        let wrapper = AuthorizedControlPayload {
            inner: ConfirmExecData {
                operation: AgentOperation {
                    risk_hint: None,
                    input: OperationInput::Exec(ExecInput {
                        target: ExecTarget::Shell {
                            shell: "bash".to_string(),
                        },
                        command: "systemctl status nginx".to_string(),
                        cwd: Some("/srv".to_string()),
                        timeout_ms: 0,
                        max_stdout_bytes: 0,
                        max_stderr_bytes: 0,
                    }),
                },
                reason: Some("operator promoted a copilot suggestion".to_string()),
            },
            authz: block(request_id, audience),
        };
        SignalingModel::new(
            request_id,
            SignalingType::ConfirmExec,
            Some("browser-conn".to_string()),
            Some("server-conn".to_string()),
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        )
    }

    #[test]
    fn wrapped_confirm_exec_from_trusted_central_is_unwrapped_to_router() {
        // The end-to-end inbound path for an operator-promoted copilot exec on a
        // trusted-central link: the wrapper validates, is stripped, and the bare
        // ConfirmExec plus its authorization block flow on to the router (which
        // re-classifies the command before any preview).
        let model = wrapped_confirm_exec_model("ce-1", "dev-1");
        match gate_authz_frame(model, InboundSignalingSource::TrustedCentral, "dev-1", NOW) {
            AuthzGateOutcome::Pass(unwrapped, Some(authz)) => {
                assert_eq!(unwrapped.signaling_type, SignalingType::ConfirmExec);
                // Unwrapped: the inner ConfirmExecData is now the frame data.
                let inner = unwrapped
                    .get_data::<desk_agent_protocol::exec::ConfirmExecData>()
                    .expect("inner ConfirmExecData");
                assert_eq!(
                    inner.reason.as_deref(),
                    Some("operator promoted a copilot suggestion")
                );
                assert_eq!(authz.request_id, "ce-1");
                assert_eq!(authz.audience, "dev-1");
            }
            AuthzGateOutcome::Pass(_, None) => {
                panic!("trusted-central wrapper must carry its validated authz block")
            }
            AuthzGateOutcome::Drop(reason) => {
                panic!("trusted-central wrapped ConfirmExec must pass, dropped: {reason}")
            }
        }
    }

    #[test]
    fn wrapped_confirm_exec_from_non_central_source_is_dropped() {
        // The same wrapped ConfirmExec arriving over a bare remote-signaling (or
        // local) upstream is dropped at the source gate — a non-central relay can
        // never inject an authorization wrapper. This is why copilot exec (like
        // diagnose exec) is only reachable on trusted-central links.
        for source in [
            InboundSignalingSource::RemoteSignaling,
            InboundSignalingSource::Local,
        ] {
            let model = wrapped_confirm_exec_model("ce-1", "dev-1");
            assert!(
                matches!(
                    gate_authz_frame(model, source, "dev-1", NOW),
                    AuthzGateOutcome::Drop(_)
                ),
                "wrapped ConfirmExec from {source:?} must be dropped"
            );
        }
    }

    // ====== EdgeExecRequest dedicated gate ======

    fn fleet_exec_plan() -> desk_agent_protocol::exec::ExecPlan {
        let template = desk_agent_protocol::command_template::SyncedCommandTemplate {
            template_id: "svc_restart".into(),
            argv: vec!["net".into(), "stop".into(), "spooler".into()],
            effect: desk_agent_protocol::exec::ExecEffect::Mutating,
        };
        let draft = desk_agent_protocol::exec_policy::build_exact_argv_draft(
            &template,
            desk_agent_protocol::exec_policy::ExecLimits::defaults(),
            None,
        );
        desk_agent_protocol::exec::ExecPlan::from_draft(
            desk_agent_protocol::exec::ExecRequestId("a1".into()),
            desk_agent_protocol::exec::ApprovalId("appr-1".into()),
            draft,
        )
    }

    fn wrapped_fleet_exec_model(request_id: &str, audience: &str) -> SignalingModel {
        let wrapper = AuthorizedControlPayload {
            inner: fleet_exec_plan(),
            authz: block(request_id, audience),
        };
        SignalingModel::new(
            request_id,
            SignalingType::EdgeExecRequest,
            None,
            None,
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        )
    }

    #[test]
    fn fleet_gate_passes_a_valid_wrapper_and_unwraps_the_plan() {
        let model = wrapped_fleet_exec_model("a1", "dev-1");
        match gate_fleet_exec_frame(model, "dev-1", NOW) {
            FleetExecGateOutcome::Pass(unwrapped, authz) => {
                // The inner ExecPlan is now the frame data (the wrapper is gone).
                let plan = unwrapped
                    .get_data::<desk_agent_protocol::exec::ExecPlan>()
                    .expect("inner ExecPlan");
                assert_eq!(plan.template_id, "svc_restart");
                assert_eq!(authz.request_id, "a1");
            }
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn fleet_gate_denies_an_audience_mismatch() {
        // Validation fails (wrong audience) → a denied result is synthesized so
        // the central pending entry resolves, rather than a silent drop.
        let model = wrapped_fleet_exec_model("a1", "dev-1");
        match gate_fleet_exec_frame(model, "other-device", NOW) {
            FleetExecGateOutcome::Denied { request_id, reason } => {
                assert_eq!(request_id, "a1");
                assert!(reason.contains("pep_rejected:authz"), "{reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn fleet_gate_denies_a_malformed_wrapper() {
        // A EdgeExecRequest whose body is not an AuthorizedControlPayload is
        // still correlatable (it has a request_id) → denied, not dropped.
        let model = SignalingModel::new(
            "a1",
            SignalingType::EdgeExecRequest,
            None,
            None,
            Some(serde_json::json!({ "not": "a wrapper" })),
            None,
        );
        match gate_fleet_exec_frame(model, "dev-1", NOW) {
            FleetExecGateOutcome::Denied { request_id, reason } => {
                assert_eq!(request_id, "a1");
                assert!(reason.contains("malformed_wrapper"), "{reason}");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn fleet_gate_drops_an_uncorrelatable_request() {
        // No request_id → no result can be attributed → drop.
        let model = wrapped_fleet_exec_model("", "dev-1");
        assert!(matches!(
            gate_fleet_exec_frame(model, "dev-1", NOW),
            FleetExecGateOutcome::Drop(_)
        ));
    }

    #[test]
    fn valid_wrapper_from_trusted_central_is_unwrapped_to_inner() {
        let model = wrapped_diagnose_model("r1", "dev-1");
        match gate_authz_frame(model, InboundSignalingSource::TrustedCentral, "dev-1", NOW) {
            AuthzGateOutcome::Pass(m, _) => {
                // The forwarded model carries the bare inner payload (no authz).
                let obj = m.get_raw_data().as_ref().unwrap().as_object().unwrap();
                assert!(!obj.contains_key("authz"));
                assert!(obj.contains_key("question"));
            }
            AuthzGateOutcome::Drop(reason) => panic!("expected unwrap, dropped: {reason}"),
        }
    }

    #[test]
    fn central_wrapper_with_wrong_audience_is_dropped() {
        let model = wrapped_diagnose_model("r1", "dev-1");
        assert!(matches!(
            gate_authz_frame(
                model,
                InboundSignalingSource::TrustedCentral,
                "other-device",
                NOW
            ),
            AuthzGateOutcome::Drop(_)
        ));
    }

    #[test]
    fn central_wrapper_expired_is_dropped() {
        let mut wrapper = AuthorizedControlPayload {
            inner: DiagnoseRequestData {
                question: "q".to_string(),
                ..Default::default()
            },
            authz: block("r1", "dev-1"),
        };
        wrapper.authz.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        let model = SignalingModel::new(
            "r1",
            SignalingType::Diagnose,
            Some("browser-conn".to_string()),
            Some("server-conn".to_string()),
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        );
        assert!(matches!(
            gate_authz_frame(model, InboundSignalingSource::TrustedCentral, "dev-1", NOW),
            AuthzGateOutcome::Drop(_)
        ));
    }

    #[test]
    fn central_wrapper_request_id_mismatch_is_dropped() {
        // Frame request_id differs from the authz block's request_id.
        let wrapper = AuthorizedControlPayload {
            inner: DiagnoseRequestData {
                question: "q".to_string(),
                ..Default::default()
            },
            authz: block("inner-req", "dev-1"),
        };
        let model = SignalingModel::new(
            "frame-req",
            SignalingType::Diagnose,
            Some("browser-conn".to_string()),
            Some("server-conn".to_string()),
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        );
        assert!(matches!(
            gate_authz_frame(model, InboundSignalingSource::TrustedCentral, "dev-1", NOW),
            AuthzGateOutcome::Drop(_)
        ));
    }

    // ====== Optional-wrapper gate for central plumbing frames ======

    fn collect_request_value(request_id: &str) -> serde_json::Value {
        serde_json::json!({
            "request_id": request_id,
            "request": { "question": "why slow?" },
        })
    }

    fn bare_collect_model(request_id: &str) -> SignalingModel {
        SignalingModel::new(
            request_id,
            SignalingType::CollectRequest,
            None,
            None,
            Some(collect_request_value(request_id)),
            None,
        )
    }

    fn wrapped_collect_model(
        frame_request_id: &str,
        block_request_id: &str,
        audience: &str,
    ) -> SignalingModel {
        let wrapper = serde_json::json!({
            "inner": collect_request_value(frame_request_id),
            "authz": serde_json::to_value(block(block_request_id, audience)).unwrap(),
        });
        SignalingModel::new(
            frame_request_id,
            SignalingType::CollectRequest,
            None,
            None,
            Some(wrapper),
            None,
        )
    }

    #[test]
    fn bare_central_plumbing_frame_passes_through() {
        // The enterprise-manager path emits bare CollectRequest; the trusted-
        // central link authentication is the trust anchor, so it passes through.
        let model = bare_collect_model("r1");
        match gate_optional_central_wrapper(model, "dev-1", NOW) {
            AuthzGateOutcome::Pass(m, authz) => {
                assert!(authz.is_none(), "bare frame carries no authz block");
                let obj = m.get_raw_data().as_ref().unwrap().as_object().unwrap();
                assert!(obj.contains_key("request"));
            }
            AuthzGateOutcome::Drop(reason) => panic!("bare frame must pass, dropped: {reason}"),
        }
    }

    #[test]
    fn wrapped_central_plumbing_frame_is_unwrapped_to_inner() {
        let model = wrapped_collect_model("r1", "r1", "dev-1");
        match gate_optional_central_wrapper(model, "dev-1", NOW) {
            AuthzGateOutcome::Pass(m, authz) => {
                assert!(authz.is_some(), "validated wrapper yields an authz block");
                let obj = m.get_raw_data().as_ref().unwrap().as_object().unwrap();
                // Inner CollectRequest is exposed bare to the router.
                assert!(!obj.contains_key("authz"));
                assert!(obj.contains_key("request"));
            }
            AuthzGateOutcome::Drop(reason) => panic!("expected unwrap, dropped: {reason}"),
        }
    }

    #[test]
    fn wrapped_central_plumbing_frame_wrong_audience_is_dropped() {
        let model = wrapped_collect_model("r1", "r1", "dev-1");
        assert!(matches!(
            gate_optional_central_wrapper(model, "other-device", NOW),
            AuthzGateOutcome::Drop(_)
        ));
    }

    #[test]
    fn wrapped_central_plumbing_frame_request_id_mismatch_is_dropped() {
        // The authz block's request_id differs from the frame's request_id.
        let model = wrapped_collect_model("frame-req", "inner-req", "dev-1");
        assert!(matches!(
            gate_optional_central_wrapper(model, "dev-1", NOW),
            AuthzGateOutcome::Drop(_)
        ));
    }

    // ====== Virtual display response routing ======

    use desk_ipc_protocol::message::{VirtualDisplayModeData, VirtualDisplayModeResponsePayload};

    #[test]
    fn build_virtual_display_response_applied_emits_success_with_mode() {
        let payload = VirtualDisplayModeResponsePayload {
            request_id: "req-42".to_string(),
            connection_id: "conn-7".to_string(),
            outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }),
        };
        let model = build_virtual_display_response(payload, None).expect("build success model");
        assert_eq!(model.request_id, "req-42");
        assert_eq!(
            model.signaling_type as i32,
            SignalingType::ChangeDisplaySettings as i32
        );
        assert_eq!(model.to_connection_id.as_deref(), Some("conn-7"));
        let state = model
            .response_state
            .clone()
            .expect("success response carries state");
        assert_eq!(state.error_code, 0);
        // Serialise to JSON to verify the payload survives.
        let text = serde_json::to_string(&model).unwrap();
        assert!(
            text.contains("1920") && text.contains("1080") && text.contains("60"),
            "expected mode fields in serialised model, got {text}"
        );
    }

    #[test]
    fn build_virtual_display_response_failed_emits_invalid_state_error() {
        let payload = VirtualDisplayModeResponsePayload {
            request_id: "req-43".to_string(),
            connection_id: "conn-8".to_string(),
            outcome: VirtualDisplayModeOutcome::Failed("driver pipe IO failed".to_string()),
        };
        let model = build_virtual_display_response(payload, None).expect("build error model");
        assert_eq!(model.request_id, "req-43");
        assert_eq!(
            model.signaling_type as i32,
            SignalingType::ChangeDisplaySettings as i32
        );
        assert_eq!(model.to_connection_id.as_deref(), Some("conn-8"));
        let state = model.response_state.expect("error response carries state");
        assert_eq!(state.error_code, DeskErrorCode::INVALID_STATE.code());
        assert_eq!(state.message.as_deref(), Some("driver pipe IO failed"));
    }

    /// Applied response must update the supervisor's full mode cache.
    /// The cache feeds two paths:
    ///   * `refresh_hz=0` fallback in the auto-resolution router path
    ///   * the same-resolution idempotent short-circuit in the router
    /// Without this update the daemon would never learn the driver's
    /// actual mode and could neither fill in refresh nor skip redundant
    /// IPC.
    #[test]
    fn build_virtual_display_response_applied_updates_supervisor_cache() {
        use crate::daemon::pc_manager::PcRegistry;
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        use crate::daemon::worker_manager::WorkerManager;
        use crate::model::settings::{Settings, SharedSettings};
        use actix_web::web;
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _rx) = WorkerManager::new(settings, pc_registry);
        let supervisor =
            std::sync::Arc::new(VirtualDisplaySupervisor::new_disabled_for_test(worker_mgr));
        // Pre-condition: cache is empty (no observation yet).
        assert_eq!(supervisor.last_refresh_hz(), 0);
        assert!(supervisor.last_known_mode().is_none());

        let payload = VirtualDisplayModeResponsePayload {
            request_id: "req-cache".to_string(),
            connection_id: "conn-cache".to_string(),
            outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                width: 1920,
                height: 1080,
                refresh_hz: 144,
            }),
        };
        let _model = build_virtual_display_response(payload, Some(&supervisor))
            .expect("build success model");
        assert_eq!(
            supervisor.last_known_mode(),
            Some((1920, 1080, 144)),
            "Applied outcome must update the full supervisor cache (W,H,Hz)",
        );
        assert_eq!(
            supervisor.last_refresh_hz(),
            144,
            "refresh accessor must stay consistent with the new full-mode cache",
        );
    }

    /// Regression: an Applied response with a zero dimension is treated
    /// as a malformed echo and must not poison the cache. Guards against
    /// a future driver bug that reports `width=0` on a transient race.
    #[test]
    fn build_virtual_display_response_applied_zero_dimension_is_ignored() {
        use crate::daemon::pc_manager::PcRegistry;
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        use crate::daemon::worker_manager::WorkerManager;
        use crate::model::settings::{Settings, SharedSettings};
        use actix_web::web;
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _rx) = WorkerManager::new(settings, pc_registry);
        let supervisor =
            std::sync::Arc::new(VirtualDisplaySupervisor::new_disabled_for_test(worker_mgr));
        // Pre-seed a fully-formed mode so the test can detect overwrite.
        supervisor.record_applied_mode(1920, 1080, 60);

        let payload = VirtualDisplayModeResponsePayload {
            request_id: "req-zero".to_string(),
            connection_id: "conn-zero".to_string(),
            outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                width: 0,
                height: 1080,
                refresh_hz: 60,
            }),
        };
        let _model = build_virtual_display_response(payload, Some(&supervisor))
            .expect("build success model");
        assert_eq!(
            supervisor.last_known_mode(),
            Some((1920, 1080, 60)),
            "zero-dimension Applied must be ignored — pre-seeded cache stays",
        );
    }

    /// Failed response must NOT update the cache — the driver did not
    /// apply anything so there is no mode to remember. Guards against a
    /// future refactor that records unconditionally and poisons the
    /// cache with a stale value after a transient driver failure.
    #[test]
    fn build_virtual_display_response_failed_does_not_update_supervisor_cache() {
        use crate::daemon::pc_manager::PcRegistry;
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        use crate::daemon::worker_manager::WorkerManager;
        use crate::model::settings::{Settings, SharedSettings};
        use actix_web::web;
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _rx) = WorkerManager::new(settings, pc_registry);
        let supervisor =
            std::sync::Arc::new(VirtualDisplaySupervisor::new_disabled_for_test(worker_mgr));
        // Pre-seed a fully-formed mode so the test can detect overwrites.
        supervisor.record_applied_mode(1280, 720, 120);

        let payload = VirtualDisplayModeResponsePayload {
            request_id: "req-fail".to_string(),
            connection_id: "conn-fail".to_string(),
            outcome: VirtualDisplayModeOutcome::Failed("driver pipe IO failed".to_string()),
        };
        let _model =
            build_virtual_display_response(payload, Some(&supervisor)).expect("build error model");
        assert_eq!(
            supervisor.last_known_mode(),
            Some((1280, 720, 120)),
            "Failed outcome must not touch supervisor cache",
        );
    }

    /// Non-service-daemon startup paths leave `RouterContext.virtual_display`
    /// at `None`. If a stale or test-induced `VirtualDisplayAttachResult`
    /// arrives, the dispatch helper must drop it without panicking.
    /// Regression guard for the original v2 plan, which did not specify
    /// behaviour for this branch.
    #[tokio::test]
    async fn dispatch_attach_result_drops_message_when_supervisor_disabled() {
        use desk_ipc_protocol::message::{
            VirtualDisplayAttachOutcome, VirtualDisplayAttachResultPayload,
        };
        let payload = VirtualDisplayAttachResultPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        };
        // No panic, no error — just a warn-and-drop side effect.
        dispatch_attach_result(payload, None).await;
    }

    // ====== AI agent response routing ======

    /// The daemon rebuilds an outbound `SignalingType::AgentResponse`
    /// model carrying the `AgentOutcome` verbatim for both the `Ok`
    /// (output) and `Err` (capability-level error) arms. The
    /// transport-level `response_state` is always success — the business
    /// error lives inside the `AgentOutcome::Err` so the control end gets
    /// the full structured `AgentError`.
    #[test]
    fn agent_response_outbound_rebuild_both_arms() {
        use desk_agent_protocol::{
            AgentError, AgentErrorKind, AgentOutcome, ContainerListOutput, OperationOutput,
            ReadContextOutput,
        };

        for (request_id, conn, outcome) in [
            (
                "req-ok",
                Some("conn-1".to_string()),
                AgentOutcome::Ok(OperationOutput::ReadContext(
                    ReadContextOutput::ContainerList(ContainerListOutput {
                        containers: vec![],
                        truncated: false,
                    }),
                )),
            ),
            (
                "req-err",
                None,
                AgentOutcome::Err(AgentError {
                    kind: AgentErrorKind::PermissionDenied,
                    message: "capability not granted".to_string(),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                }),
            ),
        ] {
            let (tx, mut rx) = broadcast::channel::<String>(4);
            send_manager_response(
                &tx,
                "AgentResponse",
                request_id,
                &conn,
                SignalingType::AgentResponse,
                Some(&outcome),
            );
            let text = rx.try_recv().expect("outbound AgentResponse broadcast");
            let model: SignalingModel = serde_json::from_str(&text).unwrap();
            assert_eq!(model.request_id, request_id);
            assert_eq!(
                model.signaling_type as i32,
                SignalingType::AgentResponse as i32
            );
            assert_eq!(model.to_connection_id, conn);
            // Transport state is success regardless of the business result.
            assert_eq!(model.response_state.as_ref().unwrap().error_code, 0);
            // The AgentOutcome round-trips out of signaling_data.
            let decoded = model.get_data::<AgentOutcome>().expect("outcome data");
            assert_eq!(decoded, outcome);
        }
    }
}
