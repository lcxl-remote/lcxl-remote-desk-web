use std::fmt::{Display, Formatter};

use desk_utils::error::{CustomDeskError, DeskErrorCode};

#[derive(Debug)]
pub enum DeskTauriError {
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// IO error
    IoError(std::io::Error),
    /// Tauri error
    TauriError(tauri::Error),
    /// Desk error
    DeskError(lcxl_remote_desk_server::error::DeskError),
    /// Desk custom error
    CustomError(CustomDeskError),
}

impl DeskTauriError {
    pub fn new_custom_error(error_code: DeskErrorCode, message: &str) -> DeskTauriError {
        DeskTauriError::CustomError(CustomDeskError::new(error_code, message))
    }
}

impl Display for DeskTauriError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskTauriError::JsonError(err) => err.fmt(f),
            DeskTauriError::IoError(err) => err.fmt(f),
            DeskTauriError::TauriError(err) => err.fmt(f),
            DeskTauriError::DeskError(err) => err.fmt(f),
            DeskTauriError::CustomError(err) => err.fmt(f),
        }
    }
}

impl From<serde_json::Error> for DeskTauriError {
    fn from(err: serde_json::Error) -> Self {
        DeskTauriError::JsonError(err)
    }
}

impl From<std::io::Error> for DeskTauriError {
    fn from(err: std::io::Error) -> Self {
        DeskTauriError::IoError(err)
    }
}

impl From<tauri::Error> for DeskTauriError {
    fn from(err: tauri::Error) -> Self {
        DeskTauriError::TauriError(err)
    }
}

impl From<lcxl_remote_desk_server::error::DeskError> for DeskTauriError {
    fn from(err: lcxl_remote_desk_server::error::DeskError) -> Self {
        DeskTauriError::DeskError(err)
    }
}
