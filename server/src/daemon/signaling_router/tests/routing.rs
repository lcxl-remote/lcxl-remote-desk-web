use super::*;

#[test]
fn offer_only_converts_expected_business_errors_to_responses() {
    assert!(is_offer_business_error(DeskErrorCode::INVALID_PARAMS));
    assert!(is_offer_business_error(
        DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED
    ));
    assert!(!is_offer_business_error(DeskErrorCode::SYSTEM_ERROR));
    assert!(!is_offer_business_error(DeskErrorCode::CLIENT_ID_NOT_FOUND));
}

/// `make_ctx` variant that installs an `Attached`-state supervisor
/// AND a mock IPC sink so the `ChangeDisplaySettings` auto-path
/// tests can both (a) reach the new auto-only logic past the
/// `is_active()` gate, and (b) observe what `send_to_worker`
/// actually dispatched. The returned `mpsc::UnboundedReceiver` is
/// the IPC stream the worker would have seen.
/// `virtual_display.enabled` is pre-flipped to `true` — otherwise
/// the FEATURE_UNAVAILABLE arm short-circuits before the auto
/// branch executes.
pub(super) async fn make_ctx_with_attached_supervisor() -> (
    RouterContext,
    broadcast::Receiver<String>,
    tokio::sync::mpsc::UnboundedReceiver<ServiceToWorker>,
) {
    let (mut ctx, rx) = make_ctx_with_rx().await;
    // Flip the system-level toggle on.
    ctx.settings.write().await.virtual_display.enabled = true;
    // Build an attached supervisor sharing the same worker_mgr the
    // ctx already holds, so `send_to_worker` and `pc_registry` both
    // route through consistent state.
    let supervisor =
        crate::daemon::virtual_display::VirtualDisplaySupervisor::new_attached_for_test(
            ctx.worker_mgr.clone(),
            "SWD\\Test\\Test",
        );
    ctx.virtual_display = Some(std::sync::Arc::new(supervisor));
    // Default to a single-client topology so auto requests reach the
    // throttle / IPC stage. The multi-client tests override this via
    // `set_test_len_extra` directly. The ChangeDisplaySettings test frames come
    // from a connection with no admission record → door1 treats it as an
    // un-admitted management frame and passes it (its own gates apply),
    // matching the pre-door1 behaviour.
    ctx.pc_registry.set_test_len_extra(1);
    // Wire a mock IPC sink so send_to_worker has somewhere to go.
    let (ipc_tx, ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;
    (ctx, rx, ipc_rx)
}

/// Variant of `make_ctx` that hands the caller a fresh
/// `outbound_rx` so the test can assert on the error response
/// the router emits via `outbound_tx`.
pub(super) async fn make_ctx_with_rx() -> (RouterContext, broadcast::Receiver<String>) {
    let mut ctx = make_ctx().await;
    let rx = ctx.outbound_tx.subscribe();
    // Drain any pre-existing receiver before the test starts so
    // we never see stale messages from earlier construction.
    let (new_tx, new_rx) = broadcast::channel::<String>(16);
    ctx.outbound_tx = new_tx;
    let _ = rx; // shadow the original
    (ctx, new_rx)
}

pub(super) fn read_response(rx: &mut broadcast::Receiver<String>) -> SignalingModel {
    let text = rx.try_recv().expect("expected outbound error response");
    serde_json::from_str::<SignalingModel>(&text).expect("response not valid JSON")
}

pub(super) fn make_change_display_settings_model(
    request_id: &str,
    payload: ChangeDisplaySettingsPayload,
) -> SignalingModel {
    SignalingModel::new(
        request_id,
        SignalingType::ChangeDisplaySettings,
        Some("conn-1".to_string()),
        None,
        Some(serde_json::to_value(payload).unwrap()),
        None,
    )
}

/// Daemon-emitted or dead inbound variants are swallowed — they
/// MUST NOT reach the worker (it has no PC to act on, and the
/// worker's `DeskSession::handle_message` would only return
/// `UNKNOWN_SIGNALING_TYPE` for the ones it can't handle and
/// bounce a confusing error to the browser).
///
/// The router swallows `ChangeDisplaySettings` (dead enum),
/// `PrivateScreenStateChanged` (worker → browser only), and
/// `AudioPlaybackError` (dead in daemon-worker mode) as
/// daemon-emitted / dead variants that must never reach the worker.
#[tokio::test]
pub(super) async fn route_swallows_daemon_emitted_variants() {
    let ctx = make_ctx().await;
    for t in [
        SignalingType::Answer,
        SignalingType::Init,
        SignalingType::AcceptControl,
        SignalingType::DenyControl,
        SignalingType::PrivateScreenStateChanged,
        SignalingType::AudioPlaybackError,
        SignalingType::MediaPipelineStateChanged,
        SignalingType::ManagerSystemStatue,
        SignalingType::ReplyFromTerminal,
        SignalingType::TerminalStarted,
        SignalingType::TerminalClosed,
        SignalingType::DesktopSwitching,
        SignalingType::DesktopReady,
        SignalingType::FetchConnections,
        SignalingType::ConnectionList,
        SignalingType::Heartbeat,
        SignalingType::Error,
        SignalingType::Unknown,
    ] {
        let model = SignalingModel::new("r", t, None, None, None, None);
        assert!(route(&model, &ctx).await.is_ok(), "{t:?}");
    }
}

/// Pin behaviour: a stray inbound `AcceptControl` (which would
/// be a protocol error from the browser, since the daemon emits
/// AcceptControl outbound) is swallowed — `route` returns Ok
/// so the message never reaches the worker. The SignalingMessage
/// bridge is gone, so the only way for an inbound `AcceptControl`
/// to leak through would be a new regression in `route()`'s match.
#[tokio::test]
pub(super) async fn route_inbound_accept_control_is_swallowed_not_bridged() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "stray-accept",
        SignalingType::AcceptControl,
        Some("conn-z".to_string()),
        None,
        None,
        None,
    );
    route(&model, &ctx)
        .await
        .expect("AcceptControl inbound must be swallowed, not surfaced as error");
}

#[tokio::test]
pub(super) async fn retry_media_pipeline_is_daemon_owned_and_reports_unknown_connection() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let model = SignalingModel::new(
        "retry-unknown",
        SignalingType::RetryMediaPipeline,
        Some("missing-connection".to_string()),
        None,
        None,
        None,
    );
    route(&model, &ctx).await.expect("retry route must respond");
    let response = read_response(&mut rx);
    assert_eq!(response.signaling_type, SignalingType::RetryMediaPipeline);
    assert_eq!(
        response.response_state.expect("error state").error_code,
        DeskErrorCode::CLIENT_ID_NOT_FOUND.code(),
    );
}

/// Every terminal-plane request type is handled
/// inline via typed `ServiceToWorker::*Request` IPC. Without an
/// active worker the typed send is logged but the route call
/// itself still succeeds.
#[tokio::test]
pub(super) async fn route_terminal_requests_handled_inline_not_bridged() {
    let ctx = make_ctx().await;
    // Terminal frames are connection-scoped capability frames: door1 only
    // admits them once the connection has an admission (here an owner one),
    // matching production where they follow the session's `RequestRemote`.
    ctx.pc_registry
        .record_admission("conn-term", pc_manager::Admission::OwnerFull)
        .await;
    let cases = [
        (
            SignalingType::StartTerminal,
            serde_json::to_value(desk_signal_facade::model::terminal::StartTerminalSession {
                command: "C:\\Windows\\System32\\cmd.exe".to_string(),
                device_id: None,
                grant_session_id: None,
            })
            .unwrap(),
        ),
        (
            SignalingType::SendDataToTerminal,
            serde_json::to_value(desk_signal_facade::model::terminal::TerminalInputData {
                content: "echo hi\n".to_string(),
            })
            .unwrap(),
        ),
        (
            SignalingType::ResizeTerminal,
            serde_json::to_value(desk_signal_facade::model::terminal::TerminalResizeData {
                rows: 30,
                cols: 100,
            })
            .unwrap(),
        ),
        (SignalingType::CloseTerminal, serde_json::Value::Null),
        (SignalingType::ListTerminal, serde_json::Value::Null),
    ];
    for (t, body) in cases {
        let signaling_data = if body.is_null() { None } else { Some(body) };
        let model = SignalingModel::new(
            "req-term",
            t,
            Some("conn-term".to_string()),
            None,
            signaling_data,
            None,
        );
        assert!(
            route(&model, &ctx).await.is_ok(),
            "{t:?} must succeed inline (no bridge fallback exists)",
        );
    }
}

/// A stamped owner `StartTerminal` on an un-admitted terminal WS connection
/// establishes the connection's admission (owner → `OwnerFull`) and marks it as
/// a terminal — the admission-establishing role that lets its later
/// SendData/Resize/Close frames pass door1.
#[tokio::test]
pub(super) async fn route_start_terminal_owner_stamp_records_admission_and_marks_terminal() {
    let mut ctx = make_ctx().await;
    let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(worker_tx).await;
    ctx.inbound_start_terminal_authz = Some(
        desk_signal_facade::model::request_remote_authz::RequestRemoteAuthz {
            version: desk_signal_facade::model::request_remote_authz::REQUEST_REMOTE_AUTHZ_VERSION,
            access_ceiling: None,
            grant_session_id: None,
            generation: 0,
            actor: desk_signal_facade::model::request_remote_authz::ActorSummary::unknown(),
            request_id: "rt".to_string(),
            audience: "aud".to_string(),
            expires_at: None,
        },
    );
    let model = SignalingModel::new(
        "rt",
        SignalingType::StartTerminal,
        Some("term-x".to_string()),
        None,
        Some(
            serde_json::to_value(desk_signal_facade::model::terminal::StartTerminalSession {
                command: "cmd.exe".to_string(),
                device_id: None,
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    );
    route(&model, &ctx).await.expect("ok");
    assert!(matches!(
        ctx.pc_registry.admission("term-x").await,
        Some(pc_manager::Admission::OwnerFull)
    ));
    assert!(ctx.pc_registry.is_terminal_connection("term-x").await);
    assert!(matches!(
        worker_rx.try_recv(),
        Ok(ServiceToWorker::StartTerminalRequest(_))
    ));
    // A bare frame (owner-only relay, no stamp) admits as owner the same way.
    let mut ctx2 = make_ctx().await;
    let (worker_tx2, mut worker_rx2) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx2.worker_mgr.install_active_for_test(worker_tx2).await;
    ctx2.inbound_start_terminal_authz = None;
    route(&model, &ctx2).await.expect("ok");
    assert!(matches!(
        ctx2.pc_registry.admission("term-x").await,
        Some(pc_manager::Admission::OwnerFull)
    ));
    assert!(matches!(
        worker_rx2.try_recv(),
        Ok(ServiceToWorker::StartTerminalRequest(_))
    ));
}

#[tokio::test]
pub(super) async fn route_start_terminal_dispatch_failure_clears_capability_footprint() {
    let mut ctx = make_ctx().await;
    ctx.inbound_start_terminal_authz = Some(
        desk_signal_facade::model::request_remote_authz::RequestRemoteAuthz {
            version: desk_signal_facade::model::request_remote_authz::REQUEST_REMOTE_AUTHZ_VERSION,
            access_ceiling: None,
            grant_session_id: None,
            generation: 0,
            actor: desk_signal_facade::model::request_remote_authz::ActorSummary::unknown(),
            request_id: "rt-failed".to_string(),
            audience: "aud".to_string(),
            expires_at: None,
        },
    );
    let model = SignalingModel::new(
        "rt-failed",
        SignalingType::StartTerminal,
        Some("term-failed".to_string()),
        None,
        Some(
            serde_json::to_value(desk_signal_facade::model::terminal::StartTerminalSession {
                command: "cmd.exe".to_string(),
                device_id: None,
                grant_session_id: None,
            })
            .unwrap(),
        ),
        None,
    );

    route(&model, &ctx)
        .await
        .expect("dispatch failure is handled");

    assert!(ctx.pc_registry.admission("term-failed").await.is_none());
    assert!(!ctx.pc_registry.is_terminal_connection("term-failed").await);
}

/// A `CloseTerminal` clears the terminal connection's whole capability
/// footprint: admission, terminal mark, and grant reverse-index (so a later
/// directed revocation cannot reach a stale id).
#[tokio::test]
pub(super) async fn route_close_terminal_clears_terminal_footprint() {
    let ctx = make_ctx().await;
    let ceiling = SecuritySettings {
        allow_terminal: Some(true),
        ..Default::default()
    };
    ctx.pc_registry
        .record_admission("term-c", pc_manager::Admission::Capped(ceiling))
        .await;
    ctx.pc_registry
        .index_grant_connection("GS-c", 0, "term-c")
        .await;
    ctx.pc_registry.mark_terminal_connection("term-c").await;
    let model = SignalingModel::new(
        "rc",
        SignalingType::CloseTerminal,
        Some("term-c".to_string()),
        None,
        None,
        None,
    );
    route(&model, &ctx).await.expect("ok");
    assert!(ctx.pc_registry.admission("term-c").await.is_none());
    assert!(!ctx.pc_registry.is_terminal_connection("term-c").await);
    assert!(
        ctx.pc_registry
            .connections_for_grant("GS-c")
            .await
            .is_empty()
    );
}

/// Terminal requests without a `from_connection_id` are protocol
/// errors — daemon logs and drops, no panic, no IPC send.
#[tokio::test]
pub(super) async fn route_terminal_request_without_connection_id_is_noop() {
    let ctx = make_ctx().await;
    for t in [
        SignalingType::StartTerminal,
        SignalingType::SendDataToTerminal,
        SignalingType::ResizeTerminal,
        SignalingType::CloseTerminal,
        SignalingType::ListTerminal,
    ] {
        let model = SignalingModel::new("req-noid", t, None, None, None, None);
        assert!(route(&model, &ctx).await.is_ok(), "{t:?}");
    }
}

/// Malformed `StartTerminal` body (not a `StartTerminalSession`
/// JSON object) must not crash the router — it should log + drop.
/// The `SendDataToTerminal` / `ResizeTerminal` analogues take the
/// `get_data_with_type` path which already returns `Ok(None)` on
/// missing data; this case verifies a parse-failure surface.
#[tokio::test]
pub(super) async fn route_start_terminal_with_invalid_payload_is_dropped() {
    let ctx = make_ctx().await;
    // Admit the connection so the frame reaches the payload-parse path rather
    // than being stopped at door1's un-admitted capability guard.
    ctx.pc_registry
        .record_admission("conn-term", pc_manager::Admission::OwnerFull)
        .await;
    let model = SignalingModel::new(
        "req-start-bad",
        SignalingType::StartTerminal,
        Some("conn-term".to_string()),
        None,
        Some(serde_json::json!("not start terminal session")),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// Manager-plane requests are handled inline by the
/// router (typed `ServiceToWorker::Manager*Request` IPC). With no
/// active worker the typed send is logged but the route call
/// itself still succeeds.
#[tokio::test]
pub(super) async fn route_manager_requests_handled_inline_not_bridged() {
    let ctx = make_ctx().await;
    let cases = [
        (SignalingType::ManagerSystemInfo, serde_json::Value::Null),
        (
            SignalingType::ManagerFileList,
            serde_json::to_value(desk_signal_facade::model::files::FileListParams {
                path: "C:\\".to_string(),
                page_no: 1,
                page_count: 50,
                ..Default::default()
            })
            .unwrap(),
        ),
        (
            SignalingType::ManagerFileDelete,
            serde_json::to_value(desk_signal_facade::model::files::DeleteFileRequest {
                file_path: "C:\\old.txt".to_string(),
                delete_permanently: Some(false),
            })
            .unwrap(),
        ),
    ];
    for (t, body) in cases {
        let signaling_data = if body.is_null() { None } else { Some(body) };
        let model = SignalingModel::new(
            "req-mgr",
            t,
            Some("conn-mgr".to_string()),
            None,
            signaling_data,
            None,
        );
        assert!(
            route(&model, &ctx).await.is_ok(),
            "{t:?} must ride typed IPC",
        );
    }
}

/// Internal non-file manager requests still allow request-id-only correlation.
#[tokio::test]
pub(super) async fn route_non_file_manager_request_without_connection_id_forwards() {
    let ctx = make_ctx().await;
    let t = SignalingType::ManagerSystemInfo;
    let model = SignalingModel::new("req-no-conn", t, None, None, None, None);
    assert!(
        route(&model, &ctx).await.is_ok(),
        "{t:?} must retain request-id-only routing",
    );
}

/// Interactive file requests without a trusted controller identity are dropped.
#[tokio::test]
pub(super) async fn route_manager_file_list_without_connection_id_is_dropped() {
    let ctx = make_ctx().await;
    let params = desk_signal_facade::model::files::FileListParams {
        path: "C:\\".to_string(),
        page_no: 1,
        page_count: 50,
        ..Default::default()
    };
    let model = SignalingModel::new(
        "req-fl-no-conn",
        SignalingType::ManagerFileList,
        None,
        None,
        Some(serde_json::to_value(&params).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `ListTerminal` is dispatched by
/// `signal-facade::controller::terminal::list_terminal` (REST GET)
/// without a `from_connection_id`. The router must forward it.
#[tokio::test]
pub(super) async fn route_list_terminal_without_connection_id_forwards() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "req-list-no-conn",
        SignalingType::ListTerminal,
        None,
        None,
        None,
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// Malformed manager request bodies (e.g. `ManagerFileList` with
/// non-`FileListParams` JSON) must not crash the router — they
/// should log + drop.
#[tokio::test]
pub(super) async fn route_manager_file_list_with_invalid_payload_is_dropped() {
    let ctx = make_ctx().await;
    // Admit the connection so the frame reaches the payload-parse path rather
    // than being stopped at door1's un-admitted capability guard.
    ctx.pc_registry
        .record_admission("conn-fl", pc_manager::Admission::OwnerFull)
        .await;
    let model = SignalingModel::new(
        "req-fl-bad",
        SignalingType::ManagerFileList,
        Some("conn-fl".to_string()),
        None,
        Some(serde_json::json!("not file list params")),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `EnablePrivateScreen` is handled inline by the router
/// (typed [`ServiceToWorker::EnablePrivateScreen`] IPC). With no
/// active worker the typed send is logged but the route call
/// itself still succeeds.
#[tokio::test]
pub(super) async fn route_enable_private_screen_handled_inline_not_bridged() {
    let ctx = make_ctx().await;
    // EnablePrivateScreen is a connection-scoped capability frame — admit the
    // connection so door1 passes it to the inline handler.
    ctx.pc_registry
        .record_admission("conn-priv", pc_manager::Admission::OwnerFull)
        .await;
    let data = desk_signal_facade::model::private_screen::EnablePrivateScreenData { enable: true };
    let model = SignalingModel::new(
        "r-eps",
        SignalingType::EnablePrivateScreen,
        Some("conn-priv".to_string()),
        None,
        Some(serde_json::to_value(&data).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `EnablePrivateScreen` arriving without a `from_connection_id`
/// is a malformed message — daemon logs and drops, no panic, no
/// IPC send.
#[tokio::test]
pub(super) async fn route_enable_private_screen_without_connection_id_is_noop() {
    let ctx = make_ctx().await;
    let data = desk_signal_facade::model::private_screen::EnablePrivateScreenData { enable: false };
    let model = SignalingModel::new(
        "r-eps-noid",
        SignalingType::EnablePrivateScreen,
        None,
        None,
        Some(serde_json::to_value(&data).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// A session-scoped `RevokeAccessGrant` (carrying a `grant_session_id`, as the
/// manager sends when the owner ends a single support session) tears down exactly
/// that grant's connections, not a whole generation range.
#[tokio::test]
pub(super) async fn route_revoke_access_grant_session_scoped_closes_only_that_grant() {
    use desk_signal_facade::model::access_grant::RevokeAccessGrantData;
    use desk_signal_facade::model::signal::RequestRemoteModel;

    let ctx = make_ctx().await;
    let s = crate::model::settings::Settings::default();
    let rr = RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: Some("GS-supp".to_string()),
    };
    // Two grant sessions live; only GS-supp is targeted.
    ctx.pc_registry
        .create_for_request_remote("conn-supp", &rr, &s)
        .await
        .expect("pc");
    ctx.pc_registry
        .index_grant_connection("GS-supp", 0, "conn-supp")
        .await;
    ctx.pc_registry
        .create_for_request_remote("conn-other", &rr, &s)
        .await
        .expect("pc");
    ctx.pc_registry
        .index_grant_connection("GS-other", 0, "conn-other")
        .await;

    let data = RevokeAccessGrantData {
        target_device: "pub-11".to_string(),
        revoked_generation: 0,
        grant_session_id: Some("GS-supp".to_string()),
        reason: "support_ended".to_string(),
    };
    let model = SignalingModel::new(
        "r-rag",
        SignalingType::RevokeAccessGrant,
        None,
        None,
        Some(serde_json::to_value(&data).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());

    // The targeted grant's connection is gone; the untargeted grant survives —
    // proving the session-scoped branch, not the generation sweep, ran.
    assert!(ctx.pc_registry.get("conn-supp").await.is_none());
    assert!(
        ctx.pc_registry
            .connections_for_grant("GS-supp")
            .await
            .is_empty()
    );
    assert!(ctx.pc_registry.get("conn-other").await.is_some());
}

/// `UpdateDeskSettings` is fully handled by the router —
/// it both fans out the typed `UpdateMediaSettings` IPC for the
/// encoder pipeline AND ships the full settings to the worker as
/// typed [`ServiceToWorker::UpdateDeskSettings`].
#[tokio::test]
pub(super) async fn route_update_desk_settings_handled_inline_not_bridged() {
    let ctx = make_ctx().await;
    let settings = desk_signal_facade::model::desk_settings::DeskSettings {
        video_fps: 45,
        video_quality: 33,
        ..desk_signal_facade::model::desk_settings::DeskSettings::default()
    };
    let model = SignalingModel::new(
        "r-update",
        SignalingType::UpdateDeskSettings,
        Some("conn-y".to_string()),
        None,
        Some(serde_json::to_value(&settings).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// The adaptive-bitrate toggle is connection-scoped: browser A
/// turning it off must clear only A's cap; B's controller keeps
/// its cap and stays enabled (a fan-out would let one browser's
/// preference disable every other session — see the handler doc).
#[tokio::test]
pub(super) async fn update_desk_settings_adaptive_bitrate_scopes_to_source_connection() {
    use crate::daemon::bitrate_controller::CapDirective;

    let ctx = make_ctx().await;
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let local_settings = crate::model::settings::Settings::default();
    let ctx_a = ctx
        .pc_registry
        .create_for_request_remote("conn-a", &request_remote, &local_settings)
        .await
        .expect("seed conn-a");
    let ctx_b = ctx
        .pc_registry
        .create_for_request_remote("conn-b", &request_remote, &local_settings)
        .await
        .expect("seed conn-b");

    // Both connections currently run with a committed cap.
    for c in [&ctx_a, &ctx_b] {
        let shared = std::sync::Arc::clone(&c.read().await.adaptive_bitrate);
        shared
            .state
            .lock()
            .await
            .commit(CapDirective::SetCap(5_000), std::time::Instant::now());
    }

    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;

    // Browser A disables adaptive bitrate via UpdateDeskSettings.
    let settings = desk_signal_facade::model::desk_settings::DeskSettings {
        adaptive_bitrate: false,
        ..desk_signal_facade::model::desk_settings::DeskSettings::default()
    };
    let model = SignalingModel::new(
        "r-ab-scope",
        SignalingType::UpdateDeskSettings,
        Some("conn-a".to_string()),
        None,
        Some(serde_json::to_value(&settings).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());

    // Exactly one clear IPC, addressed to conn-a. (Fresh PCs have
    // no cached_start_media, so the fps/quality fan-out is silent
    // and UpdateDeskSettings forwarding to the worker is typed
    // separately.)
    let mut clears = Vec::new();
    while let Ok(msg) = ipc_rx.try_recv() {
        if let ServiceToWorker::UpdateMediaSettings(p) = msg {
            clears.push((p.connection_id.clone(), p.bitrate_kbps));
        }
    }
    assert_eq!(
        clears,
        vec![("conn-a".to_string(), Some(0))],
        "only the source connection may receive the clear"
    );

    // A: disabled + cap cleared. B: untouched.
    {
        let shared = std::sync::Arc::clone(&ctx_a.read().await.adaptive_bitrate);
        let state = shared.state.lock().await;
        assert!(!state.enabled());
        assert_eq!(state.current_cap_kbps(), None);
    }
    {
        let shared = std::sync::Arc::clone(&ctx_b.read().await.adaptive_bitrate);
        let state = shared.state.lock().await;
        assert!(state.enabled(), "conn-b must keep adaptive bitrate on");
        assert_eq!(state.current_cap_kbps(), Some(5_000));
    }
}

/// Malformed `UpdateDeskSettings` payload (not a DeskSettings
/// object) must not crash the router — it should log and drop.
#[tokio::test]
pub(super) async fn route_update_desk_settings_with_invalid_payload_is_dropped() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "r-bad",
        SignalingType::UpdateDeskSettings,
        None,
        None,
        Some(serde_json::json!("not an object")),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `CloseControl` against an empty registry doesn't error — the
/// daemon logs a warning and treats it as a no-op so a stale
/// CloseControl after a previous PC dispose does not surface as
/// a handler error to the caller.
#[tokio::test]
pub(super) async fn route_close_control_empty_registry_is_ok() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "r",
        SignalingType::CloseControl,
        Some("conn-x".to_string()),
        None,
        None,
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

// ============= Virtual display routing =============

/// ChangeDisplaySettings(205) must now classify as worker-owned;
/// it used to be in the daemon-swallow batch as a dead enum.
#[test]
pub(super) fn classify_change_display_settings_is_worker_owned() {
    assert_eq!(
        classify(SignalingType::ChangeDisplaySettings),
        RouteOwnership::Worker,
    );
}

/// Non-service-daemon modes leave `RouterContext::virtual_display`
/// at `None`; the router replies with `FEATURE_UNAVAILABLE` and
/// the "only supported in service mode" message.
#[tokio::test]
pub(super) async fn route_returns_error_when_supervisor_is_none() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let model = make_change_display_settings_model(
        "req-1",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: false,
        },
    );
    assert!(route(&model, &ctx).await.is_ok());
    let resp = read_response(&mut rx);
    let state = resp.response_state.expect("error response missing state");
    assert_eq!(state.error_code, DeskErrorCode::FEATURE_UNAVAILABLE.code());
    assert_eq!(
        state.message.as_deref(),
        Some("virtual display only supported in service mode")
    );
    assert_eq!(
        resp.signaling_type as i32,
        SignalingType::ChangeDisplaySettings as i32,
    );
    assert_eq!(resp.request_id, "req-1");
}

/// Service-daemon mode with the toggle off ⇒
/// `FEATURE_UNAVAILABLE` + "not enabled".
#[tokio::test]
pub(super) async fn route_returns_error_when_toggle_off() {
    let (mut ctx, mut rx) = make_ctx_with_rx().await;
    ctx.virtual_display = Some(Arc::new(VirtualDisplaySupervisor::new_disabled_for_test(
        ctx.worker_mgr.clone(),
    )));
    // settings.virtual_display.enabled defaults to false.
    let model = make_change_display_settings_model(
        "req-2",
        ChangeDisplaySettingsPayload {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            auto: false,
        },
    );
    assert!(route(&model, &ctx).await.is_ok());
    let resp = read_response(&mut rx);
    let state = resp.response_state.expect("error response missing state");
    assert_eq!(state.error_code, DeskErrorCode::FEATURE_UNAVAILABLE.code());
    assert_eq!(
        state.message.as_deref(),
        Some("virtual display not enabled")
    );
}

/// Toggle on but supervisor never reached the `Attached` state
/// (e.g. `lifecycle.create()` returned NotSupported on the stub
/// provider). Router must reply with `FEATURE_UNAVAILABLE` +
/// "unavailable" rather than letting the IPC fly into a dead
/// pipeline.
#[tokio::test]
pub(super) async fn route_returns_error_when_supervisor_inactive() {
    let (mut ctx, mut rx) = make_ctx_with_rx().await;
    ctx.virtual_display = Some(Arc::new(VirtualDisplaySupervisor::new_disabled_for_test(
        ctx.worker_mgr.clone(),
    )));
    ctx.settings.write().await.virtual_display.enabled = true;
    let model = make_change_display_settings_model(
        "req-3",
        ChangeDisplaySettingsPayload {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            auto: false,
        },
    );
    assert!(route(&model, &ctx).await.is_ok());
    let resp = read_response(&mut rx);
    let state = resp.response_state.expect("error response missing state");
    assert_eq!(state.error_code, DeskErrorCode::FEATURE_UNAVAILABLE.code());
    assert_eq!(
        state.message.as_deref(),
        Some("virtual display unavailable")
    );
}

/// Build a router context with an *active* supervisor
/// (`Attached` state). Used by the validation / dispatch tests
/// below — they need to push past the FEATURE_UNAVAILABLE gates.
pub(super) async fn make_ctx_with_active_supervisor() -> (RouterContext, broadcast::Receiver<String>)
{
    let (mut ctx, rx) = make_ctx_with_rx().await;
    let supervisor =
        VirtualDisplaySupervisor::new_attached_for_test(ctx.worker_mgr.clone(), "MOCK\\DISPLAY1");
    ctx.virtual_display = Some(Arc::new(supervisor));
    (ctx, rx)
}

/// Validation arm: width below the minimum dimension. Active
/// supervisor lets the request through the gates; validate_mode
/// fails inside the handler → INVALID_PARAMS.
#[tokio::test]
pub(super) async fn route_returns_error_on_invalid_mode() {
    let (ctx, mut rx) = make_ctx_with_active_supervisor().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    let model = make_change_display_settings_model(
        "req-invalid-mode",
        ChangeDisplaySettingsPayload {
            width: 100,
            height: 100,
            refresh_hz: 60,
            auto: false,
        },
    );
    assert!(route(&model, &ctx).await.is_ok());
    let resp = read_response(&mut rx);
    let state = resp.response_state.expect("error response missing state");
    assert_eq!(state.error_code, DeskErrorCode::INVALID_PARAMS.code());
    assert!(
        state
            .message
            .as_deref()
            .unwrap_or("")
            .starts_with("invalid mode:"),
        "expected 'invalid mode:' prefix, got {:?}",
        state.message
    );
}

/// Payload parse arm: width sent as a string instead of int.
/// Active supervisor lets the request through the gates; serde
/// parse fails → INVALID_PARAMS.
#[tokio::test]
pub(super) async fn route_returns_error_on_payload_parse_fail() {
    let (ctx, mut rx) = make_ctx_with_active_supervisor().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    let model = SignalingModel::new(
        "req-bad-payload",
        SignalingType::ChangeDisplaySettings,
        Some("conn-1".to_string()),
        None,
        Some(serde_json::json!({"width": "not an int"})),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
    let resp = read_response(&mut rx);
    let state = resp.response_state.expect("error response missing state");
    assert_eq!(state.error_code, DeskErrorCode::INVALID_PARAMS.code());
    assert!(
        state
            .message
            .as_deref()
            .unwrap_or("")
            .starts_with("bad ChangeDisplaySettings payload"),
        "expected 'bad ChangeDisplaySettings payload' prefix, got {:?}",
        state.message
    );
}

/// Worker-unavailable arm: validate_mode passes; worker_mgr's
/// send_to_worker fails because no worker is registered →
/// REMOTE_DESK_OFFLINE.
#[tokio::test]
pub(super) async fn route_returns_error_when_worker_unavailable() {
    let (ctx, mut rx) = make_ctx_with_active_supervisor().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    let model = make_change_display_settings_model(
        "req-no-worker",
        ChangeDisplaySettingsPayload {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            auto: false,
        },
    );
    assert!(route(&model, &ctx).await.is_ok());
    let resp = read_response(&mut rx);
    let state = resp.response_state.expect("error response missing state");
    assert_eq!(state.error_code, DeskErrorCode::REMOTE_DESK_OFFLINE.code());
    assert!(
        state
            .message
            .as_deref()
            .unwrap_or("")
            .starts_with("worker unavailable:"),
        "expected 'worker unavailable:' prefix, got {:?}",
        state.message
    );
}

/// Successful dispatch — supervisor active, toggle on, payload
/// valid, worker reachable. The router emits no error response
/// (the worker's `WorkerToService::VirtualDisplayMode` will fan
/// out the real reply, but that path is wired in commit 7). The
/// test asserts on the classifier + that no error is emitted to
/// outbound_tx.
#[tokio::test]
pub(super) async fn route_dispatches_set_virtual_display_mode_with_valid_input() {
    // Build a router context wired to a live worker so
    // send_to_worker reports success rather than "No active
    // worker". We re-implement parts of make_ctx_with_rx to
    // attach a worker.
    let (mut ctx, mut rx) = make_ctx_with_rx().await;
    ctx.settings.write().await.virtual_display.enabled = true;
    // Live worker: hook a fake IPC sender into WorkerManager so
    // send_to_worker has a destination. The minimal version is
    // to start an in-process worker via a paired transport pair.
    let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(worker_tx).await;
    ctx.virtual_display = Some(Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
        ctx.worker_mgr.clone(),
        "MOCK\\DISPLAY1",
    )));
    let model = make_change_display_settings_model(
        "req-success",
        ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: false,
        },
    );
    assert!(route(&model, &ctx).await.is_ok());
    // No error response should land on outbound_tx.
    assert!(
        rx.try_recv().is_err(),
        "successful dispatch must not emit an error response on outbound_tx"
    );
    // The worker must see the typed IPC.
    let sent = worker_rx
        .try_recv()
        .expect("worker must have received SetVirtualDisplayMode IPC");
    match sent {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.request_id, "req-success");
            assert_eq!(p.width, 1920);
            assert_eq!(p.height, 1080);
            assert_eq!(p.refresh_hz, 60);
        }
        other => panic!("unexpected IPC: {other:?}"),
    }
}
