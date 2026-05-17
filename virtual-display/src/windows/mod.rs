//! Windows virtual display backend.
//!
//! Layout:
//! - [`sw_device`] owns the PnP software device lifecycle via
//!   `SwDeviceCreate` / `SwDeviceClose` and maps the resulting device
//!   instance id to a GDI display name (`\\.\DISPLAYn`).
//! - [`pipe_client`] talks to the C++ UMDF driver over the named pipe
//!   defined by [`crate::PIPE_NAME`] using the length-prefixed JSON
//!   protocol declared in [`crate::DriverRequest`] / [`DriverResponse`].
//! - [`cds`] commits the negotiated mode into the user session via
//!   `ChangeDisplaySettingsExW`.
//!
//! `WindowsLifecycle` is held by the LocalSystem daemon and produces a
//! [`VirtualDisplayHandle`] keyed by the discovered display name.
//! `WindowsController` lives in the user-session worker and combines a
//! driver-pipe write with a CDS commit per `set_mode` call.

use crate::{
    VirtualDisplayController, VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayLifecycle,
    VirtualDisplayMode, validate_mode,
};

pub mod cds;
pub mod pipe_client;
pub mod sw_device;

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
        let (handle, display_name) = sw_device::create_virtual_display()?;
        Ok(VirtualDisplayHandle::new(display_name, Box::new(handle)))
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
        display_name: &str,
        mode: VirtualDisplayMode,
    ) -> Result<VirtualDisplayMode, VirtualDisplayError> {
        // Defense in depth: the daemon router already validates, but a
        // future direct caller (e.g. a debug CLI) might bypass it.
        validate_mode(mode)?;
        let applied = pipe_client::send_set_mode(mode)?;
        cds::apply_cds(display_name, applied)?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that exercises the full daemon-side `create` path.
    /// Ignored by default because it requires the lcxl IDD driver to be
    /// installed on the host. Run manually with:
    /// `cargo test -p desk-virtual-display --release -- --ignored windows_lifecycle_create_smoke`
    #[test]
    #[ignore = "requires lcxl IDD driver installed and signed (manual E2E only)"]
    fn windows_lifecycle_create_smoke() {
        let lifecycle = WindowsLifecycle::new();
        let handle = lifecycle
            .create()
            .expect("SwDeviceCreate should succeed when driver is installed");
        assert!(
            handle.display_name.starts_with(r"\\.\DISPLAY"),
            "expected GDI display name, got {:?}",
            handle.display_name
        );
        // Drop closes SwDevice; nothing else to assert here.
    }

    /// Smoke test for the worker-side controller. Requires the driver's
    /// JSON pipe server to be running and a virtual display name already
    /// known. Skip in CI.
    #[test]
    #[ignore = "requires running lcxl IDD driver + active virtual display"]
    fn windows_controller_set_mode_smoke() {
        let ctrl = WindowsController::new();
        // Use a placeholder display name; a real run would supply the
        // value returned by WindowsLifecycle::create above.
        let display = std::env::var("LCXL_VD_DISPLAY_NAME")
            .expect("set LCXL_VD_DISPLAY_NAME before running --ignored smoke test");
        let mode = VirtualDisplayMode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let applied = ctrl.set_mode(&display, mode).expect("set_mode succeeds");
        // Driver MAY snap to the closest supported mode; we don't assert
        // exact equality, just sanity bounds.
        assert!(applied.width >= 640);
        assert!(applied.height >= 480);
        assert!(applied.refresh_hz >= 24);
    }
}
