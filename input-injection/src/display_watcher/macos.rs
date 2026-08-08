//! macOS display-change watcher backed by CoreGraphics.
//!
//! CoreGraphics may invoke the callback on a system-owned thread. The callback
//! therefore does no capture, encoder, or display enumeration work; it only
//! posts a monotonic notification to the worker's Tokio channel. The worker
//! performs geometry refresh and any media policy outside the callback.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

use super::error::DisplayWatcherError;

type DisplayReconfigurationCallback = unsafe extern "C" fn(u32, u32, *mut c_void);

const CG_ERROR_SUCCESS: i32 = 0;
const BEGIN_CONFIGURATION_FLAG: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayRegisterReconfigurationCallback(
        callback: DisplayReconfigurationCallback,
        user_info: *mut c_void,
    ) -> i32;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: DisplayReconfigurationCallback,
        user_info: *mut c_void,
    ) -> i32;
}

#[derive(Debug, Clone, Copy)]
pub struct DisplayChangeEvent {
    pub seq: u64,
}

struct CallbackContext {
    tx: mpsc::UnboundedSender<DisplayChangeEvent>,
    sequence: AtomicU64,
}

unsafe extern "C" fn display_reconfigured(_display: u32, flags: u32, user_info: *mut c_void) {
    if user_info.is_null() || flags & BEGIN_CONFIGURATION_FLAG != 0 {
        return;
    }
    // SAFETY: `user_info` points to the boxed context owned by the live
    // `DisplayChangeWatcher`. Drop unregisters this callback before reclaiming
    // the box, so registered callbacks always observe a valid context.
    let context = unsafe { &*(user_info.cast::<CallbackContext>()) };
    let seq = context.sequence.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = context.tx.send(DisplayChangeEvent { seq });
}

pub struct DisplayChangeWatcher {
    /// Stored as an integer so the watcher remains `Send` when held across the
    /// worker's async loop. It is converted back only for CoreGraphics and Drop.
    context_address: usize,
}

impl Drop for DisplayChangeWatcher {
    fn drop(&mut self) {
        let context = self.context_address as *mut CallbackContext;
        // SAFETY: this is the same callback/context pair registered by `spawn`.
        // CoreGraphics stops future delivery when removal returns; reclaim the
        // single Box allocation immediately afterwards.
        unsafe {
            let _ = CGDisplayRemoveReconfigurationCallback(
                display_reconfigured,
                context.cast::<c_void>(),
            );
            drop(Box::from_raw(context));
        }
    }
}

pub fn spawn() -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
> {
    let (tx, rx) = mpsc::unbounded_channel();
    let context = Box::into_raw(Box::new(CallbackContext {
        tx,
        sequence: AtomicU64::new(0),
    }));
    // SAFETY: `context` remains allocated until the returned watcher is dropped.
    let result = unsafe {
        CGDisplayRegisterReconfigurationCallback(display_reconfigured, context.cast::<c_void>())
    };
    if result != CG_ERROR_SUCCESS {
        // SAFETY: registration failed, so CoreGraphics retained no callback;
        // reclaim the allocation locally.
        unsafe { drop(Box::from_raw(context)) };
        return Err(DisplayWatcherError::MacRegistration(result));
    }
    Ok((
        DisplayChangeWatcher {
            context_address: context as usize,
        },
        rx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_ignores_begin_and_emits_completed_changes_monotonically() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut context = Box::new(CallbackContext {
            tx,
            sequence: AtomicU64::new(0),
        });
        let raw = (&mut *context as *mut CallbackContext).cast::<c_void>();

        unsafe { display_reconfigured(1, BEGIN_CONFIGURATION_FLAG, raw) };
        assert!(rx.try_recv().is_err());

        unsafe { display_reconfigured(1, 0, raw) };
        unsafe { display_reconfigured(2, 0, raw) };
        assert_eq!(rx.try_recv().expect("first event").seq, 1);
        assert_eq!(rx.try_recv().expect("second event").seq, 2);
    }
}
