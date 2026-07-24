use super::*;
use crate::host_control::HubMode;
use desk_utils::error::DeskErrorCode;

fn payload_with(
    host_upstream_url: Option<String>,
    auth_token: Option<String>,
) -> WorkerInitPayload {
    WorkerInitPayload {
        session_id: "session-1".into(),
        os_session_id: 1,
        desktop_name: None,
        config_json: "{}".into(),
        signaling_url: None,
        auth_token,
        host_upstream_url,
        media_pipe_name: None,
        file_pipe_name: None,
        config_file_path: None,
        remote_access_locked: false,
        remote_access_state_version: 1,
    }
}

/// When the daemon supplies a host_upstream_url the worker constructs a
/// Forwarder hub and emits a spec the caller can spawn the ws task with.
#[tokio::test]
async fn build_hub_forwarder_when_url_present() {
    let payload = payload_with(
        Some("ws://127.0.0.1:8082/ws/host_upstream".into()),
        Some("ipc-token".into()),
    );
    let (hub, spec) = build_hub_from_init(&payload);
    assert_eq!(hub.mode(), HubMode::Forwarder);
    let (upstream, url, token) = spec.expect("Forwarder must yield an upstream spec");
    assert_eq!(url, "ws://127.0.0.1:8082/ws/host_upstream");
    assert_eq!(token, "ipc-token");
    // Upstream starts disconnected; hub should mirror that until the ws
    // task connects (which the test doesn't exercise).
    assert!(!upstream.is_connected());
}

/// Missing host_upstream_url falls back to a Local hub and yields no spec.
#[test]
fn build_hub_local_when_url_absent() {
    let payload = payload_with(None, None);
    let (hub, spec) = build_hub_from_init(&payload);
    assert_eq!(hub.mode(), HubMode::Local);
    assert!(spec.is_none());
}

/// Telemetry must initialize for the named-pipe SessionWorker path
/// (`shared_hub == None`, the worker is its own OS process) and must NOT
/// initialize for the in-process portable / DeskServer path
/// (`shared_hub == Some(_)`, the host process already installed the
/// global tracing subscriber). A double-init in the in-process path
/// would panic with `SetGlobalDefaultError`, which is exactly the bug
/// surfaced when portable mode tried to spawn an in-process worker.
#[test]
fn telemetry_init_skipped_when_shared_hub_present() {
    // In-process worker: host already inited → must skip.
    assert!(!should_init_worker_telemetry(true));
    // Named-pipe worker: separate process → must init.
    assert!(should_init_worker_telemetry(false));
}

/// Forwarder hub built without an auth token still works (passes empty
/// string to ws task — daemon will reject the handshake, which is the
/// intended fail-fast behaviour).
#[tokio::test]
async fn build_hub_forwarder_empty_token_when_auth_token_none() {
    let payload = payload_with(Some("ws://127.0.0.1:8082/ws/host_upstream".into()), None);
    let (_hub, spec) = build_hub_from_init(&payload);
    let (_, _, token) = spec.expect("spec must be present");
    assert_eq!(token, "");
}

/// Heartbeat task fires on every interval tick and stops when the writer
/// queue is closed. Uses a 50 ms real interval to keep the test fast while
/// still exercising the timing path (`tokio::time::advance` would require
/// the test-util feature which isn't enabled in regular dependencies).
#[tokio::test]
async fn heartbeat_task_emits_on_interval_until_queue_closed() {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
    let interval = tokio::time::Duration::from_millis(50);
    let task = spawn_heartbeat_task(tx, interval);

    // First two ticks must arrive within ~3 intervals worth of slack.
    let first = tokio::time::timeout(interval * 3, rx.recv())
        .await
        .expect("first heartbeat must arrive")
        .expect("queue closed unexpectedly");
    assert!(matches!(first, WorkerToService::Heartbeat(_)));

    let second = tokio::time::timeout(interval * 3, rx.recv())
        .await
        .expect("second heartbeat must arrive")
        .expect("queue closed unexpectedly");
    assert!(matches!(second, WorkerToService::Heartbeat(_)));

    // Closing the receiver causes the task to detect Err on send and exit.
    drop(rx);
    tokio::time::timeout(interval * 5, task)
        .await
        .expect("heartbeat task must exit after queue closes")
        .expect("task panicked");
}

/// Forwarder task drains the dispatcher-facing mpsc and pushes onto the
/// supplied [`EventSender`] in order, then exits when all senders are
/// dropped. Uses the in-process transport so the test stays fully sync
/// (no IO scheduling); the framed-transport path is exercised by the
/// `inproc_event_round_trips` / `framed_event_round_trips_through_duplex`
/// tests in `desk_ipc_protocol::dual_transport`.
#[tokio::test]
async fn event_forwarder_drains_queue_and_exits_when_senders_dropped() {
    use desk_ipc_protocol::dual_transport::inprocess;

    let (sender, mut receiver) = inprocess::make_event::<WorkerToService>();
    let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
    let task = spawn_event_forwarder_task(rx, sender);

    tx.send(WorkerToService::Ready).expect("send Ready");
    tx.send(WorkerToService::Heartbeat(HeartbeatPayload {
        timestamp_ms: 1,
        active_connections: 0,
        cpu_usage: None,
        memory_usage: None,
    }))
    .expect("send Heartbeat");
    drop(tx);

    let m1 = receiver.recv().await.expect("recv first message");
    assert!(matches!(m1, WorkerToService::Ready));
    let m2 = receiver.recv().await.expect("recv second message");
    assert!(matches!(m2, WorkerToService::Heartbeat(_)));

    tokio::time::timeout(tokio::time::Duration::from_secs(1), task)
        .await
        .expect("forwarder task must exit after senders drop")
        .expect("task panicked");
}

/// A `PrivateScreenStateChanged`
/// blob produced by `DeskSession`'s host-control-hub bridge is
/// classified into the typed `WorkerToService::PrivateScreenStateChanged`
/// variant, carrying the inner `PrivateScreenStateChangedData`
/// verbatim. This guards the rendering decision in
/// `build_outbound_payload_from_desk_text`.
#[test]
fn outbound_dispatch_routes_private_screen_state_changed_to_typed_variant() {
    let data = PrivateScreenStateChangedData {
        visible: true,
        is_supported: true,
        error_msg: None,
    };
    let model = SignalingModel::new_request(
        SignalingType::PrivateScreenStateChanged,
        Some("conn-pss".to_string()),
        Some(&data),
    )
    .expect("build PrivateScreenStateChanged model");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::PrivateScreenStateChanged(p) => {
            assert_eq!(p.connection_id, "conn-pss");
            assert!(p.data.visible);
            assert!(p.data.is_supported);
            assert!(p.data.error_msg.is_none());
        }
        other => panic!("PrivateScreenStateChanged must take the typed path, got {other:?}",),
    }
}

/// Error responses (any SignalingType, response_state
/// with non-zero error_code) all flow through the typed
/// `WorkerToService::SignalingError` catch-all. The daemon
/// rebuilds a `SignalingModel::error(...)` from this payload so
/// the browser sees the error response on its pending request.
#[test]
fn outbound_dispatch_routes_error_responses_to_typed_signaling_error() {
    // SignalingModel::error builds the canonical wire shape.
    let model = SignalingModel::error(
        "req-bad",
        SignalingType::StartTerminal,
        None,
        Some("conn-term".to_string()),
        DeskErrorCode::PERMISSION_ERROR,
        "Permission denied",
    )
    .expect("build error response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed error route") {
        WorkerToService::SignalingError(p) => {
            assert_eq!(p.request_id, "req-bad");
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(p.signaling_type, SignalingType::StartTerminal);
            assert_eq!(p.error_code, DeskErrorCode::PERMISSION_ERROR.code());
            assert_eq!(p.error_message.as_deref(), Some("Permission denied"));
        }
        other => panic!("expected SignalingError, got {other:?}"),
    }
}

/// Malformed JSON (a `service::signaling` bug or a wire
/// corruption) is logged + dropped now — there is no
/// SignalingMessage bridge to ferry it through. Returns `None`.
#[test]
fn outbound_dispatch_drops_malformed_signaling_text() {
    let raw = "not-a-signaling-model".to_string();
    assert!(
        build_outbound_payload_from_desk_text(raw).is_none(),
        "malformed JSON must drop, not surface as a typed variant",
    );
}

/// An unrecognised `SignalingType` (e.g. `Error`,
/// `Unknown`, or a brand-new variant the worker emitted before
/// the daemon learned about) is logged + dropped. Returns `None`.
/// This is a tightening of the previous SignalingMessage fallback.
#[test]
fn outbound_dispatch_drops_unrecognised_signaling_types() {
    let model = SignalingModel::new(
        "stray",
        SignalingType::Error,
        Some("conn-x".to_string()),
        None,
        Some(serde_json::json!({"code": -1, "message": "boom"})),
        None,
    );
    let text = serde_json::to_string(&model).expect("serialise");
    assert!(build_outbound_payload_from_desk_text(text).is_none());
}

/// A `ManagerSystemInfo` response (built by the worker's
/// `send_response`) gets routed onto
/// `WorkerToService::ManagerSystemInfoResponse` carrying the
/// `request_id`, `connection_id`, and the `SystemInfo` body
/// verbatim. This guards the typed-routing decision on the
/// happy path.
#[test]
fn outbound_dispatch_routes_manager_system_info_response_to_typed_variant() {
    let info = SystemInfo {
        name: Some("alice-pc".to_string()),
        is_admin: Some(true),
        ..SystemInfo::default()
    };
    let model = SignalingModel::success_response(
        "req-info-1",
        SignalingType::ManagerSystemInfo,
        None,
        Some("conn-info".to_string()),
        Some(&info),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::ManagerSystemInfoResponse(p) => {
            assert_eq!(p.request_id, "req-info-1");
            assert_eq!(p.connection_id.as_deref(), Some("conn-info"));
            assert_eq!(p.info.name.as_deref(), Some("alice-pc"));
            assert_eq!(p.info.is_admin, Some(true));
        }
        other => panic!("expected ManagerSystemInfoResponse, got {other:?}"),
    }
}

/// Empty-body responses (`ManagerFileDelete`,
/// `ManagerUpdateSettings`) ride
/// `WorkerToService::ManagerResponseRefPayload` — only the
/// `request_id` + `connection_id` matter. Verify both variants
/// route to the right enum tag.
#[test]
fn outbound_dispatch_routes_empty_body_manager_responses_to_typed_variants() {
    for (signaling_type, expected_variant) in [
        (
            SignalingType::ManagerFileDelete,
            "ManagerFileDeleteResponse",
        ),
        (
            SignalingType::ManagerUpdateSettings,
            "ManagerUpdateSettingsResponse",
        ),
    ] {
        let model = SignalingModel::success_response(
            "req-empty",
            signaling_type,
            None,
            Some("conn-empty".to_string()),
            Some(&()),
        )
        .expect("build response");
        let text = serde_json::to_string(&model).expect("serialise");
        let routed = build_outbound_payload_from_desk_text(text).expect("typed route");
        match (expected_variant, routed) {
            ("ManagerFileDeleteResponse", WorkerToService::ManagerFileDeleteResponse(p))
            | (
                "ManagerUpdateSettingsResponse",
                WorkerToService::ManagerUpdateSettingsResponse(p),
            ) => {
                assert_eq!(p.request_id, "req-empty");
                assert_eq!(p.connection_id.as_deref(), Some("conn-empty"));
            }
            (expected, other) => {
                panic!("expected {expected}, got {other:?}");
            }
        }
    }
}

/// A `TerminalStarted` blob built by the worker's
/// `handle_manager_terminal_start` (success_response with the
/// original request_id) gets routed onto
/// `WorkerToService::TerminalStarted` carrying the request_id +
/// connection_id; daemon's send_manager_response rebuilds it
/// back to the browser as a SignalingType::TerminalStarted
/// response with that same id.
#[test]
fn outbound_dispatch_routes_terminal_started_to_typed_variant() {
    let model = SignalingModel::success_response::<()>(
        "req-start",
        SignalingType::TerminalStarted,
        None,
        Some("conn-term".to_string()),
        None,
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::TerminalStarted(p) => {
            assert_eq!(p.request_id, "req-start");
            assert_eq!(p.connection_id, "conn-term");
        }
        other => panic!("expected TerminalStarted, got {other:?}"),
    }
}

/// `TerminalClosed` is a server-initiated notification
/// (`new_request`) — `request_id` is auto-minted, no correlation
/// needed; the typed payload carries only `connection_id`.
#[test]
fn outbound_dispatch_routes_terminal_closed_to_typed_variant() {
    let model = SignalingModel::new_request::<()>(
        SignalingType::TerminalClosed,
        Some("conn-term".to_string()),
        None,
    )
    .expect("build new_request");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::TerminalClosed(p) => {
            assert_eq!(p.connection_id, "conn-term");
        }
        other => panic!("expected TerminalClosed, got {other:?}"),
    }
}

/// `ReplyFromTerminal` is the high-frequency PTY-output
/// path. The PTY reader thread builds it via `new_request` with a
/// `TerminalOutputData` body; verify the body survives the typed
/// route + the connection_id is read from `to_connection_id`
/// (server-initiated request, target browser is the destination).
#[test]
fn outbound_dispatch_routes_reply_from_terminal_to_typed_variant() {
    let body = TerminalOutputData {
        content: "hello\r\nworld\r\n".to_string(),
    };
    let model = SignalingModel::new_request(
        SignalingType::ReplyFromTerminal,
        Some("conn-term".to_string()),
        Some(&body),
    )
    .expect("build new_request");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::ReplyFromTerminal(p) => {
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(p.data.content, "hello\r\nworld\r\n");
        }
        other => panic!("expected ReplyFromTerminal, got {other:?}"),
    }
}

/// `ListTerminal` response carries the `TerminalList` in
/// the body. `handle_list_terminals` uses `send_response` which
/// writes `to_connection_id` + the original request_id.
#[test]
fn outbound_dispatch_routes_list_terminal_to_typed_variant() {
    let terminals = TerminalList {
        commands: vec![vec!["C:\\Windows\\System32\\cmd.exe".to_string()]],
        current: 0,
    };
    let model = SignalingModel::success_response(
        "req-list",
        SignalingType::ListTerminal,
        None,
        Some("conn-list".to_string()),
        Some(&terminals),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::ListTerminalResponse(p) => {
            assert_eq!(p.request_id, "req-list");
            assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
            assert_eq!(p.terminals.commands.len(), 1);
            assert_eq!(p.terminals.current, 0);
        }
        other => panic!("expected ListTerminalResponse, got {other:?}"),
    }
}

/// A `ManagerSystemInfo` response
/// produced by `handle_manager_system_info` for an internal HTTP request carries `to_connection_id == None` because
/// the original request from `signal-facade::request_peer_with_callback`
/// had no `from_connection_id`. The typed dispatcher must still
/// route it (the daemon's signal/manager bus correlates the
/// response by `request_id` alone in that case); the classifier must not require a browser connection id.
#[test]
fn outbound_dispatch_manager_response_without_to_connection_routes_with_none() {
    let info = SystemInfo::default();
    let model = SignalingModel::success_response(
        "req-info-noid",
        SignalingType::ManagerSystemInfo,
        None,
        None, // HTTP-API trigger: no originating browser PC
        Some(&info),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::ManagerSystemInfoResponse(p) => {
            assert_eq!(p.request_id, "req-info-noid");
            assert!(p.connection_id.is_none());
        }
        other => panic!("expected ManagerSystemInfoResponse, got {other:?}"),
    }
}

/// Forwarder task exits immediately if the underlying transport returns
/// `Closed` on the first send. Built by dropping the in-process
/// receiver before any forwarder send happens — the next `send` then
/// surfaces `TransportError::Closed`.
#[tokio::test]
async fn event_forwarder_exits_when_transport_closed() {
    use desk_ipc_protocol::dual_transport::inprocess;

    let (sender, receiver) = inprocess::make_event::<WorkerToService>();
    drop(receiver);
    let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
    let task = spawn_event_forwarder_task(rx, sender);

    // Push one message; forwarder will observe `Closed` and exit.
    tx.send(WorkerToService::Ready).expect("send Ready");

    tokio::time::timeout(tokio::time::Duration::from_secs(1), task)
        .await
        .expect("forwarder task must exit after transport closes")
        .expect("task panicked");
}

/// `SetVirtualDisplayMode` Applied → the worker should refresh
/// per-connection input geometry on the attached display.
/// This predicate gates that branch, so unit-test it directly.
#[test]
fn should_refresh_after_set_mode_returns_true_on_applied() {
    use desk_ipc_protocol::message::{
        VirtualDisplayModeData, VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
    };
    let response = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id: "r".into(),
        connection_id: "c".into(),
        outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
        }),
    });
    assert!(should_refresh_after_set_mode(&response));
}

/// `SetVirtualDisplayMode` Failed → no geometry refresh; the IDD
/// mode did not actually change, so the existing rect is still
/// authoritative.
#[test]
fn should_refresh_after_set_mode_returns_false_on_failed() {
    use desk_ipc_protocol::message::{
        VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
    };
    let response = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id: "r".into(),
        connection_id: "c".into(),
        outcome: VirtualDisplayModeOutcome::Failed("invalid mode".into()),
    });
    assert!(!should_refresh_after_set_mode(&response));
}

/// Non-VirtualDisplayMode responses must never trigger a refresh.
/// Guards against a typo that would catch the wrong variant via the
/// `matches!` macro.
#[test]
fn should_refresh_after_set_mode_returns_false_on_other_variants() {
    assert!(!should_refresh_after_set_mode(&WorkerToService::Ready));
}

// -----------------------------------------------------------------
// WGC mid-session restart helpers (see select_wgc_restart_steps +
// dedup_capture_keys). The SetVirtualDisplayMode handler funnels
// through these so the unit tests can drive arbitrary
// (connection, CaptureKey) fixtures without spinning up real
// capture backends.
// -----------------------------------------------------------------

use desk_ipc_protocol::message::{MediaCodec, StartMediaPayload};
use std::collections::HashMap;

fn make_step(connection_id: &str, device: &str) -> RestartStep {
    RestartStep {
        connection_id: connection_id.to_string(),
        active: StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: Some(device.to_string()),
            audio_device: None,
            fps: 60,
            bitrate_kbps: 4_000,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        },
    }
}

fn make_key(backend: &str, device: &str) -> CaptureKey {
    CaptureKey {
        backend: backend.to_string(),
        device_name: device.to_string(),
    }
}

/// The core gating contract: only WGC connections that target the
/// currently attached IDD display are eligible for forced rebuild.
/// WGC connections on a different display, and DXGI / GDI
/// connections on the same display, are filtered out.
#[test]
fn select_wgc_restart_steps_picks_only_wgc_on_attached() {
    let attached = r"\\.\DISPLAY51";
    let other_display = r"\\.\DISPLAY1";
    let steps = vec![
        make_step("c-wgc-attached", attached),
        make_step("c-wgc-other", other_display),
        make_step("c-dxgi-attached", attached),
    ];
    let mut keys: HashMap<String, CaptureKey> = HashMap::new();
    keys.insert("c-wgc-attached".into(), make_key("WGC", attached));
    keys.insert("c-wgc-other".into(), make_key("WGC", other_display));
    keys.insert("c-dxgi-attached".into(), make_key("DXGI", attached));

    let picked = select_wgc_restart_steps(steps, Some(attached), |id| keys.get(id).cloned());
    assert_eq!(picked.len(), 1);
    assert_eq!(picked[0].connection_id, "c-wgc-attached");
}

/// No attached display ⇒ nothing to rebuild. Guards the handler
/// from invoking the WGC-only branch on a Detach race.
#[test]
fn select_wgc_restart_steps_empty_when_no_attached() {
    let steps = vec![make_step("c-1", r"\\.\DISPLAY1")];
    let keys: HashMap<String, CaptureKey> = HashMap::new();
    let picked = select_wgc_restart_steps(steps, None, |id| keys.get(id).cloned());
    assert!(picked.is_empty());
}

/// A connection whose key_lookup returns None (e.g. it never
/// reached the post-subscribe checkpoint) must be skipped, not
/// panic. Defensive against transient producer state.
#[test]
fn select_wgc_restart_steps_empty_when_lookup_returns_none() {
    let attached = r"\\.\DISPLAY51";
    let steps = vec![make_step("c-1", attached)];
    let picked = select_wgc_restart_steps(steps, Some(attached), |_| None);
    assert!(picked.is_empty());
}

/// `ImageCaptureType::WGC` round-trips through `<&'static str>::from`
/// as exactly "WGC", but `eq_ignore_ascii_case` is a cheap belt to
/// avoid future-mode drift if someone renames the variant in
/// lowercase / mixed case downstream.
#[test]
fn select_wgc_restart_steps_case_insensitive_backend() {
    let attached = r"\\.\DISPLAY51";
    let steps = vec![make_step("c-1", attached)];
    let mut keys: HashMap<String, CaptureKey> = HashMap::new();
    keys.insert("c-1".into(), make_key("wgc", attached));
    let picked = select_wgc_restart_steps(steps, Some(attached), |id| keys.get(id).cloned());
    assert_eq!(picked.len(), 1);
}

/// Two connections sharing the same (backend, device_name) slot
/// must yield exactly one CaptureKey so the registry is
/// invalidated once, not twice.
#[test]
fn dedup_capture_keys_collapses_duplicates() {
    let attached = r"\\.\DISPLAY51";
    let steps = vec![
        make_step("c-1", attached),
        make_step("c-2", attached),
        make_step("c-3", attached),
        make_step("c-4", r"\\.\DISPLAY1"),
    ];
    let mut keys: HashMap<String, CaptureKey> = HashMap::new();
    let shared = make_key("WGC", attached);
    keys.insert("c-1".into(), shared.clone());
    keys.insert("c-2".into(), shared.clone());
    keys.insert("c-3".into(), shared.clone());
    keys.insert("c-4".into(), make_key("WGC", r"\\.\DISPLAY1"));

    let distinct = dedup_capture_keys(&steps, |id| keys.get(id).cloned());
    assert_eq!(distinct.len(), 2);
}

/// Regression test for the WGC mid-session resize blackscreen
/// **second iteration** (2026-05-24): a CDS `DISP_CHANGE_BADMODE`
/// (typical on browser-driven shrink to a non-standard size) makes
/// `run_set_mode` return Failed even though
/// `pipe_client::send_set_mode` already triggered the IDD
/// Departure+Arrival cycle. The previous gate
/// `if should_refresh { compute restart_steps }` would silently
/// skip the WGC rebuild, leaving the capture loop bound to the
/// dead HMONITOR (the symptom: persistent
/// `post-rebuild heartbeat tick produced 0 NALs` in
/// `desk-worker.log`). The fix decouples WGC restart from the
/// outcome variant — every successful IPC reception goes through
/// `select_wgc_restart_steps`, with `should_refresh` retained only
/// for the input-geometry refresh.
///
/// Concretely this test pins the contract:
///   1. `should_refresh_after_set_mode` returns `false` for Failed
///      (input geometry must NOT refresh).
///   2. `select_wgc_restart_steps` is independently computable —
///      it does not consult the response variant at all — so the
///      handler can still pick out the WGC connection on the
///      attached display when the outcome is Failed.
/// Tested together so a future refactor that re-couples the two
/// fails this test rather than silently regressing the fix.
#[test]
fn wgc_restart_decoupled_from_failed_outcome() {
    use desk_ipc_protocol::message::{
        VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
    };
    // Failed outcome (the CDS BADMODE case).
    let failed_response = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id: "r".into(),
        connection_id: "c".into(),
        outcome: VirtualDisplayModeOutcome::Failed(
            "BADMODE for \\\\.\\DISPLAY9 @ 850x770@60; \
                     driver did not advertise this mode"
                .into(),
        ),
    });
    assert!(
        !should_refresh_after_set_mode(&failed_response),
        "Failed must keep gating input geometry refresh \
             (no actual mode change to reflect)"
    );

    // ...but the WGC restart selector still produces non-empty
    // candidates because it is independent of the response.
    let attached = r"\\.\DISPLAY51";
    let steps = vec![make_step("c-wgc-attached", attached)];
    let mut keys: HashMap<String, CaptureKey> = HashMap::new();
    keys.insert("c-wgc-attached".into(), make_key("WGC", attached));
    let picked = select_wgc_restart_steps(steps, Some(attached), |id| keys.get(id).cloned());
    assert_eq!(
        picked.len(),
        1,
        "select_wgc_restart_steps must NOT consult the IPC outcome — \
             the IDD pipe replug already invalidated HMONITOR, so WGC \
             needs rebuilding even on CDS Failed"
    );
    assert_eq!(picked[0].connection_id, "c-wgc-attached");
}

/// Order preserved: callers iterate this list in order to drive
/// `invalidate_capture_key`, so deterministic FIFO behaviour
/// keeps log output and integration traces reproducible.
#[test]
fn dedup_capture_keys_preserves_order() {
    let attached = r"\\.\DISPLAY51";
    let other = r"\\.\DISPLAY9";
    let steps = vec![
        make_step("c-A", attached),
        make_step("c-B", other),
        make_step("c-C", attached), // dup of A
    ];
    let mut keys: HashMap<String, CaptureKey> = HashMap::new();
    let key_attached = make_key("WGC", attached);
    let key_other = make_key("WGC", other);
    keys.insert("c-A".into(), key_attached.clone());
    keys.insert("c-B".into(), key_other.clone());
    keys.insert("c-C".into(), key_attached.clone());

    let distinct = dedup_capture_keys(&steps, |id| keys.get(id).cloned());
    assert_eq!(distinct, vec![key_attached, key_other]);
}
