//! Signaling envelope, response state, and sender traits.

use std::time::Duration;

use desk_utils::error::{CustomDeskError, DeskErrorCode};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::signaling_type::SignalingType;
use crate::error::DeskSignalFacadeError;
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
