use std::fmt::{Display, Formatter};

use actix_web::ResponseError;
use desk_utils::{
    error::{CustomDeskError, DeskErrorCode},
    rest::RestResponse,
};

#[derive(Debug)]
pub enum DeskSignalError {
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// An error occurred while handling WebSocket messages.
    ActixWsClosed(actix_ws::Closed),
    /// A desk signal facade error occurred.
    DeskSignalFacadeError(desk_signal_facade::error::DeskSignalFacadeError),

    /// Desk custom error
    CustomError(CustomDeskError),
}

impl DeskSignalError {
    pub fn custom_error<T>(error_code: DeskErrorCode, message: &str) -> Result<T, DeskSignalError> {
        Err(Self::new_custom_error(error_code, message))
    }

    pub fn new_custom_error(error_code: DeskErrorCode, message: &str) -> DeskSignalError {
        DeskSignalError::CustomError(CustomDeskError::new(error_code, message))
    }
}

impl Display for DeskSignalError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskSignalError::JsonError(err) => err.fmt(f),
            DeskSignalError::ActixWsClosed(err) => err.fmt(f),
            DeskSignalError::DeskSignalFacadeError(err) => err.fmt(f),
            DeskSignalError::CustomError(err) => err.fmt(f),
        }
    }
}

impl From<serde_json::Error> for DeskSignalError {
    fn from(err: serde_json::Error) -> Self {
        DeskSignalError::JsonError(err)
    }
}
impl From<actix_ws::Closed> for DeskSignalError {
    fn from(err: actix_ws::Closed) -> Self {
        DeskSignalError::ActixWsClosed(err)
    }
}

impl From<desk_signal_facade::error::DeskSignalFacadeError> for DeskSignalError {
    fn from(err: desk_signal_facade::error::DeskSignalFacadeError) -> Self {
        DeskSignalError::DeskSignalFacadeError(err)
    }
}

impl ResponseError for DeskSignalError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::OK
    }

    fn error_response(&self) -> actix_web::HttpResponse<actix_web::body::BoxBody> {
        let error_code = match self {
            DeskSignalError::CustomError(error) => error.error_code,
            _ => DeskErrorCode::SYSTEM_ERROR,
        };
        // write as json
        let rest = RestResponse::failed(error_code, self.to_string());
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}
