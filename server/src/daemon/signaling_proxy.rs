use super::manager_link_gate::ManagerLinkGate;
use super::manager_link_state::ManagerLinkState;
use super::pc_manager::PcRegistry;
use super::signaling_router::{self, RouterContext};
use super::support_link_state::SupportLinkState;
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
use desk_agent_protocol::exec_lifecycle::{ExecLifecycleEvent, ExecLifecyclePayload};
use desk_ipc_protocol::message::{
    ERROR_CODE_MEDIA_TRANSPORT_STUCK, VirtualDisplayModeOutcome, WorkerToService,
};
use desk_signal_facade::model::{
    request_remote_authz::{AuthorizedRequestRemote, AuthorizedTerminalStart, RequestRemoteAuthz},
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
use tokio::sync::{broadcast, watch};

/// Whether the host should keep the manager link connected right now: the
/// manager URL and API token are both configured (non-empty) **and** the
/// host-local `manager_enabled` toggle is not turned off (`Some(false)`).
///
/// The single predicate behind the always-on manager upstream guard, the
/// on-demand support upstream guard, and the shared [`ManagerLinkGate`] value, so
/// none of them can drift from the others.
pub fn manager_link_should_connect(
    manager_url: &Option<String>,
    manager_api_token: &Option<String>,
    manager_enabled: Option<bool>,
) -> bool {
    let url_ok = manager_url.as_ref().map(|u| !u.is_empty()).unwrap_or(false);
    let token_ok = manager_api_token
        .as_ref()
        .map(|t| !t.is_empty())
        .unwrap_or(false);
    url_ok && token_ok && manager_enabled != Some(false)
}

#[allow(clippy::too_many_arguments)]
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
    // Shared "should the manager link be connected" gate. Flipping it to `false`
    // (host UI disabling the manager connection) tears the current manager /
    // support upstream down and puts the fleet audit sink back to purely-local.
    manager_link_gate: Arc<ManagerLinkGate>,
    // This host's durable exec ledger. Opened by the daemon entry point, which is
    // common to all three host forms, so every dispatch path has one.
    exec_ledger: Arc<crate::daemon::exec_ledger::ExecLedger>,
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

    // The audit sink always logs locally and best-effort reports each event to
    // the manager (DB persistence) over the outbound lane. Whether the manager
    // report is emitted is decided dynamically at record time from the shared
    // [`ManagerLinkGate`]: when the manager link should not be connected (unset /
    // disabled at runtime) the sink stays purely local, so the choice is never a
    // stale startup decision. When no manager upstream is live the frame is
    // dropped by the broadcast anyway, so a momentary disconnect loses nothing.
    let audit_sink: Arc<dyn AuditSink> = Arc::new(RemoteAuditSink::new(
        outbound_tx.clone(),
        manager_link_gate.clone(),
    ));

    // The daemon constructs `pc_registry` once in `daemon::mod` and shares
    // it with both `WorkerManager` (for the media-pipe receiver) and the
    // signaling proxy (for inbound SDP/ICE handlers). Using a single
    // registry across all signaling endpoints (local / remote signaling /
    // remote manager) means the same PC handles inbound messages
    // regardless of which WS surfaced them.
    let router_ctx = RouterContext {
        exec_ledger: exec_ledger.clone(),
        exec_capacity: Arc::new(crate::daemon::exec_capacity::ExecCapacity::new()),
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
        inbound_request_remote_authz: None,
        inbound_start_terminal_authz: None,
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
        let manager_link_gate = manager_link_gate.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (manager_url, manager_api_token, manager_enabled) = {
                    let s = settings.read().await;
                    (
                        s.system.manager_url.clone(),
                        s.system.manager_api_token.clone(),
                        s.system.manager_enabled,
                    )
                };

                if manager_link_should_connect(&manager_url, &manager_api_token, manager_enabled)
                    && let (Some(url), Some(token)) = (manager_url, manager_api_token)
                {
                    let rx = outbound_tx.subscribe();
                    // The manager link is the only one that enforces fatal
                    // device-quota rejection (`remote_mgr_handle` is the sole manager
                    // injection point). It is also gated by the shared
                    // `ManagerLinkGate`, so disabling the manager connection at
                    // runtime tears the current WebSocket down.
                    let outcome = maintain_proxy_connection(
                        settings.clone(),
                        &router_ctx,
                        url,
                        token,
                        rx,
                        InboundSignalingSource::TrustedCentral,
                        true,
                        Some(manager_link_state.clone()),
                        Some(manager_link_gate.subscribe()),
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

    // On-demand temporary-support request. This parks until a local user requests
    // a support code (the host REST API flips `support_link_state` active), then
    // asks the manager to mint one by broadcasting a `RequestSupportCode` frame
    // over the regular upstreams (only the central brain mints; a plain signal
    // ignores it). The manager pushes the issued code back as `SupportCodeIssued`,
    // which the daemon consumes locally to display it and arm the TTL-expiry timer.
    // There is no dedicated support upstream: a supporter redeems the code into a
    // capability-scoped grant and connects over the host's regular connection, so
    // the restriction is enforced per session (grant ceiling) rather than by
    // isolating a physical link.
    let support_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let support_link_state = support_link_state.clone();
        actix_web::rt::spawn(async move {
            loop {
                support_link_state.wait_for_start().await;

                let (manager_url, manager_api_token, manager_enabled) = {
                    let s = settings.read().await;
                    (
                        s.system.manager_url.clone(),
                        s.system.manager_api_token.clone(),
                        s.system.manager_enabled,
                    )
                };

                // Support codes are minted only by a central brain (the manager),
                // so a request is meaningful only when the manager link is on.
                if manager_link_should_connect(&manager_url, &manager_api_token, manager_enabled) {
                    match SignalingModel::new_request(
                        SignalingType::RequestSupportCode,
                        None,
                        None::<&()>,
                    ) {
                        Ok(model) => match serde_json::to_string(&model) {
                            Ok(text) => {
                                let _ = outbound_tx.send(text);
                            }
                            Err(e) => {
                                warn!("[support] failed to serialise RequestSupportCode: {e}")
                            }
                        },
                        Err(e) => warn!("[support] failed to build RequestSupportCode: {e}"),
                    }
                } else {
                    warn!(
                        "[support] start requested but the manager link is not configured or is disabled"
                    );
                }

                // Park until the local user ends support (or the inbound
                // `SupportCodeIssued` TTL-expiry timer flips the state inactive).
                // There is no dedicated upstream to tear down; an in-flight
                // supporter session ends on its own grant TTL.
                support_link_state.wait_for_stop().await;

                // Ask the manager to revoke the code so it can no longer be
                // redeemed the moment support ends, instead of only ageing out on
                // its TTL. Best-effort: if the code never arrived there is nothing
                // to revoke, and an already-expired code revokes to a no-op.
                if let Some(snapshot) = support_link_state.snapshot().await {
                    let payload = desk_signal_facade::model::support::RevokeSupportCodeData {
                        code: snapshot.code,
                    };
                    match SignalingModel::new_request(
                        SignalingType::RevokeSupportCode,
                        None,
                        Some(&payload),
                    ) {
                        Ok(model) => match serde_json::to_string(&model) {
                            Ok(text) => {
                                let _ = outbound_tx.send(text);
                            }
                            Err(e) => {
                                warn!("[support] failed to serialise RevokeSupportCode: {e}")
                            }
                        },
                        Err(e) => warn!("[support] failed to build RevokeSupportCode: {e}"),
                    }
                }

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
                pc_registry
                    .resume_active_media(&worker_mgr, virtual_display.as_ref())
                    .await;
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
            // The spawn's own outcome, reported before the command finishes. It
            // moves the ledger entry off `reserved`, which is what makes a later
            // crash distinguishable: still `reserved` means the host died mid-spawn
            // and genuinely cannot say what happened, whereas `running` means it
            // started and only the result was lost.
            WorkerToService::ExecSpawnReport(payload) => {
                use desk_ipc_protocol::message::ExecSpawnReport;
                let recorded = match &payload.report {
                    ExecSpawnReport::Started {
                        containment_identity,
                    } => {
                        router_ctx
                            .exec_ledger
                            .mark_running(&payload.request_id, containment_identity.as_deref())
                            .await
                    }
                    ExecSpawnReport::Failed { reason } => {
                        router_ctx
                            .exec_ledger
                            .mark_terminal(
                                &payload.request_id,
                                crate::daemon::exec_ledger::Terminal::SpawnFailed(reason.clone()),
                            )
                            .await
                    }
                };
                if let Err(e) = recorded {
                    log::error!(
                        "[exec-ledger] could not record the spawn of {}: {e}",
                        payload.request_id
                    );
                }
                // A command that never started occupies nothing, so give its slot
                // back now rather than letting the deadline reclaim it.
                if matches!(payload.report, ExecSpawnReport::Failed { .. }) {
                    router_ctx.exec_capacity.release(&payload.request_id);
                }
                // Tell whoever asked that the command is up. A failed spawn is not
                // reported here: it is terminal, and it travels on the result path
                // like every other ending.
                if let ExecSpawnReport::Started {
                    containment_identity,
                } = &payload.report
                {
                    send_exec_lifecycle(
                        &router_ctx.outbound_tx,
                        &payload.request_id,
                        payload.connection_id.clone(),
                        ExecLifecycleEvent::Accepted {
                            containment_identity: containment_identity.clone(),
                        },
                    );
                }
                continue;
            }
            WorkerToService::ExecHeartbeat(payload) => {
                send_exec_lifecycle(
                    &router_ctx.outbound_tx,
                    &payload.request_id,
                    payload.connection_id.clone(),
                    ExecLifecycleEvent::Heartbeat {
                        running_ms: payload.running_ms,
                    },
                );
                continue;
            }
            WorkerToService::ExecResult(payload) => {
                // Close out the ledger entry first, so the host's own record is
                // settled before the answer leaves the machine. The generation is
                // the frame id the plan was dispatched under.
                {
                    let result_json = serde_json::to_string(&payload.result.outcome)
                        .unwrap_or_else(|_| "null".to_string());
                    if let Err(e) = router_ctx
                        .exec_ledger
                        .mark_terminal(
                            &payload.request_id,
                            crate::daemon::exec_ledger::Terminal::Completed(result_json),
                        )
                        .await
                    {
                        // The command did run; failing to record that is bad but
                        // withholding the result would be worse, so log and relay.
                        log::error!(
                            "[exec-ledger] could not record the result of {}: {e}",
                            payload.request_id
                        );
                    }
                }
                // The execution is accounted for; free its slot for the next one.
                router_ctx.exec_capacity.release(&payload.request_id);
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

#[allow(clippy::too_many_arguments)]
/// Whether a signaling URL uses a TLS scheme (`wss` / `https`). Anything else
/// (`ws` / `http` / malformed) is treated as plaintext, so the transport guard
/// fails closed toward requiring TLS for a public target.
fn signaling_scheme_is_tls(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("wss://") || lower.starts_with("https://")
}

/// Normalize a signaling URL exactly as the dial does, then guard the (possibly
/// IP-literal) target before connecting. Returns the cleaned URL to dial, or an
/// error when the transport policy refuses it.
///
/// The dial strips control characters, and `char::is_control` covers code points
/// (e.g. U+007F) that URL parsing does not — so guarding the *raw* string would let
/// a control-prefixed literal fail the parse (and be deferred as if it were a
/// domain) yet clean up into a valid IP-literal dial, re-opening the metadata floor
/// and the public-plaintext refusal for literals. Cleaning first and guarding the
/// exact string that is dialed closes that mismatch. The actix-tls resolver
/// short-circuits an IP literal before the custom guard resolver runs, which is why
/// a literal must be judged here rather than only in the resolver.
fn guard_and_clean_signaling_url(
    signaling_url: &str,
    require_secure_signaling: bool,
) -> Result<String, String> {
    let url_clean = signaling_url.trim().trim_matches(|c: char| c.is_control());
    // Drop any fragment: a dial URL has none, and the auth token is appended as a
    // `?token=...` query below — after a fragment that query would both fail to
    // reach the server-side token read and land inside the fragment (defeating log
    // redaction). Stripping it keeps the token in a proper query.
    let url_clean = url_clean.split('#').next().unwrap_or(url_clean);
    let scheme_is_tls = signaling_scheme_is_tls(url_clean);
    desk_utils::ssrf::check_transport_for_url(
        url_clean,
        true,
        scheme_is_tls,
        require_secure_signaling,
    )
    .map_err(|e| format!("signaling target refused: {e}"))?;
    Ok(url_clean.to_string())
}

/// Render a dial URL for logging with every credential-bearing part neutralized:
/// the `token` query value is masked, any userinfo (`user:pass@`) is stripped, and
/// any fragment is dropped (a malformed base could otherwise push the appended
/// token into the fragment). If the URL does not parse, fail safe by keeping only
/// the part before the first `?`/`#` (a credential can only live in the query or
/// fragment) rather than logging it verbatim.
fn redact_token_in_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        // Unparseable: any part of the raw string may carry a credential (userinfo,
        // query, or fragment) and we have no parser to isolate the safe parts, so log
        // a fixed placeholder — nothing from `raw` is emitted. An unparseable URL
        // would not have dialed anyway (awc's `http::Uri` parser rejects it too), so
        // no useful debugging information is lost.
        return "<unparseable url>".to_string();
    };
    // Userinfo may carry credentials; strip it. A fragment is never dialed and could
    // carry an appended token, so drop it.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    if url.query().is_some() {
        let redacted: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case("token") {
                    (k.into_owned(), "***".to_string())
                } else {
                    (k.into_owned(), v.into_owned())
                }
            })
            .collect();
        {
            let mut qs = url.query_pairs_mut();
            qs.clear();
            for (k, v) in &redacted {
                qs.append_pair(k, v);
            }
        }
    }
    url.to_string()
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
    // Set only for the manager and support upstreams. When present, the current
    // connection is torn down the moment the shared `ManagerLinkGate` flips to
    // `false` (the host disabling the manager connection at runtime). `None` for
    // the local loopback and bare remote-signaling relays, which the manager
    // toggle does not govern.
    mut manager_link_enabled_rx: Option<watch::Receiver<bool>>,
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

    // Whether the host refuses a plaintext dial to a *public* signaling / manager
    // address. Loopback / private / LAN targets (the local loopback link, a
    // self-hosted server on a LAN) stay reachable over plaintext regardless; only
    // an internet-routable plaintext target is refused when this is on.
    let require_secure_signaling = {
        let s = settings.read().await;
        s.system.require_secure_signaling
    };

    // Every upstream link registers as a normal `Server` connection. Temporary
    // support no longer opens a dedicated restricted upstream: a support code is
    // requested over this same `Server` link and redeemed into a capability-scoped
    // grant, so the restriction is enforced per session rather than per link.
    let remote_desk_type = RemoteDeskTypeEnum::Server;

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
    // Guard the outbound dial at connect time: the metadata floor is always
    // blocked, and a plaintext (`ws://`) scheme to a public address is refused when
    // `require_secure_signaling` is on. The scheme is fixed for this dial, so bake
    // it into the resolver — no second lookup that could rebind. `allow_private` is
    // always true here: signaling legitimately reaches LAN / loopback targets.
    // Normalize the URL exactly as the dial does and guard the (possibly literal)
    // target before connecting. Returns the cleaned URL that will be dialed.
    let url_clean = guard_and_clean_signaling_url(&signaling_url, require_secure_signaling)?;
    let scheme_is_tls = signaling_scheme_is_tls(&url_clean);
    let guard = crate::transport_guard::TransportGuardResolver::system(
        crate::transport_guard::TransportPolicy {
            allow_private: true,
            scheme_is_tls,
            enforce_public_tls: require_secure_signaling,
        },
    );
    let tcp =
        actix_tls::connect::Connector::new(actix_tls::connect::Resolver::custom(guard)).service();
    let client = Client::builder()
        .connector(
            Connector::new()
                .connector(tcp)
                .timeout(Duration::from_secs(10))
                .rustls_0_23(Arc::new(
                    ClientConfig::builder()
                        .with_root_certificates(Arc::new(root_store))
                        .with_no_client_auth(),
                )),
        )
        .finish();

    let connect_url = if url_clean.contains('?') {
        format!("{url_clean}&{version_query}")
    } else {
        format!("{url_clean}?{version_query}")
    };

    info!(
        "[Proxy] Connecting to: {}",
        redact_token_in_url(&signaling_url)
    );
    debug!("[Proxy] Full URL: {}", redact_token_in_url(&connect_url));

    let (_resp, framed) = client
        .ws(&connect_url)
        .connect()
        .await
        .map_err(|e| format!("WebSocket connect failed: {e:?}"))?;

    info!(
        "[Proxy] Connected to {}",
        redact_token_in_url(&signaling_url)
    );

    // A successful (re)connection clears any prior fatal rejection so the host UI
    // stops showing the blocked state once registration goes through.
    if let Some(state) = manager_link_state.as_ref() {
        state.clear().await;
    }

    let (mut sink, mut stream) = framed.split();

    // Close a race where the manager link is disabled after `connect()` but
    // before this read loop parks on the gate: read the current value first and
    // bail out immediately if the link should no longer be up.
    if let Some(rx) = manager_link_enabled_rx.as_ref()
        && !*rx.borrow()
    {
        info!(
            "[Proxy] Manager link disabled; closing {}",
            redact_token_in_url(&signaling_url)
        );
        let _ = sink.send(awc::ws::Message::Close(None)).await;
        return Ok(ProxyConnectionOutcome::Closed);
    }

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

            // Manager / support upstreams only: tear the connection down when the
            // host disables the manager link at runtime. `None` links resolve a
            // never-completing future, so this branch is inert for them.
            _ = wait_manager_link_disabled(&mut manager_link_enabled_rx) => {
                info!(
            "[Proxy] Manager link disabled; closing {}",
            redact_token_in_url(&signaling_url)
        );
                let _ = sink.send(awc::ws::Message::Close(None)).await;
                break;
            }
        }
    }

    info!(
        "[Proxy] Connection to {} ended",
        redact_token_in_url(&signaling_url)
    );
    Ok(ProxyConnectionOutcome::Closed)
}

/// Resolve when the shared manager-link gate flips to disabled. For links the
/// manager toggle does not govern (`None` receiver) this never resolves, so the
/// `select!` branch that awaits it stays inert.
async fn wait_manager_link_disabled(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => {
            // `wait_for` re-checks the current value, so a disable that already
            // happened is observed rather than missed.
            let _ = rx.wait_for(|enabled| !*enabled).await;
        }
        None => std::future::pending::<()>().await,
    }
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

/// Outcome of the source-gated capability-ceiling check for a `RequestRemote`.
enum RequestRemoteGateOutcome {
    /// Forward this (possibly unwrapped) model to the router, carrying the
    /// validated capability-ceiling stamp when the request arrived wrapped from
    /// the trusted-central link.
    Pass(SignalingModel, Option<RequestRemoteAuthz>),
    /// Drop the frame; the string explains why (for logging).
    Drop(String),
}

/// Source-gate an inbound `RequestRemote` against the capability-ceiling stamp
/// rules. This is the anti-downgrade anchor (mirrors [`gate_authz_frame`]): the
/// trusted-central link always stamps every `RequestRemote` (owner → no ceiling,
/// redeemed grant → its ceiling), so on that link a bare request is illegitimate
/// and dropped, and a stamp from any other source is an illegitimate injection
/// and dropped.
///
/// - A **wrapper** (`AuthorizedRequestRemote`) is only legitimate from
///   `TrustedCentral`; on any other source it is dropped.
/// - On `TrustedCentral` a **bare** `RequestRemote` is dropped — a forged frame,
///   a relay fault, or a grant session stripping its stamp to masquerade as an
///   owner. Dropping it here is the only defense (there is no physical restricted
///   upstream to fall back on).
/// - On `TrustedCentral` a wrapper is validated against the frame (`request_id`),
///   this daemon's audience, and expiry; on success the inner frame is unwrapped
///   and forwarded with the validated stamp (the ceiling the router / worker
///   enforce).
/// - On a non-central source (loopback / relay / support) a bare `RequestRemote`
///   passes through unchanged: the owner-only relay path, where there is no
///   central to stamp and redeemed codes are hard-rejected at redeem time.
fn gate_request_remote_frame(
    model: SignalingModel,
    source: InboundSignalingSource,
    expected_audience: &str,
    now_rfc3339: &str,
) -> RequestRemoteGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        if source == InboundSignalingSource::TrustedCentral {
            return RequestRemoteGateOutcome::Drop(
                "bare RequestRemote from trusted-central source (capability-ceiling stamp required)"
                    .to_string(),
            );
        }
        return RequestRemoteGateOutcome::Pass(model, None);
    }

    if source != InboundSignalingSource::TrustedCentral {
        return RequestRemoteGateOutcome::Drop(format!(
            "RequestRemote carried a capability-ceiling stamp from non-central source {source:?}"
        ));
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return RequestRemoteGateOutcome::Drop("stamped frame had no data".to_string()),
    };
    let wrapper: AuthorizedRequestRemote = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return RequestRemoteGateOutcome::Drop(format!(
                "malformed RequestRemote stamp wrapper: {e}"
            ));
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return RequestRemoteGateOutcome::Drop(format!("RequestRemote stamp rejected: {e:?}"));
    }

    // Validated: forward the inner frame as a bare RequestRemote plus the
    // validated stamp, which the router threads into the session's restriction /
    // capability-ceiling enforcement.
    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    RequestRemoteGateOutcome::Pass(unwrapped, Some(wrapper.authz))
}

/// Source-gate an inbound `StartTerminal` against the capability-ceiling stamp
/// rules, the terminal analogue of [`gate_request_remote_frame`]. The remote
/// terminal opens on a distinct WS connection that never does a `RequestRemote`, so
/// `StartTerminal` is *its* admission-establishing frame and must carry the same
/// stamp discipline:
///
/// - A **wrapper** (`AuthorizedTerminalStart`) is only legitimate from
///   `TrustedCentral`; on any other source it is dropped (a non-central stamp is an
///   illegitimate injection).
/// - On `TrustedCentral` a **bare** `StartTerminal` is dropped — the central always
///   stamps (owner → no ceiling, redeemed code → its ceiling), so a bare one is a
///   forged frame or a stamp-stripping downgrade attempt.
/// - On `TrustedCentral` a wrapper is validated against the frame (`request_id`),
///   this daemon's audience, and expiry; on success the inner `StartTerminalSession`
///   is unwrapped and forwarded with the validated stamp (which
///   `handle_start_terminal_inbound` turns into the connection's ceiling + admission
///   + grant index).
/// - On a non-central source (loopback / relay) a bare `StartTerminal` passes
///   through unchanged: the owner-only relay path, where there is no central to
///   stamp and redeemed codes are hard-rejected at redeem time — identical to the
///   `RequestRemote` owner relay.
fn gate_start_terminal_frame(
    model: SignalingModel,
    source: InboundSignalingSource,
    expected_audience: &str,
    now_rfc3339: &str,
) -> RequestRemoteGateOutcome {
    let has_wrapper = model
        .get_raw_data()
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|o| o.contains_key("authz") && o.contains_key("inner"))
        .unwrap_or(false);

    if !has_wrapper {
        if source == InboundSignalingSource::TrustedCentral {
            return RequestRemoteGateOutcome::Drop(
                "bare StartTerminal from trusted-central source (capability-ceiling stamp required)"
                    .to_string(),
            );
        }
        return RequestRemoteGateOutcome::Pass(model, None);
    }

    if source != InboundSignalingSource::TrustedCentral {
        return RequestRemoteGateOutcome::Drop(format!(
            "StartTerminal carried a capability-ceiling stamp from non-central source {source:?}"
        ));
    }

    let raw = match model.get_raw_data().clone() {
        Some(v) => v,
        None => return RequestRemoteGateOutcome::Drop("stamped frame had no data".to_string()),
    };
    let wrapper: AuthorizedTerminalStart = match serde_json::from_value(raw) {
        Ok(w) => w,
        Err(e) => {
            return RequestRemoteGateOutcome::Drop(format!(
                "malformed StartTerminal stamp wrapper: {e}"
            ));
        }
    };

    if let Err(e) = wrapper
        .authz
        .validate(&model.request_id, expected_audience, now_rfc3339)
    {
        return RequestRemoteGateOutcome::Drop(format!("StartTerminal stamp rejected: {e:?}"));
    }

    let unwrapped = SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(wrapper.inner),
        model.response_state.clone(),
    );
    RequestRemoteGateOutcome::Pass(unwrapped, Some(wrapper.authz))
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
/// Signaling types that are server-originated central→daemon plumbing: they are
/// accepted only from the trusted-central link. A Local / remote-signaling origin
/// (no trusted PDP) must never inject operator templates, weaken the command
/// blocklist, drive an evidence collection, dispatch a sealed execution plan,
/// drive a remote read, surface a forged support code to the local user, or forge
/// a grant-session teardown that tears down a legitimate session.
fn is_trusted_central_only(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::CommandTemplateSync
            | SignalingType::CommandBlocklistSync
            | SignalingType::CollectRequest
            | SignalingType::EdgeExecRequest
            | SignalingType::RemoteToolRequest
            | SignalingType::SupportCodeIssued
            | SignalingType::RevokeAccessGrant
    )
}

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

    // Source gate: server-originated central→daemon plumbing (see
    // [`is_trusted_central_only`]) is accepted only from the trusted-central link.
    // For the blocklist this is critical: a forged sync with a higher revision and
    // a thinned rule set would otherwise wipe the daemon's floor and fail-open.
    if is_trusted_central_only(parsed.signaling_type)
        && source != InboundSignalingSource::TrustedCentral
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

    // `RequestRemote` carries the capability-ceiling stamp: required and validated
    // on the trusted-central link (a bare one there is dropped as a downgrade
    // attempt), rejected if stamped from a non-central source, passed through bare
    // on the owner-only relay path. The validated stamp rides into the router via
    // the context so the freshly-created session inherits its restriction /
    // ceiling.
    if parsed.signaling_type == SignalingType::RequestRemote {
        match gate_request_remote_frame(parsed, source, &expected_audience, &now) {
            RequestRemoteGateOutcome::Pass(unwrapped, authz) => {
                let effective_ctx;
                let ctx_ref = if authz.is_some() {
                    effective_ctx = RouterContext {
                        inbound_request_remote_authz: authz,
                        ..router_ctx.clone()
                    };
                    &effective_ctx
                } else {
                    router_ctx
                };
                if let Err(e) = signaling_router::route(&unwrapped, ctx_ref).await {
                    warn!("[Proxy] router handler failed for RequestRemote: {e}");
                }
            }
            RequestRemoteGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping RequestRemote: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    // `StartTerminal` carries the same capability-ceiling stamp as `RequestRemote`
    // (it is the admission-establishing frame for the distinct terminal WS): required
    // and validated on the trusted-central link, rejected if stamped from a
    // non-central source, passed through bare on the owner-only relay path. The
    // validated stamp rides into the router via the context so
    // `handle_start_terminal_inbound` can register the connection's ceiling +
    // admission + grant index.
    if parsed.signaling_type == SignalingType::StartTerminal {
        match gate_start_terminal_frame(parsed, source, &expected_audience, &now) {
            RequestRemoteGateOutcome::Pass(unwrapped, authz) => {
                let effective_ctx;
                let ctx_ref = if authz.is_some() {
                    effective_ctx = RouterContext {
                        inbound_start_terminal_authz: authz,
                        ..router_ctx.clone()
                    };
                    &effective_ctx
                } else {
                    router_ctx
                };
                if let Err(e) = signaling_router::route(&unwrapped, ctx_ref).await {
                    warn!("[Proxy] router handler failed for StartTerminal: {e}");
                }
            }
            RequestRemoteGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping StartTerminal: {reason}");
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

    // A validated central authorization rides into the handlers via a per-call
    // clone of the router context (cheap: the context is Arc-backed), keeping
    // `route()` and the AI handler signatures untouched.
    let effective_ctx;
    let ctx_ref = if authz.is_some() {
        effective_ctx = RouterContext {
            inbound_authz: authz,
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

/// Send an `ExecLifecycle(625)` about one execution to whoever asked for it.
///
/// Notification-style and best-effort: these frames report progress, and the
/// authoritative answer is always a state query against the ledger. Dropping one
/// therefore costs nothing an upstream would act on.
fn send_exec_lifecycle(
    outbound_tx: &tokio::sync::broadcast::Sender<String>,
    execution_generation: &str,
    to_connection_id: Option<String>,
    event: ExecLifecycleEvent,
) {
    let payload = ExecLifecyclePayload {
        execution_generation: execution_generation.to_string(),
        event,
    };
    let data = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[exec-lifecycle] could not serialise the frame: {e}");
            return;
        }
    };
    let frame = SignalingModel::new(
        execution_generation,
        SignalingType::ExecLifecycle,
        None,
        to_connection_id,
        Some(data),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(e) => log::warn!("[exec-lifecycle] could not serialise the frame: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pc_manager::PcRegistry;
    use crate::host_control::HostControlHub;
    use crate::model::settings::{Settings, SharedSettings};
    use desk_signal_facade::model::request_remote_authz::REQUEST_REMOTE_AUTHZ_VERSION;
    use desk_signal_facade::model::security_settings::SecuritySettings;
    use desk_signal_facade::model::signal::{RequestRemoteModel, SignalingModel, SignalingType};

    const RR_AUDIENCE: &str = "host-client-abc";
    const RR_NOW: &str = "2026-01-01T00:00:00Z";

    /// Read the one lifecycle frame that was emitted.
    fn expect_lifecycle(rx: &mut tokio::sync::broadcast::Receiver<String>) -> ExecLifecyclePayload {
        let text = rx.try_recv().expect("no lifecycle frame was sent");
        let frame: SignalingModel = serde_json::from_str(&text).unwrap();
        assert_eq!(frame.signaling_type, SignalingType::ExecLifecycle);
        frame.get_data::<ExecLifecyclePayload>().unwrap()
    }

    /// A started command is announced to whoever asked for it, carrying how the
    /// host would reclaim it — the fact an upstream previously had to infer from
    /// silence and a clock.
    #[test]
    fn a_started_command_is_announced_with_its_containment_identity() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(8);
        send_exec_lifecycle(
            &tx,
            "gen-1",
            Some("conn-1".to_string()),
            ExecLifecycleEvent::Accepted {
                containment_identity: Some("pgid:4242".to_string()),
            },
        );

        let payload = expect_lifecycle(&mut rx);
        assert_eq!(payload.execution_generation, "gen-1");
        assert_eq!(
            payload.event,
            ExecLifecycleEvent::Accepted {
                containment_identity: Some("pgid:4242".to_string()),
            }
        );
    }

    /// A heartbeat carries the host's own elapsed time rather than a wall clock or
    /// a sequence, so nothing downstream has to reconcile two clocks or survive a
    /// counter resetting.
    #[test]
    fn a_heartbeat_carries_elapsed_time_and_nothing_else() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(8);
        send_exec_lifecycle(
            &tx,
            "gen-1",
            None,
            ExecLifecycleEvent::Heartbeat { running_ms: 12_345 },
        );
        assert_eq!(
            expect_lifecycle(&mut rx).event,
            ExecLifecycleEvent::Heartbeat { running_ms: 12_345 }
        );
    }

    #[test]
    fn signaling_scheme_is_tls_recognizes_secure_schemes() {
        assert!(signaling_scheme_is_tls(
            "wss://sig.example/api/desk/signaling"
        ));
        assert!(signaling_scheme_is_tls("HTTPS://sig.example"));
        assert!(!signaling_scheme_is_tls(
            "ws://sig.example/api/desk/signaling"
        ));
        assert!(!signaling_scheme_is_tls("http://sig.example"));
        // Malformed / schemeless fails closed to plaintext, so the guard requires
        // TLS for a public target rather than assuming it is secure.
        assert!(!signaling_scheme_is_tls("sig.example:8443"));
        assert!(!signaling_scheme_is_tls(""));
    }

    #[test]
    fn guard_and_clean_signaling_url_catches_control_prefixed_literal() {
        // A control-char (U+007F) prefix makes URL parsing fail, but the dial strips
        // it via `char::is_control`, cleaning up into a metadata IP-literal dial. The
        // guard must judge the CLEANED string, so this is refused, not deferred.
        assert!(
            guard_and_clean_signaling_url("\u{7f}ws://169.254.169.254/api/desk/signaling", true)
                .is_err(),
            "control-prefixed metadata literal must be refused after cleaning"
        );
        // Public plaintext literal with a control prefix is likewise refused under
        // enforcement (the cleaned `ws://` dial would otherwise leak the token).
        assert!(
            guard_and_clean_signaling_url("\u{7f}ws://203.0.113.5/api/desk/signaling", true)
                .is_err(),
            "control-prefixed public plaintext literal must be refused"
        );
        // A clean legitimate wss literal / domain passes and returns the dial URL.
        assert_eq!(
            guard_and_clean_signaling_url("  wss://sig.example/api/desk/signaling  ", true)
                .as_deref(),
            Ok("wss://sig.example/api/desk/signaling")
        );
        // Same public literal over TLS is fine.
        assert!(guard_and_clean_signaling_url("wss://203.0.113.5/api", true).is_ok());
        // A fragment is stripped from the dialed URL (the token query is appended
        // later and must not land in a fragment).
        assert_eq!(
            guard_and_clean_signaling_url("wss://sig.example/api#frag", true).as_deref(),
            Ok("wss://sig.example/api")
        );
    }

    #[test]
    fn redact_token_in_url_masks_only_the_token() {
        let out = redact_token_in_url(
            "wss://sig.example/api/desk/signaling?token=SECRET123&build_number=42&probe=1",
        );
        // The credential is gone; the rest of the query survives for debugging.
        assert!(!out.contains("SECRET123"), "token must not appear: {out}");
        assert!(out.contains("token=%2A%2A%2A") || out.contains("token=***"));
        assert!(out.contains("build_number=42"));
        assert!(out.contains("probe=1"));
    }

    #[test]
    fn redact_token_in_url_is_noop_without_token_or_query() {
        assert_eq!(
            redact_token_in_url("wss://sig.example/api/desk/signaling"),
            "wss://sig.example/api/desk/signaling"
        );
        // A malformed URL is logged as a fixed placeholder (nothing from it emitted).
        assert_eq!(redact_token_in_url("not a url"), "<unparseable url>");
    }

    #[test]
    fn redact_token_in_url_fails_safe_on_unparseable_url_with_credentials() {
        // An unparseable URL must never be logged verbatim: neither a token in the
        // query/fragment nor userinfo credentials may survive. A control char (U+007F)
        // makes parsing fail while the string still carries both.
        for raw in [
            "\u{7f}ws://169.254.169.254/api?token=SECRET123&probe=1",
            "\u{7f}wss://user:s3cret@sig.example/api",
        ] {
            let out = redact_token_in_url(raw);
            assert_eq!(
                out, "<unparseable url>",
                "must be a fixed placeholder: {out}"
            );
        }
    }

    #[test]
    fn redact_token_in_url_masks_token_in_fragment_and_userinfo() {
        // A token pushed into the fragment (e.g. by an appended query after a `#`)
        // must not survive: the fragment is dropped entirely.
        let frag = redact_token_in_url("wss://sig.example/api#x?token=SECRET123&probe=1");
        assert!(
            !frag.contains("SECRET123"),
            "fragment token must not appear: {frag}"
        );
        // Userinfo credentials are stripped, and a real token query is still masked.
        let ui = redact_token_in_url("wss://user:s3cret@sig.example/api?token=SECRET123");
        assert!(!ui.contains("SECRET123"), "token must not appear: {ui}");
        assert!(!ui.contains("s3cret"), "userinfo must not appear: {ui}");
        assert!(
            !ui.contains("user@") && !ui.contains("user:"),
            "userinfo stripped: {ui}"
        );
    }

    fn bare_request_remote() -> SignalingModel {
        let data = serde_json::to_value(RequestRemoteModel::default()).unwrap();
        SignalingModel::new(
            "req-1",
            SignalingType::RequestRemote,
            Some("browser-1".to_string()),
            Some("host-1".to_string()),
            Some(data),
            None,
        )
    }

    fn stamped_request_remote(authz: RequestRemoteAuthz) -> SignalingModel {
        let wrapper = AuthorizedRequestRemote {
            inner: serde_json::to_value(RequestRemoteModel::default()).unwrap(),
            authz,
        };
        SignalingModel::new(
            "req-1",
            SignalingType::RequestRemote,
            Some("browser-1".to_string()),
            Some("host-1".to_string()),
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        )
    }

    fn authz(ceiling: Option<SecuritySettings>) -> RequestRemoteAuthz {
        RequestRemoteAuthz {
            version: REQUEST_REMOTE_AUTHZ_VERSION,
            access_ceiling: ceiling,
            grant_session_id: None,
            generation: 0,
            request_id: "req-1".to_string(),
            audience: RR_AUDIENCE.to_string(),
            expires_at: Some("2999-01-01T00:00:00Z".to_string()),
        }
    }

    #[test]
    fn support_code_issued_is_trusted_central_only() {
        // A `SupportCodeIssued` is server-originated (the manager mints it), so the
        // source gate must confine it to the trusted-central link — otherwise a
        // bare relay could push a forged code to the host UI.
        assert!(is_trusted_central_only(SignalingType::SupportCodeIssued));
        // A `RevokeAccessGrant` is likewise server-originated (regeneration teardown);
        // confining it stops a bare relay forging a teardown of a live session.
        assert!(is_trusted_central_only(SignalingType::RevokeAccessGrant));
        // Alongside the other central→daemon plumbing.
        assert!(is_trusted_central_only(SignalingType::CommandBlocklistSync));
        assert!(is_trusted_central_only(SignalingType::CollectRequest));
        // The host→manager support frames are NOT gated here (they egress, never
        // arrive inbound), nor are ordinary session frames.
        assert!(!is_trusted_central_only(SignalingType::RequestSupportCode));
        assert!(!is_trusted_central_only(SignalingType::RevokeSupportCode));
        assert!(!is_trusted_central_only(SignalingType::RequestRemote));
        assert!(!is_trusted_central_only(SignalingType::Offer));
    }

    #[test]
    fn request_remote_bare_from_trusted_central_is_dropped() {
        // Anti-downgrade anchor: the central always stamps, so a bare request on
        // that link is forged / a stripped stamp and must be dropped.
        match gate_request_remote_frame(
            bare_request_remote(),
            InboundSignalingSource::TrustedCentral,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Drop(_) => {}
            RequestRemoteGateOutcome::Pass(..) => panic!("bare central RequestRemote must drop"),
        }
    }

    #[test]
    fn request_remote_stamp_from_non_central_is_dropped() {
        // A stamp is only legitimate from the trusted-central link; injecting one
        // from a relay is rejected.
        match gate_request_remote_frame(
            stamped_request_remote(authz(None)),
            InboundSignalingSource::RemoteSignaling,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Drop(_) => {}
            RequestRemoteGateOutcome::Pass(..) => panic!("non-central stamp must drop"),
        }
    }

    #[test]
    fn request_remote_stamp_failing_validation_is_dropped() {
        // Wrong audience → validate() fails → drop.
        match gate_request_remote_frame(
            stamped_request_remote(authz(None)),
            InboundSignalingSource::TrustedCentral,
            "some-other-host",
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Drop(_) => {}
            RequestRemoteGateOutcome::Pass(..) => panic!("audience mismatch must drop"),
        }
    }

    #[test]
    fn request_remote_valid_owner_stamp_passes_and_unwraps() {
        // A valid owner stamp (no ceiling) unwraps to a bare RequestRemote and
        // carries the validated stamp; the inner payload is restored.
        match gate_request_remote_frame(
            stamped_request_remote(authz(None)),
            InboundSignalingSource::TrustedCentral,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Pass(unwrapped, Some(a)) => {
                assert_eq!(a.access_ceiling, None);
                // The inner frame parses back as a plain RequestRemoteModel (no
                // authz/inner wrapper left).
                assert!(unwrapped.get_data::<RequestRemoteModel>().is_ok());
            }
            _ => panic!("valid owner stamp must pass with its authz"),
        }
    }

    #[test]
    fn request_remote_valid_grant_stamp_passes_with_ceiling() {
        let ceiling = SecuritySettings {
            allow_terminal: Some(true),
            ..SecuritySettings::default()
        };
        match gate_request_remote_frame(
            stamped_request_remote(authz(Some(ceiling.clone()))),
            InboundSignalingSource::TrustedCentral,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Pass(_, Some(a)) => {
                assert_eq!(a.access_ceiling, Some(ceiling));
            }
            _ => panic!("valid grant stamp must pass with its ceiling"),
        }
    }

    #[test]
    fn request_remote_bare_from_relay_passes_unchanged() {
        // The owner-only relay path (no central to stamp) still relays a bare
        // request through unchanged.
        match gate_request_remote_frame(
            bare_request_remote(),
            InboundSignalingSource::RemoteSignaling,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Pass(_, None) => {}
            _ => panic!("bare relay RequestRemote must pass unstamped"),
        }
    }

    fn start_terminal_session() -> desk_signal_facade::model::terminal::StartTerminalSession {
        desk_signal_facade::model::terminal::StartTerminalSession {
            command: "cmd.exe".to_string(),
            device_id: None,
            grant_session_id: None,
        }
    }

    fn bare_start_terminal() -> SignalingModel {
        SignalingModel::new(
            "req-1",
            SignalingType::StartTerminal,
            Some("browser-1".to_string()),
            Some("host-1".to_string()),
            Some(serde_json::to_value(start_terminal_session()).unwrap()),
            None,
        )
    }

    fn stamped_start_terminal(authz: RequestRemoteAuthz) -> SignalingModel {
        let wrapper = AuthorizedTerminalStart {
            inner: serde_json::to_value(start_terminal_session()).unwrap(),
            authz,
        };
        SignalingModel::new(
            "req-1",
            SignalingType::StartTerminal,
            Some("browser-1".to_string()),
            Some("host-1".to_string()),
            Some(serde_json::to_value(&wrapper).unwrap()),
            None,
        )
    }

    #[test]
    fn start_terminal_bare_from_trusted_central_is_dropped() {
        // Terminal mirrors RequestRemote: the central always stamps, so a bare
        // StartTerminal on that link is forged / a stripped stamp and must drop.
        match gate_start_terminal_frame(
            bare_start_terminal(),
            InboundSignalingSource::TrustedCentral,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Drop(_) => {}
            RequestRemoteGateOutcome::Pass(..) => panic!("bare central StartTerminal must drop"),
        }
    }

    #[test]
    fn start_terminal_stamp_from_non_central_is_dropped() {
        match gate_start_terminal_frame(
            stamped_start_terminal(authz(None)),
            InboundSignalingSource::RemoteSignaling,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Drop(_) => {}
            RequestRemoteGateOutcome::Pass(..) => panic!("non-central terminal stamp must drop"),
        }
    }

    #[test]
    fn start_terminal_stamp_failing_validation_is_dropped() {
        match gate_start_terminal_frame(
            stamped_start_terminal(authz(None)),
            InboundSignalingSource::TrustedCentral,
            "some-other-host",
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Drop(_) => {}
            RequestRemoteGateOutcome::Pass(..) => panic!("terminal audience mismatch must drop"),
        }
    }

    #[test]
    fn start_terminal_valid_owner_stamp_passes_and_unwraps() {
        match gate_start_terminal_frame(
            stamped_start_terminal(authz(None)),
            InboundSignalingSource::TrustedCentral,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Pass(unwrapped, Some(a)) => {
                assert_eq!(a.access_ceiling, None);
                // The inner frame parses back as a plain StartTerminalSession.
                assert!(
                    unwrapped
                        .get_data::<desk_signal_facade::model::terminal::StartTerminalSession>()
                        .is_ok()
                );
            }
            _ => panic!("valid owner terminal stamp must pass with its authz"),
        }
    }

    #[test]
    fn start_terminal_valid_grant_stamp_passes_with_ceiling() {
        let ceiling = SecuritySettings {
            allow_terminal: Some(true),
            ..SecuritySettings::default()
        };
        match gate_start_terminal_frame(
            stamped_start_terminal(authz(Some(ceiling.clone()))),
            InboundSignalingSource::TrustedCentral,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Pass(_, Some(a)) => {
                assert_eq!(a.access_ceiling, Some(ceiling));
            }
            _ => panic!("valid grant terminal stamp must pass with its ceiling"),
        }
    }

    #[test]
    fn start_terminal_bare_from_relay_passes_unchanged() {
        // Owner-only relay path (no central to stamp) relays a bare StartTerminal
        // through unchanged, admitted as owner downstream.
        match gate_start_terminal_frame(
            bare_start_terminal(),
            InboundSignalingSource::RemoteSignaling,
            RR_AUDIENCE,
            RR_NOW,
        ) {
            RequestRemoteGateOutcome::Pass(_, None) => {}
            _ => panic!("bare relay StartTerminal must pass unstamped"),
        }
    }

    #[test]
    fn manager_link_should_connect_requires_config_and_not_disabled() {
        let url = Some("wss://manager.example/api/desk/signaling".to_string());
        let token = Some("tok".to_string());

        // Configured + enabled (None or Some(true)) -> connect. This gate is shared
        // by the always-on manager upstream, the support upstream, and the audit
        // sink, so all three agree.
        assert!(manager_link_should_connect(&url, &token, None));
        assert!(manager_link_should_connect(&url, &token, Some(true)));

        // Explicitly disabled -> never connect, even with full config (cold-start
        // with manager_enabled=false keeps both the manager and support upstreams
        // parked).
        assert!(!manager_link_should_connect(&url, &token, Some(false)));

        // Missing / empty url or token -> never connect regardless of the toggle.
        assert!(!manager_link_should_connect(&None, &token, None));
        assert!(!manager_link_should_connect(&url, &None, None));
        assert!(!manager_link_should_connect(
            &Some(String::new()),
            &token,
            None
        ));
        assert!(!manager_link_should_connect(
            &url,
            &Some(String::new()),
            None
        ));
    }

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

    async fn make_router_ctx() -> (RouterContext, broadcast::Sender<String>) {
        let (outbound_tx, _) = broadcast::channel::<String>(16);
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _rx) = WorkerManager::new(settings.clone(), pc_registry.clone());
        let ctx = RouterContext {
            exec_capacity: Arc::new(crate::daemon::exec_capacity::ExecCapacity::new()),
            exec_ledger: Arc::new(
                crate::daemon::exec_ledger::ExecLedger::open_in_memory()
                    .await
                    .expect("in-memory ledger"),
            ),
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
            inbound_request_remote_authz: None,
            inbound_start_terminal_authz: None,
            edge_exec_pending: Default::default(),
            support_link_state: Arc::new(crate::daemon::support_link_state::SupportLinkState::new()),
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
        let (router_ctx, _out_tx) = make_router_ctx().await;

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
        let (router_ctx, _out_tx) = make_router_ctx().await;
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
        let (router_ctx, _out_tx) = make_router_ctx().await;
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
        let (router_ctx, _out_tx) = make_router_ctx().await;

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
        let (router_ctx, _out_tx) = make_router_ctx().await;

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
            COMMAND_TEMPLATE_SYNC_EPOCH, COMMAND_TEMPLATE_SYNC_VERSION, CommandTemplateSyncPayload,
            SyncedCommandTemplate,
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
            epoch: COMMAND_TEMPLATE_SYNC_EPOCH,
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
        let (router_ctx, _out_tx) = make_router_ctx().await;

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

    /// A current daemon applies a current-epoch sync, ignores a payload whose version
    /// is outside the supported range (a future version reaching this older daemon),
    /// and drops a pre-narrowing (epoch 0) frame at the epoch floor — each leaving the
    /// prior applied set intact.
    #[tokio::test]
    async fn command_template_sync_applies_current_epoch_and_ignores_unknown_version() {
        use desk_agent_protocol::command_template::{
            COMMAND_TEMPLATE_SYNC_EPOCH, COMMAND_TEMPLATE_SYNC_VERSION, CommandTemplateSyncPayload,
            SyncedCommandTemplate,
        };
        use desk_agent_protocol::exec::ExecEffect;
        let (router_ctx, _out_tx) = make_router_ctx().await;

        let make_text = |version: u16, epoch: u16, revision: Option<i64>| {
            let payload = CommandTemplateSyncPayload {
                version,
                templates: vec![SyncedCommandTemplate {
                    template_id: "get_disk".into(),
                    argv: vec!["Get-Disk".into()],
                    effect: ExecEffect::ReadOnly,
                }],
                command_template_revision: revision,
                epoch,
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

        // A current-epoch sync from trusted central is applied.
        handle_inbound_signaling_text(
            make_text(
                COMMAND_TEMPLATE_SYNC_VERSION,
                COMMAND_TEMPLATE_SYNC_EPOCH,
                Some(3),
            ),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 1);
        assert_eq!(router_ctx.command_templates.revision(), Some(3));

        // An unsupported future version is ignored — the cache keeps the prior apply.
        handle_inbound_signaling_text(
            make_text(99, COMMAND_TEMPLATE_SYNC_EPOCH, Some(5)),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.len(), 1);
        assert_eq!(router_ctx.command_templates.revision(), Some(3));

        // A pre-narrowing (epoch 0) frame is dropped by the epoch floor even from a
        // trusted source — it can never re-widen the narrowed cache.
        handle_inbound_signaling_text(
            make_text(COMMAND_TEMPLATE_SYNC_VERSION, 0, Some(9)),
            &router_ctx,
            InboundSignalingSource::TrustedCentral,
            false,
        )
        .await;
        assert_eq!(router_ctx.command_templates.revision(), Some(3));
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
                org_id: None,
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
            desk_agent_protocol::exec::ExecRequestId("target-1".into()),
            "a1",
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
