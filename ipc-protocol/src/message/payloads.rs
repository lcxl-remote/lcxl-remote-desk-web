//! Core, media, manager, terminal, and per-connection IPC payloads.

use std::collections::BTreeMap;

use desk_signal_facade::model::audio_capture::AudioDevice;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams, FileListResponse};
use desk_signal_facade::model::image_capture::DisplayInfo;
use desk_signal_facade::model::policy_snapshot::{PolicyGenerations, PolicySnapshot};
use desk_signal_facade::model::private_screen::PrivateScreenStateChangedData;
use desk_signal_facade::model::signal::SignalingType;
use desk_signal_facade::model::system_info::SystemInfo;
use desk_signal_facade::model::system_settings::RemoteSystemSettings;
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalList, TerminalOutputData, TerminalResizeData,
};
use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[cfg(doc)]
use super::{ServiceToWorker, WorkerToService};
// ==================== Payload Types ====================

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
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

    /// Media pipe name. The worker connects to this pipe *in
    /// addition* to the event pipe (the `--pipe` CLI arg) so encoded
    /// video / audio frames travel over a dedicated transport
    /// independent of the event traffic. `None` on portable / standalone
    /// runs that do not need a separate media pipe — the worker treats
    /// this as "fall back to single-pipe mode".
    #[serde(default)]
    pub media_pipe_name: Option<String>,

    /// File-transfer pipe name. The worker connects to this
    /// pipe in addition to the event + media pipes so file-transfer
    /// chunks (download responses, upload commands) travel over a
    /// dedicated bidirectional transport. Carries
    /// [`FileTransferPayload`] in both directions at
    /// `FILE_QUEUE_CAP = 32` per direction. `None` only in portable /
    /// in-process mode where the daemon constructs an in-process
    /// channel pair instead — in named-pipe `ServiceDaemon` mode the
    /// daemon **must** populate this field; the worker treats a
    /// missing `file_pipe_name` in that mode as a fatal init error
    /// and exits via `WorkerToService::Error`.
    #[serde(default)]
    pub file_pipe_name: Option<String>,

    /// Absolute path of the on-disk settings file the daemon is using.
    ///
    /// `Settings.args` carries `#[serde(skip)]`, so when the worker
    /// deserializes `config_json` it cannot recover `args.config_file_path`
    /// from the wire payload — and any worker-side `Settings::save()` call
    /// (e.g. when the user picks "remember" on a security approval prompt)
    /// would fall back to the default empty path and fail with
    /// `FILE_PATH_NOT_FOUND`.
    ///
    /// The daemon fills this with `args.config_file_path.clone()` so the
    /// worker writes back to the exact same on-disk file the daemon
    /// loaded. `Option<String>` with `#[serde(default)]` keeps backwards
    /// compatibility with older daemons whose Init payloads predate the
    /// field.
    #[serde(default)]
    pub config_file_path: Option<String>,

    /// Daemon-authoritative admission state. The worker starts fail-closed and
    /// only opens its own dispatch gate after receiving this payload.
    pub remote_access_locked: bool,
    pub remote_access_state_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct RemoteAccessStatePayload {
    pub operation_id: String,
    pub state_version: u64,
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct RemoteAccessStateAppliedPayload {
    pub operation_id: String,
    pub state_version: u64,
    pub cancelled_terminals: u32,
    pub cancelled_transfers: u32,
    pub cancelled_execs: u32,
}

/// Payload for [`WorkerToService::SignalingError`]. Carries the
/// originating request's correlation fields plus a numeric
/// `DeskErrorCode` and optional message. The daemon rebuilds the
/// outbound `SignalingModel::error(...)` wire shape from these fields
/// — `signaling_type` is what the browser keys off when matching the
/// reply to its pending request.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SignalingErrorPayload {
    pub request_id: String,
    pub connection_id: String,
    pub signaling_type: SignalingType,
    pub error_code: i32,
    pub error_message: Option<String>,
}

// =============== Media + per-connection control ===============

/// One encoded media frame travelling worker → daemon over the dedicated
/// `MediaTransport`. Sized for 4K H.264 IDR frames (up to ~2 MB), validated
/// end-to-end at P99 < 16 ms.
///
/// `ts_ns` is wall-clock nanoseconds (`SystemTime::now()`) stamped at the
/// instant the encoder finished producing the frame; the daemon uses
/// `now_ns - ts_ns` for one-way latency telemetry only — RTP timestamps
/// for `webrtc-rs::TrackLocalStaticSample` are derived from the per-frame
/// `duration` field separately.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
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
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq, Hash,
)]
pub enum MediaFrameKind {
    VideoI,
    VideoP,
    Audio,
}

/// Encoder identity. Stays an enum (not free-form string) so we don't end
/// up with case-sensitive mismatches between worker and daemon.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq, Hash,
)]
pub enum MediaCodec {
    H264,
    Vp8,
    Vp9,
    Av1,
    Opus,
}

/// Worker advertises which codecs / capture sources it can drive on the
/// current desktop. Daemon decides which codec the SDP m-line offers and
/// echoes the device lists into the `InitSignalingData` reply so the
/// browser can render its capture-source picker.
///
/// `*_codecs` are ordered: index 0 is the worker's preferred choice.
/// `*_device_list` mirrors the structured maps that
/// `desk_capture_engine::list_image_capture` /
/// `list_audio_capture` produce so the daemon can pass them through to
/// `InitSignalingData::{video,audio}_device_list` without losing any
/// per-driver grouping or device metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct MediaCapabilities {
    pub video_codecs: Vec<MediaCodec>,
    pub audio_codecs: Vec<MediaCodec>,
    /// Concrete video-encoder identifiers reported verbatim from the
    /// capture-engine factory (e.g. `["X264", "VP8", "VP9", "H264",
    /// "AV1"]`). Distinct from `video_codecs` because the latter
    /// collapses every H.264 implementation onto a single
    /// `MediaCodec::H264` for SDP m-line negotiation, while this list
    /// preserves the per-implementation distinction the UI needs (so
    /// the user can pick libx264 vs OpenH264). The daemon copies this
    /// straight into `InitSignalingData::video_encoder_list`.
    #[serde(default)]
    pub video_encoders: Vec<String>,
    /// Audio counterpart of `video_encoders`. Today only `"OPUS"` is
    /// reported, but kept symmetrical so a future encoder addition
    /// doesn't need a wire-format bump.
    #[serde(default)]
    pub audio_encoders: Vec<String>,
    /// Per-backend display map (e.g. `"dxgi" -> [DISPLAY1, DISPLAY2]`).
    pub video_device_list: BTreeMap<String, Vec<DisplayInfo>>,
    /// Per-backend audio device map.
    pub audio_device_list: BTreeMap<String, Vec<AudioDevice>>,
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

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct StartMediaPayload {
    pub connection_id: String,
    pub video_codec: MediaCodec,
    pub audio_codec: MediaCodec,
    pub video_device: Option<String>,
    pub audio_device: Option<String>,
    /// Frames per second the encoder should target.
    pub fps: u32,
    /// Encoder bitrate in kbps (0 = encoder default). Historic field:
    /// it is *not* consumed as an initial bitrate cap — a fresh
    /// connection always starts at the encoder's initial ceiling and
    /// the first cap can only come from the daemon's REMB controller
    /// via `UpdateMediaSettingsPayload.bitrate_kbps`.
    pub bitrate_kbps: u32,
    /// Encoder quality knob (codec-specific 0–100; 0 = default).
    pub quality: u32,
    /// Whether the connection's SDP offer included an `m=video` section.
    /// `false` means the worker should *not* spawn the video pipeline
    /// (DXGI capture + encoder) for this connection — typical for the
    /// browser file-management page, which opens a PC purely for the
    /// `file_transfer_event` DataChannel and never wants screen capture.
    /// Defaults to `true` for back-compat with older daemons that did not
    /// thread through SDP track presence.
    #[serde(default = "default_true")]
    pub start_video: bool,
    /// Whether the connection's SDP offer included an `m=audio` section.
    /// `false` means the worker should *not* spawn the audio pipeline
    /// (WASAPI capture + Opus encoder) for this connection. Defaults to
    /// `true` for back-compat (see `start_video`).
    #[serde(default = "default_true")]
    pub start_audio: bool,
    /// Per-connection image-capture backend choice (e.g. "DXGI", "GDI"
    /// on Windows). `None` lets the worker fall back to its
    /// startup-time `DeskSettings.image_capture` (which itself
    /// defaults to the platform's preferred backend). Threading the
    /// per-connection choice through the IPC payload is required
    /// because the worker's base settings are a snapshot taken at
    /// worker spawn — without this field, a browser opening a second
    /// connection cannot pick a different backend than the first.
    #[serde(default)]
    pub image_capture: Option<String>,
    /// Per-connection override for the BGRA→YUV dirty-rect fast path
    /// in `PersistentYuvBuffer`. `None` means "use the worker's base
    /// `DeskSettings.enable_dirty_rect`" (back-compat with older
    /// daemons). `Some(false)` forces every frame through a full
    /// conversion — needed for the browser Advanced-tab kill-switch
    /// to take effect on the *first* StartMedia, before any live
    /// `UpdateMediaSettings` would be issued. Without this field a
    /// fresh connection always picks up the worker's default
    /// (`true`) regardless of the browser's offer-time setting.
    #[serde(default)]
    pub enable_dirty_rect: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct StopMediaPayload {
    pub connection_id: String,
}

/// Registers a connection's validated capability ceiling with the worker (see
/// [`ServiceToWorker::SetConnectionCeiling`]). `ceiling = None` means the
/// connection is an owner/unrestricted session with no cap.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SetConnectionCeilingPayload {
    pub connection_id: String,
    pub ceiling: Option<desk_signal_facade::model::security_settings::SecuritySettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct UpdateMediaSettingsPayload {
    pub connection_id: String,
    pub fps: Option<u32>,
    /// Runtime bitrate-cap directive with tri-state semantics —
    /// **`Some(0)` is meaningful, do not filter it out as an invalid
    /// value**:
    /// - `None` — leave the current cap alone (field not part of this
    ///   update).
    /// - `Some(0)` — clear the cap; the encoder returns to its initial
    ///   ceiling. Sent by the daemon when adaptive bitrate is switched
    ///   off so a previously tightened connection recovers.
    /// - `Some(k)` (k > 0) — cap the encoder at `k` kbps without
    ///   rebuilding it. Sent by the daemon's REMB controller at ~1 Hz
    ///   while adaptive bitrate is on.
    pub bitrate_kbps: Option<u32>,
    pub quality: Option<u32>,
    /// Toggle for the BGRA→YUV dirty-rect fast path in
    /// `PersistentYuvBuffer`. `None` means "leave the current value
    /// alone" (older daemons that never sniff the field). `Some(false)`
    /// forces every frame through a full conversion; `Some(true)`
    /// re-enables partial updates. Threaded through so the browser's
    /// Advanced-tab kill-switch can be retuned mid-stream without
    /// tearing down the encoder.
    #[serde(default)]
    pub enable_dirty_rect: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ForceKeyframePayload {
    pub connection_id: String,
}

/// Generic per-connection wrapper for opaque payloads (mouse / keyboard /
/// file-transfer / whiteboard etc.). The byte buffer is the raw DataChannel
/// payload as received from the browser; the worker's existing handlers
/// continue to deserialize JSON / bincode internally so we don't have to
/// duplicate every event schema in the IPC layer.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct InputPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ClipboardPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

/// Per-connection request that carries no payload of its own (e.g.
/// `ClipboardRequest`).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ConnectionRefPayload {
    pub connection_id: String,
}

/// Same shape as [`InputPayload`] but used for command-style payloads
/// (file-transfer / whiteboard) where the worker dispatches into a
/// handler module by re-decoding `data`. Distinct type alias keeps the
/// callsite intent explicit.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
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
///
/// `transfer_id` is populated by the worker so the daemon-side writer
/// task can reference a specific transfer when reporting a `dc.send`
/// failure back via [`ServiceToWorker::FileTransferSendFailed`]. The
/// daemon itself never parses `data` (the binary chunk header /
/// `FileTransferMessage` JSON shape lives in the worker), so without
/// this field the daemon would only know the failing `connection_id`
/// and the worker would have to abort *every* in-flight transfer on
/// that PC. The field is optional so legacy binaries / synthetic test
/// payloads still decode cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileTransferPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
    pub is_text: bool,
    #[serde(default)]
    pub transfer_id: Option<String>,
}

/// Classification of a `dc.send` failure observed by the daemon's
/// per-connection file-transfer writer task. The worker uses this to
/// pick its abort policy and to log the failure at an appropriate
/// severity:
///
/// - `PacketTooLarge` is a programmer / configuration bug — the chosen
///   chunk size exceeds the remote `a=max-message-size` SCTP advertise.
///   The whole transfer is doomed (every subsequent chunk will trip the
///   same check) so the worker must abort the transfer and surface a
///   `TransferError` to the browser. Logged at `error!` so it gets
///   investigated rather than absorbed into the warning noise.
/// - `TransportClosed` is normal teardown (peer disconnected /
///   `RTCPeerConnection` closed mid-transfer). Logged at `debug!` —
///   the failure is expected during shutdown and the transfer cleanup
///   is already on its way via `cleanup_pc`.
/// - `Other` is any unclassified `webrtc::Error`. Logged at `warn!`
///   so it surfaces in production but doesn't pollute the error budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub enum FileTransferSendErrorKind {
    /// The serialized message exceeded SCTP `max_message_size` for the
    /// outbound stream. Caused by chunk_size + binary-header > remote
    /// SDP advertise. Always fatal for the transfer.
    PacketTooLarge,
    /// The DataChannel / SCTP transport was closed before the send
    /// completed. Normal during teardown / peer disconnect.
    TransportClosed,
    /// Any unclassified error returned by `webrtc-rs`. Treated as
    /// transport-level failure for abort purposes but logged at a
    /// lower severity than `PacketTooLarge`.
    Other,
}

/// Daemon → worker payload carried by
/// [`ServiceToWorker::FileTransferSendFailed`]. The worker uses the
/// `transfer_id` (when present) to scope the abort to a single
/// in-flight transfer; if it's `None`, the worker falls back to
/// aborting every transfer on `connection_id` (legacy / unscoped
/// chunks emitted before the worker started populating `transfer_id`
/// on the file lane).
///
/// `chunk_index` is informational — the worker doesn't need it to abort,
/// but it goes straight into the `TransferError.message` so logs and
/// the browser-side toast can show exactly which chunk tripped the
/// failure.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileTransferSendFailedPayload {
    pub connection_id: String,
    #[serde(default)]
    pub transfer_id: Option<String>,
    #[serde(default)]
    pub chunk_index: Option<u32>,
    pub kind: FileTransferSendErrorKind,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct CursorDataPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
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

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
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

// ---------- Typed control plane ----------

/// Payload for [`ServiceToWorker::EnablePrivateScreen`]. Mirrors the
/// JSON shape of `desk_signal_facade::model::private_screen::
/// EnablePrivateScreenData` plus the `connection_id` the daemon
/// already had at the WS-router boundary.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct EnablePrivateScreenPayload {
    pub connection_id: String,
    pub enable: bool,
}

/// Payload for [`ServiceToWorker::UpdateDeskSettings`]. Carries the
/// full `DeskSettings` struct so the worker applies every field; the
/// daemon separately sniffs the media-relevant knobs and emits
/// [`ServiceToWorker::UpdateMediaSettings`] for the encoder pipeline
/// (see `pc_manager::broadcast_media_settings_update`).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct UpdateDeskSettingsPayload {
    pub connection_id: String,
    pub settings: DeskSettings,
}

/// Payload for [`WorkerToService::PrivateScreenStateChanged`].
/// Mirrors `desk_signal_facade::model::private_screen::
/// PrivateScreenStateChangedData` plus the `connection_id` the
/// daemon needs to pick the right outbound signaling websocket.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct PrivateScreenStateChangedPayload {
    pub connection_id: String,
    pub data: PrivateScreenStateChangedData,
}

/// Worker-confirmed file-manager activity for one controller connection.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileManagerOpenedPayload {
    pub connection_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileTransferStartedPayload {
    pub connection_id: String,
    pub transfer_id: String,
    pub direction: FileTransferDirection,
    pub file_name: String,
    /// Expected transfer size. Downloads use host filesystem metadata; uploads
    /// use the controller-declared size and validate it before completion.
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileTransferFinishedPayload {
    pub connection_id: String,
    pub transfer_id: String,
    pub outcome: FileTransferOutcome,
}

// ---------- Manager plane (typed) ----------

/// Shared envelope for body-less manager *requests*
/// (`ManagerSystemInfoRequest`, `ManagerQuerySettingsRequest`).
/// Carries the `request_id` so the worker can echo it back on the
/// matching response payload, and the `connection_id` so the daemon
/// can pick the right outbound signaling websocket when it ferries
/// the response. `connection_id` is `Option` because manager-plane
/// requests originating from a HTTP REST controller (e.g.
/// `signal-facade::controller::sysinfo` →
/// `connection.request_peer_with_callback`) carry no `from_connection_id`
/// — the daemon correlates the response by `request_id` alone in that
/// path.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerRequestRefPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
}

/// Shared envelope for body-less manager *responses*
/// (`ManagerFileDeleteResponse`, `ManagerUpdateSettingsResponse`).
/// Same shape as [`ManagerRequestRefPayload`] but kept distinct so
/// the daemon's response-direction code is symmetric with the
/// request-direction code at the type-system level. `connection_id`
/// is `Option` for the same reason — see that type's docstring.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerResponseRefPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
}

/// Payload for [`ServiceToWorker::ManagerFileListRequest`]. Carries the
/// trusted controller connection and the browser-issued list parameters.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerFileListRequestPayload {
    pub request_id: String,
    pub connection_id: String,
    pub params: FileListParams,
}

/// Payload for [`ServiceToWorker::ManagerFileDeleteRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerFileDeleteRequestPayload {
    pub request_id: String,
    pub connection_id: String,
    pub request: DeleteFileRequest,
}

/// Payload for [`ServiceToWorker::ManagerUpdateSettingsRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerUpdateSettingsRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub settings: RemoteSystemSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SetLocalePayload {
    pub locale: String,
}

/// Payload for [`ServiceToWorker::UpdateSecurityPolicy`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct UpdateSecurityPolicyPayload {
    /// Identifies this publication so the daemon can match the worker's
    /// acknowledgement to it rather than to some later one.
    pub operation_id: String,
    pub snapshot: PolicySnapshot,
}

/// What a worker did with a published policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "type", content = "payload")]
pub enum PolicyApplyOutcome {
    /// The worker now holds exactly this policy. The generations travel back in
    /// full rather than as a digest so a divergence names the capabilities it
    /// is about.
    Applied {
        seq: u64,
        generations: PolicyGenerations,
    },
    /// The worker could not reconcile what arrived with what it held and fell
    /// back to the stricter reading of the two. This is a degraded state, not a
    /// successful application: the daemon must republish rather than record the
    /// worker as caught up.
    NeedsResync { seq: u64 },
}

/// Payload for [`WorkerToService::SecurityPolicyApplied`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SecurityPolicyAppliedPayload {
    pub operation_id: String,
    pub outcome: PolicyApplyOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct LocaleAppliedPayload {
    pub locale: String,
}

/// Payload for [`WorkerToService::ManagerSystemInfoResponse`].
/// `SystemInfo` is the wire shape the worker computed from
/// `sysinfo::System` and the legacy handler used to send via
/// `send_response`. `connection_id` is `Option` because the matching
/// request can be HTTP-API-triggered with no `from_connection_id` —
/// see [`ManagerRequestRefPayload`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerSystemInfoResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub info: SystemInfo,
}

/// Payload for [`WorkerToService::ManagerFileListResponse`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerFileListResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub response: FileListResponse,
}

/// Payload for [`WorkerToService::ManagerQuerySettingsResponse`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerQuerySettingsResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub settings: RemoteSystemSettings,
}

// ---------- Terminal plane (typed) ----------

/// Payload for [`ServiceToWorker::StartTerminalRequest`]. Carries the
/// browser-supplied [`StartTerminalSession`] (the comma-separated
/// command + args string the worker splits in
/// `handle_manager_terminal_start`). `request_id` is echoed back on
/// the [`WorkerToService::TerminalStarted`] reply.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct StartTerminalRequestPayload {
    pub request_id: String,
    pub connection_id: String,
    pub session: StartTerminalSession,
}

/// Payload for [`ServiceToWorker::SendDataToTerminalRequest`]. One-way —
/// no `request_id` because the worker does not reply.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SendDataToTerminalPayload {
    pub connection_id: String,
    pub data: TerminalInputData,
}

/// Payload for [`ServiceToWorker::ResizeTerminalRequest`]. One-way.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ResizeTerminalPayload {
    pub connection_id: String,
    pub data: TerminalResizeData,
}

/// Payload for [`ServiceToWorker::CloseTerminalRequest`]. Body-less
/// (the only thing the worker needs is the connection id). Distinct
/// from [`ConnectionRefPayload`] / [`TerminalClosedPayload`] so the
/// terminal-plane direction is symmetric at the type level.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct CloseTerminalPayload {
    pub connection_id: String,
}

/// Payload for [`ServiceToWorker::ListTerminalRequest`]. Body-less;
/// `request_id` is echoed back on the
/// [`WorkerToService::ListTerminalResponse`]. `connection_id` is
/// `Option` because `signal-facade::controller::terminal::list_terminal`
/// dispatches via `connection.request_peer_with_callback` with no
/// originating browser PC — the response is correlated by
/// `request_id`.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ListTerminalRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
}

/// Payload for [`WorkerToService::TerminalStarted`]. Empty body —
/// `request_id` correlates with the originating
/// [`ServiceToWorker::StartTerminalRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct TerminalStartedPayload {
    pub request_id: String,
    pub connection_id: String,
}

/// Payload for [`WorkerToService::TerminalClosed`]. No `request_id` —
/// this is a notification fired by the worker's monitor task when
/// the PTY child process exits, not a response to a specific request.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct TerminalClosedPayload {
    pub connection_id: String,
}

/// Payload for [`WorkerToService::ReplyFromTerminal`]. Each chunk is
/// at most ~1 KB (the worker's PTY reader buffer size), so the event
/// pipe handles the rate fine without competing with media frames.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ReplyFromTerminalPayload {
    pub connection_id: String,
    pub data: TerminalOutputData,
}

/// Payload for [`WorkerToService::ListTerminalResponse`]. Carries the
/// fully resolved [`TerminalList`] the worker built from
/// `which::which`/`which_re` lookups. `connection_id` is `Option` for
/// the same reason as [`ListTerminalRequestPayload`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ListTerminalResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub terminals: TerminalList,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct DesktopChangedPayload {
    /// New input desktop name as returned by `OpenInputDesktop` +
    /// `GetUserObjectInformationW(UOI_NAME)`. Examples: "Default", "Winlogon",
    /// "Screen-saver". The daemon launches the next worker with this name as
    /// the `lpDesktop` argument to `CreateProcessAsUserW`.
    pub name: String,
}
