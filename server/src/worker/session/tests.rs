use super::*;
use crate::host_control::HubMode;
use desk_utils::error::DeskErrorCode;

fn payload_with(
    host_upstream_url: Option<String>,
    auth_token: Option<String>,
) -> WorkerInitPayload {
    WorkerInitPayload {
        worker_identity: None,
        session_id: "session-1".into(),
        os_session_id: 1,
        desktop_name: None,
        config_json: "{}".into(),
        log_dir: None,
        data_dir: None,
        signaling_url: None,
        auth_token,
        host_upstream_url,
        media_pipe_name: None,
        file_pipe_name: None,
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

#[tokio::test]
async fn restricted_event_forwarder_drops_session_user_outputs() {
    use desk_ipc_protocol::dual_transport::inprocess;

    let (sender, mut receiver) = inprocess::make_event::<WorkerToService>();
    let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
    let task = spawn_profiled_event_forwarder_task(rx, sender, WorkerProfile::RestrictedDesktop);
    tx.send(WorkerToService::TerminalClosed(TerminalClosedPayload {
        connection_id: "forbidden".to_string(),
    }))
    .unwrap();
    tx.send(WorkerToService::Ready).unwrap();
    drop(tx);

    assert!(matches!(
        receiver.recv().await,
        Some(WorkerToService::Ready)
    ));
    assert!(receiver.recv().await.is_none());
    task.await.unwrap();
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
        SignalingType::TerminalStarted,
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
            assert_eq!(p.signaling_type, SignalingType::TerminalStarted);
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

/// A `SystemInfoRetrieved` response (built by the worker's
/// `send_response`) gets routed onto
/// `WorkerToService::SystemInfoRetrieved` carrying the
/// `request_id`, `connection_id`, and the `SystemInfo` body
/// verbatim. This guards the typed-routing decision on the
/// happy path.
#[test]
fn outbound_dispatch_routes_system_info_retrieved_to_typed_variant() {
    let info = SystemInfo {
        name: Some("alice-pc".to_string()),
        is_admin: Some(true),
        ..SystemInfo::default()
    };
    let model = SignalingModel::success_response(
        "req-info-1",
        SignalingType::SystemInfoRetrieved,
        None,
        Some("conn-info".to_string()),
        Some(&info),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::SystemInfoRetrieved(p) => {
            assert_eq!(p.request_id, "req-info-1");
            assert_eq!(p.connection_id.as_deref(), Some("conn-info"));
            assert_eq!(p.info.name.as_deref(), Some("alice-pc"));
            assert_eq!(p.info.is_admin, Some(true));
        }
        other => panic!("expected SystemInfoRetrieved, got {other:?}"),
    }
}

/// Empty-body responses (`FileDeleted`) ride
/// `WorkerToService::ManagerResponseRefPayload` — only the
/// `request_id` + `connection_id` matter.
#[test]
fn outbound_dispatch_routes_empty_body_manager_responses_to_typed_variants() {
    let model = SignalingModel::success_response(
        "req-empty",
        SignalingType::FileDeleted,
        None,
        Some("conn-empty".to_string()),
        Some(&()),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::FileDeleted(p) => {
            assert_eq!(p.request_id, "req-empty");
            assert_eq!(p.connection_id.as_deref(), Some("conn-empty"));
        }
        other => panic!("expected FileDeleted, got {other:?}"),
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

/// `TerminalOutputProduced` is the high-frequency PTY-output
/// path. The PTY reader thread builds it via `new_request` with a
/// `TerminalOutputData` body; verify the body survives the typed
/// route + the connection_id is read from `to_connection_id`
/// (server-initiated request, target browser is the destination).
#[test]
fn outbound_dispatch_routes_terminal_output_produced_to_typed_variant() {
    let body = TerminalOutputData {
        content: "hello\r\nworld\r\n".to_string(),
        assistant_object_ref: None,
    };
    let model = SignalingModel::new_request(
        SignalingType::TerminalOutputProduced,
        Some("conn-term".to_string()),
        Some(&body),
    )
    .expect("build new_request");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::TerminalOutputProduced(p) => {
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(p.data.content, "hello\r\nworld\r\n");
        }
        other => panic!("expected TerminalOutputProduced, got {other:?}"),
    }
}

/// `TerminalCommandsListed` response carries the `TerminalList` in
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
        SignalingType::TerminalCommandsListed,
        None,
        Some("conn-list".to_string()),
        Some(&terminals),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::TerminalCommandsListed(p) => {
            assert_eq!(p.request_id, "req-list");
            assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
            assert_eq!(p.terminals.commands.len(), 1);
            assert_eq!(p.terminals.current, 0);
        }
        other => panic!("expected TerminalCommandsListed, got {other:?}"),
    }
}

/// A `SystemInfoRetrieved` response
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
        SignalingType::SystemInfoRetrieved,
        None,
        None, // HTTP-API trigger: no originating browser PC
        Some(&info),
    )
    .expect("build response");
    let text = serde_json::to_string(&model).expect("serialise");
    match build_outbound_payload_from_desk_text(text).expect("typed route") {
        WorkerToService::SystemInfoRetrieved(p) => {
            assert_eq!(p.request_id, "req-info-noid");
            assert!(p.connection_id.is_none());
        }
        other => panic!("expected SystemInfoRetrieved, got {other:?}"),
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
        connection_epoch: "epoch".to_string(),
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
        connection_epoch: "epoch".to_string(),
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
            resolved_wayland_control_mode: None,
            connection_id: connection_id.to_string(),
            connection_epoch: "test-epoch".to_string(),
            video_generation: 1,
            audio_generation: 1,
            video_codec: MediaCodec::H264,
            video_encoder: desk_signal_facade::model::media_capability::VideoEncoderId::X264,
            video_device: Some(device.to_string()),
            fps: 60,
            bitrate_kbps: 4_000,
            quality: 0,
            start_video: true,
            audio: None,
            image_capture: "default".to_string(),
            enable_dirty_rect: false,
            show_mouse: false,
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
        connection_epoch: "epoch".to_string(),
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

/// The security policy is applied ahead of the loop this guard protects, so a
/// revocation made while the host is locked always lands. The locale has to be
/// let through explicitly: the daemon persisted it before sending, and nothing
/// re-sends it on unlock, so dropping it would strand the worker in the old
/// language for good.
#[test]
fn a_locked_host_still_accepts_the_instructions_about_itself() {
    for msg in [
        ServiceToWorker::Shutdown,
        ServiceToWorker::SetLocale(desk_ipc_protocol::message::SetLocalePayload {
            operation_id: "op".to_string(),
            locale: "en-US".to_string(),
        }),
        ServiceToWorker::SetInteractiveRoute(
            desk_ipc_protocol::message::InteractiveRouteCommandPayload {
                route_epoch: 9,
                active: false,
            },
        ),
    ] {
        assert!(
            crate::worker::session::survives_remote_access_lock(&msg),
            "{msg:?} must not be dropped while locked"
        );
    }
}

/// Remote work is exactly what a lock exists to stop.
#[test]
fn a_locked_host_drops_remote_work() {
    let msg =
        ServiceToWorker::WhiteboardCommand(desk_ipc_protocol::message::OpaqueConnectionPayload {
            connection_id: "c1".to_string(),
            data: b"{}".to_vec(),
        });
    assert!(!crate::worker::session::survives_remote_access_lock(&msg));
}

/// The one task that drains the daemon→worker transport.
///
/// Everything the daemon sends arrives through here, `Shutdown` included, so it
/// must never be capable of waiting on anything except the next message. It also
/// answers policy publications on the way past, and answering is the one thing
/// in it that talks outward — which is where a wait would come from.
mod inbound_reader {
    use super::super::runtime::spawn_inbound_reader;
    use crate::model::settings::{Settings, SharedSettings};
    use crate::worker::policy_mirror::PolicyMirror;
    use desk_ipc_protocol::dual_transport::inprocess;
    use desk_ipc_protocol::message::{
        ServiceToWorker, UpdateSecurityPolicyPayload, WorkerToService,
    };
    use desk_signal_facade::model::policy_snapshot::PolicySnapshot;
    use desk_signal_facade::model::security_settings::SecuritySettings;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn published(allow_terminal: bool) -> UpdateSecurityPolicyPayload {
        let security = SecuritySettings {
            allow_terminal: Some(allow_terminal),
            ..SecuritySettings::default()
        };
        UpdateSecurityPolicyPayload {
            operation_id: "op-1".to_string(),
            snapshot: PolicySnapshot::new(security),
        }
    }

    #[tokio::test]
    async fn application_policy_waits_for_readers_without_blocking_shutdown_and_rejects_stale_updates()
     {
        use desk_ipc_protocol::message::ComputerUseApplicationPolicyPayload;
        let (daemon_tx, worker_rx) = inprocess::make_event::<ServiceToWorker>();
        let mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(
            SecuritySettings::default(),
        )));
        let settings = Arc::new(SharedSettings::from(Settings::default()));
        let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
        let (main_tx, mut main_rx) = mpsc::unbounded_channel();
        let reader = spawn_inbound_reader(worker_rx, mirror, settings.clone(), ack_tx, main_tx);
        let lease = settings.read().await;
        let update = ComputerUseApplicationPolicyPayload {
            operation_id: "applications-1".into(),
            revision: 2,
            allowed_application_paths: vec![
                "/Applications/Calculator.app/Contents/MacOS/Calculator".into(),
            ],
        };
        daemon_tx
            .send(ServiceToWorker::UpdateComputerUseApplicationPolicy(
                update.clone(),
            ))
            .await
            .unwrap();
        daemon_tx.send(ServiceToWorker::Shutdown).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), main_rx.recv())
                .await
                .unwrap(),
            Some(Some(ServiceToWorker::Shutdown))
        ));
        assert!(ack_rx.try_recv().is_err());
        drop(lease);
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), ack_rx.recv()).await.unwrap(), Some(WorkerToService::ComputerUseApplicationPolicyApplied(applied)) if applied == update)
        );
        daemon_tx
            .send(ServiceToWorker::UpdateComputerUseApplicationPolicy(
                ComputerUseApplicationPolicyPayload {
                    operation_id: "stale".into(),
                    revision: 1,
                    allowed_application_paths: vec![],
                },
            ))
            .await
            .unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), ack_rx.recv()).await.unwrap(), Some(WorkerToService::ComputerUseApplicationPolicyApplied(applied)) if applied.revision == 2 && applied.allowed_application_paths == update.allowed_application_paths)
        );
        drop(daemon_tx);
        reader.await.unwrap();
    }

    /// Nothing is reading the acknowledgement, and it still does not matter.
    ///
    /// A worker acknowledges a policy while the daemon may be doing anything at
    /// all — including not reading. If answering could block, the reader would
    /// stop draining, and the message behind the policy is the one that ends the
    /// session: the worker would keep running, keep enforcing, and keep holding
    /// the desktop after the daemon had asked it to stop.
    #[tokio::test]
    async fn a_policy_answer_nobody_collects_does_not_strand_the_shutdown_behind_it() {
        let (daemon_tx, worker_rx) = inprocess::make_event::<ServiceToWorker>();
        let mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(
            SecuritySettings::default(),
        )));
        let (ack_tx, mut ack_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (main_tx, mut main_rx) = mpsc::unbounded_channel::<Option<ServiceToWorker>>();
        let reader = spawn_inbound_reader(
            worker_rx,
            Arc::clone(&mirror),
            Arc::new(SharedSettings::from(Settings::default())),
            ack_tx,
            main_tx,
        );

        daemon_tx
            .send(ServiceToWorker::UpdateSecurityPolicy(published(false)))
            .await
            .expect("publish");
        daemon_tx
            .send(ServiceToWorker::Shutdown)
            .await
            .expect("shutdown");

        let delivered = tokio::time::timeout(Duration::from_secs(2), main_rx.recv())
            .await
            .expect("the reader must keep draining while an answer goes uncollected")
            .expect("the main-loop queue is open");
        assert!(
            matches!(delivered, Some(ServiceToWorker::Shutdown)),
            "the message behind the policy is what has to get through",
        );
        assert_eq!(
            mirror.snapshot().security().allow_terminal,
            Some(false),
            "the policy is applied here, not forwarded to a main loop that may be parked",
        );
        // Only now is the answer collected — long after it mattered.
        match ack_rx.try_recv().expect("the answer is waiting") {
            WorkerToService::SecurityPolicyApplied(applied) => {
                assert_eq!(applied.operation_id, "op-1");
            }
            other => panic!("expected the policy answer, got {other:?}"),
        }
        drop(daemon_tx);
        tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("the reader ends with its transport")
            .expect("the reader did not panic");
    }

    /// A closed transport is the other way this task ends, and the main loop
    /// finds out by the `None` this forwards rather than by waiting forever.
    #[tokio::test]
    async fn a_closed_transport_is_reported_to_the_main_loop() {
        let (daemon_tx, worker_rx) = inprocess::make_event::<ServiceToWorker>();
        let mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(
            SecuritySettings::default(),
        )));
        let (ack_tx, _ack_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (main_tx, mut main_rx) = mpsc::unbounded_channel::<Option<ServiceToWorker>>();
        let reader = spawn_inbound_reader(
            worker_rx,
            mirror,
            Arc::new(SharedSettings::from(Settings::default())),
            ack_tx,
            main_tx,
        );

        drop(daemon_tx);

        assert!(
            tokio::time::timeout(Duration::from_secs(2), main_rx.recv())
                .await
                .expect("the closure is reported promptly")
                .expect("the main-loop queue is open")
                .is_none(),
        );
        tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("the reader ends with its transport")
            .expect("the reader did not panic");
    }
}
