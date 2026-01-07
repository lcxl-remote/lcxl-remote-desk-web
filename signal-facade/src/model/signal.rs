use std::collections::BTreeMap;

use actix_ws::Session;
use desk_utils::error::{CustomDeskError, DeskErrorCode};
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
    error::DeskSignalFacadeError,
    model::{audio_capture::AudioDevice, desk_settings::DeskSettings, image_capture::DisplayInfo},
};

#[derive(Debug, Clone, Copy)]
pub struct SignalingType(i32);

// signaling common type codes

/// version
pub const SIGNALING_TYPE_CODE_VERSION: i32 = 11;

// webrtc signaling type codes
pub const SIGNALING_TYPE_CODE_INIT: i32 = 101;
pub const SIGNALING_TYPE_CODE_OFFER: i32 = 102;
pub const SIGNALING_TYPE_CODE_ANSWER: i32 = 103;
pub const SIGNALING_TYPE_CODE_CANID: i32 = 104;

pub const SIGNALING_TYPE_CODE_REQUIRE_CONTROL: i32 = 201;
pub const SIGNALING_TYPE_CODE_ACCEPT_CONTROL: i32 = 202;
pub const SIGNALING_TYPE_CODE_DENY_CONTROL: i32 = 203;
pub const SIGNALING_TYPE_CODE_CLOSE_CONTROL: i32 = 204;
pub const SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS: i32 = 205;

pub const SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS: i32 = 301;

// manager code
/// file operate
pub const SIGNALING_TYPE_CODE_MANAGER_FILE: i32 = 10001;
/// 
pub const SIGNALING_TYPE_CODE_MANAGER_TERMINAL: i32 = 10002;
pub const SIGNALING_TYPE_CODE_MANAGER_SYSTEM_INFO: i32 = 10003;
pub const SIGNALING_TYPE_CODE_MANAGER_SYSTEM_STATUS: i32 = 10004;

/// error code
pub const SIGNALING_TYPE_CODE_ERROR: i32 = 10000000;
/// unknown code
pub const SIGNALING_TYPE_CODE_UNKNOWN_TYPE: i32 = 10000001;

/// Signaling types.
impl SignalingType {
    /// Init message
    pub const INIT: SignalingType = SignalingType(SIGNALING_TYPE_CODE_INIT);

    /// offer message
    pub const OFFER: SignalingType = SignalingType(SIGNALING_TYPE_CODE_OFFER);

    /// answer message
    pub const ANSWER: SignalingType = SignalingType(SIGNALING_TYPE_CODE_ANSWER);

    /// candidate message
    pub const CANID: SignalingType = SignalingType(SIGNALING_TYPE_CODE_CANID);

    // error message
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
    ) -> Result<Self, DeskSignalFacadeError>
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
        error_code: DeskErrorCode,
        message: &str,
    ) -> Result<Self, DeskSignalFacadeError> {
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
    ) -> Result<Self, DeskSignalFacadeError> {
        Self::error(signaling_type, error.error_code, &error.message)
    }

    /// Get data with type
    pub fn get_data_with_type<T>(&self) -> Result<Option<T>, DeskSignalFacadeError>
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
    pub fn get_data_with_default<T>(&self) -> Result<T, DeskSignalFacadeError>
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

/// Session extension trait for signaling
pub trait SignalingSessionExt {
    /// Send signaling message
    fn send_signaling(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;
}

impl SignalingSessionExt for Session {
    async fn send_signaling(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        self.text(serde_json::to_string(signaling_model)?).await?;
        Ok(())
    }
}

///RTC IceServer
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize, Hash, ToSchema)]
pub struct LcxlRTCIceServer {
    /// List of URLs associated with the ICE server, e.g. ["stun:stun.l.google.com:19302"]
    pub urls: Vec<String>,
    /// Username for the ICE server, if any.
    pub username: String,
    /// Credential for the ICE server, if any.
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
    pub audio_device_list: BTreeMap<String, Vec<AudioDevice>>,
    /// Audio encoder list
    pub audio_encoder_list: Vec<String>,
    /// Video device list
    pub video_device_list: BTreeMap<String, Vec<DisplayInfo>>,
    /// Video encoder list
    pub video_encoder_list: Vec<String>,
    /// Current desk settings
    pub desk_settings: DeskSettings,
}

/// WebRTC Connection State
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
    /// current display info
    pub display_info: DisplayInfo,
}

/// Offer Model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferModel {
    /// offer session description
    pub offer: RTCSessionDescription,
    /// desk settings
    pub desk_settings: DeskSettings,
}

/// Remote Desk Type Enum
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub enum RemoteDeskTypeEnum {
    /// Browser type
    Browser,
    /// Lcxl remote desktop server type
    Server,
}
