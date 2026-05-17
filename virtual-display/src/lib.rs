//! Virtual display abstraction.
//!
//! Defines a platform-agnostic contract for creating and controlling
//! virtual monitors. The Windows backend (phase 2) drives a Microsoft
//! Indirect Display Driver (IDD) via a named-pipe control channel; other
//! platforms fulfil the contract with a permanent `NotSupported` stub.
//!
//! Phase 1 ships only the protocol skeleton — every platform impl is a
//! stub returning `NotSupported`. Phase 2 replaces the Windows stub
//! with the real `SwDeviceCreate` + driver-pipe + `ChangeDisplaySettings`
//! pipeline.

use serde::{Deserialize, Serialize};

#[cfg(not(target_os = "windows"))]
pub mod unsupported;
#[cfg(target_os = "windows")]
pub mod windows;

/// Lifecycle owner: held by the LocalSystem daemon. Creating a handle
/// allocates the OS-level virtual monitor resource; dropping the handle
/// releases it.
pub trait VirtualDisplayLifecycle: Send + Sync {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError>;
}

/// Opaque platform-specific payload behind [`VirtualDisplayHandle`].
/// `Drop` releases the underlying OS resource.
pub trait VirtualDisplayHandleInner: Send + Sync {}

/// Owned reference to a live virtual monitor. The handle keeps the OS
/// resource alive for as long as it lives.
pub struct VirtualDisplayHandle {
    pub display_name: String,
    #[allow(dead_code)]
    inner: Box<dyn VirtualDisplayHandleInner>,
}

impl VirtualDisplayHandle {
    /// Public constructor so platform implementations (including future
    /// out-of-crate impls) can build a handle.
    pub fn new(display_name: String, inner: Box<dyn VirtualDisplayHandleInner>) -> Self {
        Self {
            display_name,
            inner,
        }
    }
}

impl std::fmt::Debug for VirtualDisplayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualDisplayHandle")
            .field("display_name", &self.display_name)
            .finish_non_exhaustive()
    }
}

/// Runtime controller: lives in the user-session worker. Pushes a new
/// mode through the driver pipe and commits it via the windowing system.
pub trait VirtualDisplayController: Send + Sync {
    fn set_mode(
        &self,
        display_name: &str,
        mode: VirtualDisplayMode,
    ) -> Result<VirtualDisplayMode, VirtualDisplayError>;
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct VirtualDisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum VirtualDisplayError {
    #[error("platform not supported")]
    NotSupported,
    #[error("invalid mode: {0}")]
    InvalidMode(String),
    #[error("driver pipe IO failed: {0}")]
    PipeIo(String),
    #[error("driver returned NTSTATUS={0:#x}")]
    DriverFailed(u32),
    #[error("ChangeDisplaySettings failed: {0}")]
    Cds(String),
    #[error("SwDeviceCreate failed: {0}")]
    DeviceCreate(String),
    #[error("not attached")]
    NotAttached,
}

const MIN_DIMENSION: u32 = 640;
const MAX_DIMENSION: u32 = 7680;
const ALIGNMENT: u32 = 8;
const ALLOWED_REFRESH: &[u32] = &[24, 30, 50, 60, 75, 90, 120, 144, 165, 240];

/// Validate a requested mode against the conservative bounds the daemon
/// is willing to forward to the driver. Defense-in-depth: the router
/// calls this before sending the IPC, and platform controllers re-check
/// before issuing the driver request.
pub fn validate_mode(mode: VirtualDisplayMode) -> Result<(), VirtualDisplayError> {
    if mode.width == 0 || mode.height == 0 || mode.refresh_hz == 0 {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "zero dimension: width={} height={} refresh_hz={}",
            mode.width, mode.height, mode.refresh_hz
        )));
    }
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&mode.width) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "width {} out of range [{MIN_DIMENSION}, {MAX_DIMENSION}]",
            mode.width
        )));
    }
    if !(MIN_DIMENSION..=MAX_DIMENSION).contains(&mode.height) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "height {} out of range [{MIN_DIMENSION}, {MAX_DIMENSION}]",
            mode.height
        )));
    }
    if !mode.width.is_multiple_of(ALIGNMENT) || !mode.height.is_multiple_of(ALIGNMENT) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "width/height must be a multiple of {ALIGNMENT} (got {}x{})",
            mode.width, mode.height
        )));
    }
    if !ALLOWED_REFRESH.contains(&mode.refresh_hz) {
        return Err(VirtualDisplayError::InvalidMode(format!(
            "refresh_hz {} not in allowed set {ALLOWED_REFRESH:?}",
            mode.refresh_hz
        )));
    }
    Ok(())
}

/// Named pipe path of the driver's control endpoint. Shared between the
/// Rust controller (client) and the C++ driver (server).
pub const PIPE_NAME: &str = r"\\.\pipe\LcxlVirtualDisplay";

pub fn lifecycle_provider() -> Box<dyn VirtualDisplayLifecycle> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsLifecycle::new())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(unsupported::UnsupportedLifecycle)
    }
}

pub fn controller_provider() -> Box<dyn VirtualDisplayController> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsController::new())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(unsupported::UnsupportedController)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_mode_accepts_common_resolutions() {
        for (w, h, r) in [
            (640, 640, 60),
            (1280, 720, 60),
            (1920, 1080, 60),
            (1920, 1080, 144),
            (2560, 1440, 165),
            (3840, 2160, 60),
            (7680, 4320, 60),
        ] {
            validate_mode(VirtualDisplayMode {
                width: w,
                height: h,
                refresh_hz: r,
            })
            .unwrap_or_else(|e| panic!("expected valid {w}x{h}@{r}: {e}"));
        }
    }

    #[test]
    fn validate_mode_rejects_zero() {
        for m in [
            VirtualDisplayMode {
                width: 0,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 0,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 0,
            },
        ] {
            assert!(matches!(
                validate_mode(m),
                Err(VirtualDisplayError::InvalidMode(_))
            ));
        }
    }

    #[test]
    fn validate_mode_rejects_out_of_range() {
        for m in [
            VirtualDisplayMode {
                width: 320,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 8000,
                height: 720,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 320,
                refresh_hz: 60,
            },
            VirtualDisplayMode {
                width: 1280,
                height: 7688,
                refresh_hz: 60,
            },
        ] {
            assert!(
                matches!(validate_mode(m), Err(VirtualDisplayError::InvalidMode(_))),
                "expected out-of-range error for {m:?}"
            );
        }
    }

    #[test]
    fn validate_mode_rejects_odd_refresh() {
        let m = VirtualDisplayMode {
            width: 1280,
            height: 720,
            refresh_hz: 100,
        };
        assert!(matches!(
            validate_mode(m),
            Err(VirtualDisplayError::InvalidMode(_))
        ));
    }

    #[test]
    fn validate_mode_rejects_unaligned_dimensions() {
        let m = VirtualDisplayMode {
            width: 1281,
            height: 720,
            refresh_hz: 60,
        };
        assert!(matches!(
            validate_mode(m),
            Err(VirtualDisplayError::InvalidMode(_))
        ));
    }

    #[test]
    fn pipe_name_constant_unchanged() {
        assert_eq!(PIPE_NAME, r"\\.\pipe\LcxlVirtualDisplay");
    }

    #[test]
    fn mode_serde_roundtrip() {
        let m = VirtualDisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let j = serde_json::to_string(&m).unwrap();
        let back: VirtualDisplayMode = serde_json::from_str(&j).unwrap();
        assert_eq!(m, back);
    }
}
