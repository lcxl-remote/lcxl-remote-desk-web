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

async fn apply_worker_locale_ack(
    settings: &web::Data<SharedSettings>,
    host_control_hub: &HostControlHub,
    locale: &str,
) -> Result<(), String> {
    let locale = crate::locale::canonicalize(locale)
        .ok_or_else(|| format!("worker acknowledged unsupported locale {locale:?}"))?;
    {
        let mut settings = settings.write().await;
        settings.system.locale = Some(locale.to_string());
    }
    crate::locale::set_global_locale(locale)?;
    let _ = host_control_hub.send_command(
        crate::host_control::HostControlMessage::GlobalLocaleChanged {
            locale: locale.to_string(),
        },
    );
    Ok(())
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
    let host_activity = host_control_hub.host_activity();

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
                    RemoteAccessCentralLink::Local,
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
                        RemoteAccessCentralLink::RemoteSignal,
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
                        RemoteAccessCentralLink::Manager,
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
            // Daemon constructs the outbound private-screen state
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
            WorkerToService::FileManagerOpened(payload) => {
                host_activity.file_manager_opened(&payload.connection_id);
                log::info!(
                    "[SignalingProxy] file manager opened for {}",
                    payload.connection_id
                );
            }
            WorkerToService::FileTransferStarted(payload) => {
                host_activity.file_transfer_started(
                    &payload.connection_id,
                    &payload.transfer_id,
                    payload.direction,
                    &payload.file_name,
                    payload.total_bytes,
                );
                log::info!(
                    "[SignalingProxy] file transfer started for {}: {} {:?} {} bytes",
                    payload.connection_id,
                    payload.transfer_id,
                    payload.direction,
                    payload.total_bytes
                );
            }
            WorkerToService::FileTransferFinished(payload) => {
                host_activity.file_transfer_finished(&payload.connection_id, &payload.transfer_id);
                log::info!(
                    "[SignalingProxy] file transfer finished for {}: {} {:?}",
                    payload.connection_id,
                    payload.transfer_id,
                    payload.outcome
                );
            }
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
            WorkerToService::LocaleApplied(payload) => {
                if let Err(error) =
                    apply_worker_locale_ack(&settings, &host_control_hub, &payload.locale).await
                {
                    warn!("[SignalingProxy] failed to apply worker locale: {error}");
                }
            }
            // Route typed terminal events back to the matching browser connection.
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
                host_activity.terminal_started(&payload.connection_id);
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
                host_activity.terminal_closed(&payload.connection_id);
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
            WorkerToService::RemoteAccessStateApplied(payload) => {
                worker_mgr.complete_remote_access_ack(payload.clone());
                info!(
                    "[SignalingProxy] Worker applied remote-access state: operation_id={}, version={}, cancelled_terminals={}, cancelled_transfers={}, cancelled_execs={}",
                    payload.operation_id,
                    payload.state_version,
                    payload.cancelled_terminals,
                    payload.cancelled_transfers,
                    payload.cancelled_execs,
                );
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

mod responses;
use responses::*;

mod connection_policy;
use connection_policy::*;

mod connection_loop;
use connection_loop::*;

mod remote_access_link;
use remote_access_link::*;

mod authorization;
use authorization::*;

mod central_authorization;
use central_authorization::*;

mod inbound;
use inbound::*;

#[cfg(test)]
mod tests;
