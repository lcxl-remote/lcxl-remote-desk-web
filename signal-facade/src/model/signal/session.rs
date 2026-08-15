//! TURN, WebRTC session, initialization, and authorization models.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use webrtc::{
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::model::{
    audio_capture::AudioDevice,
    desk_settings::DeskSettings,
    image_capture::DisplayInfo,
    media_capability::VideoEncoderCapability,
    os::OperationSystemEnum,
    remote_session::{
        RemoteSessionSettings, SessionSettingsCapabilities, SuggestedSessionSettings,
        deserialize_required_nullable,
    },
    security_settings::SecuritySettings,
    virtual_display::{DEFAULT_ADAPTIVE_DEBOUNCE_MS, DEFAULT_ADAPTIVE_MIN_DELTA_PX},
};
/// Turn transport type
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Hash, ToSchema)]
pub enum TurnTransport {
    /// Stun transport
    Stun,
    /// Turn transport
    Turn,
}

/// RTC IceServer
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize, Hash, ToSchema)]
pub struct LcxlRTCIceServer {
    /// List of URLs associated with the ICE server, e.g. ["stun:stun.l.google.com:19302"]
    pub urls: Vec<String>,
    /// Username for the ICE server, if any.
    pub username: String,
    /// Credential for the ICE server, if any.
    pub credential: String,
}

impl LcxlRTCIceServer {
    /// Get transport type from url
    pub fn transport(&self) -> Option<TurnTransport> {
        if self.urls.is_empty() {
            return None;
        }
        let url = self.urls[0].clone();
        if url.starts_with("stun:") {
            Some(TurnTransport::Stun)
        } else if url.starts_with("turn:") {
            Some(TurnTransport::Turn)
        } else {
            None
        }
    }
}

impl From<RTCIceServer> for LcxlRTCIceServer {
    fn from(value: RTCIceServer) -> Self {
        LcxlRTCIceServer {
            urls: value.urls,
            username: value.username,
            credential: value.credential,
        }
    }
}

impl From<&LcxlRTCIceServer> for RTCIceServer {
    fn from(val: &LcxlRTCIceServer) -> Self {
        RTCIceServer {
            urls: val.urls.clone(),
            username: val.username.clone(),
            credential: val.credential.clone(),
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionPurpose {
    #[default]
    RemoteDesktop,
    FileManager,
}

/// RequestRemoteModel is used to request remote access.
/// web browser -> signaling server -> desk server
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RequestRemoteModel {
    /// Required session purpose hint. It only controls resource preparation and
    /// never grants a capability.
    pub purpose: RemoteSessionPurpose,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_wayland_control_mode: Option<String>,
    /// ICE servers, the value comes from signaling server
    #[serde(default)]
    pub ice_servers: Vec<LcxlRTCIceServer>,
    /// Browser-writable, **untrusted** selector naming which grant session this
    /// request redeems (set after redeeming a device / support code). It only
    /// *selects* a grant; the authorization fact — whether it is honored and what
    /// capability ceiling it carries — is decided server-side by looking the grant
    /// up and checking the caller's server-resolved principal, and is stamped into
    /// the trusted [`crate::model::request_remote_authz::RequestRemoteAuthz`]. A browser
    /// presenting someone else's `grant_session_id` is rejected at that principal
    /// check. `None` on a normal owner/org request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_session_id: Option<String>,
}

/// Browser-facing knobs that drive the adaptive-resolution hook. Server
/// sources these from `Settings.virtual_display.adaptive_*` and ships
/// them through `RemoteAccessInitializedData` so each browser session uses the
/// host operator's preference without round-tripping a separate REST
/// query.
///
/// `adaptive_throttle_ms` is intentionally NOT included — it is the
/// daemon's defensive rate limit and the browser does not need to know
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(default)]
pub struct AdaptiveResolutionParams {
    /// Trailing-edge debounce window (ms) the browser waits after a
    /// `resize` settles before issuing an auto ChangeDisplaySettings.
    pub debounce_ms: u64,
    /// Minimum pixel delta on either axis the browser treats as
    /// significant. Below this threshold the change is ignored.
    pub min_delta_px: u32,
}

impl Default for AdaptiveResolutionParams {
    fn default() -> Self {
        Self {
            debounce_ms: DEFAULT_ADAPTIVE_DEBOUNCE_MS,
            min_delta_px: DEFAULT_ADAPTIVE_MIN_DELTA_PX,
        }
    }
}

/// RemoteAccessInitializedData is used to initialize signaling data.
/// desk server -> signaling server -> web browser
/// See <https://github.com/webrtc-rs/webrtc/blob/254bdd5d970933e847dc000de9545040ce16f19f/webrtc/src/peer_connection/configuration.rs>.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RemoteAccessInitializedData {
    // ICE servers, the value comes from signaling server
    pub ice_servers: Vec<LcxlRTCIceServer>,
    /// User name for signaling.
    pub user_name: String,
    /// Audio device list
    pub audio_device_list: BTreeMap<String, Vec<AudioDevice>>,
    /// Audio encoder list
    pub audio_encoder_list: Vec<String>,
    /// Video device list
    pub video_device_list: BTreeMap<String, Vec<DisplayInfo>>,
    /// Video encoder list
    pub video_encoder_list: Vec<String>,
    /// Concrete encoder input constraints. Empty means a legacy host.
    #[serde(default)]
    pub video_encoder_capabilities: Vec<VideoEncoderCapability>,
    /// Host recommendation only. The controller resolves this with capabilities
    /// and its browser-scoped intent before constructing an executable offer.
    pub suggested_session_settings: SuggestedSessionSettings,
    pub session_settings_capabilities: SessionSettingsCapabilities,
    /// Opaque host-owned logical PeerConnection generation.
    pub connection_epoch: String,
    /// Whether the remote end has Tauri UI support (required for whiteboard overlay)
    #[serde(default)]
    pub has_tauri: bool,
    /// Whether the server is running with administrative privileges
    pub is_admin: bool,
    /// Whether the daemon currently has the IDD virtual display attached
    /// (service-daemon mode + `virtual_display.enabled=true` + attach
    /// resolved). The browser uses this to gate the adaptive-resolution
    /// hook — there is no point firing ChangeDisplaySettings against a
    /// host that does not own the IDD.
    #[serde(default)]
    pub virtual_display_active: bool,
    /// Most-recently-applied IDD refresh rate the daemon has seen via the
    /// worker's VirtualDisplayMode echo. `0` means the daemon has no
    /// observation yet (cold start) — the browser may use it for
    /// display purposes only; the auto path always sends `refresh_hz=0`
    /// and lets the daemon do the authoritative fallback.
    #[serde(default)]
    pub virtual_display_current_refresh_hz: u32,
    /// GDI device name (e.g. `\\.\DISPLAY8`) of the IDD virtual display
    /// when the daemon currently has it attached. `None` when no virtual
    /// display is attached (default mode / IDD detached / Disabled
    /// supervisor). The browser uses this both to label the matching
    /// entry in the display picker AND to gate the adaptive-resolution
    /// hook — auto requests only fire when the captured display equals
    /// this name, otherwise resizing the browser silently changes the
    /// IDD resolution while the worker is capturing a physical monitor.
    #[serde(default)]
    pub virtual_display_device_name: Option<String>,
    /// Browser-side adaptive resolution knobs sourced from
    /// `VirtualDisplaySettings`. Missing in legacy responses ⇒
    /// `AdaptiveResolutionParams::Default` (5000 ms / 16 px).
    #[serde(default)]
    pub adaptive_resolution: AdaptiveResolutionParams,
    /// Operating system of the remote host. Lets the browser tailor
    /// host-targeted UI (e.g. the keyboard-shortcut menu) to the host's
    /// platform instead of assuming Windows. Missing in legacy responses ⇒
    /// `OperationSystemEnum::Other` (unknown host) — NOT the deserializing
    /// machine's own OS, which is what `OperationSystemEnum::default()` yields.
    #[serde(default = "unknown_host_os")]
    pub operation_system: OperationSystemEnum,
}

/// Serde fallback for a host that does not advertise its OS. Unlike
/// `OperationSystemEnum::default()` (which resolves to the *local* compile-time
/// OS, the right answer when a host reports its own OS) a decoded-but-absent
/// field means the host OS is simply unknown.
fn unknown_host_os() -> OperationSystemEnum {
    OperationSystemEnum::Other
}

/// WebRTC Connection State
// `UpdateSettings` carries a full `DeskSettings` payload which dwarfs the other
// variants. Boxing it would ripple through every `match` site without a real
// runtime gain on this rarely-cloned enum, so we accept the size delta.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum WebRTConnectionState {
    Init,
    Connected,
    UpdateSettings(DeskSettings),
    Closed,
}

impl std::fmt::Display for WebRTConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<&RTCIceConnectionState> for WebRTConnectionState {
    fn from(value: &RTCIceConnectionState) -> Self {
        match value {
            RTCIceConnectionState::Unspecified
            | RTCIceConnectionState::New
            | RTCIceConnectionState::Checking => WebRTConnectionState::Init,
            RTCIceConnectionState::Connected => WebRTConnectionState::Connected,
            _ => WebRTConnectionState::Closed,
        }
    }
}

impl From<&RTCPeerConnectionState> for WebRTConnectionState {
    fn from(value: &RTCPeerConnectionState) -> Self {
        match value {
            RTCPeerConnectionState::Unspecified
            | RTCPeerConnectionState::New
            | RTCPeerConnectionState::Connecting => WebRTConnectionState::Init,
            RTCPeerConnectionState::Connected => WebRTConnectionState::Connected,
            _ => WebRTConnectionState::Closed,
        }
    }
}

/// Signaling State
#[derive(Debug, Clone, Default)]
pub struct SignalingState {
    /// accept control from remote peer
    pub accept_control: bool,
    /// accept clipboard sync from remote peer
    pub accept_clipboard_sync: bool,
    /// Session purpose selected during RequestRemoteAccess. It is a resource hint only.
    pub purpose: RemoteSessionPurpose,
    /// The validated capability ceiling for this connection, unwrapped from the
    /// `RequestRemoteAuthz` stamp by the host gate. `None` for a central-verified
    /// owner/full session (no ceiling) or a plain unrestricted connection;
    /// `Some(_)` for a redeemed-grant session whose effective capabilities are
    /// `meet(ceiling, global)` at each worker-side permission gate. Host-local
    /// runtime state, never carried on the wire; a plain signal leaves it `None`.
    pub access_ceiling: Option<SecuritySettings>,
    /// The grant logical-session id this connection belongs to, copied from the
    /// stamp so the daemon can index connections by grant (directed teardown /
    /// revocation) instead of by the coarse restricted-set. `None` for owner /
    /// unrestricted / legacy-support connections. Host-local runtime state.
    pub grant_session_id: Option<String>,
    /// current display info
    pub display_info: DisplayInfo,
    /// Input mode parsed and frozen when RequestRemoteAccess was admitted.
    pub resolved_wayland_control_mode: Option<crate::model::desk_settings::LinuxInputControlMode>,
}

/// Offer Model
/// web browser -> signaling server -> desk server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferModel {
    /// offer session description
    pub offer: RTCSessionDescription,
    /// Host-issued logical connection generation.
    pub connection_epoch: String,
    /// Remote desktop sends a complete object; DataChannel-only sends an
    /// explicitly present JSON null.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub session_settings: Option<RemoteSessionSettings>,
}

/// Remote Desk Type Enum
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[schema(rename_all = "kebab-case")]
pub enum RemoteDeskTypeEnum {
    /// Browser type
    Browser,
    /// Lcxl remote desktop server type
    Server,
    /// Lcxl remote desktop signal type
    Signal,
    /// Lcxl remote desktop manager type,
    /// used for manage multiple remote desktops,
    /// this enum used by another project, not this project
    /// so keep this enum but do not use it
    Manager,
    /// Temporary-support type: a desk server's dedicated, short-lived upstream
    /// connection opened solely to obtain and serve a temporary support session
    /// (a supporter the owner does not otherwise share the device with). Distinct
    /// from its main `Server` connection so it registers no device / presence and
    /// the host can hold it as a restricted, fail-closed session. Only a central
    /// brain (the manager) attaches temp-code semantics to this role; a plain
    /// signal treats it as routing-only.
    Support,
}

/// Request remote access model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestRemoteAccess {
    pub connection_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestRemoteData {}

/// Minimal user trait for signaling permission checks.
/// Both web/server-user::CurrentUser and manager::CurrentUser should implement this.
pub trait SignalingUser: Send + Sync {
    fn get_access(&self) -> Option<&str>;
    fn get_target_connection_id(&self) -> Option<&str>;
}

/// Async so an implementation can read live cluster state (e.g. the manager's
/// Redis TURN-node registry) when issuing ICE servers. Every call site is in an
/// async signaling loop, and the cost is paid once per connection/session
/// establishment (not per packet).
#[async_trait::async_trait]
pub trait TurnProvider: Send + Sync {
    async fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer;

    /// Build an ICE server with a self-signed TURN REST credential for `name`,
    /// valid for `ttl_secs`. Returns `None` when the provider cannot issue one
    /// (no static auth secret / no interface), so callers never inject an
    /// unusable entry. Default `None` keeps non-TURN providers compiling.
    async fn get_rest_ice_servers(&self, _name: &str, _ttl_secs: u64) -> Option<LcxlRTCIceServer> {
        None
    }
}
