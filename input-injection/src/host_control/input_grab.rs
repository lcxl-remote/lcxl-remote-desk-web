//! Local physical input blocking via evdev `EVIOCGRAB`.
//!
//! Grabbing the physical keyboard / mouse devices gives this process
//! exclusive access, so the compositor (X11 or Wayland) stops receiving
//! their events while a remote session is in control. Our own injected
//! input flows through separate uinput virtual devices, which are
//! deliberately *not* grabbed — see [`is_injection_device`].
//!
//! `EVIOCGRAB` is per-device. To avoid leaving the machine in a
//! half-locked state, [`LocalInputBlocker::block`] is all-or-nothing:
//! any device that fails to grab triggers a rollback of the ones already
//! grabbed.

use std::sync::Mutex;

use desk_utils::error::DeskErrorCode;

use crate::error::InputError;
use crate::linux_display::{UINPUT_KEYBOARD_DEVICE_NAME, UINPUT_MOUSE_DEVICE_NAME};

/// A physical input device that is a candidate for grabbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabCandidate {
    /// Opaque id, stable within one grabber instance.
    pub id: u64,
    pub name: String,
}

/// Whether `name` is one of our own uinput injection virtual devices.
/// These must never be grabbed, or we would block the very input we
/// inject for the remote peer.
pub fn is_injection_device(name: &str) -> bool {
    name == UINPUT_KEYBOARD_DEVICE_NAME || name == UINPUT_MOUSE_DEVICE_NAME
}

/// From the discovered devices, the ids to grab: everything except our
/// own injection virtual devices.
pub fn select_grab_targets(devices: &[GrabCandidate]) -> Vec<u64> {
    devices
        .iter()
        .filter(|d| !is_injection_device(&d.name))
        .map(|d| d.id)
        .collect()
}

/// Seam over physical-device enumeration and grab/ungrab so the
/// all-or-nothing block logic is testable without touching `/dev/input`.
pub trait InputDeviceGrabber {
    /// Discover the grabbable physical input devices (injection devices
    /// already excluded).
    fn list(&self) -> Result<Vec<GrabCandidate>, InputError>;
    /// Grab the device with the given id.
    fn grab(&self, id: u64) -> Result<(), InputError>;
    /// Release a previously grabbed device.
    fn ungrab(&self, id: u64) -> Result<(), InputError>;
}

/// Tracks the set of currently grabbed devices and enforces the
/// all-or-nothing / idempotent semantics. Releasing on drop guarantees a
/// crashed worker cannot leave the local input permanently locked.
pub struct LocalInputBlocker {
    grabber: Box<dyn InputDeviceGrabber + Send>,
    grabbed: Vec<u64>,
}

impl LocalInputBlocker {
    pub fn new(grabber: Box<dyn InputDeviceGrabber + Send>) -> Self {
        Self {
            grabber,
            grabbed: Vec::new(),
        }
    }

    pub fn is_blocking(&self) -> bool {
        !self.grabbed.is_empty()
    }

    /// Grab every candidate device. Idempotent while already blocking.
    /// All-or-nothing: if any grab fails, every device grabbed so far in
    /// this call is released and the blocker stays unblocked.
    pub fn block(&mut self) -> Result<(), InputError> {
        if self.is_blocking() {
            return Ok(());
        }
        let candidates = self.grabber.list()?;
        if candidates.is_empty() {
            return InputError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "no grabbable physical input devices found",
            );
        }
        let mut grabbed = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            match self.grabber.grab(candidate.id) {
                Ok(()) => grabbed.push(candidate.id),
                Err(e) => {
                    for id in grabbed.iter().rev() {
                        let _ = self.grabber.ungrab(*id);
                    }
                    return Err(e);
                }
            }
        }
        self.grabbed = grabbed;
        Ok(())
    }

    /// Release all grabbed devices. Idempotent while already unblocked.
    /// Reports the first ungrab error but always attempts every device.
    pub fn unblock(&mut self) -> Result<(), InputError> {
        let mut first_err = None;
        for id in std::mem::take(&mut self.grabbed) {
            if let Err(e) = self.grabber.ungrab(id)
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for LocalInputBlocker {
    fn drop(&mut self) {
        for id in std::mem::take(&mut self.grabbed) {
            let _ = self.grabber.ungrab(id);
        }
    }
}

/// Production grabber backed by evdev. Opens every physical input device
/// once on [`list`](InputDeviceGrabber::list) and keeps the handles so
/// the grab persists until [`ungrab`](InputDeviceGrabber::ungrab) or the
/// grabber is dropped.
pub struct EvdevGrabber {
    devices: Mutex<Vec<EvdevSlot>>,
}

struct EvdevSlot {
    id: u64,
    name: String,
    device: evdev::Device,
}

impl EvdevGrabber {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
        }
    }
}

impl Default for EvdevGrabber {
    fn default() -> Self {
        Self::new()
    }
}

/// A device produces input we want to suppress if it reports any keys
/// (keyboards, mouse buttons) or pointer axes (relative or absolute).
fn looks_like_input_device(device: &evdev::Device) -> bool {
    device.supported_keys().is_some()
        || device.supported_relative_axes().is_some()
        || device.supported_absolute_axes().is_some()
}

impl InputDeviceGrabber for EvdevGrabber {
    fn list(&self) -> Result<Vec<GrabCandidate>, InputError> {
        let mut slots = self.devices.lock().expect("grabber mutex poisoned");
        slots.clear();
        let mut id: u64 = 0;
        for (_path, device) in evdev::enumerate() {
            if !looks_like_input_device(&device) {
                continue;
            }
            let name = device.name().unwrap_or("").to_string();
            if is_injection_device(&name) {
                continue;
            }
            slots.push(EvdevSlot { id, name, device });
            id += 1;
        }
        Ok(slots
            .iter()
            .map(|s| GrabCandidate {
                id: s.id,
                name: s.name.clone(),
            })
            .collect())
    }

    fn grab(&self, id: u64) -> Result<(), InputError> {
        let mut slots = self.devices.lock().expect("grabber mutex poisoned");
        let slot = slots.iter_mut().find(|s| s.id == id).ok_or_else(|| {
            InputError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "unknown device id")
        })?;
        slot.device.grab()?;
        Ok(())
    }

    fn ungrab(&self, id: u64) -> Result<(), InputError> {
        let mut slots = self.devices.lock().expect("grabber mutex poisoned");
        let slot = slots.iter_mut().find(|s| s.id == id).ok_or_else(|| {
            InputError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "unknown device id")
        })?;
        slot.device.ungrab()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn select_grab_targets_skips_injection_devices() {
        let devices = vec![
            GrabCandidate {
                id: 0,
                name: "AT Translated Set 2 keyboard".to_string(),
            },
            GrabCandidate {
                id: 1,
                name: UINPUT_KEYBOARD_DEVICE_NAME.to_string(),
            },
            GrabCandidate {
                id: 2,
                name: UINPUT_MOUSE_DEVICE_NAME.to_string(),
            },
            GrabCandidate {
                id: 3,
                name: "Logitech USB Mouse".to_string(),
            },
        ];
        assert_eq!(select_grab_targets(&devices), vec![0, 3]);
    }

    /// Records grab/ungrab calls and can fail a chosen device id.
    struct FakeGrabber {
        candidates: Vec<GrabCandidate>,
        fail_on_grab: Option<u64>,
        log: RefCell<Vec<String>>,
    }

    impl InputDeviceGrabber for FakeGrabber {
        fn list(&self) -> Result<Vec<GrabCandidate>, InputError> {
            Ok(self.candidates.clone())
        }
        fn grab(&self, id: u64) -> Result<(), InputError> {
            if Some(id) == self.fail_on_grab {
                self.log.borrow_mut().push(format!("grab-fail {id}"));
                return InputError::custom_error(DeskErrorCode::PERMISSION_ERROR, "busy");
            }
            self.log.borrow_mut().push(format!("grab {id}"));
            Ok(())
        }
        fn ungrab(&self, id: u64) -> Result<(), InputError> {
            self.log.borrow_mut().push(format!("ungrab {id}"));
            Ok(())
        }
    }

    fn candidates(ids: &[u64]) -> Vec<GrabCandidate> {
        ids.iter()
            .map(|&id| GrabCandidate {
                id,
                name: format!("dev{id}"),
            })
            .collect()
    }

    #[test]
    fn block_grabs_all_candidates() {
        let grabber = FakeGrabber {
            candidates: candidates(&[0, 1, 2]),
            fail_on_grab: None,
            log: RefCell::new(vec![]),
        };
        let mut blocker = LocalInputBlocker::new(Box::new(grabber));
        blocker.block().expect("block ok");
        assert!(blocker.is_blocking());
    }

    #[test]
    fn block_is_all_or_nothing_and_rolls_back_on_failure() {
        // Device 2 fails to grab — the blocker must report the error and
        // stay unblocked (rollback order is asserted separately below).
        let grabber = FakeGrabber {
            candidates: candidates(&[0, 1, 2]),
            fail_on_grab: Some(2),
            log: RefCell::new(vec![]),
        };
        let mut blocker = LocalInputBlocker::new(Box::new(grabber));
        let err = blocker.block().unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::PERMISSION_ERROR);
        assert!(
            !blocker.is_blocking(),
            "state stays unblocked after rollback"
        );
    }

    #[test]
    fn block_rollback_releases_already_grabbed_devices() {
        use std::sync::{Arc, Mutex};
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        struct SharedLogGrabber {
            candidates: Vec<GrabCandidate>,
            fail_on_grab: u64,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl InputDeviceGrabber for SharedLogGrabber {
            fn list(&self) -> Result<Vec<GrabCandidate>, InputError> {
                Ok(self.candidates.clone())
            }
            fn grab(&self, id: u64) -> Result<(), InputError> {
                if id == self.fail_on_grab {
                    return InputError::custom_error(DeskErrorCode::PERMISSION_ERROR, "busy");
                }
                self.log.lock().unwrap().push(format!("grab {id}"));
                Ok(())
            }
            fn ungrab(&self, id: u64) -> Result<(), InputError> {
                self.log.lock().unwrap().push(format!("ungrab {id}"));
                Ok(())
            }
        }
        let grabber = SharedLogGrabber {
            candidates: candidates(&[0, 1, 2]),
            fail_on_grab: 2,
            log: log.clone(),
        };
        let mut blocker = LocalInputBlocker::new(Box::new(grabber));
        let _ = blocker.block();
        // Grabbed 0 then 1, then 2 failed -> rollback ungrabs 1 then 0.
        assert_eq!(
            *log.lock().unwrap(),
            vec!["grab 0", "grab 1", "ungrab 1", "ungrab 0"]
        );
    }

    #[test]
    fn block_is_idempotent() {
        let grabber = FakeGrabber {
            candidates: candidates(&[0, 1]),
            fail_on_grab: None,
            log: RefCell::new(vec![]),
        };
        let mut blocker = LocalInputBlocker::new(Box::new(grabber));
        blocker.block().expect("first block");
        blocker.block().expect("second block is a no-op");
        assert!(blocker.is_blocking());
    }

    #[test]
    fn block_errors_when_no_devices() {
        let grabber = FakeGrabber {
            candidates: vec![],
            fail_on_grab: None,
            log: RefCell::new(vec![]),
        };
        let mut blocker = LocalInputBlocker::new(Box::new(grabber));
        let err = blocker.block().unwrap_err();
        assert_eq!(err.to_error_code(), DeskErrorCode::SYSTEM_ERROR);
    }

    #[test]
    fn unblock_releases_and_resets_state() {
        let grabber = FakeGrabber {
            candidates: candidates(&[0, 1]),
            fail_on_grab: None,
            log: RefCell::new(vec![]),
        };
        let mut blocker = LocalInputBlocker::new(Box::new(grabber));
        blocker.block().expect("block");
        blocker.unblock().expect("unblock");
        assert!(!blocker.is_blocking());
    }
}
