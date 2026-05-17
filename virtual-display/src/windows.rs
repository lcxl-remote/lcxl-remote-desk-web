//! Phase 1 Windows stub. Returns `NotSupported` for every call so the
//! protocol skeleton compiles and exercises its full code path against
//! an inert provider. Phase 2 replaces this module with submodules
//! (`sw_device`, `pipe_client`, `cds`) implementing the real IDD pipeline.

use crate::{
    VirtualDisplayController, VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayLifecycle,
    VirtualDisplayMode,
};

pub struct WindowsLifecycle;

impl WindowsLifecycle {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualDisplayLifecycle for WindowsLifecycle {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
        Err(VirtualDisplayError::NotSupported)
    }
}

pub struct WindowsController;

impl WindowsController {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsController {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualDisplayController for WindowsController {
    fn set_mode(
        &self,
        _display_name: &str,
        _mode: VirtualDisplayMode,
    ) -> Result<VirtualDisplayMode, VirtualDisplayError> {
        Err(VirtualDisplayError::NotSupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_stub_returns_not_supported() {
        let lc = WindowsLifecycle::new();
        assert!(matches!(
            lc.create(),
            Err(VirtualDisplayError::NotSupported)
        ));
    }

    #[test]
    fn controller_stub_returns_not_supported() {
        let ctrl = WindowsController::new();
        let mode = VirtualDisplayMode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        assert!(matches!(
            ctrl.set_mode("dummy", mode),
            Err(VirtualDisplayError::NotSupported)
        ));
    }
}
