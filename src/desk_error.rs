use std::{
    backtrace::{self, Backtrace},
    fmt::{self, Display},
};

use actix_web::ResponseError;

use crate::model::common::{ErrorCode, RestResponse};

#[derive(Debug)]
pub struct CustomDeskError {
    pub error_code: ErrorCode,
    pub message: String,
}

impl fmt::Display for CustomDeskError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Custom desk error")?;

        write!(fmt, "({}): {}", self.error_code, self.message)?;

        Ok(())
    }
}

impl CustomDeskError {
    pub fn new(error_code: ErrorCode, message: String) -> CustomDeskError {
        CustomDeskError {
            error_code,
            message,
        }
    }
}

/// Custom error type for the application.
#[derive(Debug)]
pub enum DeskError {
    /// An I/O error occurred.
    IoError(Backtrace, std::io::Error),
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// A Rusqlite database error occurred.
    // RusqliteError(rusqlite::Error),
    /// A configuration error occurred.
    ConfigError(config::ConfigError),
    /// A TOML edit error occurred.
    TomlEditError(toml_edit::TomlError),
    /// A TOML ser error occurred.
    TomlError(toml::ser::Error),
    /// A connection pool error occurred.
    // R2d2Error(r2d2::Error),
    // Anyhow error occurred.
    AnyhowError(anyhow::Error),
    /// A join error occurred.
    TokioTaskJoinError(tokio::task::JoinError),
    /// An actix ws closed error occurred.
    ActixWsClosed(actix_ws::Closed),
    /// A Windows result error occurred.
    #[cfg(target_os = "windows")]
    WindowsResultError(Backtrace, windows_result::Error),
    /// A webrtc error occurred.
    WebrtcError(Backtrace, webrtc::Error),
    /// A webrtc media error occurred.
    WebrtcMediaError(webrtc_media::Error),
    /// A rtp error occurred.
    RtpError(rtp::Error),
    /// A yuv error occurred.
    YuvError(yuv::YuvError),
    /// A openh264 error occurred.
    Openh264Error(openh264::Error),
    /// A opus error occurred.
    OpusError(Backtrace, opusic_c::ErrorCode),
    /// A log parse level error occurred.
    ParseLevelError(log::ParseLevelError),
    /// Desk custom error
    CustomError(CustomDeskError),
}

impl DeskError {
    pub fn custom_error<T>(error_code: ErrorCode, message: String) -> Result<T, DeskError> {
        Err(DeskError::CustomError(CustomDeskError::new(
            error_code, message,
        )))
    }

    #[cfg(target_os = "windows")]
    pub fn windows_error<T>() -> Result<T, DeskError> {
        use windows::Win32::Foundation::GetLastError;
        unsafe {
            let last_error = GetLastError();
            //TODO use FormatMessageW(FORMAT_MESSAGE_FROM_SYSTEM)
            return DeskError::custom_error(
                ErrorCode::WINDOWS_ERROR,
                format!("windows error code: {:?}", last_error),
            );
        }
    }
}

impl Display for DeskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut _backtrace = &Backtrace::disabled();
        let err_fmt_result = match self {
            DeskError::IoError(backtrace, error) => {
                _backtrace = backtrace;
                error.fmt(f)
            }
            DeskError::JsonError(error) => error.fmt(f),
            //DeskError::RusqliteError(error) => error.fmt(f),
            DeskError::ConfigError(error) => error.fmt(f),
            DeskError::TomlEditError(error) => error.fmt(f),
            DeskError::TomlError(error) => error.fmt(f),
            //DeskError::R2d2Error(error) => error.fmt(f),
            DeskError::CustomError(error) => error.fmt(f),
            DeskError::AnyhowError(error) => error.fmt(f),
            DeskError::TokioTaskJoinError(error) => error.fmt(f),
            DeskError::ActixWsClosed(closed) => closed.fmt(f),
            #[cfg(target_os = "windows")]
            DeskError::WindowsResultError(backtrace, error) => {
                _backtrace = backtrace;
                error.fmt(f)
            }
            DeskError::WebrtcError(backtrace, error) => {
                _backtrace = backtrace;
                error.fmt(f)
            }
            DeskError::WebrtcMediaError(error) => error.fmt(f),
            DeskError::RtpError(error) => error.fmt(f),
            DeskError::YuvError(error) => error.fmt(f),
            DeskError::Openh264Error(error) => error.fmt(f),
            DeskError::OpusError(backtrace, error) => {
                _backtrace = backtrace;
                f.write_fmt(format_args!("{:?}", error))
            }
            DeskError::ParseLevelError(error) => error.fmt(f),
        };
        if let Err(error) = err_fmt_result {
            log::error!("Failed to format error: {:?}", error)
        }
        _backtrace.fmt(f)
    }
}

impl From<std::io::Error> for DeskError {
    fn from(err: std::io::Error) -> Self {
        DeskError::IoError(backtrace::Backtrace::capture(), err)
    }
}

impl From<serde_json::Error> for DeskError {
    fn from(err: serde_json::Error) -> Self {
        DeskError::JsonError(err)
    }
}
/*
impl From<rusqlite::Error> for DeskError {
    fn from(err: rusqlite::Error) -> Self {
        DeskError::RusqliteError(err)
    }
}
 */
impl From<config::ConfigError> for DeskError {
    fn from(err: config::ConfigError) -> Self {
        DeskError::ConfigError(err)
    }
}

impl From<toml_edit::TomlError> for DeskError {
    fn from(err: toml_edit::TomlError) -> Self {
        DeskError::TomlEditError(err)
    }
}

impl From<toml::ser::Error> for DeskError {
    fn from(err: toml::ser::Error) -> Self {
        DeskError::TomlError(err)
    }
}
/*
impl From<r2d2::Error> for DeskError {
    fn from(err: r2d2::Error) -> Self {
        DeskError::R2d2Error(err)
    }
}
*/
impl From<anyhow::Error> for DeskError {
    fn from(err: anyhow::Error) -> Self {
        DeskError::AnyhowError(err)
    }
}

impl From<tokio::task::JoinError> for DeskError {
    fn from(err: tokio::task::JoinError) -> Self {
        DeskError::TokioTaskJoinError(err)
    }
}

impl From<actix_ws::Closed> for DeskError {
    fn from(closed: actix_ws::Closed) -> Self {
        DeskError::ActixWsClosed(closed)
    }
}

#[cfg(target_os = "windows")]
impl From<windows_result::Error> for DeskError {
    fn from(err: windows_result::Error) -> Self {
        DeskError::WindowsResultError(backtrace::Backtrace::capture(), err)
    }
}

impl From<webrtc::Error> for DeskError {
    fn from(err: webrtc::Error) -> Self {
        DeskError::WebrtcError(backtrace::Backtrace::capture(), err)
    }
}

impl From<webrtc_media::Error> for DeskError {
    fn from(err: webrtc_media::Error) -> Self {
        DeskError::WebrtcMediaError(err)
    }
}

impl From<rtp::Error> for DeskError {
    fn from(err: rtp::Error) -> Self {
        DeskError::RtpError(err)
    }
}

impl From<yuv::YuvError> for DeskError {
    fn from(err: yuv::YuvError) -> Self {
        DeskError::YuvError(err)
    }
}

impl From<openh264::Error> for DeskError {
    fn from(err: openh264::Error) -> Self {
        DeskError::Openh264Error(err)
    }
}

impl From<opusic_c::ErrorCode> for DeskError {
    fn from(err: opusic_c::ErrorCode) -> Self {
        DeskError::OpusError(backtrace::Backtrace::capture(), err)
    }
}

impl From<log::ParseLevelError> for DeskError {
    fn from(err: log::ParseLevelError) -> Self {
        DeskError::ParseLevelError(err)
    }
}

impl ResponseError for DeskError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::OK
    }

    fn error_response(&self) -> actix_web::HttpResponse<actix_web::body::BoxBody> {
        let error_code = match self {
            DeskError::CustomError(error) => error.error_code,
            _ => ErrorCode::SYSTEM_ERROR,
        };
        // write as json
        let rest = RestResponse::failed(error_code, format!("{}", self));
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}
