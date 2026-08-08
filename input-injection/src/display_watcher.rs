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
//! - **Linux** (`linux/`): dispatches by session type at runtime — X11
//!   listens for RandR screen / CRTC / output changes on the root
//!   window; Wayland listens on the core `wl_registry` / `wl_output`
//!   protocol (output add/remove, mode, geometry, scale); a headless
//!   session (no `DISPLAY` / `WAYLAND_DISPLAY`) returns `Unsupported`.
//! - **macOS** (`macos.rs`): CoreGraphics display-reconfiguration callback.
//! - **Other platforms** (`stub.rs`): no-op. `spawn()` returns
//!   `Err(DisplayWatcherError::Unsupported)` so the caller can degrade
//!   gracefully.

mod error;

pub use error::DisplayWatcherError;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod stub;

#[cfg(target_os = "windows")]
pub use self::windows::{DisplayChangeEvent, DisplayChangeWatcher, spawn};

#[cfg(target_os = "linux")]
pub use self::linux::{DisplayChangeEvent, DisplayChangeWatcher, spawn};

#[cfg(target_os = "macos")]
pub use self::macos::{DisplayChangeEvent, DisplayChangeWatcher, spawn};

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub use self::stub::{DisplayChangeEvent, DisplayChangeWatcher, spawn};
