use super::*;
use desk_signal_facade::model::audio_capture::{AudioDataFlow, SelectedAudioDevice};
use desk_signal_facade::model::desk_settings::LinuxInputControlMode;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams, FileListResponse};
use desk_signal_facade::model::image_capture::Resolution;
use desk_signal_facade::model::media_capability::VideoEncoderId;
use desk_signal_facade::model::media_pipeline::{MediaPipelinePhase, MediaPipelineStateData};
use desk_signal_facade::model::policy_snapshot::{PolicyGenerations, PolicySnapshot};
use desk_signal_facade::model::private_screen::PrivateScreenStateChangedData;
use desk_signal_facade::model::remote_session::AudioPipelineSettings;
use desk_signal_facade::model::security_settings::{SecurityPermissionType, SecuritySettings};
use desk_signal_facade::model::signal::SignalingType;
use desk_signal_facade::model::system_info::SystemInfo;
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalList, TerminalOutputData, TerminalResizeData,
};
use std::collections::BTreeMap;

/// The spawn report round-trips both ways, including the containment identity
/// the daemon needs to reclaim a tree it has lost track of.
#[test]
fn exec_spawn_report_round_trips() {
    for report in [
        ExecSpawnReport::Started {
            containment_identity: Some("pgid:4242".to_string()),
        },
        ExecSpawnReport::Started {
            containment_identity: None,
        },
        ExecSpawnReport::Failed {
            reason: "no such program".to_string(),
        },
    ] {
        let original = WorkerToService::ExecSpawnReport(ExecSpawnReportPayload {
            request_id: "gen-1".to_string(),
            connection_id: Some("conn-1".to_string()),
            report: report.clone(),
        });
        match wincode_round_trip(&original) {
            WorkerToService::ExecSpawnReport(p) => {
                assert_eq!(p.request_id, "gen-1");
                assert_eq!(p.report, report);
            }
            other => panic!("expected ExecSpawnReport, got {other:?}"),
        }
    }
}

/// New `host_upstream_url` + repurposed `auth_token` fields round-trip cleanly.
#[test]
fn worker_init_payload_round_trip_with_host_upstream_fields() {
    let original = WorkerInitPayload {
        session_id: "session-1".to_string(),
        os_session_id: 7,
        desktop_name: Some("Default".to_string()),
        config_json: "{}".to_string(),
        log_dir: Some(r"C:\ProgramData\LCXL Remote Desktop\logs".to_string()),
        signaling_url: None,
        auth_token: Some("ipc-token".to_string()),
        host_upstream_url: Some("ws://127.0.0.1:8082/ws/host_upstream".to_string()),
        media_pipe_name: Some(r"\\.\pipe\lcxl-desk-ipc-7-uuid-media".to_string()),
        file_pipe_name: Some(r"\\.\pipe\lcxl-desk-file-ipc-7-uuid".to_string()),
        remote_access_locked: true,
        remote_access_state_version: 8,
    };
    let json = serde_json::to_string(&original).unwrap();
    let decoded: WorkerInitPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.session_id, original.session_id);
    assert_eq!(decoded.os_session_id, original.os_session_id);
    assert_eq!(decoded.auth_token, original.auth_token);
    assert_eq!(decoded.host_upstream_url, original.host_upstream_url);
    assert_eq!(decoded.media_pipe_name, original.media_pipe_name);
    assert_eq!(decoded.file_pipe_name, original.file_pipe_name);
    assert!(decoded.remote_access_locked);
    assert_eq!(decoded.remote_access_state_version, 8);
}

/// `DesktopChanged` round-trips with the same JSON shape the IPC reader
/// expects (tag = "type", content = "payload").
#[test]
fn desktop_changed_round_trips() {
    let msg = WorkerToService::DesktopChanged(DesktopChangedPayload {
        name: "Winlogon".to_string(),
    });
    let json = serde_json::to_string(&msg).unwrap();
    let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
    match decoded {
        WorkerToService::DesktopChanged(payload) => assert_eq!(payload.name, "Winlogon"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Optional transport paths may be omitted, but the security gate is a
/// required part of every worker initialization payload.
#[test]
fn worker_init_payload_accepts_missing_optional_fields() {
    let legacy = serde_json::json!({
        "session_id": "session-1",
        "os_session_id": 7,
        "desktop_name": null,
        "config_json": "{}",
        "signaling_url": null,
        "auth_token": null,
        "remote_access_locked": false,
        "remote_access_state_version": 1,
    });
    let decoded: WorkerInitPayload = serde_json::from_value(legacy).unwrap();
    assert!(decoded.host_upstream_url.is_none());
    assert!(decoded.auth_token.is_none());
    assert!(decoded.media_pipe_name.is_none());
    assert!(decoded.file_pipe_name.is_none());
}

// ============== IPC variants — wincode round-trips ==============

use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

/// Unbounded wincode `Configuration` matching the production IPC
/// path (`IPC_CONFIG` in `transport.rs` / `dual_transport.rs`):
/// preallocation limit disabled so encode + decode accept the full
/// 16 MB transport-layer ceiling without firing the 4 MiB default
/// safety net.
type WincodeUnbounded = Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED>;

fn wincode_round_trip<T>(value: &T) -> T
where
    T: wincode::SchemaWrite<WincodeUnbounded, Src = T>
        + for<'de> wincode::SchemaRead<'de, WincodeUnbounded, Dst = T>,
{
    let config: WincodeUnbounded = Configuration::new();
    let bytes = wincode::config::serialize(value, config).expect("encode");
    wincode::config::deserialize(&bytes, config).expect("decode")
}

#[test]
fn start_media_round_trips_wincode() {
    let msg = ServiceToWorker::StartMedia(StartMediaPayload {
        connection_id: "conn-1".to_string(),
        connection_epoch: "epoch-1".to_string(),
        video_generation: 2,
        audio_generation: 3,
        video_codec: MediaCodec::H264,
        video_encoder: VideoEncoderId::X264,
        video_device: Some("\\\\.\\DISPLAY1".to_string()),
        fps: 60,
        bitrate_kbps: 6_000,
        quality: 0,
        start_video: true,
        audio: Some(StartAudioSettings {
            codec: MediaCodec::Opus,
            pipeline: AudioPipelineSettings {
                audio_capture: "wasapi".to_string(),
                audio_device: SelectedAudioDevice {
                    audio_data_flow: AudioDataFlow::Render,
                    audio_device_id: None,
                },
                audio_encoder: desk_signal_facade::model::remote_session::AudioEncoderId::Opus,
            },
        }),
        image_capture: "default".to_string(),
        resolved_wayland_control_mode: Some(LinuxInputControlMode::Portal),
        enable_dirty_rect: false,
        show_mouse: true,
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::StartMedia(p) => {
            assert_eq!(p.connection_id, "conn-1");
            assert_eq!(p.video_codec, MediaCodec::H264);
            assert_eq!(p.video_encoder, VideoEncoderId::X264);
            assert_eq!(p.connection_epoch, "epoch-1");
            assert_eq!(p.video_generation, 2);
            assert_eq!(p.audio_generation, 3);
            assert_eq!(
                p.audio.as_ref().map(|audio| audio.codec),
                Some(MediaCodec::Opus)
            );
            assert_eq!(
                p.resolved_wayland_control_mode,
                Some(LinuxInputControlMode::Portal)
            );
            assert_eq!(p.fps, 60);
            assert!(p.start_video);
            assert!(p.audio.is_some());
            assert_eq!(
                p.enable_dirty_rect, false,
                "enable_dirty_rect must survive StartMedia wincode round-trip"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// DataChannel-only connections (browser file-management UI) ship
/// `start_video=false, audio=None` so the worker skips both
/// capture pipelines. Round-trip the negative case so a wincode
/// schema bump that drops the new fields is caught here.
#[test]
fn start_media_data_channel_only_round_trips() {
    let msg = ServiceToWorker::StartMedia(StartMediaPayload {
        connection_id: "conn-files".to_string(),
        connection_epoch: "epoch-files".to_string(),
        video_generation: 1,
        audio_generation: 1,
        video_codec: MediaCodec::H264,
        video_encoder: desk_signal_facade::model::media_capability::VideoEncoderId::X264,
        video_device: None,
        fps: 0,
        bitrate_kbps: 0,
        quality: 0,
        start_video: false,
        audio: None,
        image_capture: "default".to_string(),
        resolved_wayland_control_mode: None,
        enable_dirty_rect: false,
        show_mouse: false,
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::StartMedia(p) => {
            assert!(!p.start_video, "start_video=false must round-trip");
            assert!(p.audio.is_none(), "audio=None must round-trip");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// The unpublished protocol switches atomically: required media fields must
/// not be inferred from an older payload.
#[test]
fn start_media_json_missing_required_fields_is_rejected() {
    let json = r#"{
            "connection_id": "conn-legacy",
            "video_codec": "H264",
            "audio_codec": "Opus",
            "video_device": null,
            "audio_device": null,
            "fps": 30,
            "bitrate_kbps": 0,
            "quality": 0
        }"#;
    assert!(serde_json::from_str::<StartMediaPayload>(json).is_err());
}

/// `UpdateMediaSettings` carries the live-tune knobs the daemon
/// receives from connection-scoped settings commands. Round-trip pins
/// the field set (especially
/// `enable_dirty_rect`) so a future schema bump that drops the
/// dirty-rect flag fails this test instead of silently regressing
/// the kill-switch back to "frontend toggle ignored".
#[test]
fn update_media_settings_round_trips_wincode_with_dirty_rect() {
    let msg = ServiceToWorker::UpdateMediaSettings(UpdateMediaSettingsPayload {
        connection_id: "conn-dr".to_string(),
        connection_epoch: "epoch-dr".to_string(),
        video_generation: 4,
        fps: Some(45),
        bitrate_kbps: None,
        quality: Some(22),
        enable_dirty_rect: Some(false),
        show_mouse: Some(true),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::UpdateMediaSettings(p) => {
            assert_eq!(p.connection_id, "conn-dr");
            assert_eq!(p.fps, Some(45));
            assert_eq!(p.bitrate_kbps, None);
            assert_eq!(p.quality, Some(22));
            assert_eq!(
                p.enable_dirty_rect,
                Some(false),
                "enable_dirty_rect must survive wincode round-trip"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Required terminal-state protocol fields are rejected when absent.
#[test]
fn update_media_settings_json_missing_required_fields_is_rejected() {
    let json = r#"{
            "connection_id": "conn-legacy",
            "fps": 30,
            "bitrate_kbps": null,
            "quality": 50
        }"#;
    assert!(serde_json::from_str::<UpdateMediaSettingsPayload>(json).is_err());
}

#[test]
fn stop_media_round_trips_wincode() {
    let msg = ServiceToWorker::StopMedia(StopMediaPayload {
        connection_id: "conn-2".to_string(),
        connection_epoch: "epoch-2".to_string(),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::StopMedia(p) => assert_eq!(p.connection_id, "conn-2"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn force_keyframe_round_trips_wincode() {
    let msg = ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
        connection_id: "conn-3".to_string(),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::ForceKeyframe(p) => assert_eq!(p.connection_id, "conn-3"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// MouseInput / MouseMoveInput / KeyboardInput share `InputPayload` —
/// verify the variant tag survives round-trip (wincode discriminant).
#[test]
fn input_variants_distinguishable_after_round_trip() {
    let mouse = ServiceToWorker::MouseInput(InputPayload {
        connection_id: "c".to_string(),
        data: vec![1, 2, 3],
    });
    let mouse_move = ServiceToWorker::MouseMoveInput(InputPayload {
        connection_id: "c".to_string(),
        data: vec![1, 2, 3],
    });
    let keyboard = ServiceToWorker::KeyboardInput(InputPayload {
        connection_id: "c".to_string(),
        data: vec![1, 2, 3],
    });
    assert!(matches!(
        wincode_round_trip(&mouse),
        ServiceToWorker::MouseInput(_)
    ));
    assert!(matches!(
        wincode_round_trip(&mouse_move),
        ServiceToWorker::MouseMoveInput(_)
    ));
    assert!(matches!(
        wincode_round_trip(&keyboard),
        ServiceToWorker::KeyboardInput(_)
    ));
}

#[test]
fn capabilities_round_trips_wincode() {
    use desk_signal_facade::model::audio_capture::{AudioDataFlow, AudioDevice};
    use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};

    let mut video_device_list: BTreeMap<String, Vec<DisplayInfo>> = BTreeMap::new();
    video_device_list.insert(
        "dxgi".to_string(),
        vec![DisplayInfo {
            device_name: "\\\\.\\DISPLAY1".to_string(),
            display_device_name: Some("Generic PnP Monitor".to_string()),
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            resolutions: vec![],
            attached_to_desktop: true,
            rotation: 0,
            current_capture_resolution: None,
        }],
    );
    let mut audio_device_list: BTreeMap<String, Vec<AudioDevice>> = BTreeMap::new();
    audio_device_list.insert(
        "wasapi".to_string(),
        vec![AudioDevice {
            id: "mic-1".to_string(),
            firendly_name: "Microphone (Realtek)".to_string(),
            data_flow: AudioDataFlow::Capture,
            default: true,
        }],
    );
    let msg = WorkerToService::Capabilities(MediaCapabilities {
        video_codecs: vec![MediaCodec::H264, MediaCodec::Vp9],
        audio_codecs: vec![MediaCodec::Opus],
        video_encoders: vec!["X264".to_string(), "H264".to_string(), "VP9".to_string()],
        video_encoder_capabilities: vec![],
        audio_encoders: vec!["Opus".to_string()],
        video_device_list: video_device_list.clone(),
        audio_device_list: audio_device_list.clone(),
        has_tauri: true,
        is_admin: false,
        desktop_name: "Default".to_string(),
    });
    match wincode_round_trip(&msg) {
        WorkerToService::Capabilities(c) => {
            assert_eq!(c.video_codecs, vec![MediaCodec::H264, MediaCodec::Vp9]);
            assert_eq!(
                c.video_encoders,
                vec!["X264".to_string(), "H264".to_string(), "VP9".to_string()],
                "X264 and H264 must remain distinct entries — the UI \
                     needs them to expose the libx264 vs OpenH264 choice"
            );
            assert_eq!(c.audio_encoders, vec!["Opus".to_string()]);
            assert_eq!(c.video_device_list.len(), 1);
            assert_eq!(
                c.video_device_list["dxgi"][0].device_name,
                "\\\\.\\DISPLAY1"
            );
            assert_eq!(c.audio_device_list.len(), 1);
            assert_eq!(c.audio_device_list["wasapi"][0].id, "mic-1");
            assert!(c.has_tauri);
            assert!(!c.is_admin);
            assert_eq!(c.desktop_name, "Default");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `FileTransferPayload` carries an `is_text` flag alongside
/// `connection_id` + `data`. Verify both true and false survive
/// round-trip — a flipped bit would break the daemon's
/// `dc.send_text` vs `dc.send` decision and corrupt downloads.
/// Since the file-transfer payload now rides its own dedicated
/// IPC lane (see `dual_transport::FILE_QUEUE_CAP`), this round-trip
/// is on the bare `FileTransferPayload` struct rather than a
/// `WorkerToService` / `ServiceToWorker` enum wrapper.
#[test]
fn file_transfer_payload_round_trip_preserves_is_text_flag() {
    for is_text in [true, false] {
        let original = FileTransferPayload {
            connection_id: "ft-1".to_string(),
            data: vec![1, 2, 3],
            is_text,
            transfer_id: None,
        };
        let decoded = wincode_round_trip(&original);
        assert_eq!(decoded.connection_id, "ft-1");
        assert_eq!(decoded.data, vec![1, 2, 3]);
        assert_eq!(decoded.is_text, is_text);
        assert!(decoded.transfer_id.is_none());
    }
}

/// `transfer_id` survives wincode round-trip in both `Some` and
/// `None` form. The daemon-side writer task reads this field on
/// `dc.send` failure and forwards it via
/// [`ServiceToWorker::FileTransferSendFailed`] so the worker can
/// abort the specific transfer rather than all transfers on the
/// PC; losing the field would silently coarsen the abort scope.
#[test]
fn file_transfer_payload_transfer_id_round_trips_wincode() {
    for transfer_id in [
        None,
        Some("11111111-2222-3333-4444-555555555555".to_string()),
    ] {
        let original = FileTransferPayload {
            connection_id: "ft-1".to_string(),
            data: vec![1, 2, 3],
            is_text: false,
            transfer_id: transfer_id.clone(),
        };
        let decoded = wincode_round_trip(&original);
        assert_eq!(decoded.transfer_id, transfer_id);
    }
}

/// `FileTransferSendFailedPayload` survives a wincode round trip
/// in every error-kind variant. The worker dispatches its abort
/// policy off `kind`; a silent re-ordering of the enum would map
/// `PacketTooLarge` to `Other` (or vice versa), demoting a
/// configuration bug to a warning and skipping the
/// fatal-transfer abort.
#[test]
fn file_transfer_send_failed_round_trips_all_kinds() {
    for kind in [
        FileTransferSendErrorKind::PacketTooLarge,
        FileTransferSendErrorKind::TransportClosed,
        FileTransferSendErrorKind::Other,
    ] {
        let msg = ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
            connection_id: "conn-ft".to_string(),
            transfer_id: Some("00000000-0000-0000-0000-000000000001".to_string()),
            chunk_index: Some(42),
            kind,
            error: "outbound packet too large".to_string(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::FileTransferSendFailed(p) => {
                assert_eq!(p.connection_id, "conn-ft");
                assert_eq!(
                    p.transfer_id.as_deref(),
                    Some("00000000-0000-0000-0000-000000000001")
                );
                assert_eq!(p.chunk_index, Some(42));
                assert_eq!(p.kind, kind);
                assert_eq!(p.error, "outbound packet too large");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// Coarse-grained variant: `transfer_id` and `chunk_index` are
/// optional so the daemon can still send a failure notification
/// when it cannot attribute the failure to a specific transfer
/// (e.g. the failing payload was a legacy chunk emitted before the
/// worker started populating `transfer_id`). The worker treats
/// `None` as "abort everything for this connection".
#[test]
fn file_transfer_send_failed_round_trips_without_transfer_id() {
    let msg = ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
        connection_id: "conn-ft".to_string(),
        transfer_id: None,
        chunk_index: None,
        kind: FileTransferSendErrorKind::TransportClosed,
        error: "channel closed".to_string(),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::FileTransferSendFailed(p) => {
            assert_eq!(p.connection_id, "conn-ft");
            assert!(p.transfer_id.is_none());
            assert!(p.chunk_index.is_none());
            assert_eq!(p.kind, FileTransferSendErrorKind::TransportClosed);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Wincode payloads written by older binaries that pre-date the
/// `transfer_id` field MUST still decode — `#[serde(default)]`
/// makes the field optional so a worker built before the
/// `FileTransferSendFailed` rollout can still hand chunks to a
/// newer daemon (and vice versa) without an IPC framing mismatch.
#[test]
fn file_transfer_payload_accepts_legacy_json_without_transfer_id() {
    let legacy = serde_json::json!({
        "connection_id": "ft-1",
        "data": [1, 2, 3],
        "is_text": false,
    });
    let decoded: FileTransferPayload = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.connection_id, "ft-1");
    assert_eq!(decoded.data, vec![1, 2, 3]);
    assert!(!decoded.is_text);
    assert!(decoded.transfer_id.is_none());
}

/// `ErrorPayload.connection_id` survives a wincode round-trip in
/// both `Some` and `None` forms. The daemon's `MediaTransportStuck`
/// recovery path keys off this field — losing it would silently
/// regress the self-heal we just wired up.
#[test]
fn error_payload_connection_id_round_trips_wincode() {
    let scoped = WorkerToService::Error(ErrorPayload {
        code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
        message: "stuck".to_string(),
        recoverable: true,
        connection_id: Some("conn-7".to_string()),
    });
    match wincode_round_trip(&scoped) {
        WorkerToService::Error(p) => {
            assert_eq!(p.code, ERROR_CODE_MEDIA_TRANSPORT_STUCK);
            assert_eq!(p.connection_id.as_deref(), Some("conn-7"));
            assert!(p.recoverable);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let global = WorkerToService::Error(ErrorPayload {
        code: -1,
        message: "init failed".to_string(),
        recoverable: false,
        connection_id: None,
    });
    match wincode_round_trip(&global) {
        WorkerToService::Error(p) => {
            assert_eq!(p.code, -1);
            assert!(p.connection_id.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// JSON payloads emitted by older binaries that pre-date the
/// `connection_id` field must still decode (the `#[serde(default)]`
/// attribute is what makes this work).
#[test]
fn error_payload_accepts_legacy_json_without_connection_id() {
    let legacy = serde_json::json!({
        "code": -1,
        "message": "boom",
        "recoverable": false,
    });
    let decoded: ErrorPayload = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.code, -1);
    assert!(!decoded.recoverable);
    assert!(decoded.connection_id.is_none());
}

/// `ERROR_CODE_MEDIA_TRANSPORT_STUCK` is part of the IPC contract;
/// pin its numeric value so a refactor that accidentally renames or
/// shadows it shows up as a test failure.
#[test]
fn media_transport_stuck_error_code_is_stable() {
    assert_eq!(ERROR_CODE_MEDIA_TRANSPORT_STUCK, -1001);
}

/// `MediaFrame` is the hot path on the media transport — sanity check
/// 200 KB P-frame size encodes/decodes cleanly.
#[test]
fn media_frame_round_trips_wincode_200kb() {
    let payload = vec![0xABu8; 200 * 1024];
    let original = MediaFrame {
        connection_id: "conn-1".to_string(),
        connection_epoch: "epoch-1".to_string(),
        generation: 9,
        seq: 42,
        ts_ns: 1_700_000_000_000_000_000,
        duration_ns: 16_666_666,
        kind: MediaFrameKind::VideoP,
        codec: MediaCodec::H264,
        payload: payload.clone(),
    };
    let decoded = wincode_round_trip(&original);
    assert_eq!(decoded.connection_id, "conn-1");
    assert_eq!(decoded.seq, 42);
    assert_eq!(decoded.kind, MediaFrameKind::VideoP);
    assert_eq!(decoded.payload.len(), payload.len());
    assert_eq!(decoded.payload, payload);
}

// === Typed control plane — round-trip tests ===

/// Private-screen setting carries the request id through daemon/worker IPC.
#[test]
fn set_private_screen_visibility_round_trips_wincode() {
    for visible in [true, false] {
        let msg = ServiceToWorker::SetPrivateScreenVisibility(SetPrivateScreenVisibilityPayload {
            request_id: "req-priv".to_string(),
            connection_id: "conn-priv".to_string(),
            visible,
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::SetPrivateScreenVisibility(p) => {
                assert_eq!(p.request_id, "req-priv");
                assert_eq!(p.connection_id, "conn-priv");
                assert_eq!(p.visible, visible);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// `PrivateScreenStateChanged` is the reverse path (worker →
/// daemon → browser). Round-trip with `is_supported = false` +
/// an `error_msg` so a future schema change to
/// `PrivateScreenStateChangedData` shows up as a test failure
/// rather than as a silent wire-format drift.
#[test]
fn private_screen_state_changed_round_trips_wincode() {
    let msg = WorkerToService::PrivateScreenStateChanged(PrivateScreenStateChangedPayload {
        request_id: Some("req-pss".to_string()),
        connection_id: "conn-pss".to_string(),
        data: PrivateScreenStateChangedData {
            visible: false,
            is_supported: false,
            error_msg: Some("hub denied".to_string()),
        },
    });
    match wincode_round_trip(&msg) {
        WorkerToService::PrivateScreenStateChanged(p) => {
            assert_eq!(p.connection_id, "conn-pss");
            assert!(!p.data.visible);
            assert!(!p.data.is_supported);
            assert_eq!(p.data.error_msg.as_deref(), Some("hub denied"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// === Manager plane — round-trip tests ===

/// Body-less manager request envelopes carry only `request_id` +
/// `connection_id`; verify the field order survives wincode (a
/// reorder would silently swap them on matched-version pairs).
#[test]
fn manager_request_ref_round_trips_wincode() {
    let msg = ServiceToWorker::GetSystemInfo(ManagerRequestRefPayload {
        request_id: "req-info-1".to_string(),
        connection_id: Some("conn-mgr".to_string()),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::GetSystemInfo(p) => {
            assert_eq!(p.request_id, "req-info-1");
            assert_eq!(p.connection_id.as_deref(), Some("conn-mgr"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    // HTTP-API-triggered manager requests (e.g.
    // `signal-facade::controller::sysinfo`) have no originating
    // browser PC; verify a `None` connection_id round-trips so
    // the daemon's manager handlers don't drop the request.
    let none_msg = ServiceToWorker::GetSystemInfo(ManagerRequestRefPayload {
        request_id: "req-info-no-conn".to_string(),
        connection_id: None,
    });
    match wincode_round_trip(&none_msg) {
        ServiceToWorker::GetSystemInfo(p) => {
            assert!(p.connection_id.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `ListFiles` ferries `FileListParams` (carries 4
/// `Option<DateTime<Local>>` fields via the wincode chrono adapter).
/// Use a non-default page_count (and filename
/// filter) so a stripped field shows up as a test failure.
#[test]
fn list_files_round_trips_wincode() {
    let params = FileListParams {
        path: "C:\\Users".to_string(),
        page_no: 2,
        page_count: 50,
        file_name: Some("readme".to_string()),
        ..Default::default()
    };
    let msg = ServiceToWorker::ListFiles(ListFilesPayload {
        request_id: "req-fl".to_string(),
        connection_id: "conn-fl".to_string(),
        params,
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::ListFiles(p) => {
            assert_eq!(p.request_id, "req-fl");
            assert_eq!(p.connection_id, "conn-fl");
            assert_eq!(p.params.path, "C:\\Users");
            assert_eq!(p.params.page_no, 2);
            assert_eq!(p.params.page_count, 50);
            assert_eq!(p.params.file_name.as_deref(), Some("readme"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Body-less manager response envelopes are distinct from request
/// envelopes at the type level; round-trip pins the variant tag.
#[test]
fn manager_response_ref_round_trips_wincode() {
    let msg = WorkerToService::FileDeleted(ManagerResponseRefPayload {
        request_id: "req-del".to_string(),
        connection_id: Some("conn-del".to_string()),
    });
    match wincode_round_trip(&msg) {
        WorkerToService::FileDeleted(p) => {
            assert_eq!(p.request_id, "req-del");
            assert_eq!(p.connection_id.as_deref(), Some("conn-del"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `SystemInfoRetrieved` carries the full `SystemInfo`
/// blob; verify `startup_mode` + `is_admin` survive (the legacy
/// handler set both at runtime so they are the most likely
/// round-trip regression points).
#[test]
fn system_info_retrieved_round_trips_wincode() {
    let info = SystemInfo {
        name: Some("alice-pc".to_string()),
        is_admin: Some(true),
        ..SystemInfo::default()
    };
    let msg = WorkerToService::SystemInfoRetrieved(SystemInfoRetrievedPayload {
        request_id: "req-info".to_string(),
        connection_id: Some("conn-info".to_string()),
        info,
    });
    match wincode_round_trip(&msg) {
        WorkerToService::SystemInfoRetrieved(p) => {
            assert_eq!(p.request_id, "req-info");
            assert_eq!(p.info.name.as_deref(), Some("alice-pc"));
            assert_eq!(p.info.is_admin, Some(true));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// === Terminal plane — round-trip tests ===

/// `StartTerminal` ferries `StartTerminalSession` over the
/// wincode derive. A non-trivial `command` (with
/// comma-separated args) survives the round-trip — a stripped or
/// reordered field would break terminal launch on matched-version
/// daemon/worker pairs.
#[test]
fn start_terminal_round_trips_wincode() {
    let msg = ServiceToWorker::StartTerminal(StartTerminalPayload {
        request_id: "req-start".to_string(),
        connection_id: "conn-term".to_string(),
        session: StartTerminalSession {
            command: "C:\\Windows\\System32\\cmd.exe,/k,echo,hello".to_string(),
            device_id: None,
            grant_session_id: None,
        },
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::StartTerminal(p) => {
            assert_eq!(p.request_id, "req-start");
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(
                p.session.command,
                "C:\\Windows\\System32\\cmd.exe,/k,echo,hello"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `SendTerminalInput` is the keystroke / paste path —
/// arbitrary UTF-8 (including newlines + escape codes) must
/// round-trip verbatim.
#[test]
fn send_terminal_input_round_trips_wincode() {
    let msg = ServiceToWorker::SendTerminalInput(SendTerminalInputPayload {
        connection_id: "conn-term".to_string(),
        data: TerminalInputData {
            content: "ls -la\n\x1b[1;31mred\x1b[0m\n".to_string(),
        },
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::SendTerminalInput(p) => {
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(p.data.content, "ls -la\n\x1b[1;31mred\x1b[0m\n");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `ResizeTerminal` carries a u16 rows × cols pair; pin
/// the round-trip so a future field reorder does not silently
/// swap rows and cols at the wire.
#[test]
fn resize_terminal_round_trips_wincode() {
    let msg = ServiceToWorker::ResizeTerminal(ResizeTerminalPayload {
        connection_id: "conn-term".to_string(),
        data: TerminalResizeData {
            rows: 50,
            cols: 200,
        },
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::ResizeTerminal(p) => {
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(p.data.rows, 50);
            assert_eq!(p.data.cols, 200);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `CloseTerminal` and `ListTerminalCommands` are body-less;
/// verify the variant tag survives wincode (a reorder of the
/// terminal-plane variants would silently misroute one onto the
/// other on matched-version pairs).
#[test]
fn close_and_list_terminal_commands_round_trip_wincode() {
    let close = ServiceToWorker::CloseTerminal(CloseTerminalPayload {
        connection_id: "conn-term".to_string(),
    });
    assert!(matches!(
        wincode_round_trip(&close),
        ServiceToWorker::CloseTerminal(_)
    ));

    let list = ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
        request_id: "req-list".to_string(),
        connection_id: Some("conn-list".to_string()),
    });
    match wincode_round_trip(&list) {
        ServiceToWorker::ListTerminalCommands(p) => {
            assert_eq!(p.request_id, "req-list");
            assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    // HTTP-API-triggered list_terminal (signal-facade controller)
    // dispatches with no `from_connection_id`; verify `None`
    // round-trips so the daemon's terminal handler doesn't drop
    // it.
    let list_no_conn = ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
        request_id: "req-list-no-conn".to_string(),
        connection_id: None,
    });
    match wincode_round_trip(&list_no_conn) {
        ServiceToWorker::ListTerminalCommands(p) => {
            assert!(p.connection_id.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `TerminalStarted` is the success response for `StartTerminal`.
/// Empty body — `request_id` correlates back to the original
/// `StartTerminal`. Verify the variant survives wincode
/// alongside `TerminalClosed` (notification, no `request_id`)
/// so the daemon's reverse-direction code can keep them
/// straight.
#[test]
fn terminal_started_and_closed_round_trip_wincode() {
    let started = WorkerToService::TerminalStarted(TerminalStartedPayload {
        request_id: "req-start".to_string(),
        connection_id: "conn-term".to_string(),
    });
    match wincode_round_trip(&started) {
        WorkerToService::TerminalStarted(p) => {
            assert_eq!(p.request_id, "req-start");
            assert_eq!(p.connection_id, "conn-term");
        }
        other => panic!("unexpected: {other:?}"),
    }

    let closed = WorkerToService::TerminalClosed(TerminalClosedPayload {
        connection_id: "conn-term".to_string(),
    });
    match wincode_round_trip(&closed) {
        WorkerToService::TerminalClosed(p) => {
            assert_eq!(p.connection_id, "conn-term");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `TerminalOutputProduced` is the high-frequency PTY-output path.
/// Verify a reasonably large chunk (4 KB — well above the
/// worker's 1 KB read buffer to leave headroom) survives wincode
/// without truncation.
#[test]
fn terminal_output_produced_round_trips_wincode_with_large_chunk() {
    let body = "abcdefgh".repeat(512); // 4 KB
    let msg = WorkerToService::TerminalOutputProduced(TerminalOutputProducedPayload {
        connection_id: "conn-term".to_string(),
        data: TerminalOutputData {
            content: body.clone(),
            assistant_object_ref: Some(desk_agent_protocol::computer_use::ObjectRef {
                token: "opaque-terminal-token".into(),
                snapshot_id: "worker-1:4".into(),
                object_kind: desk_agent_protocol::computer_use::ObjectKind::TerminalOutput,
                expires_at: "2026-08-25T20:00:00Z".into(),
            }),
        },
    });
    match wincode_round_trip(&msg) {
        WorkerToService::TerminalOutputProduced(p) => {
            assert_eq!(p.connection_id, "conn-term");
            assert_eq!(p.data.content.len(), body.len());
            assert_eq!(p.data.content, body);
            assert_eq!(
                p.data.assistant_object_ref.unwrap().object_kind,
                desk_agent_protocol::computer_use::ObjectKind::TerminalOutput
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// === Additional payloads — round-trip tests ===

/// `SignalingError.signaling_type` is `SignalingType`, a facade enum
/// whose wincode derive uses `tag_encoding = "i32"` matched to its
/// `#[repr(i32)]` discriminants. Round-trip a representative
/// type + non-zero `error_code` + an explicit message so a
/// wire-format drift on any field shows up as a test failure rather
/// than a silent corruption that swaps which `SignalingType` the
/// browser thinks the error belongs to.
#[test]
fn signaling_error_round_trips_wincode() {
    let msg = WorkerToService::SignalingError(SignalingErrorPayload {
        request_id: "req-err-1".to_string(),
        connection_id: "conn-err".to_string(),
        signaling_type: SignalingType::TerminalStarted,
        error_code: 401,
        error_message: Some("Permission denied".to_string()),
    });
    match wincode_round_trip(&msg) {
        WorkerToService::SignalingError(p) => {
            assert_eq!(p.request_id, "req-err-1");
            assert_eq!(p.connection_id, "conn-err");
            assert_eq!(p.signaling_type, SignalingType::TerminalStarted);
            assert_eq!(p.error_code, 401);
            assert_eq!(p.error_message.as_deref(), Some("Permission denied"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    // `error_message` is Option<String>; verify the None case
    // (some send_error callers omit a message).
    let msg = WorkerToService::SignalingError(SignalingErrorPayload {
        request_id: "req-err-2".to_string(),
        connection_id: "conn-err".to_string(),
        signaling_type: SignalingType::FilesListed,
        error_code: -1,
        error_message: None,
    });
    match wincode_round_trip(&msg) {
        WorkerToService::SignalingError(p) => {
            assert_eq!(p.error_code, -1);
            assert!(p.error_message.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `TerminalCommandsListed` ferries `TerminalList` over the wincode
/// derive. Round-trip a non-empty list so a
/// stripped field shows up as a test failure rather than a silent
/// wire-format drift.
#[test]
fn terminal_commands_listed_round_trips_wincode() {
    let terminals = TerminalList {
        commands: vec![
            vec!["C:\\Windows\\System32\\cmd.exe".to_string()],
            vec!["C:\\Program Files\\PowerShell\\7\\pwsh.exe".to_string()],
        ],
        current: 1,
    };
    let msg = WorkerToService::TerminalCommandsListed(TerminalCommandsListedPayload {
        request_id: "req-list".to_string(),
        connection_id: Some("conn-list".to_string()),
        terminals,
    });
    match wincode_round_trip(&msg) {
        WorkerToService::TerminalCommandsListed(p) => {
            assert_eq!(p.request_id, "req-list");
            assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
            assert_eq!(p.terminals.commands.len(), 2);
            assert_eq!(p.terminals.current, 1);
            assert_eq!(p.terminals.commands[0][0], "C:\\Windows\\System32\\cmd.exe");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// === ServiceToWorker / WorkerToService full-variant coverage ===

#[test]
fn wayland_portal_commands_and_status_keep_operation_fencing() {
    let authorize = ServiceToWorker::AuthorizeWaylandPortal(AuthorizeWaylandPortalPayload {
        operation_id: "portal-op-7".to_string(),
        target: desk_wayland_portal::AuthorizationTarget::ScreenAndInput,
    });
    match wincode_round_trip(&authorize) {
        ServiceToWorker::AuthorizeWaylandPortal(payload) => {
            assert_eq!(payload.operation_id, "portal-op-7");
            assert_eq!(
                payload.target,
                desk_wayland_portal::AuthorizationTarget::ScreenAndInput
            );
        }
        other => panic!("unexpected: {other:?}"),
    }

    let mut snapshot = desk_wayland_portal::PortalSnapshot::not_configured(
        desk_wayland_portal::PortalAvailability::default(),
    );
    snapshot.operation_id = Some("portal-op-7".to_string());
    snapshot.generation = 7;
    let status = WorkerToService::WaylandPortalStatus(WaylandPortalStatusPayload { snapshot });
    match wincode_round_trip(&status) {
        WorkerToService::WaylandPortalStatus(payload) => {
            assert_eq!(
                payload.snapshot.operation_id.as_deref(),
                Some("portal-op-7")
            );
            assert_eq!(payload.snapshot.generation, 7);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Exhaustive `ServiceToWorker` round-trip. Per-variant tests above
/// cover the field-level invariants for the high-traffic variants;
/// this table-driven test guarantees **every** variant — including
/// the body-less ones (`Shutdown`) and the manager / terminal
/// envelopes — has wincode `SchemaWrite` + `SchemaRead` wired up
/// before e2e. Any variant missing a derive or carrying a payload
/// without wincode support breaks here, not on the wire.
///
/// If the compiler complains about a non-exhaustive `match` after a
/// new `ServiceToWorker` variant is added, extend the `cases`
/// vec **and** the matching round-trip discriminant check below.
#[test]
fn service_to_worker_all_variants_round_trip() {
    let cases: Vec<ServiceToWorker> = vec![
        ServiceToWorker::Init(WorkerInitPayload {
            session_id: "s".to_string(),
            os_session_id: 1,
            desktop_name: Some("Default".to_string()),
            config_json: "{}".to_string(),
            log_dir: None,
            signaling_url: None,
            auth_token: None,
            host_upstream_url: None,
            media_pipe_name: None,
            file_pipe_name: None,
            remote_access_locked: false,
            remote_access_state_version: 1,
        }),
        ServiceToWorker::Shutdown,
        ServiceToWorker::SetRemoteAccessState(RemoteAccessStatePayload {
            operation_id: "lock-op".to_string(),
            state_version: 2,
            locked: true,
        }),
        ServiceToWorker::StartMedia(StartMediaPayload {
            connection_id: "c".to_string(),
            connection_epoch: "epoch-c".to_string(),
            video_generation: 1,
            audio_generation: 1,
            video_codec: MediaCodec::H264,
            video_encoder: desk_signal_facade::model::media_capability::VideoEncoderId::X264,
            video_device: None,
            fps: 30,
            bitrate_kbps: 4_000,
            quality: 0,
            start_video: true,
            audio: Some(StartAudioSettings {
                codec: MediaCodec::Opus,
                pipeline: AudioPipelineSettings {
                    audio_capture: "default".to_string(),
                    audio_device: SelectedAudioDevice {
                        audio_data_flow: AudioDataFlow::Render,
                        audio_device_id: None,
                    },
                    audio_encoder: desk_signal_facade::model::remote_session::AudioEncoderId::Opus,
                },
            }),
            image_capture: "default".to_string(),
            resolved_wayland_control_mode: None,
            enable_dirty_rect: false,
            show_mouse: false,
        }),
        ServiceToWorker::StopMedia(StopMediaPayload {
            connection_id: "c".to_string(),
            connection_epoch: "epoch-c".to_string(),
        }),
        ServiceToWorker::UpdateMediaSettings(UpdateMediaSettingsPayload {
            connection_id: "c".to_string(),
            connection_epoch: "epoch-c".to_string(),
            video_generation: 1,
            fps: Some(60),
            bitrate_kbps: Some(6_000),
            quality: Some(50),
            enable_dirty_rect: Some(false),
            show_mouse: Some(true),
        }),
        ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
            connection_id: "c".to_string(),
        }),
        ServiceToWorker::MouseInput(InputPayload {
            connection_id: "c".to_string(),
            data: vec![1, 2, 3],
        }),
        ServiceToWorker::MouseMoveInput(InputPayload {
            connection_id: "c".to_string(),
            data: vec![4, 5, 6],
        }),
        ServiceToWorker::KeyboardInput(InputPayload {
            connection_id: "c".to_string(),
            data: vec![7, 8, 9],
        }),
        ServiceToWorker::ClipboardWrite(ClipboardPayload {
            connection_id: "c".to_string(),
            data: vec![0xAA],
        }),
        ServiceToWorker::ClipboardRequest(ConnectionRefPayload {
            connection_id: "c".to_string(),
        }),
        ServiceToWorker::WhiteboardCommand(OpaqueConnectionPayload {
            connection_id: "c".to_string(),
            data: vec![0xBB, 0xCC],
        }),
        ServiceToWorker::SetPrivateScreenVisibility(SetPrivateScreenVisibilityPayload {
            request_id: "r-ps".to_string(),
            connection_id: "c".to_string(),
            visible: true,
        }),
        ServiceToWorker::GetSystemInfo(ManagerRequestRefPayload {
            request_id: "r1".to_string(),
            connection_id: Some("c".to_string()),
        }),
        ServiceToWorker::ListFiles(ListFilesPayload {
            request_id: "r2".to_string(),
            connection_id: "c".to_string(),
            params: FileListParams::default(),
        }),
        ServiceToWorker::DeleteFile(DeleteFilePayload {
            request_id: "r3".to_string(),
            connection_id: "c".to_string(),
            request: DeleteFileRequest::default(),
        }),
        ServiceToWorker::SetLocale(SetLocalePayload {
            operation_id: "op-locale".to_string(),
            locale: "en-US".to_string(),
        }),
        ServiceToWorker::StartTerminal(StartTerminalPayload {
            request_id: "r6".to_string(),
            connection_id: "c".to_string(),
            session: StartTerminalSession {
                command: "cmd.exe".to_string(),
                device_id: None,
                grant_session_id: None,
            },
        }),
        ServiceToWorker::SendTerminalInput(SendTerminalInputPayload {
            connection_id: "c".to_string(),
            data: TerminalInputData {
                content: "ls\n".to_string(),
            },
        }),
        ServiceToWorker::ResizeTerminal(ResizeTerminalPayload {
            connection_id: "c".to_string(),
            data: TerminalResizeData { rows: 24, cols: 80 },
        }),
        ServiceToWorker::CloseTerminal(CloseTerminalPayload {
            connection_id: "c".to_string(),
        }),
        ServiceToWorker::ListTerminalCommands(ListTerminalCommandsPayload {
            request_id: "r7".to_string(),
            connection_id: Some("c".to_string()),
        }),
        ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
            connection_id: "c".to_string(),
            transfer_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
            chunk_index: Some(0),
            kind: FileTransferSendErrorKind::PacketTooLarge,
            error: "outbound packet too large".to_string(),
        }),
        ServiceToWorker::SetVirtualDisplayMode(SetVirtualDisplayModePayload {
            request_id: "r8".to_string(),
            connection_id: "c".to_string(),
            connection_epoch: "epoch".to_string(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }),
        ServiceToWorker::AttachVirtualDisplay(AttachVirtualDisplayPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
        }),
        ServiceToWorker::DetachVirtualDisplay,
        ServiceToWorker::RefreshCapabilities,
        ServiceToWorker::InvokeAgentCapability(AgentRequestPayload {
            request_id: "r-ai".to_string(),
            connection_id: Some("c".to_string()),
            envelope: sample_readonly_agent_envelope(),
        }),
        ServiceToWorker::ComputerActionPlan(ComputerActionPlanPayload {
            request_id: "r-computer".to_string(),
            connection_id: Some("c".to_string()),
            plan: sample_computer_action_plan(),
        }),
        ServiceToWorker::ComputerActionCancel(ComputerActionCancelPayload {
            request_id: "r-computer-cancel".to_string(),
            connection_id: Some("c".to_string()),
            cancel: desk_agent_protocol::computer_use::ComputerActionCancel {
                work_id: "work-1".to_string(),
                action_request_id: "action-1".to_string(),
                execution_generation: "generation-1".to_string(),
                reason: "owner stopped".to_string(),
            },
        }),
        ServiceToWorker::ComputerActionStateQuery(ComputerActionStateQueryPayload {
            request_id: "r-computer-query".to_string(),
            connection_id: Some("c".to_string()),
            query: desk_agent_protocol::computer_use::ComputerActionStateQuery {
                work_id: "work-1".to_string(),
                action_request_id: "action-1".to_string(),
                execution_generation: "generation-1".to_string(),
            },
        }),
        ServiceToWorker::ExecPlan(ExecPlanPayload {
            request_id: "r-exec".to_string(),
            connection_id: Some("c".to_string()),
            plan: sample_exec_plan(),
            audit_source_request_id: Some("frame-req".to_string()),
        }),
        ServiceToWorker::UpdateSecurityPolicy(UpdateSecurityPolicyPayload {
            operation_id: "op-policy".to_string(),
            snapshot: PolicySnapshot::new(SecuritySettings::default()),
        }),
        ServiceToWorker::AuthorizeWaylandPortal(AuthorizeWaylandPortalPayload {
            operation_id: "op-wayland".to_string(),
            target: desk_wayland_portal::AuthorizationTarget::ScreenAndInput,
        }),
        ServiceToWorker::CancelWaylandPortal(CancelWaylandPortalPayload {
            operation_id: "op-wayland".to_string(),
            generation: 4,
        }),
    ];
    for case in &cases {
        let decoded = wincode_round_trip(case);
        // Discriminant equality is enough — per-variant payload
        // assertions live in the variant-specific tests above.
        assert_eq!(
            std::mem::discriminant(case),
            std::mem::discriminant(&decoded),
            "variant {case:?} did not round-trip to the same discriminant"
        );
    }
}

/// A published policy has to arrive with its ordering intact. The sequence and
/// the per-capability stamps are what the receiving side uses to decide whether
/// to adopt it and what to invalidate; a snapshot that arrived without them
/// would be indistinguishable from an initial one.
#[test]
fn a_published_policy_keeps_its_ordering_across_the_wire() {
    let mut snapshot = PolicySnapshot::new(SecuritySettings::default());
    snapshot.set(SecuritySettings {
        allow_terminal: Some(false),
        ..SecuritySettings::default()
    });

    let published = ServiceToWorker::UpdateSecurityPolicy(UpdateSecurityPolicyPayload {
        operation_id: "op-1".to_string(),
        snapshot: snapshot.clone(),
    });
    match wincode_round_trip(&published) {
        ServiceToWorker::UpdateSecurityPolicy(payload) => {
            assert_eq!(payload.operation_id, "op-1");
            assert_eq!(payload.snapshot, snapshot);
            assert_eq!(payload.snapshot.seq(), snapshot.seq());
            assert_eq!(
                payload
                    .snapshot
                    .changed_at(SecurityPermissionType::Terminal),
                snapshot.seq(),
                "the capability stamp has to survive, not just the policy"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// A remembered answer is only safe to apply against the policy it was given
/// under, so the stamp has to arrive with it. A payload that lost the stamp
/// would be indistinguishable from one answering the current policy.
#[test]
fn a_remembered_answer_carries_the_policy_it_was_given_under() {
    let upstream = WorkerToService::RememberSecurityDecision(RememberSecurityDecisionPayload {
        capability: SecurityPermissionType::PrivateScreen,
        approved: false,
        expected_generation: 12,
    });
    match wincode_round_trip(&upstream) {
        WorkerToService::RememberSecurityDecision(payload) => {
            assert_eq!(payload.capability, SecurityPermissionType::PrivateScreen);
            assert!(!payload.approved);
            assert_eq!(payload.expected_generation, 12);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// A locale acknowledgement that lost its operation id could be mistaken for
/// the answer to a later change, which is how a stale ack used to roll the
/// locale back.
#[test]
fn a_locale_instruction_and_its_acknowledgement_stay_paired() {
    let instruction = ServiceToWorker::SetLocale(SetLocalePayload {
        operation_id: "op-9".to_string(),
        locale: "en-US".to_string(),
    });
    match wincode_round_trip(&instruction) {
        ServiceToWorker::SetLocale(payload) => {
            assert_eq!(payload.operation_id, "op-9");
            assert_eq!(payload.locale, "en-US");
        }
        other => panic!("unexpected: {other:?}"),
    }

    let ack = WorkerToService::LocaleApplied(LocaleAppliedPayload {
        operation_id: "op-9".to_string(),
        locale: "en-US".to_string(),
    });
    match wincode_round_trip(&ack) {
        WorkerToService::LocaleApplied(payload) => {
            assert_eq!(payload.operation_id, "op-9");
            assert_eq!(payload.locale, "en-US");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// The outcome is what distinguishes a converged worker from one holding a
/// locally tightened policy. A `NeedsResync` that decoded as `Applied` would
/// report the exact opposite of the truth and stop the daemon republishing.
#[test]
fn policy_apply_outcomes_round_trip() {
    for outcome in [
        PolicyApplyOutcome::Applied {
            seq: 3,
            generations: PolicyGenerations {
                allow_terminal: 3,
                ..PolicyGenerations::default()
            },
        },
        PolicyApplyOutcome::NeedsResync { seq: 9 },
    ] {
        let applied = WorkerToService::SecurityPolicyApplied(SecurityPolicyAppliedPayload {
            operation_id: "op-2".to_string(),
            outcome: outcome.clone(),
        });
        match wincode_round_trip(&applied) {
            WorkerToService::SecurityPolicyApplied(payload) => {
                assert_eq!(payload.operation_id, "op-2");
                assert_eq!(payload.outcome, outcome);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// File activity payload fields survive the event-lane codec.
#[test]
fn file_activity_events_round_trip() {
    let started = WorkerToService::FileTransferStarted(FileTransferStartedPayload {
        connection_id: "conn-file".to_string(),
        transfer_id: "transfer-1".to_string(),
        direction: FileTransferDirection::Upload,
        file_name: "photo.png".to_string(),
        total_bytes: 4096,
    });
    match wincode_round_trip(&started) {
        WorkerToService::FileTransferStarted(payload) => {
            assert_eq!(payload.connection_id, "conn-file");
            assert_eq!(payload.transfer_id, "transfer-1");
            assert_eq!(payload.direction, FileTransferDirection::Upload);
            assert_eq!(payload.file_name, "photo.png");
            assert_eq!(payload.total_bytes, 4096);
        }
        other => panic!("unexpected: {other:?}"),
    }

    let finished = WorkerToService::FileTransferFinished(FileTransferFinishedPayload {
        connection_id: "conn-file".to_string(),
        transfer_id: "transfer-1".to_string(),
        outcome: FileTransferOutcome::Failed,
    });
    match wincode_round_trip(&finished) {
        WorkerToService::FileTransferFinished(payload) => {
            assert_eq!(payload.connection_id, "conn-file");
            assert_eq!(payload.transfer_id, "transfer-1");
            assert_eq!(payload.outcome, FileTransferOutcome::Failed);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Every worker event keeps its enum discriminant across wincode.
#[test]
fn worker_to_service_all_variants_round_trip() {
    let cases: Vec<WorkerToService> = vec![
        WorkerToService::Ready,
        WorkerToService::Capabilities(MediaCapabilities::default()),
        WorkerToService::WaylandPortalStatus(WaylandPortalStatusPayload {
            snapshot: desk_wayland_portal::PortalSnapshot::not_configured(
                desk_wayland_portal::PortalAvailability::default(),
            ),
        }),
        WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            signaling_type: SignalingType::Error,
            error_code: 1,
            error_message: None,
        }),
        WorkerToService::Heartbeat(HeartbeatPayload {
            timestamp_ms: 1,
            active_connections: 0,
            cpu_usage: None,
            memory_usage: None,
        }),
        WorkerToService::DesktopChanged(DesktopChangedPayload {
            name: "Default".to_string(),
        }),
        WorkerToService::Error(ErrorPayload {
            code: -1,
            message: "boom".to_string(),
            recoverable: false,
            connection_id: None,
        }),
        WorkerToService::ClipboardRead(ClipboardPayload {
            connection_id: "c".to_string(),
            data: vec![0xDE, 0xAD],
        }),
        WorkerToService::CursorData(CursorDataPayload {
            connection_id: "c".to_string(),
            data: vec![0xBE, 0xEF],
        }),
        WorkerToService::FileManagerOpened(FileManagerOpenedPayload {
            connection_id: "c".to_string(),
        }),
        WorkerToService::FileTransferStarted(FileTransferStartedPayload {
            connection_id: "c".to_string(),
            transfer_id: "t".to_string(),
            direction: FileTransferDirection::Download,
            file_name: "report.txt".to_string(),
            total_bytes: 42,
        }),
        WorkerToService::FileTransferFinished(FileTransferFinishedPayload {
            connection_id: "c".to_string(),
            transfer_id: "t".to_string(),
            outcome: FileTransferOutcome::Completed,
        }),
        WorkerToService::PrivateScreenStateChanged(PrivateScreenStateChangedPayload {
            request_id: None,
            connection_id: "c".to_string(),
            data: PrivateScreenStateChangedData {
                visible: true,
                is_supported: true,
                error_msg: None,
            },
        }),
        WorkerToService::MediaPipelineState(MediaPipelineStatePayload {
            connection_id: "c".to_string(),
            connection_epoch: "epoch-c".to_string(),
            video_generation: 1,
            data: MediaPipelineStateData {
                phase: MediaPipelinePhase::Blocked,
                encoder: Some(VideoEncoderId::OpenH264),
                source_resolution: Some(Resolution::new(4096, 2160)),
                compatible_encoders: vec![VideoEncoderId::X264],
                reason_code: None,
                message: Some("unsupported dimensions".to_string()),
            },
        }),
        WorkerToService::SystemInfoRetrieved(SystemInfoRetrievedPayload {
            request_id: "r".to_string(),
            connection_id: Some("c".to_string()),
            info: SystemInfo::default(),
        }),
        WorkerToService::FilesListed(FilesListedPayload {
            request_id: "r".to_string(),
            connection_id: Some("c".to_string()),
            response: FileListResponse {
                file_info_list: vec![],
                total_count: 0,
            },
        }),
        WorkerToService::FileDeleted(ManagerResponseRefPayload {
            request_id: "r".to_string(),
            connection_id: Some("c".to_string()),
        }),
        WorkerToService::LocaleApplied(LocaleAppliedPayload {
            operation_id: "op-locale".to_string(),
            locale: "en-US".to_string(),
        }),
        WorkerToService::SecurityPolicyApplied(SecurityPolicyAppliedPayload {
            operation_id: "op-policy".to_string(),
            outcome: PolicyApplyOutcome::Applied {
                seq: 7,
                generations: PolicyGenerations::default(),
            },
        }),
        WorkerToService::RememberSecurityDecision(RememberSecurityDecisionPayload {
            capability: SecurityPermissionType::FileTransfer,
            approved: true,
            expected_generation: 4,
        }),
        WorkerToService::TerminalStarted(TerminalStartedPayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
        }),
        WorkerToService::TerminalClosed(TerminalClosedPayload {
            connection_id: "c".to_string(),
        }),
        WorkerToService::TerminalOutputProduced(TerminalOutputProducedPayload {
            connection_id: "c".to_string(),
            data: TerminalOutputData {
                content: "hi".to_string(),
                assistant_object_ref: None,
            },
        }),
        WorkerToService::TerminalCommandsListed(TerminalCommandsListedPayload {
            request_id: "r".to_string(),
            connection_id: Some("c".to_string()),
            terminals: TerminalList {
                commands: vec![],
                current: 0,
            },
        }),
        WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            connection_epoch: "epoch".to_string(),
            outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }),
        }),
        WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
        }),
        WorkerToService::AgentCapabilityCompleted(AgentResponsePayload {
            request_id: "r-ai".to_string(),
            connection_id: Some("c".to_string()),
            outcome: desk_agent_protocol::AgentOutcome::Err(desk_agent_protocol::AgentError {
                kind: desk_agent_protocol::AgentErrorKind::Internal,
                message: "x".to_string(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        }),
        WorkerToService::ComputerActionStarted(ComputerActionStartedPayload {
            request_id: "r-computer".to_string(),
            connection_id: Some("c".to_string()),
            started: desk_agent_protocol::computer_use::ComputerActionStarted {
                work_id: "work-1".to_string(),
                action_request_id: "action-1".to_string(),
                execution_generation: "generation-1".to_string(),
                disposition:
                    desk_agent_protocol::computer_use::ComputerActionStartDisposition::MayHaveStarted,
                reason: None,
            },
        }),
        WorkerToService::ComputerActionCompleted(ComputerActionCompletedPayload {
            request_id: "r-computer".to_string(),
            connection_id: Some("c".to_string()),
            completed: desk_agent_protocol::computer_use::ComputerActionCompleted {
                work_id: "work-1".to_string(),
                action_request_id: "action-1".to_string(),
                execution_generation: "generation-1".to_string(),
                result: desk_agent_protocol::computer_use::ComputerActionResultClass::Verified,
                facts: vec![],
                message: None,
            },
        }),
        WorkerToService::ComputerActionStateReported(ComputerActionStateReportedPayload {
            request_id: "r-computer-query".to_string(),
            connection_id: Some("c".to_string()),
            state: desk_agent_protocol::computer_use::ComputerActionStateReport {
                work_id: "work-1".to_string(),
                action_request_id: "action-1".to_string(),
                execution_generation: "generation-1".to_string(),
                phase: desk_agent_protocol::computer_use::ComputerActionPhase::Completed,
                result: Some(
                    desk_agent_protocol::computer_use::ComputerActionResultClass::Verified,
                ),
            },
        }),
        WorkerToService::ComputerUseReadinessUpdated(ComputerUseReadinessPayload {
            readiness: sample_computer_use_readiness(),
        }),
        WorkerToService::ExecutionCompleted(ExecResultIpcPayload {
            request_id: "r-exec".to_string(),
            connection_id: Some("c".to_string()),
            result: desk_agent_protocol::exec::ExecResultPayload {
                exec_request_id: desk_agent_protocol::exec::ExecRequestId("e1".to_string()),
                outcome: desk_agent_protocol::AgentOutcome::Err(desk_agent_protocol::AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::Timeout,
                    message: "x".to_string(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }),
            },
            audit_source_request_id: Some("frame-req".to_string()),
        }),
        WorkerToService::RemoteAccessStateApplied(RemoteAccessStateAppliedPayload {
            operation_id: "lock-op".to_string(),
            state_version: 2,
            cancelled_terminals: 1,
            cancelled_transfers: 2,
            cancelled_execs: 3,
        }),
    ];
    for case in &cases {
        let decoded = wincode_round_trip(case);
        assert_eq!(
            std::mem::discriminant(case),
            std::mem::discriminant(&decoded),
            "variant {case:?} did not round-trip to the same discriminant"
        );
    }
}

// === SignalingErrorPayload full SignalingType coverage ===

/// `SignalingErrorPayload.signaling_type` rides the wincode tag
/// on the `SignalingType` enum. Iterate every one of
/// every error-routable variant so a missing `#[wincode(tag = N)]` (or a
/// wrongly-numbered one) surfaces here instead of as a silent
/// browser-side mismatch on a SignalingError reply.
#[test]
fn signaling_error_round_trips_wincode_for_every_signaling_type() {
    let all_types = [
        SignalingType::SendHeartbeat,
        SignalingType::HeartbeatAcknowledged,
        SignalingType::FetchConnections,
        SignalingType::ConnectionsFetched,
        SignalingType::ConnectionRemoved,
        SignalingType::RequestRemoteAccess,
        SignalingType::RemoteAccessInitialized,
        SignalingType::Offer,
        SignalingType::Answer,
        SignalingType::IceCandidate,
        SignalingType::RequireControl,
        SignalingType::ControlAccepted,
        SignalingType::ControlDenied,
        SignalingType::ReleaseControl,
        SignalingType::ControlReleased,
        SignalingType::CloseRemoteSession,
        SignalingType::ChangeDisplaySettings,
        SignalingType::SetPrivateScreenVisibility,
        SignalingType::PrivateScreenStateChanged,
        SignalingType::AudioPlaybackFailed,
        SignalingType::MediaPipelineStateChanged,
        SignalingType::RetryMediaPipeline,
        SignalingType::PrivateScreenVisibilitySet,
        SignalingType::DisplaySettingsChanged,
        SignalingType::MediaPipelineRetryCompleted,
        SignalingType::ApplyRemoteSessionSettings,
        SignalingType::RemoteSessionSettingsApplied,
        SignalingType::UpdateAdaptiveVideoQuality,
        SignalingType::SystemAudioCaptureStateChanged,
        SignalingType::GetSystemInfo,
        SignalingType::SystemInfoRetrieved,
        SignalingType::ListFiles,
        SignalingType::FilesListed,
        SignalingType::DeleteFile,
        SignalingType::FileDeleted,
        SignalingType::StartTerminal,
        SignalingType::SendTerminalInput,
        SignalingType::ResizeTerminal,
        SignalingType::CloseTerminal,
        SignalingType::TerminalOutputProduced,
        SignalingType::ListTerminalCommands,
        SignalingType::TerminalCommandsListed,
        SignalingType::TerminalStarted,
        SignalingType::TerminalClosed,
        SignalingType::DesktopSwitching,
        SignalingType::DesktopReady,
        SignalingType::Error,
        SignalingType::Unknown,
    ];
    assert_eq!(
        all_types.len(),
        48,
        "SignalingType variant count drift — add new variant + tag here"
    );
    for ty in all_types {
        let original = WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: format!("req-{}", ty as i32),
            connection_id: "c".to_string(),
            signaling_type: ty,
            error_code: ty as i32,
            error_message: Some(format!("{ty:?}")),
        });
        match wincode_round_trip(&original) {
            WorkerToService::SignalingError(p) => {
                assert_eq!(
                    p.signaling_type as i32, ty as i32,
                    "signaling_type discriminant drift for {ty:?}"
                );
                assert_eq!(p.error_code, ty as i32);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

// ============== Virtual display variants ==============

#[test]
fn set_virtual_display_mode_round_trips_wincode() {
    let msg = ServiceToWorker::SetVirtualDisplayMode(SetVirtualDisplayModePayload {
        request_id: "req-1".to_string(),
        connection_id: "conn-1".to_string(),
        connection_epoch: "epoch-1".to_string(),
        width: 2560,
        height: 1440,
        refresh_hz: 144,
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.request_id, "req-1");
            assert_eq!(p.connection_id, "conn-1");
            assert_eq!(p.width, 2560);
            assert_eq!(p.height, 1440);
            assert_eq!(p.refresh_hz, 144);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn set_virtual_display_mode_round_trips_serde_json() {
    let msg = ServiceToWorker::SetVirtualDisplayMode(SetVirtualDisplayModePayload {
        request_id: "req-1".to_string(),
        connection_id: "conn-1".to_string(),
        connection_epoch: "epoch-1".to_string(),
        width: 1280,
        height: 720,
        refresh_hz: 60,
    });
    let json = serde_json::to_string(&msg).expect("encode");
    let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
    match back {
        ServiceToWorker::SetVirtualDisplayMode(p) => {
            assert_eq!(p.width, 1280);
            assert_eq!(p.height, 720);
            assert_eq!(p.refresh_hz, 60);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn attach_virtual_display_round_trips_wincode() {
    let msg = ServiceToWorker::AttachVirtualDisplay(AttachVirtualDisplayPayload {
        instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::AttachVirtualDisplay(p) => {
            assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn attach_virtual_display_round_trips_serde_json() {
    let msg = ServiceToWorker::AttachVirtualDisplay(AttachVirtualDisplayPayload {
        instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
    });
    let json = serde_json::to_string(&msg).expect("encode");
    let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
    match back {
        ServiceToWorker::AttachVirtualDisplay(p) => {
            assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn virtual_display_attach_outcome_attached_wincode_roundtrip() {
    let original = WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
        instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
    });
    match wincode_round_trip(&original) {
        WorkerToService::VirtualDisplayAttachResult(p) => {
            assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
            assert_eq!(
                p.outcome,
                VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
            );
        }
        other => panic!("unexpected variant after wincode round-trip: {other:?}"),
    }
}

#[test]
fn virtual_display_attach_outcome_failed_wincode_roundtrip() {
    let original = WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
        instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
        outcome: VirtualDisplayAttachOutcome::Failed(
            "find_display_name: seen=[] after 6 retries".to_string(),
        ),
    });
    match wincode_round_trip(&original) {
        WorkerToService::VirtualDisplayAttachResult(p) => {
            assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
            assert!(
                matches!(p.outcome, VirtualDisplayAttachOutcome::Failed(ref msg) if msg.contains("seen=[]")),
                "expected Failed with diagnostic message, got {:?}",
                p.outcome
            );
        }
        other => panic!("unexpected variant after wincode round-trip: {other:?}"),
    }
}

#[test]
fn worker_to_service_virtual_display_attach_result_serde_attached_and_failed() {
    // serde JSON is the on-wire form used by anything that bridges
    // the wincode IPC frames out to text (e.g. log diagnostics or
    // future REST-shaped tooling). Cover both Attached + Failed
    // variants so a future enum tweak (renaming, changing tag
    // attributes) gets flagged here.
    for case in [
        WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
        }),
        WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
            outcome: VirtualDisplayAttachOutcome::Failed("driver pipe IO failed".to_string()),
        }),
    ] {
        let json = serde_json::to_string(&case).expect("encode");
        let back: WorkerToService = serde_json::from_str(&json).expect("decode");
        match (case, back) {
            (
                WorkerToService::VirtualDisplayAttachResult(a),
                WorkerToService::VirtualDisplayAttachResult(b),
            ) => {
                assert_eq!(a.instance_id, b.instance_id);
                assert_eq!(a.outcome, b.outcome);
            }
            (a, b) => panic!("round-trip variant drift: {a:?} -> {b:?}"),
        }
    }
}

#[test]
fn detach_virtual_display_round_trips_wincode() {
    let msg = ServiceToWorker::DetachVirtualDisplay;
    let back = wincode_round_trip(&msg);
    assert!(matches!(back, ServiceToWorker::DetachVirtualDisplay));
}

#[test]
fn detach_virtual_display_round_trips_serde_json() {
    let msg = ServiceToWorker::DetachVirtualDisplay;
    let json = serde_json::to_string(&msg).expect("encode");
    let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
    assert!(matches!(back, ServiceToWorker::DetachVirtualDisplay));
}

/// New in v4: daemon → worker `RefreshCapabilities` is a unit
/// variant. Both encodings must round-trip cleanly so future
/// daemon / worker version drift cannot silently corrupt the
/// virtual-display capabilities refresh path.
#[test]
fn refresh_capabilities_round_trips_wincode() {
    let msg = ServiceToWorker::RefreshCapabilities;
    let back = wincode_round_trip(&msg);
    assert!(matches!(back, ServiceToWorker::RefreshCapabilities));
}

#[test]
fn refresh_capabilities_round_trips_serde_json() {
    let msg = ServiceToWorker::RefreshCapabilities;
    let json = serde_json::to_string(&msg).expect("encode");
    let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
    assert!(matches!(back, ServiceToWorker::RefreshCapabilities));
}

#[test]
fn virtual_display_mode_response_applied_round_trips_wincode() {
    let msg = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id: "req-9".to_string(),
        connection_id: "conn-9".to_string(),
        connection_epoch: "epoch-9".to_string(),
        outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        }),
    });
    match wincode_round_trip(&msg) {
        WorkerToService::VirtualDisplayMode(p) => {
            assert_eq!(p.request_id, "req-9");
            assert_eq!(p.connection_id, "conn-9");
            match p.outcome {
                VirtualDisplayModeOutcome::Applied(m) => {
                    assert_eq!(m.width, 1920);
                    assert_eq!(m.height, 1080);
                    assert_eq!(m.refresh_hz, 60);
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn virtual_display_mode_response_failed_round_trips_wincode() {
    let msg = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id: "req-10".to_string(),
        connection_id: "conn-10".to_string(),
        connection_epoch: "epoch-10".to_string(),
        outcome: VirtualDisplayModeOutcome::Failed("driver pipe IO failed".to_string()),
    });
    match wincode_round_trip(&msg) {
        WorkerToService::VirtualDisplayMode(p) => match p.outcome {
            VirtualDisplayModeOutcome::Failed(reason) => {
                assert_eq!(reason, "driver pipe IO failed");
            }
            other => panic!("unexpected outcome: {other:?}"),
        },
        other => panic!("unexpected: {other:?}"),
    }
}

/// `desired = true` round-trips with a non-trivial `op_id` and
/// `prompt_duration_ms`. Pins the wire shape (struct layout +
/// field order) so a future schema-write edit immediately surfaces
/// in CI.
#[test]
fn set_virtual_display_exclusive_enter_round_trips_wincode() {
    let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
        op_id: 42,
        desired: true,
        prompt_duration_ms: 5_000,
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::SetVirtualDisplayExclusive(p) => {
            assert_eq!(p.op_id, 42);
            assert!(p.desired);
            assert_eq!(p.prompt_duration_ms, 5_000);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `desired = false` round-trips. `prompt_duration_ms` is preserved
/// even though the worker ignores it for leave requests — the
/// wire format does not get to skip the field.
#[test]
fn set_virtual_display_exclusive_leave_round_trips_wincode() {
    let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
        op_id: u64::MAX - 1,
        desired: false,
        prompt_duration_ms: 0,
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::SetVirtualDisplayExclusive(p) => {
            assert_eq!(p.op_id, u64::MAX - 1);
            assert!(!p.desired);
            assert_eq!(p.prompt_duration_ms, 0);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// JSON encoding is the secondary wire (used by some test helpers
/// and the manager's REST surface for debugging). The op_id must
/// round-trip through JSON too — `serde_json` defaults to a u64
/// number which is fine up to 2^53 in JSON parsers; tests pin the
/// representation so a switch to a string encoding (e.g. to avoid
/// the JS precision cliff) shows up as a failing test.
#[test]
fn set_virtual_display_exclusive_round_trips_serde_json() {
    let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
        op_id: 7,
        desired: true,
        prompt_duration_ms: 5_000,
    });
    let json = serde_json::to_string(&msg).expect("encode");
    let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
    match back {
        ServiceToWorker::SetVirtualDisplayExclusive(p) => {
            assert_eq!(p.op_id, 7);
            assert!(p.desired);
            assert_eq!(p.prompt_duration_ms, 5_000);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Every `ExclusiveOutcome` variant must round-trip. The pipeline
/// emits exactly these four shapes; a regression that adds or
/// removes one is a wire break. EnterCancelled is intentionally
/// absent because it is not part of the wire contract.
#[test]
fn exclusive_result_all_four_outcomes_round_trip_wincode() {
    let cases = [
        (
            100u64,
            ExclusiveDirection::Entering,
            ExclusiveOutcome::Entered,
        ),
        (
            101u64,
            ExclusiveDirection::Entering,
            ExclusiveOutcome::EnterFailed("snapshot failed".to_string()),
        ),
        (102u64, ExclusiveDirection::Leaving, ExclusiveOutcome::Left),
        (
            103u64,
            ExclusiveDirection::Leaving,
            ExclusiveOutcome::LeftWithErrors("partial: \\\\.\\DISPLAY2".to_string()),
        ),
    ];
    for (op_id, direction, outcome) in cases {
        let msg = WorkerToService::ExclusiveResult(ExclusiveResultPayload {
            op_id,
            direction,
            outcome: outcome.clone(),
        });
        match wincode_round_trip(&msg) {
            WorkerToService::ExclusiveResult(p) => {
                assert_eq!(p.op_id, op_id);
                assert_eq!(p.direction, direction);
                assert_eq!(p.outcome, outcome);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

/// `op_id` is a u64 — must serialise as 8 bytes little-endian
/// when used through the wincode `Configuration<true, _>` setup
/// the IPC pipeline uses (FixInt + LittleEndian). The first 8
/// bytes after the enum tag + struct framing belong to op_id.
///
/// We don't pin the absolute offset because the enum tag width
/// is wincode-controlled — but encoding a known op_id value of
/// `0x_01_02_03_04_05_06_07_08` (i.e. each byte distinct) and
/// scanning the produced bytes lets us assert the byte sequence
/// `08 07 06 05 04 03 02 01` appears as a contiguous run — the
/// LE bit-pattern. A flip to BE or Varint would not produce that
/// run, so a wire regression fails this test immediately.
#[test]
fn op_id_is_serialized_le_8_bytes() {
    let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
        op_id: 0x_01_02_03_04_05_06_07_08,
        desired: true,
        prompt_duration_ms: 0,
    });
    let config: WincodeUnbounded = Configuration::new();
    let bytes = wincode::config::serialize(&msg, config).expect("encode");
    let needle = [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    let found = bytes.windows(needle.len()).any(|w| w == needle);
    assert!(
        found,
        "expected LE u64 bit pattern in encoded bytes; got {bytes:?}"
    );
}

/// Build a representative server-stamped `AgentEnvelope` for the
/// AI-plane IPC round-trip tests.
fn sample_agent_envelope() -> desk_agent_protocol::AgentEnvelope {
    use desk_agent_protocol::*;
    AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        request_id: RequestId("req-ai-1".to_string()),
        parent_task_id: Some(TaskId("task-ai-1".to_string())),
        target: TargetRef {
            device_id: "dev-1".to_string(),
            session_id: Some("sess-1".to_string()),
            worker_id: None,
        },
        actor: ActorRef {
            actor_type: ActorType::User,
            actor_id: "user-1".to_string(),
        },
        caller: CallerRef {
            caller_type: CallerType::Human,
            model_provider: None,
            model_name: None,
            adapter: None,
        },
        scope: AgentScope {
            granted: vec![Capability::ProcessList],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        operation: AgentOperation {
            risk_hint: None,
            input: OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ProcessList(ProcessListParams::default()),
            }),
        },
        audit: AuditMeta {
            approval_id: None,
            reason: Some("diagnose".to_string()),
        },
    }
}

fn sample_readonly_agent_envelope() -> desk_agent_protocol::ReadonlyAgentEnvelope {
    sample_agent_envelope()
        .try_into()
        .expect("sample envelope is read-only")
}

fn sample_object_ref() -> desk_agent_protocol::computer_use::ObjectRef {
    use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
    ObjectRef {
        token: "opaque-token".to_string(),
        snapshot_id: "snapshot-1".to_string(),
        object_kind: ObjectKind::UiElement,
        expires_at: "2026-08-23T12:00:00Z".to_string(),
    }
}

fn sample_computer_action_plan() -> desk_agent_protocol::computer_use::SealedComputerActionPlan {
    use desk_agent_protocol::computer_use::*;
    SealedComputerActionPlan {
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        work_id: "work-1".to_string(),
        action_request_id: "action-1".to_string(),
        execution_generation: "generation-1".to_string(),
        device_id: "dev-1".to_string(),
        interactive_session_incarnation: "session-1".to_string(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::WindowsUia,
            version: "1".to_string(),
        },
        approval_id: "approval-1".to_string(),
        approved_actor_id: "user-1".to_string(),
        draft_hash: "sha256:draft".to_string(),
        expires_at: "2026-08-23T12:00:00Z".to_string(),
        timeout_ms: 10_000,
        actions: vec![ComputerActionStep {
            target: sample_object_ref(),
            action: ComputerActionKind::Ui(UiSemanticAction::Invoke),
            before_summary: "idle".to_string(),
            after_intent: "invoke".to_string(),
            verification: "state changed".to_string(),
        }],
    }
}

fn sample_computer_use_readiness() -> desk_agent_protocol::computer_use::ComputerUseReadiness {
    use desk_agent_protocol::computer_use::*;
    ComputerUseReadiness {
        schema_version: COMPUTER_USE_SCHEMA_VERSION,
        revision: 1,
        observed_at: "2026-08-23T11:00:00Z".to_string(),
        expires_at: "2026-08-23T11:01:00Z".to_string(),
        server_api_version: 2,
        os: "windows".to_string(),
        interactive_session_incarnation: "session-1".to_string(),
        local_ceiling_revision: 1,
        capabilities: vec![ComputerUseCapabilityReadiness {
            capability: desk_agent_protocol::Capability::DesktopUiInspect,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::WindowsUia,
                version: "1".to_string(),
            },
            supported: true,
            ready: true,
            reason: None,
        }],
        context_references: Vec::new(),
    }
}

fn sample_exec_plan() -> desk_agent_protocol::exec::ExecPlan {
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::{
        ApprovalId, ExecExecutionBasis, ExecPlan, ExecRequestId, ExecShellKind,
    };
    ExecPlan {
        execution_generation: "gen-1".into(),
        exec_request_id: ExecRequestId("exec-1".to_string()),
        program: "docker".to_string(),
        argv: vec!["restart".to_string(), "web1".to_string()],
        cwd: None,
        shell: ExecShellKind::Native,
        risk: RiskLevel::High,
        execution_basis: ExecExecutionBasis::Template,
        template_id: "docker_restart".to_string(),
        approval_id: ApprovalId("appr-1".to_string()),
        fingerprint: "fp".to_string(),
        timeout_ms: 30_000,
        max_stdout_bytes: 65_536,
        max_stderr_bytes: 65_536,
        containment: Default::default(),
    }
}

/// `ServiceToWorker::ExecPlan` carries the full sealed plan across the
/// daemon → worker wire, and `WorkerToService::ExecutionCompleted` carries the
/// `exec_request_id`-tagged result back.
#[test]
fn exec_plan_and_result_round_trip_wincode() {
    let plan_msg = ServiceToWorker::ExecPlan(ExecPlanPayload {
        request_id: "r-exec".to_string(),
        connection_id: Some("conn-1".to_string()),
        plan: sample_exec_plan(),
        audit_source_request_id: Some("frame-req-9".to_string()),
    });
    match wincode_round_trip(&plan_msg) {
        ServiceToWorker::ExecPlan(p) => {
            assert_eq!(p.request_id, "r-exec");
            assert_eq!(p.plan, sample_exec_plan());
            assert_eq!(p.audit_source_request_id.as_deref(), Some("frame-req-9"));
        }
        other => panic!("unexpected: {other:?}"),
    }

    let result_msg = WorkerToService::ExecutionCompleted(ExecResultIpcPayload {
        request_id: "r-exec".to_string(),
        connection_id: Some("conn-1".to_string()),
        result: desk_agent_protocol::exec::ExecResultPayload {
            exec_request_id: desk_agent_protocol::exec::ExecRequestId("exec-1".to_string()),
            outcome: desk_agent_protocol::AgentOutcome::Ok(
                desk_agent_protocol::OperationOutput::Exec(desk_agent_protocol::ExecOutput {
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    duration_ms: 5,
                    redactions: vec![],
                }),
            ),
        },
        audit_source_request_id: Some("frame-req-9".to_string()),
    });
    match wincode_round_trip(&result_msg) {
        WorkerToService::ExecutionCompleted(p) => {
            assert_eq!(p.request_id, "r-exec");
            assert_eq!(p.result.exec_request_id.0, "exec-1");
            assert_eq!(p.audit_source_request_id.as_deref(), Some("frame-req-9"));
            assert!(matches!(
                p.result.outcome,
                desk_agent_protocol::AgentOutcome::Ok(_)
            ));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// `ServiceToWorker::InvokeAgentCapability` carries the full embedded
/// `AgentEnvelope` across the daemon → worker wire byte-for-byte.
#[test]
fn invoke_agent_capability_round_trips_wincode() {
    let msg = ServiceToWorker::InvokeAgentCapability(AgentRequestPayload {
        request_id: "req-ai-1".to_string(),
        connection_id: Some("conn-1".to_string()),
        envelope: sample_readonly_agent_envelope(),
    });
    match wincode_round_trip(&msg) {
        ServiceToWorker::InvokeAgentCapability(p) => {
            assert_eq!(p.request_id, "req-ai-1");
            assert_eq!(p.connection_id.as_deref(), Some("conn-1"));
            assert_eq!(p.envelope, sample_readonly_agent_envelope());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn invoke_agent_capability_cannot_decode_an_exec_envelope() {
    use desk_agent_protocol::{ExecInput, ExecTarget, OperationInput};
    let mut mutation = sample_agent_envelope();
    mutation.operation.input = OperationInput::Exec(ExecInput {
        target: ExecTarget::Shell {
            shell: "powershell".to_string(),
        },
        command: "Get-Service".to_string(),
        cwd: None,
        timeout_ms: 10_000,
        max_stdout_bytes: 65_536,
        max_stderr_bytes: 65_536,
    });
    let config: WincodeUnbounded = Configuration::new();
    let bytes = wincode::config::serialize(&mutation, config).expect("encode legacy envelope");
    let decoded: Result<desk_agent_protocol::ReadonlyAgentEnvelope, _> =
        wincode::config::deserialize(&bytes, config);
    assert!(
        decoded.is_err(),
        "exec bytes must not decode as a read-only envelope"
    );
}

#[test]
fn computer_action_ipc_family_round_trips_independently() {
    let plan = ServiceToWorker::ComputerActionPlan(ComputerActionPlanPayload {
        request_id: "r-computer".to_string(),
        connection_id: Some("conn-1".to_string()),
        plan: sample_computer_action_plan(),
    });
    assert!(matches!(
        wincode_round_trip(&plan),
        ServiceToWorker::ComputerActionPlan(_)
    ));

    let readiness = WorkerToService::ComputerUseReadinessUpdated(ComputerUseReadinessPayload {
        readiness: sample_computer_use_readiness(),
    });
    assert!(matches!(
        wincode_round_trip(&readiness),
        WorkerToService::ComputerUseReadinessUpdated(_)
    ));
}

/// `WorkerToService::AgentCapabilityCompleted` reuses `AgentOutcome` verbatim;
/// both the `Ok` (output) and `Err` (capability-level error) arms
/// survive the worker → daemon wire.
#[test]
fn agent_capability_completed_round_trips_wincode_both_arms() {
    use desk_agent_protocol::*;
    let ok = WorkerToService::AgentCapabilityCompleted(AgentResponsePayload {
        request_id: "req-ai-1".to_string(),
        connection_id: Some("conn-1".to_string()),
        outcome: AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ProcessList(ProcessListOutput {
                processes: vec![],
                truncated: false,
            }),
        )),
    });
    match wincode_round_trip(&ok) {
        WorkerToService::AgentCapabilityCompleted(p) => {
            assert_eq!(p.request_id, "req-ai-1");
            assert!(matches!(p.outcome, AgentOutcome::Ok(_)));
        }
        other => panic!("unexpected: {other:?}"),
    }

    let err = WorkerToService::AgentCapabilityCompleted(AgentResponsePayload {
        request_id: "req-ai-2".to_string(),
        connection_id: None,
        outcome: AgentOutcome::Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "not implemented yet".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }),
    });
    match wincode_round_trip(&err) {
        WorkerToService::AgentCapabilityCompleted(p) => {
            assert_eq!(p.connection_id, None);
            match p.outcome {
                AgentOutcome::Err(e) => {
                    assert_eq!(e.kind, AgentErrorKind::UnsupportedCapability)
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
        other => panic!("unexpected: {other:?}"),
    }
}
