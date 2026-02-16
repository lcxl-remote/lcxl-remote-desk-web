use std::fmt::{Display, Formatter};

use actix_web::ResponseError;
use desk_utils::{
    error::{CustomDeskError, DeskErrorCode},
    rest::RestResponse,
};

#[derive(Debug)]
pub enum DeskSignalFacadeError {
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// An error occurred while handling WebSocket messages.
    ActixWsClosed(actix_ws::Closed),
    /// Desk custom error
    CustomError(CustomDeskError),
}

impl DeskSignalFacadeError {
    pub fn custom_error<T>(
        error_code: DeskErrorCode,
        message: &str,
    ) -> Result<T, DeskSignalFacadeError> {
        Err(DeskSignalFacadeError::CustomError(CustomDeskError::new(
            error_code, message,
        )))
    }
}

impl Display for DeskSignalFacadeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskSignalFacadeError::JsonError(err) => err.fmt(f),
            DeskSignalFacadeError::ActixWsClosed(err) => err.fmt(f),
            DeskSignalFacadeError::CustomError(err) => err.fmt(f),
        }
    }
}

impl From<serde_json::Error> for DeskSignalFacadeError {
    fn from(err: serde_json::Error) -> Self {
        DeskSignalFacadeError::JsonError(err)
    }
}
impl From<actix_ws::Closed> for DeskSignalFacadeError {
    fn from(code: actix_ws::Closed) -> Self {
        DeskSignalFacadeError::ActixWsClosed(code)
    }
}

impl ResponseError for DeskSignalFacadeError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::OK
    }

    fn error_response(&self) -> actix_web::HttpResponse<actix_web::body::BoxBody> {
        let error_code = match self {
            DeskSignalFacadeError::CustomError(error) => error.error_code,
            _ => DeskErrorCode::SYSTEM_ERROR,
        };
        // write as json
        let rest = RestResponse::failed(error_code, self.to_string());
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}
