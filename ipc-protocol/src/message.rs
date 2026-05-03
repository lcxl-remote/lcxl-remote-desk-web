use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Messages sent from Service Core (daemon) to Worker process over the
/// **event** transport (low-latency, never-drop). Large media payloads do
/// not travel here — they go on the dedicated media transport (see
/// [`MediaFrame`]).
///
/// Each variant that drives a specific peer connection carries a
/// `connection_id` field so the worker can route to the right
/// per-connection encoder / state. ID-less variants (`Init`, `Shutdown`,
/// etc.) are worker-process-wide.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(tag = "type", content = "payload")]
pub enum ServiceToWorker {
    /// Initialize the worker with session and configuration info
    Init(WorkerInitPayload),

    // ---------- Arch III legacy (deprecated, removed by PR 7) ----------
    /// Forward a signaling message (SDP offer/answer, ICE candidate) to the worker.
    ///
    /// **Deprecated (Arch IV)**: PC moves into the daemon, so signaling no
    /// longer transits the worker. PR 7 will remove this variant once the
    /// daemon-side `signaling_router` (PR 2) handles every `SignalingType`
    /// natively.
    SignalingMessage(SignalingPayload),

    /// Notify the worker that a desktop switch is happening; the worker
    /// should prepare to shut down.
    ///
    /// **Deprecated (Arch IV)**: desktop drift is detected by the worker's
    /// own `desktop_monitor`; it now reports up via
    /// [`WorkerToService::DesktopChanged`]. The daemon decides when to
    /// kill+respawn the worker without needing to pre-announce it.
    DesktopSwitching,

    /// Force the worker to shut down immediately.
    Shutdown,

    // ---------- Arch IV media control (event pipe) ----------
    /// Start a per-connection media pipeline (capture + per-connection
    /// video/audio encoder). Capture is shared across connections; the
    /// encoder is exclusive to `connection_id`.
    StartMedia(StartMediaPayload),

    /// Stop a per-connection media pipeline. The worker fully drops the
    /// encoder + per-connection IPC sender + any per-connection state.
    /// Capture stops only when the last encoder for the current desktop
    /// has stopped.
    StopMedia(StopMediaPayload),

    /// Update encoder parameters mid-stream (bitrate / fps / quality).
    UpdateMediaSettings(UpdateMediaSettingsPayload),

    /// Force the per-connection encoder to emit an IDR (key-) frame on the
    /// next encode call. Sent by the daemon when (a) a new worker is taking
    /// over the connection, or (b) the daemon's RTCP reader saw a PLI for
    /// this connection. **Routed per-connection** (broadcasting would cause
    /// IDR bursts on unrelated browsers).
    ForceKeyframe(ForceKeyframePayload),

    // ---------- Arch IV input / clipboard (event pipe) ----------
    /// Mouse non-move (button / wheel) event from the browser DataChannel.
    /// `data` is the raw payload as decoded from the channel (currently
    /// JSON; the worker re-decodes with its existing input handler).
    /// The daemon authorises (`accept_control`) before forwarding.
    MouseInput(InputPayload),

    /// High-frequency mouse-move event. Carried separately because the
    /// browser sends them on a dedicated DC at >100 Hz; keeping the variant
    /// distinct lets the worker apply move-specific coalescing.
    MouseMoveInput(InputPayload),

    /// Keyboard event from the browser DataChannel.
    KeyboardInput(InputPayload),

    /// Browser → host clipboard write. Only delivered when
    /// `accept_clipboard_sync` is true on the daemon side.
    ClipboardWrite(ClipboardPayload),

    /// Browser asked the host to send back the current clipboard contents.
    /// Worker replies via [`WorkerToService::ClipboardRead`].
    ClipboardRequest(ConnectionRefPayload),

    // ---------- Arch IV file / whiteboard pass-through ----------
    /// Opaque file-transfer command from the browser DataChannel. Worker
    /// dispatches into its existing `file_transfer` module.
    FileTransferCommand(OpaqueConnectionPayload),

    /// Opaque whiteboard / private-screen / Tauri-shell command from the
    /// browser DataChannel. Worker dispatches via the existing
    /// host_control forwarder.
    WhiteboardCommand(OpaqueConnectionPayload),
}

/// Messages sent from Worker process to Service Core (daemon) over the
/// **event** transport.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(tag = "type", content = "payload")]
pub enum WorkerToService {
    /// Worker has started and is ready to accept connections
    Ready,

    /// Worker reports its capability matrix once at startup (codecs +
    /// devices it can drive). The daemon uses this to pick the SDP m-line
    /// codec for new offers and to populate the UI's device pickers.
    Capabilities(MediaCapabilities),

    /// **Deprecated (Arch IV)**: see
    /// [`ServiceToWorker::SignalingMessage`].
    SignalingMessage(SignalingPayload),

    /// Worker reports its health status
    Heartbeat(HeartbeatPayload),

    /// **Deprecated (Arch IV)**: PC stays alive across worker swaps, so
    /// the post-switch "ready" handshake collapses into the daemon
    /// receiving the new worker's `Ready` + `Capabilities` and dispatching
    /// `StartMedia` + `ForceKeyframe`.
    DesktopReady,

    /// Worker detected the user-input desktop changed (e.g. user invoked UAC
    /// → secure desktop "Winlogon", switched to lock screen "ScreenSaver",
    /// etc.). The daemon's session-monitor running in session 0 cannot see
    /// across-window-station desktop changes — only a process living in
    /// the user's WinSta0 (i.e. the worker) can. The daemon reacts by
    /// shutting down this worker and launching a fresh one bound to the
    /// new desktop.
    DesktopChanged(DesktopChangedPayload),

    /// Worker reports an error
    Error(ErrorPayload),

    // ---------- Arch IV upstream events (event pipe) ----------
    /// Worker → daemon clipboard read: either spontaneous (host clipboard
    /// changed and `accept_clipboard_sync` is on for some connection) or
    /// in response to [`ServiceToWorker::ClipboardRequest`]. Daemon writes
    /// to the matching connection's clipboard DataChannel.
    ClipboardRead(ClipboardPayload),

    /// Cursor shape / position update for the cursor-sync DataChannel.
    /// Routed by daemon to the per-connection `cursor_sync` DC.
    CursorData(CursorDataPayload),

    /// Opaque worker → daemon response for file-transfer commands.
    FileTransferData(OpaqueConnectionPayload),

    // ---------- Arch III legacy (deprecated, removed by PR 7) ----------
    /// **Deprecated (Arch IV)**: SignalingState now lives in the daemon
    /// (it owns the PeerConnection). The worker no longer holds any
    /// per-connection accept state, so it has nothing to report.
    /// PR 7 removes this variant together with
    /// [`WorkerInitPayload::preapproved_connections`].
    ConnectionAcceptStateChanged {
        connection_id: String,
        state: ConnectionAcceptState,
    },

    /// **Deprecated (Arch IV)**: connection lifecycle is owned by the
    /// daemon; cleanup propagates worker-ward via
    /// [`ServiceToWorker::StopMedia`] instead. Worker no longer reports
    /// `ConnectionClosed`.
    ConnectionClosed { connection_id: String },
}

/// Messages sent from Service Core to Tauri UI
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(tag = "type", content = "payload")]
pub enum ServiceToUI {
    /// Service status update
    StatusUpdate(ServiceStatus),

    /// Connection state changed
    ConnectionState(ConnectionStatePayload),

    /// Desktop switch event
    DesktopSwitchEvent(DesktopSwitchPayload),
}

/// Messages sent from Tauri UI to Service Core
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
#[serde(tag = "type", content = "payload")]
pub enum UIToService {
    /// Request service status
    GetStatus,

    /// Start/stop service
    SetServiceState { enabled: bool },

    /// Update configuration
    UpdateConfig(String), // JSON config string
}

// ==================== Payload Types ====================

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct WorkerInitPayload {
    /// Session ID for this worker instance
    pub session_id: String,
    /// OS session ID
    pub os_session_id: u32,
    /// Desktop name being served
    pub desktop_name: Option<String>,
    /// Configuration JSON (DeskSettings serialized)
    pub config_json: String,
    /// Signaling server URL to connect to (or proxy through service)
    pub signaling_url: Option<String>,
    /// Authentication token. In daemon-spawned workers this is the
    /// `tauri_ipc_token` used to authenticate the worker's host-control
    /// upstream ws connection back to the daemon.
    pub auth_token: Option<String>,
    /// URL of the daemon's `/ws/host_upstream` endpoint. When `Some`, the
    /// worker constructs a Forwarder-mode `HostControlHub` that bridges
    /// approval / private-screen / whiteboard traffic through the daemon to
    /// the connected Tauri shell. When `None`, the worker falls back to a
    /// Local hub (used by tests / standalone runs).
    #[serde(default)]
    pub host_upstream_url: Option<String>,

    /// Arch IV media pipe name. The worker connects to this pipe *in
    /// addition* to the event pipe (the `--pipe` CLI arg) so encoded
    /// video / audio frames travel over a dedicated transport
    /// independent of the event traffic. `None` on Arch III workers
    /// and on portable / standalone runs that do not need a separate
    /// media pipe — the worker treats this as "fall back to single-pipe
    /// mode" until the cut that wires media_producer (PR 2 / cut 4).
    #[serde(default)]
    pub media_pipe_name: Option<String>,

    /// **Deprecated (Arch IV)**: PC lifetime moves into the daemon, so
    /// the daemon owns `SignalingState` and never has to ship per-connection
    /// accept state across worker restarts. PR 7 will remove this field.
    /// New code should not read or set it.
    #[serde(default)]
    pub preapproved_connections: Vec<(String, ConnectionAcceptState)>,
}

/// Per-peer-connection acceptance state. The daemon caches this map keyed by
/// `connection_id` and ships it across worker restarts (see
/// `WorkerToService::ConnectionAcceptStateChanged` and
/// `WorkerInitPayload::preapproved_connections`).
///
/// Each `bool` corresponds 1:1 to a field on the worker's
/// `desk_signal_facade::SignalingState`. Both default to `false`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Encode, Decode, PartialEq, Eq)]
pub struct ConnectionAcceptState {
    /// Remote peer was granted mouse / keyboard input.
    pub accept_control: bool,
    /// Remote peer was granted bidirectional clipboard sync. Independent of
    /// `accept_control` — clipboard can be denied even when control was
    /// granted, so the daemon must never infer this from control alone.
    pub accept_clipboard_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct SignalingPayload {
    /// The raw signaling message (SDP, ICE, etc.) as JSON
    pub message: String,
    /// Connection ID this message is associated with
    pub connection_id: Option<String>,
}

// =============== Arch IV: media + per-connection control ===============

/// One encoded media frame travelling worker → daemon over the dedicated
/// `MediaTransport`. Sized for 4K H.264 IDR frames (up to ~2 MB) which
/// the POC validated end-to-end at P99 < 16 ms.
///
/// `ts_ns` is wall-clock nanoseconds (`SystemTime::now()`) stamped at the
/// instant the encoder finished producing the frame; the daemon uses
/// `now_ns - ts_ns` for one-way latency telemetry only — RTP timestamps
/// for `webrtc-rs::TrackLocalStaticSample` are derived from the per-frame
/// `duration` field separately.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct MediaFrame {
    pub connection_id: String,
    pub seq: u64,
    /// Wall-clock ns since `UNIX_EPOCH` at encode-out time.
    pub ts_ns: u64,
    /// Frame duration in nanoseconds (passed through to
    /// `TrackLocalStaticSample::write_sample`).
    pub duration_ns: u64,
    pub kind: MediaFrameKind,
    /// Codec id, mirrored from [`MediaCapabilities`]. Worker stamps it so
    /// the daemon does not have to track per-connection codec separately.
    pub codec: MediaCodec,
    pub payload: Vec<u8>,
}

/// Frame classification on the media transport. The daemon uses this
/// (a) to know whether to suppress write_sample during worker swaps
/// (resume only on `VideoI`), and (b) to record latency histograms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, Hash)]
pub enum MediaFrameKind {
    VideoI,
    VideoP,
    Audio,
}

/// Encoder identity. Stays an enum (not free-form string) so we don't end
/// up with case-sensitive mismatches between worker and daemon.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Encode, Decode, PartialEq, Eq, Hash)]
pub enum MediaCodec {
    H264,
    Vp8,
    Vp9,
    Av1,
    Opus,
}

/// Worker advertises which codecs / capture sources it can drive on the
/// current desktop. Daemon decides which codec the SDP m-line offers.
///
/// All `Vec` fields are ordered: index 0 is the worker's preferred choice.
/// Field additions should default to empty so newer workers stay decodable
/// by older daemons during a partial rollout — but in this Arch IV cutover
/// daemon and worker bump together, so version skew is not a steady-state
/// concern.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Encode, Decode)]
pub struct MediaCapabilities {
    pub video_codecs: Vec<MediaCodec>,
    pub audio_codecs: Vec<MediaCodec>,
    pub video_devices: Vec<String>,
    pub audio_devices: Vec<String>,
    /// Whether this worker can talk to a Tauri shell on the same desktop
    /// (for whiteboard / private-screen rendering).
    pub has_tauri: bool,
    /// Whether the worker process token is elevated. The daemon uses this
    /// to decide whether to enable UAC-shielding paths.
    pub is_admin: bool,
    /// `OpenInputDesktop` desktop name the worker is bound to. Empty on
    /// non-Windows.
    pub desktop_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct StartMediaPayload {
    pub connection_id: String,
    pub video_codec: MediaCodec,
    pub audio_codec: MediaCodec,
    pub video_device: Option<String>,
    pub audio_device: Option<String>,
    /// Frames per second the encoder should target.
    pub fps: u32,
    /// Encoder bitrate in kbps (0 = encoder default).
    pub bitrate_kbps: u32,
    /// Encoder quality knob (codec-specific 0–100; 0 = default).
    pub quality: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct StopMediaPayload {
    pub connection_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct UpdateMediaSettingsPayload {
    pub connection_id: String,
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub quality: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ForceKeyframePayload {
    pub connection_id: String,
}

/// Generic per-connection wrapper for opaque payloads (mouse / keyboard /
/// file-transfer / whiteboard etc.). The byte buffer is the raw DataChannel
/// payload as received from the browser; the worker's existing handlers
/// continue to deserialize JSON / bincode internally so we don't have to
/// duplicate every event schema in the IPC layer.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct InputPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ClipboardPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

/// Per-connection request that carries no payload of its own (e.g.
/// `ClipboardRequest`).
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ConnectionRefPayload {
    pub connection_id: String,
}

/// Same shape as [`InputPayload`] but used for command-style payloads
/// (file-transfer / whiteboard) where the worker dispatches into a
/// handler module by re-decoding `data`. Distinct type alias keeps the
/// callsite intent explicit.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct OpaqueConnectionPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct CursorDataPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct HeartbeatPayload {
    /// Current timestamp
    pub timestamp_ms: u64,
    /// Number of active WebRTC connections
    pub active_connections: u32,
    /// CPU usage percentage
    pub cpu_usage: Option<f32>,
    /// Memory usage in bytes
    pub memory_usage: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ErrorPayload {
    /// Error code
    pub code: i32,
    /// Error message
    pub message: String,
    /// Whether the worker can continue operating
    pub recoverable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ServiceStatus {
    /// Whether the service is running as a Windows service
    pub is_service_mode: bool,
    /// Whether a worker is currently active
    pub worker_active: bool,
    /// Current OS session ID
    pub current_session_id: Option<u32>,
    /// Current desktop name
    pub current_desktop: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ConnectionStatePayload {
    /// Connection ID
    pub connection_id: String,
    /// Connection state
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DesktopChangedPayload {
    /// New input desktop name as returned by `OpenInputDesktop` +
    /// `GetUserObjectInformationW(UOI_NAME)`. Examples: "Default", "Winlogon",
    /// "Screen-saver". The daemon launches the next worker with this name as
    /// the `lpDesktop` argument to `CreateProcessAsUserW`.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DesktopSwitchPayload {
    /// Previous desktop name
    pub from_desktop: Option<String>,
    /// New desktop name
    pub to_desktop: Option<String>,
    /// Phase of the switch
    pub phase: DesktopSwitchPhase,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// New `host_upstream_url` + repurposed `auth_token` fields round-trip cleanly.
    #[test]
    fn worker_init_payload_round_trip_with_host_upstream_fields() {
        let original = WorkerInitPayload {
            session_id: "session-1".to_string(),
            os_session_id: 7,
            desktop_name: Some("Default".to_string()),
            config_json: "{}".to_string(),
            signaling_url: None,
            auth_token: Some("ipc-token".to_string()),
            host_upstream_url: Some("ws://127.0.0.1:8082/ws/host_upstream".to_string()),
            preapproved_connections: Vec::new(),
            media_pipe_name: Some(r"\\.\pipe\lcxl-desk-ipc-7-uuid-media".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: WorkerInitPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.session_id, original.session_id);
        assert_eq!(decoded.os_session_id, original.os_session_id);
        assert_eq!(decoded.auth_token, original.auth_token);
        assert_eq!(decoded.host_upstream_url, original.host_upstream_url);
        assert_eq!(decoded.media_pipe_name, original.media_pipe_name);
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

    /// Older daemons that don't yet emit `host_upstream_url` /
    /// `media_pipe_name` must still be accepted by newer workers (both
    /// fields carry `#[serde(default)]`).
    #[test]
    fn worker_init_payload_accepts_missing_optional_fields() {
        let legacy = serde_json::json!({
            "session_id": "session-1",
            "os_session_id": 7,
            "desktop_name": null,
            "config_json": "{}",
            "signaling_url": null,
            "auth_token": null,
        });
        let decoded: WorkerInitPayload = serde_json::from_value(legacy).unwrap();
        assert!(decoded.host_upstream_url.is_none());
        assert!(decoded.auth_token.is_none());
        assert!(decoded.preapproved_connections.is_empty());
        assert!(decoded.media_pipe_name.is_none());
    }

    /// `preapproved_connections` round-trips and carries the per-connection
    /// `ConnectionAcceptState` faithfully.
    #[test]
    fn worker_init_payload_preapproved_round_trip() {
        let original = WorkerInitPayload {
            session_id: "session-1".to_string(),
            os_session_id: 1,
            desktop_name: None,
            config_json: "{}".to_string(),
            signaling_url: None,
            auth_token: None,
            host_upstream_url: None,
            preapproved_connections: vec![
                (
                    "conn-a".to_string(),
                    ConnectionAcceptState {
                        accept_control: true,
                        accept_clipboard_sync: false,
                    },
                ),
                (
                    "conn-b".to_string(),
                    ConnectionAcceptState {
                        accept_control: true,
                        accept_clipboard_sync: true,
                    },
                ),
            ],
            media_pipe_name: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: WorkerInitPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.preapproved_connections,
            original.preapproved_connections
        );
    }

    /// `ConnectionAcceptState` defaults to all-false (the safe initial state
    /// for any new `connection_id` the daemon has not yet seen approved).
    #[test]
    fn connection_accept_state_default_is_all_false() {
        let s = ConnectionAcceptState::default();
        assert!(!s.accept_control);
        assert!(!s.accept_clipboard_sync);
    }

    /// `WorkerToService::ConnectionAcceptStateChanged` round-trips with the
    /// shared tag/content shape (`type` / `payload`) used by the IPC reader.
    #[test]
    fn connection_accept_state_changed_round_trips() {
        let msg = WorkerToService::ConnectionAcceptStateChanged {
            connection_id: "conn-42".to_string(),
            state: ConnectionAcceptState {
                accept_control: true,
                accept_clipboard_sync: true,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
        match decoded {
            WorkerToService::ConnectionAcceptStateChanged {
                connection_id,
                state,
            } => {
                assert_eq!(connection_id, "conn-42");
                assert!(state.accept_control);
                assert!(state.accept_clipboard_sync);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `WorkerToService::ConnectionClosed` round-trips and carries only the
    /// `connection_id`.
    #[test]
    fn connection_closed_round_trips() {
        let msg = WorkerToService::ConnectionClosed {
            connection_id: "conn-x".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
        match decoded {
            WorkerToService::ConnectionClosed { connection_id } => {
                assert_eq!(connection_id, "conn-x");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ============== Arch IV variants — bincode v2 round-trips ==============

    fn bincode_round_trip<T>(value: &T) -> T
    where
        T: bincode::Encode + bincode::Decode<()>,
    {
        let bytes = bincode::encode_to_vec(value, bincode::config::standard()).unwrap();
        let (decoded, _) =
            bincode::decode_from_slice::<T, _>(&bytes, bincode::config::standard()).unwrap();
        decoded
    }

    #[test]
    fn start_media_round_trips_bincode() {
        let msg = ServiceToWorker::StartMedia(StartMediaPayload {
            connection_id: "conn-1".to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: Some("\\\\.\\DISPLAY1".to_string()),
            audio_device: None,
            fps: 60,
            bitrate_kbps: 6_000,
            quality: 0,
        });
        match bincode_round_trip(&msg) {
            ServiceToWorker::StartMedia(p) => {
                assert_eq!(p.connection_id, "conn-1");
                assert_eq!(p.video_codec, MediaCodec::H264);
                assert_eq!(p.audio_codec, MediaCodec::Opus);
                assert_eq!(p.fps, 60);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn stop_media_round_trips_bincode() {
        let msg = ServiceToWorker::StopMedia(StopMediaPayload {
            connection_id: "conn-2".to_string(),
        });
        match bincode_round_trip(&msg) {
            ServiceToWorker::StopMedia(p) => assert_eq!(p.connection_id, "conn-2"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn force_keyframe_round_trips_bincode() {
        let msg = ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
            connection_id: "conn-3".to_string(),
        });
        match bincode_round_trip(&msg) {
            ServiceToWorker::ForceKeyframe(p) => assert_eq!(p.connection_id, "conn-3"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// MouseInput / MouseMoveInput / KeyboardInput share `InputPayload` —
    /// verify the variant tag survives round-trip (bincode discriminant).
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
            bincode_round_trip(&mouse),
            ServiceToWorker::MouseInput(_)
        ));
        assert!(matches!(
            bincode_round_trip(&mouse_move),
            ServiceToWorker::MouseMoveInput(_)
        ));
        assert!(matches!(
            bincode_round_trip(&keyboard),
            ServiceToWorker::KeyboardInput(_)
        ));
    }

    #[test]
    fn capabilities_round_trips_bincode() {
        let msg = WorkerToService::Capabilities(MediaCapabilities {
            video_codecs: vec![MediaCodec::H264, MediaCodec::Vp9],
            audio_codecs: vec![MediaCodec::Opus],
            video_devices: vec!["display-1".to_string()],
            audio_devices: vec!["mic-1".to_string()],
            has_tauri: true,
            is_admin: false,
            desktop_name: "Default".to_string(),
        });
        match bincode_round_trip(&msg) {
            WorkerToService::Capabilities(c) => {
                assert_eq!(c.video_codecs, vec![MediaCodec::H264, MediaCodec::Vp9]);
                assert!(c.has_tauri);
                assert!(!c.is_admin);
                assert_eq!(c.desktop_name, "Default");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `MediaFrame` is the hot path on the media transport — sanity check
    /// 200 KB P-frame size encodes/decodes cleanly.
    #[test]
    fn media_frame_round_trips_bincode_200kb() {
        let payload = vec![0xABu8; 200 * 1024];
        let original = MediaFrame {
            connection_id: "conn-1".to_string(),
            seq: 42,
            ts_ns: 1_700_000_000_000_000_000,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoP,
            codec: MediaCodec::H264,
            payload: payload.clone(),
        };
        let decoded = bincode_round_trip(&original);
        assert_eq!(decoded.connection_id, "conn-1");
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.kind, MediaFrameKind::VideoP);
        assert_eq!(decoded.payload.len(), payload.len());
        assert_eq!(decoded.payload, payload);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub enum DesktopSwitchPhase {
    /// Switch is starting, worker may disconnect
    Starting,
    /// New worker is initializing
    WorkerInitializing,
    /// Switch complete, connections are being re-established
    Reconnecting,
    /// Switch complete, all connections restored
    Complete,
    /// Switch failed
    Failed(String),
}
