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

// ───── Exclusive-mode stubs ─────
//
// Mirrors the Windows-side types so cross-platform builds compile and
// daemon-level logic can name the types without `cfg`-gating. The
// helper functions all return `NotSupported` since detaching physical
// displays is meaningless on Linux/macOS hosts (the worker that drives
// CDS is Windows-only).

/// Stand-in for the Windows snapshot. Carries an opaque
/// `device_name` so debug prints look sensible.
#[derive(Debug, Clone)]
pub struct PhysicalDisplaySnapshot {
    pub device_name: String,
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct ExclusiveLayout {
    pub physical_snapshots: Vec<PhysicalDisplaySnapshot>,
    pub virtual_snapshot: PhysicalDisplaySnapshot,
}

pub fn snapshot_layout(_virtual_display_name: &str) -> Result<ExclusiveLayout, VirtualDisplayError> {
    Err(VirtualDisplayError::NotSupported)
}

pub fn enter_exclusive(_layout: &ExclusiveLayout) -> Result<(), VirtualDisplayError> {
    Err(VirtualDisplayError::NotSupported)
}

pub fn leave_exclusive(_layout: &ExclusiveLayout) -> Result<(), VirtualDisplayError> {
    Err(VirtualDisplayError::NotSupported)
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
