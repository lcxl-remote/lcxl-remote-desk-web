use actix_ws::Session;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use webrtc::{
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    peer_connection::peer_connection_state::RTCPeerConnectionState,
};

use crate::{
    desk_error::DeskError,
    model::{record_audio::AudioDevice, record_screen::DisplayInfo},
};

#[derive(Debug, Clone, Copy)]
pub struct SignalingType(i32);

pub const SIGNALING_TYPE_CODE_INIT: i32 = 0;
pub const SIGNALING_TYPE_CODE_OFFER: i32 = 100;
pub const SIGNALING_TYPE_CODE_ANSWER: i32 = 101;
pub const SIGNALING_TYPE_CODE_CANID: i32 = 200;
pub const SIGNALING_TYPE_CODE_ERROR: i32 = 1000;
pub const SIGNALING_TYPE_CODE_UNKNOWN_TYPE: i32 = 1001;

/// Signaling types.
impl SignalingType {
    /// Hello message
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

    pub fn error(signaling_type: SignalingType, message: &str) -> Result<Self, DeskError> {
        let error_data = SignalingErrorData {
            signaling_type: signaling_type.0,
            message: message.to_string(),
            signaling_data: None,
        };
        SignalingModel::new_json_data(SignalingType::ERROR, &error_data)
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
    pub ice_servers: Vec<LcxlRTCIceServer>,
    pub user_name: String,
    pub audio_device_list: Vec<AudioDevice>,
    pub video_device_list: Vec<DisplayInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WebRTConnectionState {
    Init,
    Connected,
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
