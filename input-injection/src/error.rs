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
    /// A zbus (D-Bus) error occurred (Wayland portal).
    #[cfg(target_os = "linux")]
    ZbusError(zbus::Error),
    /// A zbus zvariant error occurred (Wayland portal).
    #[cfg(target_os = "linux")]
    ZbusZvariantError(zbus::zvariant::Error),
    /// Failed to connect to the X11 display server.
    #[cfg(target_os = "linux")]
    X11ConnectError(x11rb::errors::ConnectError),
    /// An X11 request failed to be sent / the connection broke.
    #[cfg(target_os = "linux")]
    X11ConnectionError(x11rb::errors::ConnectionError),
    /// An X11 request returned an error reply (e.g. RandR / DPMS).
    #[cfg(target_os = "linux")]
    X11ReplyError(x11rb::errors::ReplyError),
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
            InputError::custom_error(
                DeskErrorCode::WINDOWS_ERROR,
                &format!("windows error code: {:?}", last_error),
            )
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
            #[cfg(target_os = "linux")]
            InputError::ZbusError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            InputError::ZbusZvariantError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            InputError::X11ConnectError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            InputError::X11ConnectionError(error) => error.fmt(f),
            #[cfg(target_os = "linux")]
            InputError::X11ReplyError(error) => error.fmt(f),
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

#[cfg(target_os = "linux")]
impl From<zbus::Error> for InputError {
    fn from(err: zbus::Error) -> Self {
        InputError::ZbusError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<zbus::zvariant::Error> for InputError {
    fn from(err: zbus::zvariant::Error) -> Self {
        InputError::ZbusZvariantError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectError> for InputError {
    fn from(err: x11rb::errors::ConnectError) -> Self {
        InputError::X11ConnectError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ConnectionError> for InputError {
    fn from(err: x11rb::errors::ConnectionError) -> Self {
        InputError::X11ConnectionError(err)
    }
}

#[cfg(target_os = "linux")]
impl From<x11rb::errors::ReplyError> for InputError {
    fn from(err: x11rb::errors::ReplyError) -> Self {
        InputError::X11ReplyError(err)
    }
}

/// Bridge capture-engine errors raised by the shared Wayland portal
/// helpers (`pipewire_utils`) into the input-injection error type,
/// preserving the original [`DeskErrorCode`].
#[cfg(target_os = "linux")]
impl From<desk_capture_engine::error::CaptureError> for InputError {
    fn from(err: desk_capture_engine::error::CaptureError) -> Self {
        InputError::new_custom_error(err.to_error_code(), &err.to_string())
    }
}

impl std::error::Error for InputError {}

#[cfg(test)]
mod arboard_tests {
    use super::*;

    /// An `arboard::Error` raised by the clipboard host-control paths must
    /// convert into `InputError::ArboardError` (capturing a backtrace) so the
    /// `?` propagation in `mac_host_control` keeps the original clipboard
    /// failure text. As a non-custom error it falls back to `SYSTEM_ERROR`.
    #[test]
    fn arboard_error_maps_to_arboard_variant() {
        let err: InputError = arboard::Error::ContentNotAvailable.into();
        assert!(matches!(err, InputError::ArboardError(_, _)));
        assert_eq!(err.to_error_code(), DeskErrorCode::SYSTEM_ERROR);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A raw zbus error must surface as `InputError::ZbusError` so the
    /// Wayland portal `?` propagation in `service::wayland_remote_desktop`
    /// keeps the original D-Bus failure text.
    #[test]
    fn zbus_error_maps_to_zbus_variant() {
        let err: InputError = zbus::Error::Failure("boom".to_owned()).into();
        assert!(matches!(err, InputError::ZbusError(_)));
        assert!(err.to_string().contains("boom"));
        // Non-custom errors fall back to SYSTEM_ERROR.
        assert_eq!(err.to_error_code(), DeskErrorCode::SYSTEM_ERROR);
    }

    /// A failed `OwnedObjectPath::try_from` raises a zvariant error, which
    /// the portal session-handle construction propagates via `?`.
    #[test]
    fn zvariant_error_maps_to_zvariant_variant() {
        let err: InputError = zbus::zvariant::OwnedObjectPath::try_from("not a path")
            .expect_err("invalid object path must error")
            .into();
        assert!(matches!(err, InputError::ZbusZvariantError(_)));
    }

    /// An X11 connection error (raised by the RandR / DPMS host-control
    /// paths) maps to `X11ConnectionError` and falls back to
    /// `SYSTEM_ERROR` as a non-custom error.
    #[test]
    fn x11_connection_error_maps_to_x11_variant() {
        let err: InputError = x11rb::errors::ConnectionError::UnknownError.into();
        assert!(matches!(err, InputError::X11ConnectionError(_)));
        assert_eq!(err.to_error_code(), DeskErrorCode::SYSTEM_ERROR);
    }

    /// Capture-engine errors raised by the shared `pipewire_utils` helpers
    /// must keep their original [`DeskErrorCode`] after bridging into
    /// `InputError`, so portal probes report the right code upstream.
    #[test]
    fn capture_error_bridge_preserves_error_code() {
        let capture_err = desk_capture_engine::error::CaptureError::new_custom_error(
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "portal missing",
        );
        let err: InputError = capture_err.into();
        assert_eq!(err.to_error_code(), DeskErrorCode::FEATURE_UNAVAILABLE);
        assert!(err.to_string().contains("portal missing"));
    }
}
