use std::{collections::BTreeMap, time::Duration};

use desk_utils::error::{CustomDeskError, DeskErrorCode};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use strum_macros::{Display, FromRepr};
use utoipa::{IntoParams, ToSchema};
use utoipa_repr::ToSchema_repr;
use uuid::Uuid;
use webrtc::{
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::{
    error::DeskSignalFacadeError,
    model::{audio_capture::AudioDevice, desk_settings::DeskSettings, image_capture::DisplayInfo},
};

#[derive(
    Copy,
    Clone,
    Debug,
    Display,
    FromRepr,
    ToSchema_repr,
    Serialize_repr,
    Deserialize_repr,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
/// Signaling Type
#[repr(i32)]
// wincode's default `u32` positional tag would silently disagree with the
// `Serialize_repr` JSON form (which emits the explicit `i32` discriminant).
// Lock the wincode tag to the same `i32` encoding so the daemon ↔ worker
// wire bytes and the browser-facing JSON identify the same variant by the
// same number.
#[wincode(tag_encoding = "i32")]
pub enum SignalingType {
    /// Heartbeat for keeping WebSocket alive through reverse proxies
    #[wincode(tag = 1)]
    Heartbeat = 1,

    // /// API version, should not be used
    // Version = 11,
    /// Request list connections
    #[wincode(tag = 21)]
    FetchConnections = 21,

    /// Response session list
    #[wincode(tag = 22)]
    ConnectionList = 22,

    /// Signaling server → Server peers: a connection has just left the
    /// server's connection map. The signaling server fans this out
    /// (currently only when a `Browser` peer exits) so the daemon-side
    /// PC manager can release per-`connection_id` resources (DXGI
    /// duplication, encoder, IPC senders, …) immediately, without
    /// waiting for the multi-second ICE `disconnected → failed`
    /// fallback. Carries the departed peer's `connection_id` in
    /// `from_connection_id`; data payload is intentionally empty.
    #[wincode(tag = 23)]
    ConnectionRemoved = 23,

    /// WebRTC request remote access
    #[wincode(tag = 100)]
    RequestRemote = 100,
    /// WebRTC init signaling type
    #[wincode(tag = 101)]
    Init = 101,
    /// WebRTC offer signaling type
    #[wincode(tag = 102)]
    Offer = 102,
    /// WebRTC answer signaling type
    #[wincode(tag = 103)]
    Answer = 103,
    /// WebRTC CANID signaling type
    #[wincode(tag = 104)]
    Canid = 104,

    #[wincode(tag = 201)]
    RequireControl = 201,
    #[wincode(tag = 202)]
    AcceptControl = 202,
    #[wincode(tag = 203)]
    DenyControl = 203,
    #[wincode(tag = 204)]
    CloseControl = 204,
    #[wincode(tag = 205)]
    ChangeDisplaySettings = 205,

    /// Enable or disable private screen mode
    #[wincode(tag = 206)]
    EnablePrivateScreen = 206,
    /// Private screen state changed notification
    #[wincode(tag = 207)]
    PrivateScreenStateChanged = 207,
    /// Audio playback error notification
    #[wincode(tag = 208)]
    AudioPlaybackError = 208,

    #[wincode(tag = 301)]
    UpdateDeskSettings = 301,

    #[wincode(tag = 10003)]
    ManagerSystemInfo = 10003,
    #[wincode(tag = 10004)]
    ManagerSystemStatue = 10004,

    #[wincode(tag = 10005)]
    ManagerFileList = 10005,
    #[wincode(tag = 10006)]
    ManagerFileDelete = 10006,
    /// Start terminal
    #[wincode(tag = 10007)]
    StartTerminal = 10007,
    /// Send data to terminal
    #[wincode(tag = 10008)]
    SendDataToTerminal = 10008,
    /// Resize terminal
    #[wincode(tag = 10009)]
    ResizeTerminal = 10009,
    /// Close terminal
    #[wincode(tag = 10010)]
    CloseTerminal = 10010,
    /// Reply from terminal
    #[wincode(tag = 10011)]
    ReplyFromTerminal = 10011,
    /// List terminal
    #[wincode(tag = 10012)]
    ListTerminal = 10012,
    /// Terminal started
    #[wincode(tag = 10013)]
    TerminalStarted = 10013,
    /// Terminal closed
    #[wincode(tag = 10014)]
    TerminalClosed = 10014,
    /// Query remote system settings via signaling
    #[wincode(tag = 10015)]
    ManagerQuerySettings = 10015,
    /// Update remote system settings via signaling
    #[wincode(tag = 10016)]
    ManagerUpdateSettings = 10016,

    /// ServiceDaemon → Browser: desktop is switching, WebRTC will drop shortly
    #[wincode(tag = 500)]
    DesktopSwitching = 500,
    /// ServiceDaemon → Browser: new Worker is ready, reconnect now
    #[wincode(tag = 501)]
    DesktopReady = 501,

    /// Error
    #[wincode(tag = -1)]
    Error = -1,
    /// Unrecognized signaling type will map to this on the JSON path
    /// (via `#[serde(other)]`). The wincode wire never emits this
    /// variant — daemon and worker are version-locked so an "unknown"
    /// discriminant cannot reach the IPC boundary. We still assign it
    /// a wincode tag so the type implements `SchemaWrite` / `SchemaRead`.
    #[serde(other)]
    #[wincode(tag = -100)]
    Unknown = -100,
}

#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingResponseState {
    /// error code
    ///
    /// see alse: desk_utils::DeskErrorCode
    pub error_code: i32,
    /// error message
    pub message: Option<String>,
}

impl SignalingResponseState {
    pub fn success() -> Self {
        Self {
            error_code: DeskErrorCode::SUCCESS.code(),
            message: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.error_code == DeskErrorCode::SUCCESS.code()
    }
}
/// Signaling model
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingModel {
    /// Request id
    pub request_id: String,
    /// Signaling type
    pub signaling_type: SignalingType,
    /// From connection id, if None, means from signal server
    pub from_connection_id: Option<String>,
    /// To connection id, if None, means to signal server
    pub to_connection_id: Option<String>,
    /// Signaling data
    signaling_data: Option<serde_json::Value>,
    /// Signaling response state. Some means this is a response message.
    pub response_state: Option<SignalingResponseState>,
}

impl SignalingModel {
    pub fn new(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        signaling_data: Option<serde_json::Value>,
        response_state: Option<SignalingResponseState>,
    ) -> Self {
        Self {
            request_id: request_id.to_string(),
            signaling_type,
            from_connection_id,
            to_connection_id,
            signaling_data,
            response_state,
        }
    }

    /// New request signaling model
    pub fn new_request<T>(
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        signaling_data: Option<&T>,
    ) -> Result<Self, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize,
    {
        Ok(Self::new(
            &Uuid::new_v4().to_string(),
            signaling_type,
            None,
            to_connection_id,
            signaling_data.map(serde_json::to_value).transpose()?,
            None,
        ))
    }

    /// New response signaling model
    pub fn new_response<T>(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        signaling_data: Option<&T>,
        response_state: SignalingResponseState,
    ) -> Result<Self, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize,
    {
        Ok(Self::new(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            signaling_data.map(serde_json::to_value).transpose()?,
            Some(response_state),
        ))
    }

    /// New success response signaling model
    pub fn success_response<T>(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        signaling_data: Option<&T>,
    ) -> Result<Self, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize,
    {
        Self::new_response(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            signaling_data,
            SignalingResponseState::success(),
        )
    }

    /// New response signaling model with none data
    pub fn error(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        error_code: DeskErrorCode,
        message: &str,
    ) -> Result<Self, DeskSignalFacadeError> {
        let error_data = SignalingResponseState {
            error_code: error_code.code(),
            message: Some(message.to_string()),
        };
        Self::new_response::<()>(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            None,
            error_data,
        )
    }

    pub fn custom_desk_error(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        error: CustomDeskError,
    ) -> Result<Self, DeskSignalFacadeError> {
        Self::error(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            error.error_code,
            &error.message,
        )
    }

    /// Get data with type
    pub fn get_data_with_type<T>(&self) -> Result<Option<T>, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a>,
    {
        let value = if let Some(data) = &self.signaling_data {
            Some(serde_json::from_value(data.clone())?)
        } else {
            None
        };
        Ok(value)
    }

    /// Get data with type
    pub fn get_data_with_default<T>(&self) -> Result<T, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a> + Default,
    {
        let value = if let Some(data) = &self.signaling_data {
            serde_json::from_value(data.clone())?
        } else {
            T::default()
        };
        Ok(value)
    }

    /// Get data with type, if data is none, will throw error
    pub fn get_data<T>(&self) -> Result<T, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a>,
    {
        let data_opt = self.get_data_with_type::<T>()?;
        if let Some(data) = data_opt {
            Ok(data)
        } else {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::BLANK_SIGNALING_DATA,
                &format!("Data can't be none, signal type: {}", self.signaling_type),
            )
        }
    }

    pub fn get_raw_data(&self) -> &Option<serde_json::Value> {
        &self.signaling_data
    }

    pub fn check_and_get_from_connection_id(&self) -> Result<&str, DeskSignalFacadeError> {
        if let Some(from_connection_id) = &self.from_connection_id {
            Ok(from_connection_id.as_str())
        } else {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "From connection id can't be none, signal type: {}",
                    self.signaling_type
                ),
            )
        }
    }

    pub fn check_and_get_to_connection_id(&self) -> Result<&str, DeskSignalFacadeError> {
        if let Some(to_connection_id) = &self.to_connection_id {
            Ok(to_connection_id.as_str())
        } else {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "To connection id can't be none, signal type: {}",
                    self.signaling_type
                ),
            )
        }
    }

    pub fn is_request(&self) -> bool {
        self.response_state.is_none()
    }

    pub fn is_response(&self) -> bool {
        self.response_state.is_some()
    }
}

/// Peer signaling sender trait
pub trait PeerSignalingSender {
    /// Send signaling message
    fn send_response<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        signaling_data: &T,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send
    where
        T: ?Sized + Serialize + Sync;

    fn send_error(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        error_code: DeskErrorCode,
        error_message: &str,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;

    /// Send to peer session
    fn send_to_peer<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: &str,
        data: T,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send
    where
        T: Serialize + Sync + Send;
}

pub trait ForwardSignalingSender {
    /// Send response signaling message
    fn send_response(
        &self,
        from_connection_id: Option<String>,
        signaling_model: &SignalingModel,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;

    /// Forward to peer session
    fn send_to_peer(
        &self,
        from_connection_id: &str,
        signaling_model: &SignalingModel,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;

    /// Send request signaling message with callback
    /// There is no from_connection_id in this function, because it is used by http api
    fn request_peer_with_callback<T>(
        &self,
        signaling_type: SignalingType,
        data: Option<&T>,
        timeout: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<SignalingModel, DeskSignalFacadeError>> + Send
    where
        T: ?Sized + Serialize + Sync;
}

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
/// RequestRemoteModel is used to request remote access.
/// web browser -> signaling server -> desk server
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RequestRemoteModel {
    /// ICE servers, the value comes from signaling server
    #[serde(default)]
    pub ice_servers: Vec<LcxlRTCIceServer>,
}

/// InitSignalingData is used to initialize signaling data.
/// desk server -> signaling server -> web browser
/// see https://github.com/webrtc-rs/webrtc/blob/254bdd5d970933e847dc000de9545040ce16f19f/webrtc/src/peer_connection/configuration.rs
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InitSignalingData {
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
    /// Current desk settings
    pub desk_settings: DeskSettings,
    /// Whether the remote end has Tauri UI support (required for whiteboard overlay)
    #[serde(default)]
    pub has_tauri: bool,
    /// Whether the server is running with administrative privileges
    pub is_admin: bool,
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
    /// current display info
    pub display_info: DisplayInfo,
    /// wayland control mode: portal/uinput/auto/none
    pub wayland_control_mode: Option<String>,
}

/// Offer Model
/// web browser -> signaling server -> desk server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferModel {
    /// offer session description
    pub offer: RTCSessionDescription,
    /// desk settings
    pub desk_settings: DeskSettings,
}

/// Remote Desk Type Enum
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq)]
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
}

/// Request remote access model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestRemote {
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

pub trait TurnProvider: Send + Sync {
    fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer;
}

#[cfg(test)]
mod wincode_tests {
    //! Wincode `SignalingType` coverage. The enum has 36 variants with
    //! explicit `#[repr(i32)]` discriminants, and the wincode tag is
    //! locked to `i32` via `#[wincode(tag_encoding = "i32")]` so the
    //! daemon ↔ worker wire bytes use the same number the JSON wire
    //! emits (via `Serialize_repr`).
    //!
    //! Two tests cover this from different angles:
    //!
    //!   * `signaling_type_round_trips_wincode` — encode + decode each
    //!     variant and assert the decoded value matches the input. This
    //!     catches "did we forget to add `#[derive(...)]` or
    //!     `#[wincode(tag_encoding = ...)]`?" kinds of bugs.
    //!
    //!   * `signaling_type_wire_tag_matches_discriminant_for_all_variants`
    //!     — encode each variant and assert the *first four bytes* of
    //!     the encoded payload equal `(variant as i32).to_le_bytes()`.
    //!     This is the byte-level check the migration plan and code
    //!     review both call out: a round-trip test pairs encode and
    //!     decode, so a `#[wincode(tag = N)]` that silently disagrees
    //!     with the `repr(i32)` discriminant for a single variant
    //!     (e.g. typo `tag = 101` on a `= 102` variant) would still
    //!     pass round-trip — encode + decode would both use the same
    //!     wrong tag. Only by asserting against the *expected*
    //!     discriminant separately do we catch tag drift.
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// Table of every `SignalingType` variant paired with the explicit
    /// `i32` discriminant it carries. When a new variant is added to
    /// the enum, this table must be extended — leaving it incomplete
    /// is precisely the regression `signaling_type_wire_tag_matches_…`
    /// is built to catch.
    fn all_variants_with_tag() -> [(SignalingType, i32); 36] {
        [
            (SignalingType::Heartbeat, 1),
            (SignalingType::FetchConnections, 21),
            (SignalingType::ConnectionList, 22),
            (SignalingType::ConnectionRemoved, 23),
            (SignalingType::RequestRemote, 100),
            (SignalingType::Init, 101),
            (SignalingType::Offer, 102),
            (SignalingType::Answer, 103),
            (SignalingType::Canid, 104),
            (SignalingType::RequireControl, 201),
            (SignalingType::AcceptControl, 202),
            (SignalingType::DenyControl, 203),
            (SignalingType::CloseControl, 204),
            (SignalingType::ChangeDisplaySettings, 205),
            (SignalingType::EnablePrivateScreen, 206),
            (SignalingType::PrivateScreenStateChanged, 207),
            (SignalingType::AudioPlaybackError, 208),
            (SignalingType::UpdateDeskSettings, 301),
            (SignalingType::DesktopSwitching, 500),
            (SignalingType::DesktopReady, 501),
            (SignalingType::ManagerSystemInfo, 10003),
            (SignalingType::ManagerSystemStatue, 10004),
            (SignalingType::ManagerFileList, 10005),
            (SignalingType::ManagerFileDelete, 10006),
            (SignalingType::StartTerminal, 10007),
            (SignalingType::SendDataToTerminal, 10008),
            (SignalingType::ResizeTerminal, 10009),
            (SignalingType::CloseTerminal, 10010),
            (SignalingType::ReplyFromTerminal, 10011),
            (SignalingType::ListTerminal, 10012),
            (SignalingType::TerminalStarted, 10013),
            (SignalingType::TerminalClosed, 10014),
            (SignalingType::ManagerQuerySettings, 10015),
            (SignalingType::ManagerUpdateSettings, 10016),
            (SignalingType::Error, -1),
            (SignalingType::Unknown, -100),
        ]
    }

    #[test]
    fn signaling_type_round_trips_wincode() {
        let config = unbounded_config();
        for (variant, _expected) in all_variants_with_tag() {
            let bytes = wincode::config::serialize(&variant, config)
                .unwrap_or_else(|err| panic!("encode {variant:?}: {err}"));
            let back: SignalingType = wincode::config::deserialize(&bytes, config)
                .unwrap_or_else(|err| panic!("decode {variant:?}: {err}"));
            assert_eq!(
                back as i32, variant as i32,
                "round-trip mismatch for {variant:?}",
            );
        }
    }

    #[test]
    fn signaling_type_wire_tag_matches_discriminant_for_all_variants() {
        let config = unbounded_config();
        for (variant, expected_tag) in all_variants_with_tag() {
            let bytes = wincode::config::serialize(&variant, config)
                .unwrap_or_else(|err| panic!("encode {variant:?}: {err}"));
            assert!(bytes.len() >= 4, "{variant:?} produced fewer than 4 bytes",);
            let tag = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(
                tag, expected_tag,
                "wincode wire tag for {variant:?} does not match its repr(i32) discriminant",
            );
        }
    }
}
