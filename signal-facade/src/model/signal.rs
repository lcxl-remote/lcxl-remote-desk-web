use std::collections::BTreeMap;

use actix_ws::Session;
use desk_utils::error::{CustomDeskError, DeskErrorCode};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use strum_macros::{Display, FromRepr};
use utoipa::{IntoParams, ToSchema};
use utoipa_repr::ToSchema_repr;
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
    /// API version
    Version = 11,

    /// Request list sessions
    FetchSessions = 21,

    /// Response session list
    SessionList = 22,

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

    UpdateDeskSettings = 301,

    ManagerFile = 10001,
    ManagerTerminal = 10002,
    ManagerSystemInfo = 10003,
    ManagerSystemStatue = 10004,

    Error = 10000000,
    Unknown = 10000001,
}

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingModel {
    /// signaling type
    pub signaling_type: SignalingType,
    /// signaling data
    pub signaling_data: Option<String>,
}

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingErrorData {
    /// signaling type which errors occurred.
    pub signaling_type: SignalingType,
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
            signaling_type: signaling_type,
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
            signaling_type: signaling_type,
            signaling_data: Some(serde_json::to_string(signaling_data)?),
        })
    }

    pub fn error(
        signaling_type: SignalingType,
        error_code: DeskErrorCode,
        message: &str,
    ) -> Result<Self, DeskSignalFacadeError> {
        let error_data = SignalingErrorData {
            signaling_type: signaling_type,
            error_code: error_code.0,
            message: message.to_string(),
            signaling_data: None,
        };
        SignalingModel::new_json_data(SignalingType::Error, &error_data)
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
        if let Some(data) = &self.signaling_data {
            Ok(Some(serde_json::from_str::<T>(data)?))
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

    /// Get data with type, if data is none, will throw error
    pub fn get_data<T>(&self) -> Result<T, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a>,
    {
        let data_opt = self.get_data_with_type::<T>()?;
        if let Some(data) = data_opt {
            return Ok(data);
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Data can't be none".to_string(),
            );
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
