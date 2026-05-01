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
    Copy, Clone, Debug, Display, FromRepr, ToSchema_repr, Serialize_repr, Deserialize_repr,
)]
/// Signaling Type
#[repr(i32)]
pub enum SignalingType {
    /// Heartbeat for keeping WebSocket alive through reverse proxies
    Heartbeat = 1,

    // /// API version, should not be used
    // Version = 11,
    /// Request list connections
    FetchConnections = 21,

    /// Response session list
    ConnectionList = 22,

    /// WebRTC request remote access
    RequestRemote = 100,
    /// WebRTC init signaling type
    Init = 101,
    /// WebRTC offer signaling type
    Offer = 102,
    /// WebRTC answer signaling type
    Answer = 103,
    /// WebRTC CANID signaling type
    Canid = 104,

    RequireControl = 201,
    AcceptControl = 202,
    DenyControl = 203,
    CloseControl = 204,
    ChangeDisplaySettings = 205,

    /// Enable or disable private screen mode
    EnablePrivateScreen = 206,
    /// Private screen state changed notification
    PrivateScreenStateChanged = 207,
    /// Audio playback error notification
    AudioPlaybackError = 208,

    UpdateDeskSettings = 301,

    ManagerSystemInfo = 10003,
    ManagerSystemStatue = 10004,

    ManagerFileList = 10005,
    ManagerFileDelete = 10006,
    /// Start terminal
    StartTerminal = 10007,
    /// Send data to terminal
    SendDataToTerminal = 10008,
    /// Resize terminal
    ResizeTerminal = 10009,
    /// Close terminal
    CloseTerminal = 10010,
    /// Reply from terminal
    ReplyFromTerminal = 10011,
    /// List terminal
    ListTerminal = 10012,
    /// Terminal started
    TerminalStarted = 10013,
    /// Terminal closed
    TerminalClosed = 10014,
    /// Query remote system settings via signaling
    ManagerQuerySettings = 10015,
    /// Update remote system settings via signaling
    ManagerUpdateSettings = 10016,

    /// ServiceDaemon → Browser: desktop is switching, WebRTC will drop shortly
    DesktopSwitching = 500,
    /// ServiceDaemon → Browser: new Worker is ready, reconnect now
    DesktopReady = 501,

    /// Error
    Error = -1,
    /// Unrecognized signaling type will map to this
    #[serde(other)]
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
