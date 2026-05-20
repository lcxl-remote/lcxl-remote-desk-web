//! # Display-change watcher
//!
//! Spawns a per-process OS listener that notifies the worker whenever
//! the display configuration changes (resolution change, monitor
//! add/remove, primary-display swap). The worker turns each event into
//! an `InputDispatcher::refresh_geometry(None)` call so the cursor
//! lands correctly without the WebRTC connection being torn down.
//!
//! ## Platform support
//!
//! - **Windows** (`windows.rs`): hidden top-level window receiving
//!   `WM_DISPLAYCHANGE` on a dedicated thread. **Not** an `HWND_MESSAGE`
//!   message-only window — broadcast messages (which `WM_DISPLAYCHANGE`
//!   is) are not delivered to message-only windows.
//! - **Non-Windows** (`stub.rs`): no-op. `spawn()` returns
//!   `Err(DisplayWatcherError::Unsupported)` so the caller can degrade
//!   gracefully. The Linux equivalent would be RandR / Wayland portal
//!   events; macOS would use `CGDisplayRegisterReconfigurationCallback`.
//!   Both are out of scope for the present PR.

mod error;

pub use error::DisplayWatcherError;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(target_os = "windows"))]
mod stub;

#[cfg(target_os = "windows")]
pub use self::windows::{DisplayChangeEvent, DisplayChangeWatcher, spawn};

#[cfg(not(target_os = "windows"))]
pub use self::stub::{DisplayChangeEvent, DisplayChangeWatcher, spawn};
