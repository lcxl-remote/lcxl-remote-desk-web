use super::*;
use desk_signal_facade::model::media_capability::VideoEncoderId;

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
    // Seed the source PC with a deterministic epoch. Terminal display
    // commands are connection-scoped and are rejected when either the
    // source PC or its epoch is absent.
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let pc = ctx
        .pc_registry
        .create_for_request_remote(
            "conn-1",
            &request_remote,
            &crate::model::settings::Settings::default(),
        )
        .await
        .expect("seed display source PC");
    pc.write().await.connection_epoch = TEST_CONNECTION_EPOCH.to_string();
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
/// `AudioPlaybackFailed` (dead in daemon-worker mode) as
/// daemon-emitted / dead variants that must never reach the worker.
#[tokio::test]
pub(super) async fn route_swallows_daemon_emitted_variants() {
    let ctx = make_ctx().await;
    for t in [
        SignalingType::Answer,
        SignalingType::RemoteAccessInitialized,
        SignalingType::ControlAccepted,
        SignalingType::ControlDenied,
        SignalingType::PrivateScreenStateChanged,
        SignalingType::AudioPlaybackFailed,
        SignalingType::MediaPipelineStateChanged,
        SignalingType::SystemInfoRetrieved,
        SignalingType::TerminalOutputProduced,
        SignalingType::TerminalStarted,
        SignalingType::TerminalClosed,
        SignalingType::DesktopSwitching,
        SignalingType::DesktopReady,
        SignalingType::FetchConnections,
        SignalingType::ConnectionsFetched,
        SignalingType::SendHeartbeat,
        SignalingType::Error,
        SignalingType::Unknown,
    ] {
        let model = SignalingModel::new("r", t, None, None, None, None);
        assert!(route(&model, &ctx).await.is_ok(), "{t:?}");
    }
}

/// Pin behaviour: a stray inbound `ControlAccepted` (which would
/// be a protocol error from the browser, since the daemon emits
/// `ControlAccepted` outbound) is swallowed — `route` returns Ok
/// so the message never reaches the worker. The SignalingMessage
/// bridge is gone, so the only way for an inbound `ControlAccepted`
/// to leak through would be a new regression in `route()`'s match.
#[tokio::test]
pub(super) async fn route_inbound_accept_control_is_swallowed_not_bridged() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "stray-accept",
        SignalingType::ControlAccepted,
        Some("conn-z".to_string()),
        None,
        None,
        None,
    );
    route(&model, &ctx)
        .await
        .expect("ControlAccepted inbound must be swallowed, not surfaced as error");
}

#[tokio::test]
pub(super) async fn retry_media_pipeline_is_daemon_owned_and_reports_unknown_connection() {
    let (ctx, mut rx) = make_ctx_with_rx().await;
    let model = SignalingModel::new(
        "retry-unknown",
        SignalingType::RetryMediaPipeline,
        Some("missing-connection".to_string()),
        None,
        Some(
            serde_json::to_value(
                desk_signal_facade::model::remote_session::ConnectionEpochPayload {
                    connection_epoch: TEST_CONNECTION_EPOCH.to_string(),
                },
            )
            .unwrap(),
        ),
        None,
    );
    route(&model, &ctx).await.expect("retry route must respond");
    let response = read_response(&mut rx);
    assert_eq!(
        response.signaling_type,
        SignalingType::MediaPipelineRetryCompleted
    );
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
    // matching production where they follow the session's `RequestRemoteAccess`.
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
            SignalingType::SendTerminalInput,
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
        (SignalingType::ListTerminalCommands, serde_json::Value::Null),
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
        Ok(ServiceToWorker::StartTerminal(_))
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
        Ok(ServiceToWorker::StartTerminal(_))
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
        SignalingType::SendTerminalInput,
        SignalingType::ResizeTerminal,
        SignalingType::CloseTerminal,
        SignalingType::ListTerminalCommands,
    ] {
        let model = SignalingModel::new("req-noid", t, None, None, None, None);
        assert!(route(&model, &ctx).await.is_ok(), "{t:?}");
    }
}

/// Malformed `StartTerminal` body (not a `StartTerminalSession`
/// JSON object) must not crash the router — it should log + drop.
/// The `SendTerminalInput` / `ResizeTerminal` analogues take the
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
    route(&model, &ctx)
        .await
        .expect("stale finalized close must remain idempotent");
}

/// Manager-plane requests are handled inline by the
/// router (typed `ServiceToWorker::Manager*Request` IPC). With no
/// active worker the typed send is logged but the route call
/// itself still succeeds.
#[tokio::test]
pub(super) async fn route_manager_requests_handled_inline_not_bridged() {
    let ctx = make_ctx().await;
    let cases = [
        (SignalingType::GetSystemInfo, serde_json::Value::Null),
        (
            SignalingType::ListFiles,
            serde_json::to_value(desk_signal_facade::model::files::FileListParams {
                path: "C:\\".to_string(),
                page_no: 1,
                page_count: 50,
                ..Default::default()
            })
            .unwrap(),
        ),
        (
            SignalingType::DeleteFile,
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
    let t = SignalingType::GetSystemInfo;
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
        SignalingType::ListFiles,
        None,
        None,
        Some(serde_json::to_value(&params).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `ListTerminalCommands` is dispatched by
/// `signal-facade::controller::terminal::list_terminal` (REST GET)
/// without a `from_connection_id`. The router must forward it.
#[tokio::test]
pub(super) async fn route_list_terminal_without_connection_id_forwards() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "req-list-no-conn",
        SignalingType::ListTerminalCommands,
        None,
        None,
        None,
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// Malformed manager request bodies (e.g. `ListFiles` with
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
        SignalingType::ListFiles,
        Some("conn-fl".to_string()),
        None,
        Some(serde_json::json!("not file list params")),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `SetPrivateScreenVisibility` is handled inline by the router
/// (typed [`ServiceToWorker::SetPrivateScreenVisibility`] IPC). With no
/// active worker the typed send is logged but the route call
/// itself still succeeds.
#[tokio::test]
pub(super) async fn route_enable_private_screen_handled_inline_not_bridged() {
    let ctx = make_ctx().await;
    // SetPrivateScreenVisibility is a connection-scoped capability frame — admit the
    // connection so door1 passes it to the inline handler.
    ctx.pc_registry
        .record_admission("conn-priv", pc_manager::Admission::OwnerFull)
        .await;
    let data =
        desk_signal_facade::model::private_screen::SetPrivateScreenVisibilityData { visible: true };
    let model = SignalingModel::new(
        "r-eps",
        SignalingType::SetPrivateScreenVisibility,
        Some("conn-priv".to_string()),
        None,
        Some(serde_json::to_value(&data).unwrap()),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

/// `SetPrivateScreenVisibility` arriving without a `from_connection_id`
/// is a malformed message — daemon logs and drops, no panic, no
/// IPC send.
#[tokio::test]
pub(super) async fn route_enable_private_screen_without_connection_id_is_noop() {
    let ctx = make_ctx().await;
    let data = desk_signal_facade::model::private_screen::SetPrivateScreenVisibilityData {
        visible: false,
    };
    let model = SignalingModel::new(
        "r-eps-noid",
        SignalingType::SetPrivateScreenVisibility,
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

/// The adaptive-bitrate toggle is connection-scoped: browser A
/// turning it off must clear only A's cap; B's controller keeps
/// its cap and stays enabled (a fan-out would let one browser's
/// preference disable every other session — see the handler doc).
#[tokio::test]
pub(super) async fn apply_remote_settings_adaptive_bitrate_scopes_to_source_connection() {
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
    ctx.pc_registry
        .record_admission("conn-a", pc_manager::Admission::OwnerFull)
        .await;
    ctx.pc_registry
        .record_admission("conn-b", pc_manager::Admission::OwnerFull)
        .await;

    // Keep this fixture video-only so the test exercises only the adaptive
    // bitrate toggle instead of also entering an audio Stop transition.
    {
        let mut pc = ctx_a.write().await;
        pc.host_settings.audio_capture = None;
        pc.host_settings.audio_device = None;
        pc.host_settings.audio_encoder = None;
        pc.host_settings.image_capture = Some("default".to_string());
        pc.host_settings.video_encoder = Some("X264".to_string());
    }

    // Both connections currently run with a committed cap.
    for c in [&ctx_a, &ctx_b] {
        let shared = std::sync::Arc::clone(&c.read().await.adaptive_bitrate);
        let mut state = shared.state.lock().await;
        let _ = state.set_enabled_and_decide_clear(true);
        state.commit(CapDirective::SetCap(5_000), std::time::Instant::now());
    }

    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;

    // Browser A disables adaptive bitrate through a full session apply.
    let epoch_a = ctx_a.read().await.connection_epoch.clone();
    let host = ctx_a.read().await.host_settings.clone();
    let video_encoder = host
        .video_encoder
        .as_deref()
        .and_then(desk_signal_facade::model::media_capability::VideoEncoderId::from_setting_name)
        .unwrap_or(desk_signal_facade::model::media_capability::VideoEncoderId::X264);
    let settings = desk_signal_facade::model::remote_session::RemoteSessionSettings {
        image_capture: host.image_capture.clone().unwrap_or_default(),
        video_device_name: host.video_device_name.clone(),
        show_mouse: host.show_mouse,
        video_encoder,
        video_quality: host.video_quality,
        video_fps: host.video_fps,
        enable_dirty_rect: host.enable_dirty_rect,
        adaptive_bitrate: false,
        audio: None,
    };
    let start = desk_ipc_protocol::message::StartMediaPayload {
        connection_id: "conn-a".to_string(),
        connection_epoch: epoch_a.clone(),
        video_generation: 1,
        audio_generation: 1,
        video_codec: MediaCodec::H264,
        video_encoder,
        video_device: None,
        fps: host.video_fps,
        bitrate_kbps: 0,
        quality: host.video_quality,
        start_video: true,
        audio: None,
        image_capture: host.image_capture.clone().unwrap_or_default(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: host.enable_dirty_rect,
        show_mouse: host.show_mouse,
    };
    assert!(
        ctx_a
            .read()
            .await
            .install_initial_media(
                start,
                Some({
                    let mut baseline = settings.clone();
                    baseline.adaptive_bitrate = true;
                    baseline
                })
            )
            .await
    );
    let model = SignalingModel::new(
        "r-ab-scope",
        SignalingType::ApplyRemoteSessionSettings,
        Some("conn-a".to_string()),
        None,
        Some(
            serde_json::to_value(
                &desk_signal_facade::model::remote_session::ApplyRemoteSessionSettings {
                    connection_epoch: epoch_a,
                    settings,
                },
            )
            .unwrap(),
        ),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());

    // Exactly one clear IPC, addressed to conn-a. (Fresh PCs have
    // no cached_start_media, so the fps/quality fan-out is silent
    // and connection-scoped settings forwarding to the worker is typed
    // separately.)
    let clear = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(ServiceToWorker::UpdateMediaSettings(payload)) = ipc_rx.recv().await {
                break (payload.connection_id, payload.bitrate_kbps);
            }
        }
    })
    .await
    .expect("asynchronous 301 completion must emit the bitrate clear");
    assert_eq!(
        clear,
        ("conn-a".to_string(), Some(0)),
        "only the source connection may receive the clear"
    );
    assert!(
        ipc_rx.try_recv().is_err(),
        "conn-b must receive no directive"
    );

    // A: disabled + cap cleared. B: untouched.
    {
        let pc = ctx_a.read().await;
        let shared = std::sync::Arc::clone(&pc.adaptive_bitrate);
        let state = shared.state.lock().await;
        assert!(!state.enabled());
        assert_eq!(state.current_cap_kbps(), None);
        assert_eq!(
            pc.media_coordinator
                .lock()
                .await
                .accepted_baseline
                .as_ref()
                .map(|settings| settings.adaptive_bitrate),
            Some(false),
            "the accepted browser baseline must track the applied controller toggle"
        );
    }
    {
        let shared = std::sync::Arc::clone(&ctx_b.read().await.adaptive_bitrate);
        let state = shared.state.lock().await;
        assert!(state.enabled(), "conn-b must keep adaptive bitrate on");
        assert_eq!(state.current_cap_kbps(), Some(5_000));
    }
}

#[tokio::test]
pub(super) async fn codec_change_compares_the_accepted_offer_baseline() {
    let (ctx, mut outbound) = make_ctx_with_rx().await;
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let pc = ctx
        .pc_registry
        .create_for_request_remote(
            "conn-codec",
            &request_remote,
            &crate::model::settings::Settings::default(),
        )
        .await
        .expect("seed codec pc");
    ctx.pc_registry
        .record_admission("conn-codec", pc_manager::Admission::OwnerFull)
        .await;
    let epoch = pc.read().await.connection_epoch.clone();
    assert!(
        pc.read().await.host_settings.video_encoder.is_none(),
        "fixture must retain the host automatic default"
    );
    let baseline = desk_signal_facade::model::remote_session::RemoteSessionSettings {
        image_capture: "default".into(),
        video_device_name: String::new(),
        show_mouse: true,
        video_encoder: VideoEncoderId::Vp8,
        video_quality: 22,
        video_fps: 60,
        enable_dirty_rect: true,
        adaptive_bitrate: true,
        audio: None,
    };
    let start = desk_ipc_protocol::message::StartMediaPayload {
        connection_id: "conn-codec".into(),
        connection_epoch: epoch.clone(),
        video_generation: 1,
        audio_generation: 1,
        video_codec: MediaCodec::Vp8,
        video_encoder: VideoEncoderId::Vp8,
        video_device: None,
        fps: 60,
        bitrate_kbps: 0,
        quality: 22,
        start_video: true,
        audio: None,
        image_capture: "default".into(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: true,
        show_mouse: true,
    };
    assert!(
        pc.read()
            .await
            .install_initial_media(start, Some(baseline.clone()))
            .await
    );

    let mut requested = baseline;
    requested.video_encoder = VideoEncoderId::OpenH264;
    let model = SignalingModel::new(
        "r-codec-change",
        SignalingType::ApplyRemoteSessionSettings,
        Some("conn-codec".into()),
        None,
        Some(
            serde_json::to_value(
                &desk_signal_facade::model::remote_session::ApplyRemoteSessionSettings {
                    connection_epoch: epoch,
                    settings: requested,
                },
            )
            .unwrap(),
        ),
        None,
    );
    route(&model, &ctx).await.unwrap();

    let response = read_response(&mut outbound);
    let applied = response
        .get_data::<desk_signal_facade::model::remote_session::RemoteSessionSettingsApplied>()
        .unwrap();
    assert_eq!(
        applied.effects.connection,
        desk_signal_facade::model::remote_session::ConnectionSettingsEffect::NeedsReconnect
    );
    assert_eq!(applied.baseline_settings.video_encoder, VideoEncoderId::Vp8);
}

#[tokio::test]
pub(super) async fn same_wire_codec_restart_advances_video_output_fence() {
    let (ctx, mut outbound) = make_ctx_with_rx().await;
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let pc = ctx
        .pc_registry
        .create_for_request_remote(
            "conn-h264-impl",
            &request_remote,
            &crate::model::settings::Settings::default(),
        )
        .await
        .expect("seed H.264 implementation-switch PC");
    ctx.pc_registry
        .record_admission("conn-h264-impl", pc_manager::Admission::OwnerFull)
        .await;
    let epoch = pc.read().await.connection_epoch.clone();
    let baseline = desk_signal_facade::model::remote_session::RemoteSessionSettings {
        image_capture: "default".into(),
        video_device_name: String::new(),
        show_mouse: true,
        video_encoder: VideoEncoderId::X264,
        video_quality: 22,
        video_fps: 60,
        enable_dirty_rect: true,
        adaptive_bitrate: true,
        audio: None,
    };
    let start = desk_ipc_protocol::message::StartMediaPayload {
        connection_id: "conn-h264-impl".into(),
        connection_epoch: epoch.clone(),
        video_generation: 1,
        audio_generation: 1,
        video_codec: MediaCodec::H264,
        video_encoder: VideoEncoderId::X264,
        video_device: None,
        fps: 60,
        bitrate_kbps: 0,
        quality: 22,
        start_video: true,
        audio: None,
        image_capture: "default".into(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: true,
        show_mouse: true,
    };
    assert!(
        pc.read()
            .await
            .install_initial_media(start, Some(baseline.clone()))
            .await
    );
    {
        let guard = pc.read().await;
        let mut fence = guard.media_output_fence.write().await;
        fence.video_epoch = epoch.clone();
        fence.video_generation = 1;
    }
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;

    let mut requested = baseline;
    requested.video_encoder = VideoEncoderId::OpenH264;
    let model = SignalingModel::new(
        "r-h264-impl",
        SignalingType::ApplyRemoteSessionSettings,
        Some("conn-h264-impl".into()),
        None,
        Some(
            serde_json::to_value(
                &desk_signal_facade::model::remote_session::ApplyRemoteSessionSettings {
                    connection_epoch: epoch.clone(),
                    settings: requested,
                },
            )
            .unwrap(),
        ),
        None,
    );
    route(&model, &ctx).await.unwrap();

    let command = tokio::time::timeout(std::time::Duration::from_secs(1), ipc_rx.recv())
        .await
        .expect("video restart must be sent")
        .expect("worker lane must remain open");
    let ServiceToWorker::ApplyMediaSettings(command) = command else {
        panic!("expected video slot command");
    };
    assert!(matches!(
        command.action,
        desk_ipc_protocol::message::MediaSettingsAction::Restart {
            current_generation: 1,
            new_generation: 2,
            ..
        }
    ));
    {
        let guard = pc.read().await;
        let fence = guard.media_output_fence.read().await;
        assert_eq!(fence.video_epoch, epoch);
        assert_eq!(
            fence.video_generation, 2,
            "the replacement encoder's first IDR must pass the daemon output fence",
        );
    }

    assert!(
        ctx.pc_registry
            .record_media_pipeline_state(
                "conn-h264-impl",
                &epoch,
                2,
                desk_signal_facade::model::media_pipeline::MediaPipelineStateData {
                    phase: desk_signal_facade::model::media_pipeline::MediaPipelinePhase::Streaming,
                    encoder: Some(VideoEncoderId::OpenH264),
                    source_resolution: None,
                    compatible_encoders: vec![],
                    reason_code: None,
                    message: None,
                },
            )
            .await
    );
    let response = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let text = outbound.recv().await.expect("outbound settings response");
            let response: SignalingModel = serde_json::from_str(&text).unwrap();
            if response.signaling_type == SignalingType::RemoteSessionSettingsApplied {
                break response;
            }
        }
    })
    .await
    .expect("same-codec restart must complete");
    let applied = response
        .get_data::<desk_signal_facade::model::remote_session::RemoteSessionSettingsApplied>()
        .unwrap();
    assert_eq!(
        applied.effects.video,
        desk_signal_facade::model::remote_session::VideoSettingsEffect::Restarted,
    );
    assert_eq!(
        applied.baseline_settings.video_encoder,
        VideoEncoderId::OpenH264
    );
}

#[tokio::test]
pub(super) async fn audio_stop_compares_the_accepted_baseline_not_host_defaults() {
    let ctx = make_ctx().await;
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let pc = ctx
        .pc_registry
        .create_for_request_remote(
            "conn-audio-baseline",
            &request_remote,
            &crate::model::settings::Settings::default(),
        )
        .await
        .expect("seed audio pc");
    ctx.pc_registry
        .record_admission("conn-audio-baseline", pc_manager::Admission::OwnerFull)
        .await;
    let epoch = pc.read().await.connection_epoch.clone();
    assert!(
        pc.read().await.host_settings.audio_capture.is_none(),
        "fixture must retain host-global audio auto/default state"
    );
    let audio = desk_signal_facade::model::remote_session::AudioPipelineSettings {
        audio_capture: "WASAPI".into(),
        audio_device: desk_signal_facade::model::audio_capture::SelectedAudioDevice {
            audio_data_flow: desk_signal_facade::model::audio_capture::AudioDataFlow::Render,
            audio_device_id: None,
        },
        audio_encoder: desk_signal_facade::model::remote_session::AudioEncoderId::Opus,
    };
    let baseline = desk_signal_facade::model::remote_session::RemoteSessionSettings {
        image_capture: "default".into(),
        video_device_name: String::new(),
        show_mouse: true,
        video_encoder: VideoEncoderId::Vp8,
        video_quality: 22,
        video_fps: 60,
        enable_dirty_rect: true,
        adaptive_bitrate: true,
        audio: Some(audio.clone()),
    };
    let start = desk_ipc_protocol::message::StartMediaPayload {
        connection_id: "conn-audio-baseline".into(),
        connection_epoch: epoch.clone(),
        video_generation: 1,
        audio_generation: 3,
        video_codec: MediaCodec::Vp8,
        video_encoder: VideoEncoderId::Vp8,
        video_device: None,
        fps: 60,
        bitrate_kbps: 0,
        quality: 22,
        start_video: true,
        audio: Some(desk_ipc_protocol::message::StartAudioSettings {
            codec: MediaCodec::Opus,
            pipeline: audio,
        }),
        image_capture: "default".into(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: true,
        show_mouse: true,
    };
    assert!(
        pc.read()
            .await
            .install_initial_media(start, Some(baseline.clone()))
            .await
    );
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;

    let mut requested = baseline;
    requested.audio = None;
    let model = SignalingModel::new(
        "r-stop-audio",
        SignalingType::ApplyRemoteSessionSettings,
        Some("conn-audio-baseline".into()),
        None,
        Some(
            serde_json::to_value(
                &desk_signal_facade::model::remote_session::ApplyRemoteSessionSettings {
                    connection_epoch: epoch,
                    settings: requested,
                },
            )
            .unwrap(),
        ),
        None,
    );
    route(&model, &ctx).await.unwrap();

    let command = tokio::time::timeout(std::time::Duration::from_secs(1), ipc_rx.recv())
        .await
        .expect("audio stop must be sent")
        .expect("worker lane must remain open");
    let ServiceToWorker::ApplyMediaSettings(command) = command else {
        panic!("expected audio slot command");
    };
    assert_eq!(
        command.media_kind,
        desk_ipc_protocol::message::MediaKind::Audio
    );
    assert!(matches!(
        command.action,
        desk_ipc_protocol::message::MediaSettingsAction::Stop {
            target_generation: 3
        }
    ));
}

#[tokio::test]
pub(super) async fn audio_prompt_does_not_block_the_signaling_handler() {
    let (ctx, mut outbound) = make_ctx_with_rx().await;
    // A live local subscriber makes Prompt genuinely wait for a user answer.
    let _approval_rx = ctx.host_control_hub.subscribe_outbound();
    ctx.host_control_hub.mark_tauri_connected();
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let pc = ctx
        .pc_registry
        .create_for_request_remote(
            "conn-prompt",
            &request_remote,
            &crate::model::settings::Settings::default(),
        )
        .await
        .unwrap();
    ctx.pc_registry
        .record_admission("conn-prompt", pc_manager::Admission::OwnerFull)
        .await;
    let epoch = pc.read().await.connection_epoch.clone();
    let baseline = desk_signal_facade::model::remote_session::RemoteSessionSettings {
        image_capture: "default".into(),
        video_device_name: String::new(),
        show_mouse: true,
        video_encoder: VideoEncoderId::Vp8,
        video_quality: 22,
        video_fps: 60,
        enable_dirty_rect: true,
        adaptive_bitrate: true,
        audio: None,
    };
    {
        let mut pc = pc.write().await;
        pc.host_settings.image_capture = Some(baseline.image_capture.clone());
        pc.host_settings.video_device_name = baseline.video_device_name.clone();
        pc.host_settings.show_mouse = baseline.show_mouse;
        pc.host_settings.video_encoder = Some("VP8".into());
        pc.host_settings.video_quality = baseline.video_quality;
        pc.host_settings.video_fps = baseline.video_fps;
        pc.host_settings.enable_dirty_rect = baseline.enable_dirty_rect;
        pc.host_settings.adaptive_bitrate = baseline.adaptive_bitrate;
        pc.host_settings.audio_capture = None;
        pc.host_settings.audio_device = None;
        pc.host_settings.audio_encoder = None;
    }
    let start = desk_ipc_protocol::message::StartMediaPayload {
        connection_id: "conn-prompt".into(),
        connection_epoch: epoch.clone(),
        video_generation: 1,
        audio_generation: 1,
        video_codec: MediaCodec::Vp8,
        video_encoder: VideoEncoderId::Vp8,
        video_device: None,
        fps: 60,
        bitrate_kbps: 0,
        quality: 22,
        start_video: true,
        audio: None,
        image_capture: "default".into(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: true,
        show_mouse: true,
    };
    assert!(
        pc.read()
            .await
            .install_initial_media(start, Some(baseline.clone()))
            .await
    );
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;

    let mut requested = baseline;
    requested.audio = Some(
        desk_signal_facade::model::remote_session::AudioPipelineSettings {
            audio_capture: "WASAPI".into(),
            audio_device: desk_signal_facade::model::audio_capture::SelectedAudioDevice {
                audio_data_flow: desk_signal_facade::model::audio_capture::AudioDataFlow::Render,
                audio_device_id: None,
            },
            audio_encoder: desk_signal_facade::model::remote_session::AudioEncoderId::Opus,
        },
    );
    let model = SignalingModel::new(
        "r-audio-prompt",
        SignalingType::ApplyRemoteSessionSettings,
        Some("conn-prompt".into()),
        None,
        Some(
            serde_json::to_value(
                &desk_signal_facade::model::remote_session::ApplyRemoteSessionSettings {
                    connection_epoch: epoch,
                    settings: requested,
                },
            )
            .unwrap(),
        ),
        None,
    );

    tokio::time::timeout(std::time::Duration::from_millis(100), route(&model, &ctx))
        .await
        .expect("router must return while Prompt remains pending")
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while ctx.host_control_hub.pending_replay_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background apply must reach the local approval hub");
    assert_eq!(ctx.host_control_hub.pending_replay_count(), 1);
    let replay = ctx.host_control_hub.replay_messages_for_tauri();
    let crate::host_control::HostControlMessage::SecurityApprovalRequest { req_id, .. } =
        &replay[0]
    else {
        panic!("unexpected approval replay: {:?}", replay[0]);
    };
    assert!(ctx.host_control_hub.submit_approval(
        req_id,
        crate::host_control::ApprovalResponse {
            approved: false,
            remember: false,
        },
    ));

    let applied = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let text = outbound.recv().await.expect("outbound 301 result");
            let response: SignalingModel = serde_json::from_str(&text).unwrap();
            if response.signaling_type == SignalingType::RemoteSessionSettingsApplied {
                break response
                    .get_data::<desk_signal_facade::model::remote_session::RemoteSessionSettingsApplied>()
                    .unwrap();
            }
        }
    })
    .await
    .expect("denied prompt must still terminate the 301");
    assert_eq!(applied.errors.len(), 1);
    assert_eq!(applied.errors[0].field, "audio");
    assert_eq!(applied.errors[0].code, DeskErrorCode::PERMISSION_ERROR);
    assert!(
        ipc_rx.try_recv().is_err(),
        "permission denial must not send media work or trigger recovery"
    );
    let pc_guard = pc.read().await;
    let coordinator = pc_guard.media_coordinator.lock().await;
    assert_eq!(coordinator.audio.lifecycle, MediaSlotLifecycle::Stable);
    assert_eq!(coordinator.audio.generation, 1);
    assert!(coordinator.current_apply_request_id.is_none());
}

#[tokio::test]
pub(super) async fn adaptive_quality_command_updates_only_runtime_override() {
    let ctx = make_ctx().await;
    let request_remote = desk_signal_facade::model::signal::RequestRemoteModel {
        requested_wayland_control_mode: Some("auto".to_string()),
        purpose: RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let local_settings = crate::model::settings::Settings::default();
    let pc = ctx
        .pc_registry
        .create_for_request_remote("conn-quality", &request_remote, &local_settings)
        .await
        .expect("seed quality pc");
    ctx.pc_registry
        .record_admission("conn-quality", pc_manager::Admission::OwnerFull)
        .await;
    let epoch = pc.read().await.connection_epoch.clone();
    let baseline = desk_signal_facade::model::remote_session::RemoteSessionSettings {
        image_capture: "default".to_string(),
        video_device_name: "display".to_string(),
        show_mouse: true,
        video_encoder: desk_signal_facade::model::media_capability::VideoEncoderId::X264,
        video_quality: 22,
        video_fps: 60,
        enable_dirty_rect: true,
        adaptive_bitrate: true,
        audio: None,
    };
    let start = desk_ipc_protocol::message::StartMediaPayload {
        connection_id: "conn-quality".to_string(),
        connection_epoch: epoch.clone(),
        video_generation: 7,
        audio_generation: 1,
        video_codec: MediaCodec::H264,
        video_encoder: desk_signal_facade::model::media_capability::VideoEncoderId::X264,
        video_device: Some("display".to_string()),
        fps: 60,
        bitrate_kbps: 0,
        quality: 22,
        start_video: true,
        audio: None,
        image_capture: "default".to_string(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: true,
        show_mouse: true,
    };
    assert!(
        pc.read()
            .await
            .install_initial_media(start, Some(baseline))
            .await
    );
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    ctx.worker_mgr.install_active_for_test(ipc_tx).await;
    let model = SignalingModel::new(
        "quality-1",
        SignalingType::UpdateAdaptiveVideoQuality,
        Some("conn-quality".to_string()),
        None,
        Some(serde_json::json!({
            "connection_epoch": epoch,
            "video_quality": 17
        })),
        None,
    );

    route(&model, &ctx).await.expect("quality command routes");
    let ServiceToWorker::UpdateMediaSettings(update) = ipc_rx.recv().await.unwrap() else {
        panic!("expected a narrow media update");
    };
    assert_eq!(update.connection_id, "conn-quality");
    assert_eq!(update.video_generation, 7);
    assert_eq!(update.quality, Some(17));
    assert!(update.fps.is_none());
    let coordinator = pc.read().await.media_coordinator.clone();
    assert_eq!(coordinator.lock().await.adaptive_quality_override, Some(17));
}

/// Malformed `ApplyRemoteSessionSettings` payload
/// object) must not crash the router — it should log and drop.
#[tokio::test]
pub(super) async fn route_apply_remote_session_settings_with_invalid_payload_is_dropped() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "r-bad",
        SignalingType::ApplyRemoteSessionSettings,
        None,
        None,
        Some(serde_json::json!("not an object")),
        None,
    );
    assert!(route(&model, &ctx).await.is_ok());
}

#[tokio::test]
pub(super) async fn door1_rejection_returns_a_correlated_protocol_error() {
    let (ctx, mut outbound) = make_ctx_with_rx().await;
    let model = SignalingModel::new(
        "unadmitted-settings",
        SignalingType::UpdateAdaptiveVideoQuality,
        Some("not-admitted".into()),
        None,
        Some(serde_json::json!({
            "connection_epoch": "stale",
            "video_quality": 20
        })),
        None,
    );

    route(&model, &ctx).await.unwrap();
    let response = read_response(&mut outbound);
    assert_eq!(response.request_id, "unadmitted-settings");
    assert_eq!(response.signaling_type, SignalingType::Error);
    assert_eq!(
        response.response_state.unwrap().error_code,
        DeskErrorCode::PERMISSION_ERROR.code()
    );
}

/// `CloseRemoteSession` against an empty registry doesn't error — the
/// daemon logs a warning and treats it as a no-op so a stale
/// close after a previous PC dispose does not surface as
/// a handler error to the caller.
#[tokio::test]
pub(super) async fn route_close_remote_session_empty_registry_is_ok() {
    let ctx = make_ctx().await;
    let model = SignalingModel::new(
        "r",
        SignalingType::CloseRemoteSession,
        Some("conn-x".to_string()),
        None,
        Some(
            serde_json::to_value(
                desk_signal_facade::model::remote_session::CloseRemoteSessionPayload {
                    connection_epoch: TEST_CONNECTION_EPOCH.to_string(),
                    finalize_logical_connection: true,
                },
            )
            .unwrap(),
        ),
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
        SignalingType::DisplaySettingsChanged as i32,
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
    seed_test_desktop_pc(&ctx, "conn-1").await;
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
    seed_test_desktop_pc(&ctx, "conn-1").await;
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
