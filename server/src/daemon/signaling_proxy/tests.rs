use super::*;
use crate::daemon::pc_manager::PcRegistry;
use crate::host_control::HostControlHub;
use crate::model::settings::{Settings, SharedSettings};
use desk_signal_facade::model::request_remote_authz::REQUEST_REMOTE_AUTHZ_VERSION;
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{RequestRemoteModel, SignalingModel, SignalingType};

const RR_AUDIENCE: &str = "host-client-abc";
const RR_NOW: &str = "2026-01-01T00:00:00Z";

#[tokio::test]
async fn worker_locale_ack_converges_daemon_and_broadcasts_to_tauri_shells() {
    let settings = web::Data::new(SharedSettings::from(Settings::default()));
    let hub = HostControlHub::new_local();
    let mut outbound = hub.subscribe_outbound();

    apply_worker_locale_ack(&settings, &hub, "en-US")
        .await
        .unwrap();

    assert_eq!(
        settings.read().await.system.locale.as_deref(),
        Some("en-US")
    );
    assert_eq!(crate::locale::current_locale(), "en-US");
    assert!(matches!(
        outbound.try_recv(),
        Ok(crate::host_control::HostControlMessage::GlobalLocaleChanged { locale })
            if locale == "en-US"
    ));
}

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
        guard_and_clean_signaling_url("\u{7f}ws://203.0.113.5/api/desk/signaling", true).is_err(),
        "control-prefixed public plaintext literal must be refused"
    );
    // A clean legitimate wss literal / domain passes and returns the dial URL.
    assert_eq!(
        guard_and_clean_signaling_url("  wss://sig.example/api/desk/signaling  ", true).as_deref(),
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
        actor: desk_signal_facade::model::request_remote_authz::ActorSummary::unknown(),
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
fn remote_access_central_link_uses_service_daemon_loopback() {
    assert_eq!(
        select_remote_access_central_link(false, false, &StartupMode::Default),
        RemoteAccessCentralLink::Local
    );
    assert_eq!(
        select_remote_access_central_link(false, false, &StartupMode::ServiceDaemon),
        RemoteAccessCentralLink::Local
    );
    assert_eq!(
        select_remote_access_central_link(false, false, &StartupMode::DeskServer),
        RemoteAccessCentralLink::None
    );
    assert_eq!(
        select_remote_access_central_link(false, true, &StartupMode::ServiceDaemon),
        RemoteAccessCentralLink::RemoteSignal
    );
    assert_eq!(
        select_remote_access_central_link(true, true, &StartupMode::ServiceDaemon),
        RemoteAccessCentralLink::Manager
    );
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
        session_approvals: Arc::new(crate::daemon::session_approval::SessionApprovalStore::new()),
        command_templates: Arc::new(crate::daemon::command_templates::CommandTemplateCache::new()),
        command_blocklist: Arc::new(crate::daemon::command_blocklist::CommandBlocklistCache::new()),
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
    handle_inbound_signaling_text(text, &router_ctx, InboundSignalingSource::Local, false).await;
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
    handle_inbound_signaling_text(text, &router_ctx, InboundSignalingSource::Local, false).await;
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
    handle_inbound_signaling_text(text, &router_ctx, InboundSignalingSource::Local, false).await;
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
            containment: Default::default(),
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
                containment: Default::default(),
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
        containment: Default::default(),
    };
    let draft = desk_agent_protocol::exec_policy::build_exact_argv_draft(
        &template,
        None,
        desk_agent_protocol::exec_policy::DEFAULT_OUTPUT_BYTES,
        desk_agent_protocol::exec_policy::DEFAULT_OUTPUT_BYTES,
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
    let _model =
        build_virtual_display_response(payload, Some(&supervisor)).expect("build success model");
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
    let _model =
        build_virtual_display_response(payload, Some(&supervisor)).expect("build success model");
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
/// Regression guard for the disabled-supervisor branch.
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
