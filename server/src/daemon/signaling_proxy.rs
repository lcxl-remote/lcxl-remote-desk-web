use super::pc_manager::PcRegistry;
use super::signaling_router::{self, RouterContext};
use super::virtual_display::VirtualDisplaySupervisor;
use super::worker_manager::{WorkerManager, WorkerMessageReceiver};
use crate::diagnose::DiagnoseOrchestrator;
use crate::diagnose::collector::AgentContextCollector;
use crate::diagnose::model::ModelBackedDiagnoseModel;
use crate::diagnose::model::openai::OpenAiCompatAdapter;
use crate::diagnose::redaction::RegexRedactor;
use crate::host_control::HostControlHub;
use crate::model::settings::{SharedSettings, StartupMode};
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::audit_sink::LogAuditSink;
use actix_web::web;
use awc::{Client, Connector};
use desk_agent_protocol::audit::AuditSink;
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
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Signaling proxy starting");

    let (outbound_tx, _seed_rx) = broadcast::channel::<String>(128);

    // The diagnose orchestrator runs daemon-side wherever an in-process worker
    // can collect locally (Default / DeskServer). ServiceDaemon leaves it
    // `None`, so `Diagnose` replies feature-unavailable until the cross-process
    // collection path lands. Evidence is gathered through the in-process agent,
    // scrubbed by the regex redactor, then sent to the configured model via the
    // OpenAI-compatible adapter (which degrades to a not-configured diagnosis
    // when no model is set).
    let diagnose_orchestrator = match settings.read().await.args.startup_mode {
        StartupMode::ServiceDaemon => None,
        _ => {
            let audit: Arc<dyn AuditSink> = Arc::new(LogAuditSink);
            let agent = Arc::new(
                LocalDeviceAgent::with_settings(settings.clone().into_inner())
                    .with_audit(audit.clone()),
            );
            let collector = Arc::new(AgentContextCollector::new(
                agent,
                settings.clone().into_inner(),
            ));
            let model = Arc::new(ModelBackedDiagnoseModel::new(
                Arc::new(OpenAiCompatAdapter::new()),
                settings.clone().into_inner(),
                audit.clone(),
            ));
            Some(Arc::new(DiagnoseOrchestrator::new(
                collector,
                Arc::new(RegexRedactor::new()),
                model,
                audit,
            )))
        }
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
        // Confirmed execution is available wherever an in-process worker can
        // execute (Default / DeskServer), gated like the diagnose orchestrator.
        exec_supported: diagnose_orchestrator.is_some(),
        exec_approvals: Arc::new(crate::daemon::exec_approval::PendingApprovalStore::new()),
        // Single-machine confirmed-execution audit uses the structured log sink
        // (the audit carrier when there is no manager DB).
        audit: Arc::new(LogAuditSink),
        diagnose_tasks: Default::default(),
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
                let _ = maintain_proxy_connection(
                    settings.clone(),
                    &router_ctx,
                    local_url,
                    local_token,
                    rx,
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
                    let _ =
                        maintain_proxy_connection(settings.clone(), &router_ctx, url, token, rx)
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
                    let _ =
                        maintain_proxy_connection(settings.clone(), &router_ctx, url, token, rx)
                            .await;
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
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
                        .record(desk_agent_protocol::audit::AuditEvent::command_completed(
                            uuid::Uuid::new_v4().to_string(),
                            chrono::Utc::now().to_rfc3339(),
                            &payload.result.exec_request_id.0,
                            success,
                            summary,
                            redactions,
                            0,
                        ))
                        .await;
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

async fn maintain_proxy_connection(
    settings: web::Data<SharedSettings>,
    router_ctx: &RouterContext,
    signaling_url: String,
    auth_token: String,
    mut outbound_rx: broadcast::Receiver<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_name = {
        let s = settings.read().await;
        s.desk.display_name.clone()
    };
    let display_name = display_name.or_else(sysinfo::System::host_name);

    let client_id = {
        let s = settings.read().await;
        s.system.get_client_id().map_err(|e| format!("{e}"))?
    };

    let mut version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        crate::version::SERVER_BUILD_NUMBER,
        crate::version::SERVER_COMMIT_HASH.to_string(),
        RemoteDeskTypeEnum::Server,
        display_name,
        Some(client_id),
    );
    version_info.token = Some(auth_token);
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

    let (mut sink, mut stream) = framed.split();

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
                                handle_inbound_signaling_text(text_str, router_ctx).await;
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
    Ok(())
}

/// Inbound-text dispatcher pulled out of `maintain_proxy_connection`
/// so the parse / route sequence is reusable for tests and the
/// per-frame logic stays out of the WS select loop.
///
/// Parses the inbound text once and hands the model to
/// [`signaling_router::route`]. The router exhaustively dispatches:
/// PC / SDP / ICE types are handled inline, worker-bound types ride
/// dedicated `ServiceToWorker::*` typed IPC variants, and
/// daemon-emitted notifications are trace-logged + dropped. After
/// batch 4 of the typed-IPC migration there is no fallback path —
/// the previous opaque `SignalingMessage` bridge has been removed.
async fn handle_inbound_signaling_text(text_str: String, router_ctx: &RouterContext) {
    let parsed = match serde_json::from_str::<SignalingModel>(&text_str) {
        Ok(m) => m,
        Err(e) => {
            warn!("[Proxy] Dropping malformed signaling text: {e}");
            return;
        }
    };

    if let Err(e) = signaling_router::route(&parsed, router_ctx).await {
        warn!(
            "[Proxy] router handler failed for {:?}: {e}; dropping unrouted message",
            parsed.signaling_type,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pc_manager::PcRegistry;
    use crate::host_control::HostControlHub;
    use crate::model::settings::{Settings, SharedSettings};
    use desk_signal_facade::model::signal::{SignalingModel, SignalingType};

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
            exec_supported: false,
            exec_approvals: Arc::new(crate::daemon::exec_approval::PendingApprovalStore::new()),
            audit: Arc::new(LogAuditSink),
            diagnose_tasks: Default::default(),
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
        handle_inbound_signaling_text(text, &router_ctx).await;
    }

    /// Malformed JSON arriving on the WS is dropped with a warning
    /// rather than crashing the proxy loop.
    #[tokio::test]
    async fn drops_malformed_json() {
        let (router_ctx, _out_tx) = make_router_ctx();
        handle_inbound_signaling_text("{ this is not valid json".to_string(), &router_ctx).await;
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
        handle_inbound_signaling_text(text, &router_ctx).await;
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
        handle_inbound_signaling_text(text, &router_ctx).await;
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
