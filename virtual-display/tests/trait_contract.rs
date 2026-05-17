//! Cross-platform trait contract test. Uses an in-test mock provider so it
//! runs on Windows / Linux / macOS regardless of which factory the platform
//! cfg selects.

use desk_virtual_display::{
    VirtualDisplayController, VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayHandleInner,
    VirtualDisplayLifecycle, VirtualDisplayMode, lifecycle_provider, validate_mode,
};
#[cfg(not(target_os = "windows"))]
use desk_virtual_display::controller_provider;
use std::sync::Mutex;

struct MockHandleInner;
impl VirtualDisplayHandleInner for MockHandleInner {}

struct MockLifecycle {
    calls: Mutex<u32>,
}

impl VirtualDisplayLifecycle for MockLifecycle {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
        *self.calls.lock().unwrap() += 1;
        // After the Session-0 fix the handle carries a PnP instance id,
        // not a GDI display name. The mock mirrors the real Windows
        // format so the assertions in `mock_lifecycle_creates_handle`
        // remain meaningful as documentation of the post-fix contract.
        Ok(VirtualDisplayHandle::new(
            "SWD\\MOCK\\MOCK".into(),
            Box::new(MockHandleInner),
        ))
    }
}

struct MockController {
    last_mode: Mutex<Option<VirtualDisplayMode>>,
}

impl VirtualDisplayController for MockController {
    fn set_mode(
        &self,
        _display_name: &str,
        mode: VirtualDisplayMode,
    ) -> Result<VirtualDisplayMode, VirtualDisplayError> {
        validate_mode(mode)?;
        *self.last_mode.lock().unwrap() = Some(mode);
        Ok(mode)
    }
}

#[test]
fn mock_lifecycle_creates_handle() {
    let lc = MockLifecycle {
        calls: Mutex::new(0),
    };
    let handle = lc.create().expect("create");
    assert_eq!(handle.instance_id, "SWD\\MOCK\\MOCK");
    assert_eq!(*lc.calls.lock().unwrap(), 1);
}

#[test]
fn mock_controller_records_set_mode() {
    let ctrl = MockController {
        last_mode: Mutex::new(None),
    };
    let mode = VirtualDisplayMode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let applied = ctrl.set_mode("MOCK\\DISPLAY1", mode).expect("set_mode");
    assert_eq!(applied, mode);
    assert_eq!(ctrl.last_mode.lock().unwrap().as_ref(), Some(&mode));
}

#[test]
fn mock_controller_rejects_invalid_mode() {
    let ctrl = MockController {
        last_mode: Mutex::new(None),
    };
    let bad = VirtualDisplayMode {
        width: 0,
        height: 0,
        refresh_hz: 0,
    };
    assert!(matches!(
        ctrl.set_mode("MOCK\\DISPLAY1", bad),
        Err(VirtualDisplayError::InvalidMode(_))
    ));
}

/// Non-Windows platforms keep the permanent `unsupported` stub forever
/// — there is no IDD equivalent off Windows. The factory must return a
/// provider whose `create` / `set_mode` reject every call with
/// `NotSupported`. The Windows factory exercises real Win32 calls and
/// is covered by the `#[ignore]` smoke tests in `windows::tests`.
#[cfg(not(target_os = "windows"))]
#[test]
fn platform_factory_returns_not_supported_on_unsupported_platforms() {
    let lc = lifecycle_provider();
    let err = lc.create().expect_err("unsupported platform should not create");
    assert!(matches!(err, VirtualDisplayError::NotSupported));

    let ctrl = controller_provider();
    let mode = VirtualDisplayMode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let err = ctrl
        .set_mode("any", mode)
        .expect_err("unsupported platform should not set mode");
    assert!(matches!(err, VirtualDisplayError::NotSupported));
}

/// On Windows the factory MUST hand back the real production provider
/// rather than an inert stub. We can't reach the driver from CI, so we
/// only verify that the lifecycle attempt fails with `DeviceCreate`
/// (driver missing) — never with `NotSupported`, which would mean we
/// regressed to the phase-1 stub.
#[cfg(target_os = "windows")]
#[test]
fn platform_factory_returns_real_windows_provider() {
    let lc = lifecycle_provider();
    let err = lc
        .create()
        .expect_err("CI host has no lcxl IDD driver installed");
    assert!(
        !matches!(err, VirtualDisplayError::NotSupported),
        "Windows factory regressed to the phase-1 NotSupported stub: {err}"
    );
    // The only reasonable error on a stock CI host is DeviceCreate
    // (driver missing / no driver match). Anything else means the
    // lifecycle started doing more than it should before the driver
    // becomes available.
    assert!(
        matches!(err, VirtualDisplayError::DeviceCreate(_)),
        "expected DeviceCreate on driverless host, got: {err}"
    );
}
