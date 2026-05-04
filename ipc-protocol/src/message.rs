use bincode::{Decode, Encode};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::private_screen::PrivateScreenStateChangedData;
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

    /// Force the worker to shut down immediately.
    Shutdown,

    /// Forward a raw `SignalingType` JSON envelope to the worker.
    ///
    /// **Transitional bridge.** Arch IV moved WebRTC SDP/ICE handling and
    /// the per-connection accept-state into the daemon, but the
    /// `signaling_router` still classifies many user-session signaling
    /// types (terminal management, manager file/system queries,
    /// `EnablePrivateScreen`, `UpdateDeskSettings`, etc.) as
    /// `RouteOutcome::ForwardToWorker` because their handlers live in
    /// the user-session worker process. Until each of those types has a
    /// typed event-transport variant, the daemon ships them through
    /// this opaque envelope.
    SignalingMessage(SignalingPayload),

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
    /// dispatches into its existing `file_transfer` module. Carries
    /// `is_text` so the worker can distinguish JSON control frames
    /// (download/upload requests, completion ack) from binary chunk
    /// uploads, matching the Arch III `DataChannelMessage::is_string`
    /// dispatch in `service::file_transfer::handle_file_transfer_event`.
    FileTransferCommand(FileTransferPayload),

    /// Opaque whiteboard / private-screen / Tauri-shell command from the
    /// browser DataChannel. Worker dispatches via the existing
    /// host_control forwarder.
    WhiteboardCommand(OpaqueConnectionPayload),

    // ---------- Arch IV typed-IPC migration batch 1 ----------
    /// Browser-issued private-screen toggle. Worker enables / disables
    /// the per-connection private screen via its `host_control_helper`.
    /// Replaces the legacy `EnablePrivateScreen` flow over the
    /// `SignalingMessage` bridge.
    EnablePrivateScreen(EnablePrivateScreenPayload),

    /// Browser-issued desk-settings update. Carries the full
    /// `DeskSettings` so the worker can apply non-media fields
    /// (`wayland_control_mode`, `private_screen`, ...). The daemon
    /// also sniffs the media-relevant knobs and fans them out as
    /// [`Self::UpdateMediaSettings`] separately so the per-connection
    /// encoder pipeline retunes live (see `pc_manager::
    /// broadcast_media_settings_update`). Replaces the legacy
    /// `UpdateDeskSettings` flow over the `SignalingMessage` bridge.
    UpdateDeskSettings(UpdateDeskSettingsPayload),
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

    /// Forward a worker-emitted signaling JSON envelope back to the
    /// browser. Counterpart to [`ServiceToWorker::SignalingMessage`] —
    /// the daemon writes the message verbatim onto the corresponding
    /// signaling WebSocket. Used for terminal output, manager file /
    /// system info responses, and any other reply produced by the
    /// worker's `DeskSession::handle_message` paths that have not yet
    /// migrated to a typed event-transport variant.
    SignalingMessage(SignalingPayload),

    /// Worker reports its health status
    Heartbeat(HeartbeatPayload),

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

    /// Worker → daemon response for file-transfer commands. Carries
    /// `is_text` so the daemon writes the bytes as a text frame (JSON
    /// control message — DownloadResponse / TransferComplete /
    /// TransferError) or a binary frame (downloaded chunk) on the
    /// browser's `file_transfer_event` DataChannel.
    FileTransferData(FileTransferPayload),

    // ---------- Arch IV typed-IPC migration batch 1 ----------
    /// Worker → daemon notification that the per-connection private
    /// screen visibility / support state changed. Sourced from the
    /// worker's `HostControlHub::subscribe_state` broadcast bus.
    /// The daemon forwards this to the browser as a
    /// `SignalingType::PrivateScreenStateChanged` reply on the matching
    /// signaling websocket. Replaces the legacy reverse path through
    /// the `SignalingMessage` bridge.
    PrivateScreenStateChanged(PrivateScreenStateChangedPayload),
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
}

/// Opaque signaling envelope used by the
/// [`ServiceToWorker::SignalingMessage`] /
/// [`WorkerToService::SignalingMessage`] transitional bridge. Carries
/// the raw `SignalingType` JSON so the receiving end can re-parse
/// using its existing dispatcher.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct SignalingPayload {
    /// Raw `SignalingType` JSON (the wire shape sent over the
    /// browser ↔ signaling-server WebSocket).
    pub message: String,
    /// `from_connection_id` extracted by the daemon at parse time so
    /// the worker can dispatch without a second JSON parse.
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

/// File-transfer wire shape. Same as [`OpaqueConnectionPayload`] plus
/// an `is_text` discriminator: the WebRTC DataChannel distinguishes
/// text vs binary frames at the wire level (`DataChannelMessage::
/// is_string`), and the file-transfer protocol uses both — JSON
/// control messages travel as text, file chunks travel as binary —
/// so the IPC has to preserve that bit for the daemon's `dc.send_text`
/// vs `dc.send` decision.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct FileTransferPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
    pub is_text: bool,
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

/// Sentinel `ErrorPayload::code` value the worker emits when its media
/// transport blocked an I-frame send for longer than the configured
/// `MediaTransport` timeout. The daemon uses the matching `connection_id`
/// to issue `StopMedia` + `StartMedia` for that connection so the encoder
/// pipeline is reset rather than left wedged behind a saturated pipe.
///
/// Picked deliberately outside the `DeskErrorCode` u16 range so daemon-
/// side dispatch can match on it without colliding with broader IPC
/// error codes.
pub const ERROR_CODE_MEDIA_TRANSPORT_STUCK: i32 = -1001;

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct ErrorPayload {
    /// Error code
    pub code: i32,
    /// Error message
    pub message: String,
    /// Whether the worker can continue operating
    pub recoverable: bool,
    /// Optional per-connection scope. When `Some`, the daemon can
    /// take per-connection recovery action (e.g. `StopMedia` +
    /// `StartMedia` on [`ERROR_CODE_MEDIA_TRANSPORT_STUCK`]). `None`
    /// for worker-wide errors that don't map to a single PC.
    #[serde(default)]
    pub connection_id: Option<String>,
}

// ---------- Arch IV typed-IPC migration batch 1 ----------

/// Payload for [`ServiceToWorker::EnablePrivateScreen`]. Mirrors the
/// JSON shape of `desk_signal_facade::model::private_screen::
/// EnablePrivateScreenData` plus the `connection_id` the daemon
/// already had at the WS-router boundary.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct EnablePrivateScreenPayload {
    pub connection_id: String,
    pub enable: bool,
}

/// Payload for [`ServiceToWorker::UpdateDeskSettings`]. Carries the
/// full `DeskSettings` struct so the worker applies every field; the
/// daemon separately sniffs the media-relevant knobs and emits
/// [`ServiceToWorker::UpdateMediaSettings`] for the encoder pipeline
/// (see `pc_manager::broadcast_media_settings_update`).
///
/// `DeskSettings` itself does not derive [`Encode`]/[`Decode`] (it
/// lives in `desk-signal-facade`, a leaf model crate that should not
/// know about bincode); we ride bincode 2's `with_serde` field
/// attribute to delegate to its serde impl on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct UpdateDeskSettingsPayload {
    pub connection_id: String,
    #[bincode(with_serde)]
    pub settings: DeskSettings,
}

/// Payload for [`WorkerToService::PrivateScreenStateChanged`].
/// Mirrors `desk_signal_facade::model::private_screen::
/// PrivateScreenStateChangedData` plus the `connection_id` the
/// daemon needs to pick the right outbound signaling websocket.
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct PrivateScreenStateChangedPayload {
    pub connection_id: String,
    #[bincode(with_serde)]
    pub data: PrivateScreenStateChangedData,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode)]
pub struct DesktopChangedPayload {
    /// New input desktop name as returned by `OpenInputDesktop` +
    /// `GetUserObjectInformationW(UOI_NAME)`. Examples: "Default", "Winlogon",
    /// "Screen-saver". The daemon launches the next worker with this name as
    /// the `lpDesktop` argument to `CreateProcessAsUserW`.
    pub name: String,
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
        assert!(decoded.media_pipe_name.is_none());
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

    /// PR 4 cut 2: `FileTransferPayload` carries an `is_text` flag
    /// alongside `connection_id` + `data`. Verify both true and false
    /// survive round-trip — a flipped bit would break the daemon's
    /// `dc.send_text` vs `dc.send` decision and corrupt downloads.
    #[test]
    fn file_transfer_payload_round_trip_preserves_is_text_flag() {
        for is_text in [true, false] {
            let cmd = ServiceToWorker::FileTransferCommand(FileTransferPayload {
                connection_id: "ft-1".to_string(),
                data: vec![1, 2, 3],
                is_text,
            });
            match bincode_round_trip(&cmd) {
                ServiceToWorker::FileTransferCommand(p) => {
                    assert_eq!(p.connection_id, "ft-1");
                    assert_eq!(p.data, vec![1, 2, 3]);
                    assert_eq!(p.is_text, is_text);
                }
                other => panic!("unexpected: {other:?}"),
            }
            let resp = WorkerToService::FileTransferData(FileTransferPayload {
                connection_id: "ft-1".to_string(),
                data: vec![9, 8, 7],
                is_text,
            });
            match bincode_round_trip(&resp) {
                WorkerToService::FileTransferData(p) => {
                    assert_eq!(p.connection_id, "ft-1");
                    assert_eq!(p.data, vec![9, 8, 7]);
                    assert_eq!(p.is_text, is_text);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// `ErrorPayload.connection_id` survives a bincode round-trip in
    /// both `Some` and `None` forms. The daemon's `MediaTransportStuck`
    /// recovery path keys off this field — losing it would silently
    /// regress the self-heal we just wired up.
    #[test]
    fn error_payload_connection_id_round_trips_bincode() {
        let scoped = WorkerToService::Error(ErrorPayload {
            code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
            message: "stuck".to_string(),
            recoverable: true,
            connection_id: Some("conn-7".to_string()),
        });
        match bincode_round_trip(&scoped) {
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
        match bincode_round_trip(&global) {
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

    // === Arch IV typed-IPC migration batch 1 — round-trip tests ===

    /// `EnablePrivateScreen` carries the same bool the legacy
    /// `EnablePrivateScreenData` JSON used. Round-tripping it under
    /// bincode pins the wire shape — a reorder of `connection_id`
    /// vs `enable` would silently flip enable-vs-disable on
    /// matched-version daemon/worker pairs.
    #[test]
    fn enable_private_screen_round_trips_bincode() {
        for enable in [true, false] {
            let msg = ServiceToWorker::EnablePrivateScreen(EnablePrivateScreenPayload {
                connection_id: "conn-priv".to_string(),
                enable,
            });
            match bincode_round_trip(&msg) {
                ServiceToWorker::EnablePrivateScreen(p) => {
                    assert_eq!(p.connection_id, "conn-priv");
                    assert_eq!(p.enable, enable);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// `UpdateDeskSettings` rides bincode 2's `with_serde` field
    /// attribute to delegate to `DeskSettings`'s serde impl. Verify
    /// non-default media + non-media fields both survive — these are
    /// the ones the worker's `handle_update_desk_settings` and the
    /// daemon's `broadcast_media_settings_update` both read.
    #[test]
    fn update_desk_settings_round_trips_bincode() {
        let mut settings = DeskSettings::default();
        settings.video_fps = 45;
        settings.video_quality = 33;
        settings.wayland_control_mode = Some("portal".to_string());
        let msg = ServiceToWorker::UpdateDeskSettings(UpdateDeskSettingsPayload {
            connection_id: "conn-uds".to_string(),
            settings: settings.clone(),
        });
        match bincode_round_trip(&msg) {
            ServiceToWorker::UpdateDeskSettings(p) => {
                assert_eq!(p.connection_id, "conn-uds");
                assert_eq!(p.settings.video_fps, 45);
                assert_eq!(p.settings.video_quality, 33);
                assert_eq!(p.settings.wayland_control_mode.as_deref(), Some("portal"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `PrivateScreenStateChanged` is the reverse path (worker →
    /// daemon → browser). Round-trip with `is_supported = false` +
    /// an `error_msg` so a future schema change to
    /// `PrivateScreenStateChangedData` shows up as a test failure
    /// rather than as a silent wire-format drift.
    #[test]
    fn private_screen_state_changed_round_trips_bincode() {
        let msg =
            WorkerToService::PrivateScreenStateChanged(PrivateScreenStateChangedPayload {
                connection_id: "conn-pss".to_string(),
                data: PrivateScreenStateChangedData {
                    visible: false,
                    is_supported: false,
                    error_msg: Some("hub denied".to_string()),
                },
            });
        match bincode_round_trip(&msg) {
            WorkerToService::PrivateScreenStateChanged(p) => {
                assert_eq!(p.connection_id, "conn-pss");
                assert!(!p.data.visible);
                assert!(!p.data.is_supported);
                assert_eq!(p.data.error_msg.as_deref(), Some("hub denied"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
