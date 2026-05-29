//! Platform host-control services backing the input-injection handlers.
//!
//! Currently only the Linux Wayland `RemoteDesktop` portal client lives
//! here; it is the single source of truth shared by both the Wayland
//! portal keyboard and mouse handlers (`keyboard_event::wayland_portal`
//! and `mouse_event::wayland_portal`). The `server` crate re-uses the
//! same type for its portal health-probe.
pub mod wayland_remote_desktop;
