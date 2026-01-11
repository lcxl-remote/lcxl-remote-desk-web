use std::fmt::{Display, Formatter};

use desk_utils::error::{CustomDeskError, DeskErrorCode};

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
    pub fn custom_error<T>(
        error_code: DeskErrorCode,
        message: String,
    ) -> Result<T, DeskSignalError> {
        Err(DeskSignalError::CustomError(CustomDeskError::new(
            error_code, message,
        )))
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
