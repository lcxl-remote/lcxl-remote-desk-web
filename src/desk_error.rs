use std::{
    backtrace::{self, Backtrace},
    fmt::{self, Display},
};

use actix_web::ResponseError;

use crate::model::{
    common::{ErrorCode, RestResponse},
    signaling::WebRTConnectionState,
};

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
    /// A tokio send error occurred.
    TokioWebrtcSendError(tokio::sync::watch::error::SendError<WebRTConnectionState>),
    /// An actix ws closed error occurred.
    ActixWsClosed(actix_ws::Closed),
    /// A Windows result error occurred.
    #[cfg(target_os = "windows")]
    WindowsResultError(Backtrace, windows_result::Error),
    /// A X11 connection error occurred.
    #[cfg(target_os = "linux")]
    X11ConnectError(x11rb::errors::ConnectError),
    /// A X11 connection error occurred.
    #[cfg(target_os = "linux")]
    X11ConnectionError(x11rb::errors::ConnectionError),
    /// A ALSA error occurred.
    #[cfg(target_os = "linux")]
    AlsaError(alsa::Error),
    #[cfg(target_os = "linux")]
    PipewireError(pipewire::Error),
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
    /// A vpx encode error occurred.
    VpxEncodeError(vpx_encode::Error),
    /// A opus error occurred.
    //OpusError(Backtrace, opus::Error),
    OpusError(Backtrace, opusic_c::ErrorCode),
    /// A log parse level error occurred.
    ParseLevelError(log::ParseLevelError),
    /// A from utf16 error occurred.
    FromUtf16Error(std::string::FromUtf16Error),
    /// A which error occurred.
    WhichError(which::Error),
    /// A regex error occurred.
    RegexError(regex::Error),
    /// A log set logger error occurred.
    SetLoggerError(log::SetLoggerError),
    /// A mpsc recv timeout error occurred.
    MpscRecvTimeoutError(std::sync::mpsc::RecvTimeoutError),
    /// A mpsc recv error occurred.
    MpscRecvError(std::sync::mpsc::RecvError),
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
        let err_fmt_result = match self {
            DeskError::IoError(_backtrace, error) => error.fmt(f),
            DeskError::JsonError(error) => error.fmt(f),
            DeskError::ConfigError(error) => error.fmt(f),
            DeskError::TomlEditError(error) => error.fmt(f),
            DeskError::TomlError(error) => error.fmt(f),
            DeskError::CustomError(error) => error.fmt(f),
            DeskError::AnyhowError(error) => error.fmt(f),
            DeskError::TokioTaskJoinError(error) => error.fmt(f),
            DeskError::ActixWsClosed(closed) => closed.fmt(f),
            #[cfg(target_os = "windows")]
            DeskError::WindowsResultError(_backtrace, error) => error.fmt(f),
            DeskError::WebrtcError(_backtrace, error) => error.fmt(f),
            DeskError::WebrtcMediaError(error) => error.fmt(f),
            DeskError::RtpError(error) => error.fmt(f),
            DeskError::YuvError(error) => error.fmt(f),
            DeskError::Openh264Error(error) => error.fmt(f),
            DeskError::VpxEncodeError(error) => error.fmt(f),
            DeskError::OpusError(_backtrace, error) => f.write_fmt(format_args!("{:?}", error)),
            DeskError::ParseLevelError(error) => error.fmt(f),
            DeskError::FromUtf16Error(error) => error.fmt(f),
            DeskError::RegexError(error) => error.fmt(f),
            DeskError::WhichError(error) => error.fmt(f),
            DeskError::TokioWebrtcSendError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            DeskError::X11ConnectError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            DeskError::X11ConnectionError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            DeskError::AlsaError(error) => error.fmt(f),
            DeskError::SetLoggerError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            DeskError::PipewireError(error) => error.fmt(f),
            DeskError::MpscRecvTimeoutError(error) => error.fmt(f),
            DeskError::MpscRecvError(error) => error.fmt(f),
        };
        if let Err(ref error) = err_fmt_result {
            log::error!("Failed to format error: {:?}", error)
        }
        err_fmt_result
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

impl From<tokio::sync::watch::error::SendError<WebRTConnectionState>> for DeskError {
    fn from(err: tokio::sync::watch::error::SendError<WebRTConnectionState>) -> Self {
        DeskError::TokioWebrtcSendError(err)
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

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectError> for DeskError {
    fn from(err: x11rb::errors::ConnectError) -> Self {
        DeskError::X11ConnectError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectionError> for DeskError {
    fn from(err: x11rb::errors::ConnectionError) -> Self {
        DeskError::X11ConnectionError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<alsa::Error> for DeskError {
    fn from(err: alsa::Error) -> Self {
        DeskError::AlsaError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<pipewire::Error> for DeskError {
    fn from(err: pipewire::Error) -> Self {
        DeskError::PipewireError(err)
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

impl From<vpx_encode::Error> for DeskError {
    fn from(err: vpx_encode::Error) -> Self {
        DeskError::VpxEncodeError(err)
    }
}

impl From<opusic_c::ErrorCode> for DeskError {
    fn from(err: opusic_c::ErrorCode) -> Self {
        DeskError::OpusError(backtrace::Backtrace::capture(), err)
    }
}
/*
impl From<opus::Error> for DeskError {
    fn from(err: opus::Error) -> Self {
        DeskError::OpusError(backtrace::Backtrace::capture(), err)
    }
}
 */
impl From<log::ParseLevelError> for DeskError {
    fn from(err: log::ParseLevelError) -> Self {
        DeskError::ParseLevelError(err)
    }
}

impl From<std::string::FromUtf16Error> for DeskError {
    fn from(err: std::string::FromUtf16Error) -> Self {
        DeskError::FromUtf16Error(err)
    }
}

impl From<which::Error> for DeskError {
    fn from(err: which::Error) -> Self {
        DeskError::WhichError(err)
    }
}

impl From<regex::Error> for DeskError {
    fn from(err: regex::Error) -> Self {
        DeskError::RegexError(err)
    }
}

impl From<log::SetLoggerError> for DeskError {
    fn from(err: log::SetLoggerError) -> Self {
        DeskError::SetLoggerError(err)
    }
}

impl From<std::sync::mpsc::RecvTimeoutError> for DeskError {
    fn from(err: std::sync::mpsc::RecvTimeoutError) -> Self {
        DeskError::MpscRecvTimeoutError(err)
    }
}

impl From<std::sync::mpsc::RecvError> for DeskError {
    fn from(err: std::sync::mpsc::RecvError) -> Self {
        DeskError::MpscRecvError(err)
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
        let rest = RestResponse::failed(error_code, self.to_string());
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}
