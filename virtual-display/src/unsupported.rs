//! Permanent Linux/macOS stub. The virtual display feature is Windows-only;
//! this module keeps cross-platform builds compiling without pulling in
//! OS-specific dependencies.

use crate::{
    VirtualDisplayController, VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayLifecycle,
    VirtualDisplayMode,
};

pub struct UnsupportedLifecycle;

impl VirtualDisplayLifecycle for UnsupportedLifecycle {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
        Err(VirtualDisplayError::NotSupported)
    }
}

pub struct UnsupportedController;

impl VirtualDisplayController for UnsupportedController {
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
        let lc = UnsupportedLifecycle;
        assert!(matches!(
            lc.create(),
            Err(VirtualDisplayError::NotSupported)
        ));
    }

    #[test]
    fn controller_stub_returns_not_supported() {
        let ctrl = UnsupportedController;
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
