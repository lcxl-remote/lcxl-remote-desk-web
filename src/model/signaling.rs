use actix_ws::Session;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use webrtc::{
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::{
    desk_error::{CustomDeskError, DeskError},
    model::{
        capture::DisplayInfo, common::ErrorCode, record_audio::AudioDevice, settings::DeskSettings,
    },
};

pub const DATA_CHANNEL_LABEL_MOUSE_EVENT: &str = "mouse_event";
pub const DATA_CHANNEL_LABEL_KEYBOARD_EVENT: &str = "keyboard_event";
pub const DATA_CHANNEL_LABEL_CLIPBOARD_EVENT: &str = "clipboard_event";
pub const DATA_CHANNEL_LABEL_FILE_TRANSFER_EVENT: &str = "file_transfer_event";

#[derive(Debug, Clone, Copy)]
pub struct SignalingType(i32);

pub const SIGNALING_TYPE_CODE_INIT: i32 = 0;
pub const SIGNALING_TYPE_CODE_OFFER: i32 = 100;
pub const SIGNALING_TYPE_CODE_ANSWER: i32 = 101;
pub const SIGNALING_TYPE_CODE_CANID: i32 = 102;

pub const SIGNALING_TYPE_CODE_REQUIRE_CONTROL: i32 = 201;
pub const SIGNALING_TYPE_CODE_ACCEPT_CONTROL: i32 = 202;
pub const SIGNALING_TYPE_CODE_DENY_CONTROL: i32 = 203;

pub const SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS: i32 = 301;

pub const SIGNALING_TYPE_CODE_ERROR: i32 = 1000;
pub const SIGNALING_TYPE_CODE_UNKNOWN_TYPE: i32 = 1001;

/// Signaling types.
impl SignalingType {
    /// Init message
    pub const INIT: SignalingType = SignalingType(SIGNALING_TYPE_CODE_INIT);

    pub const OFFER: SignalingType = SignalingType(SIGNALING_TYPE_CODE_OFFER);
    pub const ANSWER: SignalingType = SignalingType(SIGNALING_TYPE_CODE_ANSWER);

    pub const CANID: SignalingType = SignalingType(SIGNALING_TYPE_CODE_CANID);

    // error
    pub const ERROR: SignalingType = SignalingType(SIGNALING_TYPE_CODE_ERROR);
    // unknown signaling type
    pub const UNKNOWN_TYPE: SignalingType = SignalingType(SIGNALING_TYPE_CODE_UNKNOWN_TYPE);

    fn new(code: i32) -> Self {
        SignalingType(code)
    }
}

impl From<i32> for SignalingType {
    fn from(code: i32) -> Self {
        SignalingType::new(code)
    }
}

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingModel {
    /// signaling type
    pub signaling_type: i32,
    /// signaling data
    pub signaling_data: Option<String>,
}

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingErrorData {
    /// signaling type which errors occurred.
    pub signaling_type: i32,
    /// error code
    pub error_code: i32,
    /// error message
    pub message: String,
    /// signaling data
    pub signaling_data: Option<String>,
}

impl SignalingModel {
    pub fn new_str_data(signaling_type: SignalingType, signaling_data: &str) -> Self {
        Self {
            signaling_type: signaling_type.0,
            signaling_data: Some(signaling_data.to_string()),
        }
    }

    pub fn new_json_data<T>(
        signaling_type: SignalingType,
        signaling_data: &T,
    ) -> Result<Self, DeskError>
    where
        T: ?Sized + Serialize,
    {
        Ok(Self {
            signaling_type: signaling_type.0,
            signaling_data: Some(serde_json::to_string(signaling_data)?),
        })
    }

    pub fn error(
        signaling_type: SignalingType,
        error_code: ErrorCode,
        message: &str,
    ) -> Result<Self, DeskError> {
        let error_data = SignalingErrorData {
            signaling_type: signaling_type.0,
            error_code: error_code.0,
            message: message.to_string(),
            signaling_data: None,
        };
        SignalingModel::new_json_data(SignalingType::ERROR, &error_data)
    }

    pub fn custom_desk_error(
        signaling_type: SignalingType,
        error: CustomDeskError,
    ) -> Result<Self, DeskError> {
        Self::error(signaling_type, error.error_code, &error.message)
    }

    /// Get data with type
    pub fn get_data_with_type<T>(&self) -> Result<Option<T>, DeskError>
    where
        T: for<'a> Deserialize<'a>,
    {
        if let Some(data) = self.signaling_data.clone() {
            Ok(Some(serde_json::from_str::<T>(&data)?))
        } else {
            Ok(None)
        }
    }

    /// Get data with type
    pub fn get_data_with_default<T>(&self) -> Result<T, DeskError>
    where
        T: for<'a> Deserialize<'a> + Default,
    {
        if let Some(data) = self.signaling_data.clone() {
            Ok(serde_json::from_str::<T>(&data)?)
        } else {
            Ok(T::default())
        }
    }
}

pub trait SignalingSessionExt {
    fn send_signaling(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> impl std::future::Future<Output = Result<(), DeskError>> + Send;
}

impl SignalingSessionExt for Session {
    async fn send_signaling(&mut self, signaling_model: &SignalingModel) -> Result<(), DeskError> {
        self.text(serde_json::to_string(signaling_model)?).await?;
        Ok(())
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize, Hash, ToSchema)]
pub struct LcxlRTCIceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
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

/// InitSignalingData is used to initialize signaling data.
/// see https://github.com/webrtc-rs/webrtc/blob/254bdd5d970933e847dc000de9545040ce16f19f/webrtc/src/peer_connection/configuration.rs
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InitSignalingData {
    /// ICE servers to use for signaling.
    pub ice_servers: Vec<LcxlRTCIceServer>,
    /// User name for signaling.
    pub user_name: String,
    /// Audio device list
    pub audio_device_list: Vec<AudioDevice>,
    /// Video device list
    pub video_device_list: Vec<DisplayInfo>,
    /// Current desk settings
    pub desk_settings: DeskSettings,
}

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
#[derive(Debug, Clone, Copy, Default)]
pub struct SignalingState {
    /// accept control from remote peer
    pub accept_control: bool,
}

/// Offer Model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferModel {
    pub offer: RTCSessionDescription,
    pub desk_settings: DeskSettings,
}
