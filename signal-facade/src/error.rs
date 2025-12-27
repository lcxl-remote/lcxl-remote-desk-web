use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum DeskSignalFacadeError {
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// An error occurred while handling WebSocket messages.
    ActixWsClosed(actix_ws::Closed),
}

impl Display for DeskSignalFacadeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskSignalFacadeError::JsonError(err) => err.fmt(f),
            DeskSignalFacadeError::ActixWsClosed(err) => err.fmt(f),
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
