use super::manager_link_gate::ManagerLinkGate;
use super::manager_link_state::ManagerLinkState;
use super::pc_manager::PcRegistry;
use super::signaling_router::{self, RouterContext};
use super::support_link_state::SupportLinkState;
use super::virtual_display::VirtualDisplaySupervisor;
use super::worker_manager::{WorkerManager, WorkerMessageReceiver};
use crate::agent_adapter::redaction::RegexRedactor;
use crate::diagnose::DiagnoseOrchestrator;
use crate::diagnose::collector::AgentContextCollector;
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
    ERROR_CODE_MEDIA_TRANSPORT_STUCK, MediaKind, MediaSettingsAppliedPayload,
    MediaSettingsApplyOutcome, VirtualDisplayModeOutcome, WorkerToService,
};
use desk_signal_facade::model::{
    remote_session::{SystemAudioCaptureState, SystemAudioCaptureStateData},
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

fn take_edge_exec_correlation(
    pending: &std::sync::Mutex<std::collections::HashSet<String>>,
    request_id: &str,
) -> bool {
    pending
        .lock()
        .map(|mut pending| pending.remove(request_id))
        .unwrap_or(false)
}

fn emit_typed_signaling<T: serde::Serialize>(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    signaling_type: SignalingType,
    to_connection_id: Option<String>,
    payload: &T,
) {
    let value = match serde_json::to_value(payload) {
        Ok(value) => value,
        Err(error) => {
            warn!("[SignalingProxy] Failed to serialise {signaling_type:?} payload: {error}");
            return;
        }
    };
    let frame = SignalingModel::new(
        request_id,
        signaling_type,
        None,
        to_connection_id,
        Some(value),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(error) => {
            warn!("[SignalingProxy] Failed to serialise {signaling_type:?} frame: {error}");
        }
    }
}

fn audio_phase_is_authorized(
    expected_terminal: Option<desk_ipc_protocol::message::AudioPipelinePhase>,
    desired_active: bool,
    phase: desk_ipc_protocol::message::AudioPipelinePhase,
) -> bool {
    if phase == desk_ipc_protocol::message::AudioPipelinePhase::Active && !desired_active {
        return false;
    }
    expected_terminal != Some(desk_ipc_protocol::message::AudioPipelinePhase::Off)
        || matches!(
            phase,
            desk_ipc_protocol::message::AudioPipelinePhase::Off
                | desk_ipc_protocol::message::AudioPipelinePhase::Failed
        )
}

fn reject_media_settings_command(
    coordinator: &mut crate::daemon::pc_manager::PerConnectionMediaCoordinator,
    payload: &MediaSettingsAppliedPayload,
) -> bool {
    if payload.outcome == MediaSettingsApplyOutcome::Accepted
        || payload
            .source_request_id
            .as_deref()
            .is_some_and(|request_id| {
                coordinator.current_apply_request_id.as_deref() != Some(request_id)
            })
    {
        return false;
    }
    match payload.media_kind {
        MediaKind::Video if coordinator.video.generation == payload.generation => {
            coordinator.video.lifecycle = crate::daemon::pc_manager::MediaSlotLifecycle::Stable;
            coordinator.video.pending_generation = None;
            if coordinator
                .video_terminal_waiter
                .as_ref()
                .is_some_and(|(generation, _)| *generation == payload.generation)
                && let Some((_, waiter)) = coordinator.video_terminal_waiter.take()
            {
                let _ = waiter.send(Err(payload.outcome));
            }
            true
        }
        MediaKind::Audio if coordinator.audio.generation == payload.generation => {
            coordinator.audio.lifecycle = crate::daemon::pc_manager::MediaSlotLifecycle::Stable;
            coordinator.audio.pending_generation = None;
            coordinator.audio_expected_terminal = None;
            coordinator.audio_desired_active = false;
            if coordinator
                .audio_terminal_waiter
                .as_ref()
                .is_some_and(|(generation, _)| *generation == payload.generation)
                && let Some((_, waiter)) = coordinator.audio_terminal_waiter.take()
            {
                let _ = waiter.send(Err(payload.outcome));
            }
            true
        }
        _ => false,
    }
}

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
    settings_coordinator: Arc<crate::model::settings_coordinator::SettingsCoordinator>,
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
    inprocess_computer_use_broker: Option<
        Arc<crate::worker::agent::computer_use_broker::ComputerUseBroker>,
    >,
    // This host's durable exec ledger. Opened by the daemon entry point, which is
    // common to all three host forms, so every dispatch path has one.
    exec_ledger: Arc<crate::daemon::exec_ledger::ExecLedger>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Signaling proxy starting");
    let host_activity = host_control_hub.host_activity();

    let (outbound_tx, _seed_rx) = broadcast::channel::<String>(128);
    pc_registry.set_outbound_tx(outbound_tx.clone());
    let credential_scopes =
        crate::daemon::manager_credential_scope::ManagerCredentialScopeRegistry::default();
    pc_registry.set_manager_credential_scopes(credential_scopes.clone());

    // Operator command templates (built-in baseline ∪ manager-synced) are shared by
    // every inbound execution path so preview and dispatch see the same snapshot.
    let command_templates = Arc::new(crate::daemon::command_templates::CommandTemplateCache::new());
    let command_blocklist =
        Arc::new(crate::daemon::command_blocklist::CommandBlocklistCache::new());

    // The shared Provider read adapter is available wherever an in-process worker
    // can read locally (Default / DeskServer); ServiceDaemon leaves it `None`.
    let (diagnose_orchestrator, remote_read) = match settings.read().await.args.startup_mode {
        StartupMode::ServiceDaemon => (None, None),
        _ => {
            let audit: Arc<dyn AuditSink> = Arc::new(LogAuditSink);
            let agent = Arc::new(match inprocess_computer_use_broker.clone() {
                Some(broker) => LocalDeviceAgent::with_settings_and_broker(
                    settings.clone().into_inner(),
                    broker,
                )
                .with_audit(audit),
                None => {
                    LocalDeviceAgent::with_settings(settings.clone().into_inner()).with_audit(audit)
                }
            });
            let collector = Arc::new(AgentContextCollector::new(
                agent.clone(),
                settings.clone().into_inner(),
            ));
            let orchestrator = Arc::new(DiagnoseOrchestrator::new(
                collector,
                Arc::new(RegexRedactor::new()),
            ));
            // Serves a central read-tool call against the same in-process
            // agent, redacting fail-closed.
            let edge_read = Arc::new(crate::agent_adapter::remote_read::EdgeReadInvoker::new(
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
        admission_origin: crate::daemon::pc_manager::AdmissionOrigin::Local,
        manager_credential_link: None,
        outbound_tx: outbound_tx.clone(),
        settings: settings.clone(),
        policy: crate::model::policy_access::PolicyAccess::authoritative(Arc::clone(
            &settings_coordinator,
        )),
        host_control_hub: host_control_hub.clone(),
        worker_mgr: worker_mgr.clone(),
        // Some(...) only in service-daemon mode; in-process and
        // desk-server modes leave this None so the router replies
        // with FEATURE_UNAVAILABLE for every inbound
        // ChangeDisplaySettings.
        virtual_display: virtual_display.clone(),
        diagnose_orchestrator,
        remote_read: remote_read.clone(),
        // Confirmed execution is available wherever an in-process worker can
        // execute (Default / DeskServer), using the same in-process availability.
        exec_supported: remote_read.is_some(),
        exec_approvals: Arc::new(crate::daemon::exec_approval::PendingApprovalStore::new()),
        session_approvals: Arc::new(crate::daemon::session_approval::SessionApprovalStore::new()),
        command_templates: command_templates.clone(),
        command_blocklist: command_blocklist.clone(),
        // Audit sink: in fleet mode (a manager is configured) report events to
        // the manager for DB persistence; otherwise keep the local log sink.
        audit: audit_sink.clone(),
        // Per-call trusted-central authorization is injected by the inbound
        // dispatcher; the shared base context carries none.
        inbound_authz: None,
        inbound_request_remote_authz: None,
        inbound_start_terminal_authz: None,
        // Fleet exec correlation set, shared with the worker-message loop below so
        // a worker `ExecutionCompleted` for an in-flight fleet attempt is relayed to the
        // manager as a `EdgeExecResult`.
        edge_exec_pending: Default::default(),
        // On-demand temporary-support lifecycle, shared with the support loop.
        support_link_state: support_link_state.clone(),
    };

    let credential_expiry_handle = {
        let mut expiry_rx = credential_scopes.subscribe_expirations();
        let router_ctx = router_ctx.clone();
        actix_web::rt::spawn(async move {
            loop {
                match expiry_rx.recv().await {
                    Ok(expiry) => {
                        teardown_manager_members(
                            &router_ctx,
                            &expiry.members,
                            "manager-credential-proof-expired",
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        error!(
                            "[credential-proof] expiry consumer lagged by {skipped} events; \
                             credential teardown capacity is insufficient"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    let local_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
        let credential_scopes = credential_scopes.clone();
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
                let display_url = redact_token_in_url(&local_url);
                if let Err(error) = maintain_proxy_connection(
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
                    &credential_scopes,
                )
                .await
                {
                    warn!("[Proxy] Local signaling connection to {display_url} failed: {error}");
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    let remote_sig_handle = {
        let settings = settings.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
        let credential_scopes = credential_scopes.clone();
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
                        token.clone(),
                        rx,
                        InboundSignalingSource::RemoteSignaling,
                        false,
                        None,
                        None,
                        RemoteAccessCentralLink::RemoteSignal,
                        &credential_scopes,
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
        let credential_scopes = credential_scopes.clone();
        actix_web::rt::spawn(async move {
            let mut manager_reconnect_attempt = 0_u32;
            let mut suspended_recovery_attempt = 0_u32;
            let mut suspended_recovery = false;
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
                        token.clone(),
                        rx,
                        InboundSignalingSource::TrustedCentral,
                        true,
                        Some(manager_link_state.clone()),
                        Some(manager_link_gate.subscribe()),
                        RemoteAccessCentralLink::Manager,
                        &credential_scopes,
                    )
                    .await;

                    match outcome {
                        Ok(ProxyConnectionOutcome::FatalReject { .. }) => {
                            // Terminal token rejection cannot heal under the same
                            // credential. Park until explicit replacement/retry.
                            manager_link_state.await_retry().await;
                            manager_link_state.clear().await;
                            manager_reconnect_attempt = 0;
                            suspended_recovery_attempt = 0;
                            suspended_recovery = false;
                            continue;
                        }
                        Ok(ProxyConnectionOutcome::CredentialSuspended { .. }) => {
                            // Reversible owner/token state: retry the same token on
                            // a deliberately slow lane, never reissue it.
                            suspended_recovery = true;
                            let delay = next_suspended_recovery_delay(
                                &mut suspended_recovery_attempt,
                                rand::random(),
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        Ok(ProxyConnectionOutcome::Closed) => {
                            suspended_recovery = false;
                            suspended_recovery_attempt = 0;
                            manager_reconnect_attempt = 0;
                        }
                        Ok(ProxyConnectionOutcome::CredentialExpired) => {
                            suspended_recovery = false;
                            suspended_recovery_attempt = 0;
                            manager_reconnect_attempt = 0;
                            tokio::time::sleep(credential_expiry_reconnect_delay(
                                lease_expiry_reconnect_jitter_ms(rand::random()),
                            ))
                            .await;
                            continue;
                        }
                        Err(_) if suspended_recovery => {
                            let delay = next_suspended_recovery_delay(
                                &mut suspended_recovery_attempt,
                                rand::random(),
                            );
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        Err(_) => {}
                    }

                    let delay = manager_host_reconnect_delay(
                        manager_reconnect_attempt,
                        manager_reconnect_jitter_ms(rand::random()),
                    );
                    manager_reconnect_attempt = manager_reconnect_attempt.saturating_add(1);
                    tokio::time::sleep(delay).await;
                    continue;
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
                        Ok(model) => {
                            support_link_state
                                .begin_request(model.request_id.clone())
                                .await;
                            match serde_json::to_string(&model) {
                                Ok(text) => {
                                    let _ = outbound_tx.send(text);
                                }
                                Err(e) => {
                                    warn!("[support] failed to serialise RequestSupportCode: {e}")
                                }
                            }
                        }
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

    while let Some(worker_message) = worker_rx.recv().await {
        // Every IPC message — heartbeat, signaling, desktop change —
        // counts as a sign of life for the watchdog. Updating before
        // the match keeps the bookkeeping in one place and avoids the
        // watchdog firing on a worker that's actively talking but
        // hasn't happened to send a Heartbeat in the last interval.
        //
        // It counts for the worker that sent it, though, and only while that
        // worker is still the one running. Replacing a worker does not silence
        // it: what it had already queued arrives afterwards, and every handler
        // below assumes it is hearing from the worker in charge. A desktop
        // switch, a crash restart or a remote-access recycle would otherwise
        // let the outgoing worker overwrite its successor's capability
        // snapshot, order the successor replaced in turn, or stand in for the
        // successor's heartbeat while the successor has never spoken. When a
        // worker goes, everything it was doing goes with it — the daemon has
        // already cleared its capabilities and dropped its activity — so there
        // is nothing left for its backlog to say.
        if !worker_mgr
            .note_message_from(
                worker_message.worker_key.as_ref(),
                worker_message.incarnation,
            )
            .await
        {
            debug!(
                "[SignalingProxy] dropping a message from worker {} — it has been replaced",
                worker_message.incarnation
            );
            continue;
        }

        let resident_worker_key = worker_message.worker_key.clone();
        if let Some(key) = resident_worker_key.as_ref() {
            let interactive_output = matches!(
                &worker_message.message,
                WorkerToService::CursorData(_)
                    | WorkerToService::MediaPipelineState(_)
                    | WorkerToService::AudioPipelineStateChanged(_)
                    | WorkerToService::MediaSettingsApplied(_)
            );
            if interactive_output
                && !worker_mgr
                    .resident_worker_is_active_interactive(key, worker_message.incarnation)
            {
                debug!(
                    "[SignalingProxy] dropping inactive interactive output from resident worker {:?}",
                    key
                );
                continue;
            }
            let global_control = matches!(
                &worker_message.message,
                WorkerToService::Ready
                    | WorkerToService::Heartbeat(_)
                    | WorkerToService::Capabilities(_)
                    | WorkerToService::DesktopChanged(_)
                    | WorkerToService::InteractiveRouteApplied(_)
                    | WorkerToService::RemoteAccessStateApplied(_)
                    | WorkerToService::LocaleApplied(_)
                    | WorkerToService::SecurityPolicyApplied(_)
            );
            let connection_owned =
                worker_message
                    .message
                    .connection_id()
                    .is_some_and(|connection_id| {
                        worker_mgr.resident_worker_owns_connection(key, connection_id)
                    });
            if !global_control && !connection_owned {
                warn!(
                    "[SignalingProxy] dropping unbound or cross-session output from resident worker {:?}",
                    key
                );
                continue;
            }
        }

        match worker_message.message {
            WorkerToService::Ready => {
                info!("[SignalingProxy] Worker is Ready");
            }
            WorkerToService::WaylandPortalStatus(payload) => {
                worker_mgr.set_wayland_portal_snapshot(payload.snapshot);
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
                if let Some(key) = resident_worker_key.as_ref() {
                    if !worker_mgr.set_resident_worker_capabilities(key, caps).await {
                        debug!(
                            "[SignalingProxy] resident capabilities arrived after slot removal: {:?}",
                            key
                        );
                    }
                    continue;
                }
                worker_mgr.set_worker_capabilities(caps);
                // A fresh worker starts from the policy serialized into its Init
                // payload: the right values, but at sequence zero, which cannot
                // be compared with what the daemon has been counting. Restating
                // the current policy puts both sides back on one numbering.
                // Keyed off `Capabilities` rather than `Ready` because only the
                // named-pipe handshake sends `Ready` — an in-process worker
                // announces itself here.
                settings_coordinator.republish().await;
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
                if let Some(key) = resident_worker_key.as_ref() {
                    if !worker_mgr.resident_desktop_observation_is_current(
                        key,
                        worker_message.incarnation,
                        payload.observed_at_unix_ms,
                    ) {
                        debug!(
                            "[SignalingProxy] stale or standby resident worker {:?} observed desktop '{}' at {}; ignoring",
                            key, payload.name, payload.observed_at_unix_ms
                        );
                        continue;
                    }
                    // The switch waits for worker acknowledgements. Run it
                    // outside this reader loop so the acknowledgements can be
                    // consumed and matched below.
                    let manager = worker_mgr.clone();
                    let session = key.session.clone();
                    let desktop_name = payload.name;
                    tokio::spawn(async move {
                        match manager
                            .switch_interactive_desktop(&session, &desktop_name)
                            .await
                        {
                            Ok(epoch) => info!(
                                "[SignalingProxy] switched {:?} to desktop '{}' at route epoch {}",
                                session, desktop_name, epoch
                            ),
                            Err(error) => warn!(
                                "[SignalingProxy] refusing desktop switch for {:?} to '{}': {}",
                                session, desktop_name, error
                            ),
                        }
                    });
                    continue;
                }
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
            WorkerToService::InteractiveRouteApplied(payload) => {
                let Some(key) = resident_worker_key.as_ref() else {
                    warn!("[SignalingProxy] legacy worker sent an interactive-route ack");
                    continue;
                };
                if !worker_mgr.complete_interactive_route_ack(
                    key,
                    worker_message.incarnation,
                    payload.clone(),
                ) {
                    debug!(
                        "[SignalingProxy] stale/unexpected interactive-route ack from {:?} incarnation {} epoch {} active={}",
                        key, worker_message.incarnation, payload.route_epoch, payload.active
                    );
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
                let model = match payload.request_id.as_deref() {
                    Some(request_id) => SignalingModel::success_response(
                        request_id,
                        SignalingType::PrivateScreenVisibilitySet,
                        None,
                        Some(payload.connection_id.clone()),
                        Some(&payload.data),
                    ),
                    None => SignalingModel::new_request(
                        SignalingType::PrivateScreenStateChanged,
                        Some(payload.connection_id.clone()),
                        Some(&payload.data),
                    ),
                };
                match model {
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
            WorkerToService::MediaPipelineState(payload) => {
                if !pc_registry
                    .record_media_pipeline_state(
                        &payload.connection_id,
                        &payload.connection_epoch,
                        payload.video_generation,
                        payload.data.clone(),
                    )
                    .await
                {
                    debug!(
                        "[SignalingProxy] dropping MediaPipelineState for unknown connection {}",
                        payload.connection_id
                    );
                    continue;
                }
                match SignalingModel::new_request(
                    SignalingType::MediaPipelineStateChanged,
                    Some(payload.connection_id.clone()),
                    Some(&payload.data),
                ) {
                    Ok(model) => match serde_json::to_string(&model) {
                        Ok(text) => {
                            let _ = outbound_tx.send(text);
                        }
                        Err(e) => warn!(
                            "[SignalingProxy] Failed to serialise MediaPipelineStateChanged for {}: {e}",
                            payload.connection_id
                        ),
                    },
                    Err(e) => warn!(
                        "[SignalingProxy] Failed to build MediaPipelineStateChanged for {}: {e}",
                        payload.connection_id
                    ),
                }
            }
            WorkerToService::AudioPipelineStateChanged(payload) => {
                let Some(pc) = pc_registry.get(&payload.connection_id).await else {
                    debug!(
                        "[SignalingProxy] dropping audio state for unknown connection {}",
                        payload.connection_id
                    );
                    continue;
                };
                let pc = pc.read().await;
                if pc.connection_epoch != payload.connection_epoch {
                    continue;
                }
                let mut coordinator = pc.media_coordinator.lock().await;
                if coordinator.audio.generation != payload.audio_generation {
                    continue;
                }
                let expected_terminal = coordinator.audio_expected_terminal;
                if !audio_phase_is_authorized(
                    expected_terminal,
                    coordinator.audio_desired_active,
                    payload.phase,
                ) {
                    debug!(
                        "[SignalingProxy] ignoring late {:?} while audio generation {} is stopping for {}",
                        payload.phase, payload.audio_generation, payload.connection_id
                    );
                    continue;
                }
                coordinator.actual_audio_phase = Some(payload.phase);
                let terminal = matches!(
                    payload.phase,
                    desk_ipc_protocol::message::AudioPipelinePhase::Off
                        | desk_ipc_protocol::message::AudioPipelinePhase::Active
                        | desk_ipc_protocol::message::AudioPipelinePhase::Failed
                );
                let expected_terminal_reached = terminal
                    && (expected_terminal.is_none()
                        || expected_terminal == Some(payload.phase)
                        || payload.phase == desk_ipc_protocol::message::AudioPipelinePhase::Failed);
                if expected_terminal_reached {
                    coordinator.audio.lifecycle =
                        crate::daemon::pc_manager::MediaSlotLifecycle::Stable;
                    coordinator.audio.pending_generation = None;
                    coordinator.audio_expected_terminal = None;
                    if matches!(
                        payload.phase,
                        desk_ipc_protocol::message::AudioPipelinePhase::Off
                            | desk_ipc_protocol::message::AudioPipelinePhase::Failed
                    ) {
                        coordinator.audio_desired_active = false;
                    }
                }
                if coordinator
                    .audio_terminal_waiter
                    .as_ref()
                    .is_some_and(|(generation, _)| *generation == payload.audio_generation)
                    && expected_terminal_reached
                    && let Some((_, waiter)) = coordinator.audio_terminal_waiter.take()
                {
                    let _ = waiter.send(Ok(payload.phase));
                }
                let state = match payload.phase {
                    desk_ipc_protocol::message::AudioPipelinePhase::Off => {
                        SystemAudioCaptureState::Off
                    }
                    desk_ipc_protocol::message::AudioPipelinePhase::Starting => {
                        SystemAudioCaptureState::Starting
                    }
                    desk_ipc_protocol::message::AudioPipelinePhase::Active => {
                        SystemAudioCaptureState::Active
                    }
                    desk_ipc_protocol::message::AudioPipelinePhase::Restarting => {
                        SystemAudioCaptureState::Restarting
                    }
                    desk_ipc_protocol::message::AudioPipelinePhase::Failed => {
                        SystemAudioCaptureState::Failed
                    }
                };
                {
                    let mut fence = pc.media_output_fence.write().await;
                    fence.audio_open = state == SystemAudioCaptureState::Active
                        && coordinator.audio_desired_active;
                    fence.audio_epoch = payload.connection_epoch.clone();
                    fence.audio_generation = payload.audio_generation;
                }
                pc_registry.set_system_audio_capture_activity(
                    &payload.connection_id,
                    state == SystemAudioCaptureState::Active,
                );
                let accepted_audio = coordinator
                    .accepted_baseline
                    .as_ref()
                    .and_then(|settings| settings.audio.clone());
                drop(coordinator);
                let snapshot = SystemAudioCaptureStateData {
                    connection_epoch: payload.connection_epoch,
                    state,
                    accepted_audio,
                    resolved_audio_device_id: payload.resolved_audio_device_id,
                    error_code: payload
                        .error_code
                        .and_then(|code| serde_json::from_value(serde_json::json!(code)).ok()),
                };
                if let Ok(model) = SignalingModel::new_request(
                    SignalingType::SystemAudioCaptureStateChanged,
                    Some(payload.connection_id),
                    Some(&snapshot),
                ) && let Ok(text) = serde_json::to_string(&model)
                {
                    let _ = outbound_tx.send(text);
                }
            }
            WorkerToService::MediaSettingsApplied(payload) => {
                debug!(
                    "[SignalingProxy] media action {:?} for {} generation={} outcome={:?}",
                    payload.media_kind, payload.connection_id, payload.generation, payload.outcome
                );
                if payload.outcome != MediaSettingsApplyOutcome::Accepted
                    && let Some(pc) = pc_registry.get(&payload.connection_id).await
                {
                    let pc = pc.read().await;
                    if pc.connection_epoch != payload.connection_epoch {
                        continue;
                    }
                    let mut coordinator = pc.media_coordinator.lock().await;
                    reject_media_settings_command(&mut coordinator, &payload);
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
            WorkerToService::SystemInfoRetrieved(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "SystemInfoRetrieved",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::SystemInfoRetrieved,
                    Some(&payload.info),
                );
            }
            WorkerToService::FilesListed(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "FilesListed",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::FilesListed,
                    Some(&payload.response),
                );
            }
            WorkerToService::FileDeleted(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "FileDeleted",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::FileDeleted,
                    Option::<&()>::None,
                );
            }
            // Confirmation only. The daemon committed the locale before telling
            // the worker, so treating this as a value to write back would let a
            // slow acknowledgement undo a newer change that overtook it.
            WorkerToService::LocaleApplied(payload) => {
                debug!(
                    "[SignalingProxy] worker is running in {} (operation {})",
                    payload.locale, payload.operation_id
                );
            }
            WorkerToService::SecurityPolicyApplied(payload) => {
                if worker_mgr.note_policy_applied(&payload).await {
                    // The worker could not reconcile what it received and fell
                    // back to the stricter reading of the two. That is a safe
                    // place to sit but not one it can leave: the tightening
                    // pushed its own sequence past the daemon's, so every
                    // policy it already has looks stale to it. Restating the
                    // current one is what it is waiting for, and the mirror
                    // takes the first policy after a contradiction whatever the
                    // sequences say, so this converges in one round.
                    settings_coordinator.republish().await;
                }
            }
            // A user answered a worker-side prompt with "remember this". Only
            // the daemon can store it, and it applies the same staleness rule it
            // applies to its own prompts, so both roles converge on one decision.
            WorkerToService::RememberSecurityDecision(payload) => {
                settings_coordinator
                    .remember(
                        payload.capability,
                        payload.approved,
                        payload.expected_generation,
                    )
                    .await
                    .report(payload.capability);
            }
            // Route typed terminal events back to the matching browser connection.
            // Each `Terminal*` variant rebuilds the matching outbound
            // `SignalingType::*` model and writes it onto the
            // outbound channel for the WS sinks to ship to the
            // browser. `TerminalStarted` is a `success_response`
            // (StartTerminal correlation); `TerminalClosed` and
            // `TerminalOutputProduced` is a server-initiated `new_request`
            // notifications (no `request_id` correlation);
            // `TerminalCommandsListed` is a `success_response` for
            // `ListTerminalCommands`.
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
            WorkerToService::TerminalOutputProduced(payload) => {
                send_terminal_notification(
                    &outbound_tx,
                    "TerminalOutputProduced",
                    &payload.connection_id,
                    SignalingType::TerminalOutputProduced,
                    Some(&payload.data),
                );
            }
            WorkerToService::TerminalCommandsListed(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "TerminalCommandsListed",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::TerminalCommandsListed,
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
            // `SignalingType::AgentCapabilityCompleted` model carrying the
            // `AgentOutcome` verbatim as signaling_data and write it onto
            // the control end's signaling WS. Capability-level errors live
            // inside the `AgentOutcome::Err` (the response state stays a
            // transport-level success), so the control-end UI receives the
            // full structured `AgentError`. Mirrors the
            // manager-plane response rebuild.
            WorkerToService::AgentCapabilityCompleted(payload) => {
                send_manager_response(
                    &outbound_tx,
                    "AgentCapabilityCompleted",
                    &payload.request_id,
                    &payload.connection_id,
                    SignalingType::AgentCapabilityCompleted,
                    Some(&payload.outcome),
                );
            }
            // AI exec result: rebuild the outbound `SignalingType::ExecutionCompleted`
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
            WorkerToService::ExecutionCompleted(payload) => {
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
                // Fleet exec correlation: if this result is for an in-flight
                // fleet attempt, relay it to the manager as a `EdgeExecResult`
                // (`Executed`) instead of an `ExecutionCompleted(609)` toward a browser.
                let is_fleet =
                    take_edge_exec_correlation(&router_ctx.edge_exec_pending, &payload.request_id);
                if is_fleet {
                    signaling_router::send_edge_execution_completed(
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
                            SignalingType::ExecutionCompleted,
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
                                "[SignalingProxy] Failed to serialise ExecutionCompleted frame for \
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
            WorkerToService::ComputerActionStarted(payload) => {
                emit_typed_signaling(
                    &outbound_tx,
                    &payload.request_id,
                    SignalingType::ComputerActionStarted,
                    payload.connection_id,
                    &payload.started,
                );
            }
            WorkerToService::ComputerActionCompleted(payload) => {
                emit_typed_signaling(
                    &outbound_tx,
                    &payload.request_id,
                    SignalingType::ComputerActionCompleted,
                    payload.connection_id,
                    &payload.completed,
                );
            }
            WorkerToService::ComputerActionStateReported(payload) => {
                emit_typed_signaling(
                    &outbound_tx,
                    &payload.request_id,
                    SignalingType::ComputerActionStateReported,
                    payload.connection_id,
                    &payload.state,
                );
            }
            WorkerToService::ComputerUseReadinessUpdated(payload) => {
                let request_id = format!(
                    "computer-use-readiness:{}:{}:{}",
                    payload.readiness.interactive_session_incarnation,
                    payload.readiness.revision,
                    payload.readiness.observed_at,
                );
                emit_typed_signaling(
                    &outbound_tx,
                    &request_id,
                    SignalingType::ComputerUseReadinessUpdated,
                    None,
                    &payload.readiness,
                );
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
    credential_expiry_handle.abort();

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
