//! Cross-platform trait contract test. Uses an in-test mock provider so it
//! runs on Windows / Linux / macOS regardless of which factory the platform
//! cfg selects.

use desk_virtual_display::{
    VirtualDisplayController, VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayHandleInner,
    VirtualDisplayLifecycle, VirtualDisplayMode, controller_provider, lifecycle_provider,
    validate_mode,
};
use std::sync::Mutex;

struct MockHandleInner;
impl VirtualDisplayHandleInner for MockHandleInner {}

struct MockLifecycle {
    calls: Mutex<u32>,
}

impl VirtualDisplayLifecycle for MockLifecycle {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
        *self.calls.lock().unwrap() += 1;
        Ok(VirtualDisplayHandle::new(
            "MOCK\\DISPLAY1".into(),
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
    assert_eq!(handle.display_name, "MOCK\\DISPLAY1");
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

#[test]
fn platform_factory_returns_not_supported_in_phase_one() {
    // Phase 1: every platform factory returns an inert provider whose
    // create() / set_mode() respond with NotSupported. This guarantees
    // the protocol skeleton is exercised end-to-end without the real
    // driver pipeline being live.
    let lc = lifecycle_provider();
    let err = lc.create().expect_err("phase 1 should not create");
    assert!(matches!(err, VirtualDisplayError::NotSupported));

    let ctrl = controller_provider();
    let mode = VirtualDisplayMode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let err = ctrl
        .set_mode("any", mode)
        .expect_err("phase 1 should not set mode");
    assert!(matches!(err, VirtualDisplayError::NotSupported));
}
