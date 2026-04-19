use std::{
    backtrace::{self, Backtrace},
    fmt::{self, Display},
};

use desk_utils::error::{CustomDeskError, DeskErrorCode};

/// Input injection specific error type.
#[derive(Debug)]
pub enum InputError {
    /// An I/O error occurred.
    IoError(Backtrace, std::io::Error),
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// A Windows result error occurred.
    #[cfg(target_os = "windows")]
    WindowsResultError(Backtrace, windows_result::Error),
    /// An arboard clipboard error occurred.
    ArboardError(Backtrace, arboard::Error),
    /// Desk custom error
    CustomError(CustomDeskError),
}

impl InputError {
    pub fn custom_error<T>(error_code: DeskErrorCode, message: &str) -> Result<T, InputError> {
        Err(InputError::CustomError(CustomDeskError::new(
            error_code, message,
        )))
    }

    pub fn new_custom_error(error_code: DeskErrorCode, message: &str) -> InputError {
        InputError::CustomError(CustomDeskError::new(error_code, message))
    }

    #[cfg(target_os = "windows")]
    pub fn windows_error<T>() -> Result<T, InputError> {
        use windows::Win32::Foundation::GetLastError;
        unsafe {
            let last_error = GetLastError();
            return InputError::custom_error(
                DeskErrorCode::WINDOWS_ERROR,
                &format!("windows error code: {:?}", last_error),
            );
        }
    }

    pub fn to_error_code(&self) -> DeskErrorCode {
        match self {
            InputError::CustomError(error) => error.error_code,
            _ => DeskErrorCode::SYSTEM_ERROR,
        }
    }
}

impl Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::IoError(backtrace, error) => {
                desk_utils::error::format_backtrace(f, backtrace, error)
            }
            InputError::JsonError(error) => error.fmt(f),
            #[cfg(target_os = "windows")]
            InputError::WindowsResultError(backtrace, error) => {
                desk_utils::error::format_backtrace(f, backtrace, error)
            }
            InputError::ArboardError(backtrace, error) => {
                desk_utils::error::format_backtrace(f, backtrace, error)
            }
            InputError::CustomError(error) => error.fmt(f),
        }
    }
}

impl From<std::io::Error> for InputError {
    fn from(err: std::io::Error) -> Self {
        InputError::IoError(backtrace::Backtrace::capture(), err)
    }
}

impl From<serde_json::Error> for InputError {
    fn from(err: serde_json::Error) -> Self {
        InputError::JsonError(err)
    }
}

#[cfg(target_os = "windows")]
impl From<windows_result::Error> for InputError {
    fn from(err: windows_result::Error) -> Self {
        InputError::WindowsResultError(backtrace::Backtrace::capture(), err)
    }
}

impl From<arboard::Error> for InputError {
    fn from(err: arboard::Error) -> Self {
        InputError::ArboardError(backtrace::Backtrace::capture(), err)
    }
}

impl std::error::Error for InputError {}
