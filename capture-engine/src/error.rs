use std::{
    backtrace::{self, Backtrace},
    fmt::{self, Display},
};

use desk_utils::error::{CustomDeskError, DeskErrorCode};

/// Capture engine specific error type.
/// This is a lightweight error type without framework dependencies (no actix-web, no webrtc).
#[derive(Debug)]
pub enum CaptureError {
    /// An I/O error occurred.
    IoError(Backtrace, std::io::Error),
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// A yuv error occurred.
    YuvError(yuv::YuvError),
    /// A openh264 error occurred.
    Openh264Error(openh264::Error),
    /// A vpx encode error occurred.
    VpxEncodeError(vpx_encode::Error),
    /// A opus error occurred.
    OpusError(Backtrace, opusic_c::ErrorCode),
    /// A from utf16 error occurred.
    FromUtf16Error(std::string::FromUtf16Error),
    /// A from utf8 error occurred.
    FromUtf8Error(std::string::FromUtf8Error),
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
    #[cfg(target_os = "linux")]
    ZbusError(zbus::Error),
    #[cfg(target_os = "linux")]
    ZbusZvariantError(zbus::zvariant::Error),
    /// Anyhow error occurred.
    AnyhowError(anyhow::Error),
    /// A mpsc recv timeout error occurred.
    MpscRecvTimeoutError(std::sync::mpsc::RecvTimeoutError),
    /// A mpsc recv error occurred.
    MpscRecvError(std::sync::mpsc::RecvError),
    /// Desk custom error
    CustomError(CustomDeskError),
}

impl CaptureError {
    pub fn custom_error<T>(error_code: DeskErrorCode, message: &str) -> Result<T, CaptureError> {
        Err(CaptureError::CustomError(CustomDeskError::new(
            error_code, message,
        )))
    }

    /// Create a new custom error
    pub fn new_custom_error(error_code: DeskErrorCode, message: &str) -> CaptureError {
        CaptureError::CustomError(CustomDeskError::new(error_code, message))
    }

    #[cfg(target_os = "windows")]
    pub fn windows_error<T>() -> Result<T, CaptureError> {
        use windows::Win32::Foundation::GetLastError;
        unsafe {
            let last_error = GetLastError();
            CaptureError::custom_error(
                DeskErrorCode::WINDOWS_ERROR,
                &format!("windows error code: {:?}", last_error),
            )
        }
    }

    pub fn to_error_code(&self) -> DeskErrorCode {
        match self {
            CaptureError::CustomError(error) => error.error_code,
            _ => DeskErrorCode::SYSTEM_ERROR,
        }
    }
}

impl Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::IoError(backtrace, error) => {
                desk_utils::error::format_backtrace(f, backtrace, error)
            }
            CaptureError::JsonError(error) => error.fmt(f),
            CaptureError::YuvError(error) => error.fmt(f),
            CaptureError::Openh264Error(error) => error.fmt(f),
            CaptureError::VpxEncodeError(error) => error.fmt(f),
            CaptureError::OpusError(backtrace, error) => {
                desk_utils::error::format_debug_backtrace(f, backtrace, error)
            }
            CaptureError::FromUtf16Error(error) => error.fmt(f),
            CaptureError::FromUtf8Error(error) => error.fmt(f),
            #[cfg(target_os = "windows")]
            CaptureError::WindowsResultError(backtrace, error) => {
                desk_utils::error::format_backtrace(f, backtrace, error)
            }
            #[cfg(target_os = "linux")]
            CaptureError::X11ConnectError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            CaptureError::X11ConnectionError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            CaptureError::AlsaError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            CaptureError::PipewireError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            CaptureError::ZbusError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            CaptureError::ZbusZvariantError(error) => error.fmt(f),
            CaptureError::AnyhowError(error) => error.fmt(f),
            CaptureError::MpscRecvTimeoutError(error) => error.fmt(f),
            CaptureError::MpscRecvError(error) => error.fmt(f),
            CaptureError::CustomError(error) => error.fmt(f),
        }
    }
}

impl From<std::io::Error> for CaptureError {
    fn from(err: std::io::Error) -> Self {
        CaptureError::IoError(backtrace::Backtrace::capture(), err)
    }
}

impl From<serde_json::Error> for CaptureError {
    fn from(err: serde_json::Error) -> Self {
        CaptureError::JsonError(err)
    }
}

impl From<yuv::YuvError> for CaptureError {
    fn from(err: yuv::YuvError) -> Self {
        CaptureError::YuvError(err)
    }
}

impl From<openh264::Error> for CaptureError {
    fn from(err: openh264::Error) -> Self {
        CaptureError::Openh264Error(err)
    }
}

impl From<vpx_encode::Error> for CaptureError {
    fn from(err: vpx_encode::Error) -> Self {
        CaptureError::VpxEncodeError(err)
    }
}

impl From<opusic_c::ErrorCode> for CaptureError {
    fn from(err: opusic_c::ErrorCode) -> Self {
        CaptureError::OpusError(backtrace::Backtrace::capture(), err)
    }
}

impl From<std::string::FromUtf16Error> for CaptureError {
    fn from(err: std::string::FromUtf16Error) -> Self {
        CaptureError::FromUtf16Error(err)
    }
}

impl From<std::string::FromUtf8Error> for CaptureError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        CaptureError::FromUtf8Error(err)
    }
}

#[cfg(target_os = "windows")]
impl From<windows_result::Error> for CaptureError {
    fn from(err: windows_result::Error) -> Self {
        CaptureError::WindowsResultError(backtrace::Backtrace::capture(), err)
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectError> for CaptureError {
    fn from(err: x11rb::errors::ConnectError) -> Self {
        CaptureError::X11ConnectError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectionError> for CaptureError {
    fn from(err: x11rb::errors::ConnectionError) -> Self {
        CaptureError::X11ConnectionError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<alsa::Error> for CaptureError {
    fn from(err: alsa::Error) -> Self {
        CaptureError::AlsaError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<pipewire::Error> for CaptureError {
    fn from(err: pipewire::Error) -> Self {
        CaptureError::PipewireError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<zbus::Error> for CaptureError {
    fn from(err: zbus::Error) -> Self {
        CaptureError::ZbusError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<zbus::zvariant::Error> for CaptureError {
    fn from(err: zbus::zvariant::Error) -> Self {
        CaptureError::ZbusZvariantError(err)
    }
}

impl From<anyhow::Error> for CaptureError {
    fn from(err: anyhow::Error) -> Self {
        CaptureError::AnyhowError(err)
    }
}

impl From<std::sync::mpsc::RecvTimeoutError> for CaptureError {
    fn from(err: std::sync::mpsc::RecvTimeoutError) -> Self {
        CaptureError::MpscRecvTimeoutError(err)
    }
}

impl From<std::sync::mpsc::RecvError> for CaptureError {
    fn from(err: std::sync::mpsc::RecvError) -> Self {
        CaptureError::MpscRecvError(err)
    }
}

impl std::error::Error for CaptureError {}
