mod broker;
mod input;
mod model;
mod token_store;

#[cfg(target_os = "linux")]
mod xdg;

pub use broker::{LivePortalSession, PortalBackend, PreparedPortalSession, WaylandPortalBroker};
pub use input::PortalInputSender;
pub use model::*;
pub use token_store::{RestoreToken, RestoreTokenStore};

#[cfg(target_os = "linux")]
pub use xdg::XdgPortalBackend;

#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    #[error("Wayland Portal authorization is required")]
    AuthorizationRequired,
    #[error("Wayland Portal operation was cancelled")]
    Cancelled,
    #[error("Wayland Portal is unsupported: {0}")]
    Unsupported(String),
    #[error("invalid operation id")]
    InvalidOperationId,
    #[error("invalid Wayland Portal restore token store")]
    InvalidTokenStore,
    #[error("Wayland Portal returned an invalid session: {0}")]
    InvalidSession(String),
    #[error("Wayland Portal did not grant both keyboard and pointer input")]
    InputDevicesNotGranted,
    #[error("Wayland Portal backend error: {0}")]
    Backend(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Zbus(#[from] zbus::Error),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Zvariant(#[from] zbus::zvariant::Error),
}

impl PortalError {
    pub const fn user_code(&self) -> desk_utils::error::DeskErrorCode {
        use desk_utils::error::DeskErrorCode;

        match self {
            Self::AuthorizationRequired => DeskErrorCode::WAYLAND_PORTAL_AUTHORIZATION_REQUIRED,
            Self::Cancelled => DeskErrorCode::WAYLAND_PORTAL_AUTHORIZATION_CANCELLED,
            Self::Unsupported(_) => DeskErrorCode::FEATURE_UNAVAILABLE,
            Self::InvalidOperationId => DeskErrorCode::INVALID_PARAMS,
            Self::InputDevicesNotGranted => DeskErrorCode::WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED,
            Self::InvalidTokenStore
            | Self::InvalidSession(_)
            | Self::Backend(_)
            | Self::Io(_)
            | Self::Json(_) => DeskErrorCode::WAYLAND_PORTAL_BACKEND_FAILED,
            #[cfg(target_os = "linux")]
            Self::Zbus(_) | Self::Zvariant(_) => DeskErrorCode::WAYLAND_PORTAL_BACKEND_FAILED,
        }
    }

    pub fn user_reason(&self) -> String {
        match self {
            Self::AuthorizationRequired => "Authorization is required on the host".into(),
            Self::Cancelled => "Authorization was cancelled".into(),
            Self::Unsupported(reason) | Self::InvalidSession(reason) | Self::Backend(reason) => {
                reason.clone()
            }
            Self::InputDevicesNotGranted => {
                "Portal did not grant both keyboard and pointer input".into()
            }
            Self::InvalidOperationId => "The authorization operation id is invalid".into(),
            Self::InvalidTokenStore => "The saved authorization could not be restored".into(),
            Self::Io(error) => format!("Local authorization state error: {error}"),
            Self::Json(_) => "The saved authorization state is invalid".into(),
            #[cfg(target_os = "linux")]
            Self::Zbus(error) => format!("Desktop Portal communication failed: {error}"),
            #[cfg(target_os = "linux")]
            Self::Zvariant(error) => format!("Desktop Portal response was invalid: {error}"),
        }
    }
}
