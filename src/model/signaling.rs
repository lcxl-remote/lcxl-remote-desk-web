use actix_ws::Session;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use webrtc::ice_transport::ice_server::RTCIceServer;

use crate::desk_error::DeskError;

#[derive(Debug, Clone, Copy)]
pub struct SignalingType(i32);

pub const SIGNALING_TYPE_CODE_INIT: i32 = 0;
pub const SIGNALING_TYPE_CODE_OFFER: i32 = 100;
pub const SIGNALING_TYPE_CODE_ANSWER: i32 = 101;
pub const SIGNALING_TYPE_CODE_CANID: i32 = 200;
pub const SIGNALING_TYPE_CODE_ERROR: i32 = 1000;

/// Signaling types.
impl SignalingType {
    /// Hello message
    pub const INIT: SignalingType = SignalingType(SIGNALING_TYPE_CODE_INIT);

    pub const OFFER: SignalingType = SignalingType(SIGNALING_TYPE_CODE_OFFER);
    pub const ANSWER: SignalingType = SignalingType(SIGNALING_TYPE_CODE_ANSWER);

    pub const CANID: SignalingType = SignalingType(SIGNALING_TYPE_CODE_CANID);

    pub const ERROR: SignalingType = SignalingType(SIGNALING_TYPE_CODE_ERROR);
}

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingModel {
    /// signaling type
    pub signaling_type: i32,
    /// signaling success flag
    pub signaling_success: bool,
    /// signaling status code, use http status code
    pub signaling_status_code: i32,
    pub signaling_message: Option<String>,
    /// signaling data
    pub signaling_data: String,
}

impl SignalingModel {
    pub fn new_str_data(signaling_type: SignalingType, signaling_data: &str) -> Self {
        Self {
            signaling_type: signaling_type.0,
            signaling_success: true,
            signaling_status_code: 200,
            signaling_message: None,
            signaling_data: signaling_data.to_string(),
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
            signaling_success: true,
            signaling_status_code: 200,
            signaling_message: None,
            signaling_data: serde_json::to_string(signaling_data)?,
        })
    }

    pub fn error(signaling_type: SignalingType, message: &str) -> Self {
        Self {
            signaling_type: signaling_type.0,
            signaling_success: false,
            signaling_status_code: 500,
            signaling_message: Some(message.to_string()),
            signaling_data: String::new(),
        }
    }
}

pub trait SignalingSessionExt {
    async fn send_signaling(&mut self, signaling_model: &SignalingModel) -> Result<(), DeskError>;
}

impl SignalingSessionExt for Session {
    async fn send_signaling(&mut self, signaling_model: &SignalingModel) -> Result<(), DeskError> {
        self.text(serde_json::to_string(signaling_model)?).await?;
        Ok(())
    }
}

/// InitSignalingData is used to initialize signaling data.
/// see https://github.com/webrtc-rs/webrtc/blob/254bdd5d970933e847dc000de9545040ce16f19f/webrtc/src/peer_connection/configuration.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitSignalingData {
    pub ice_server: RTCIceServer,
    pub user_name: String,
}
