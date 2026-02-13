use std::fmt::{Display, Formatter};

use desk_utils::error::{CustomDeskError, DeskErrorCode};

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
