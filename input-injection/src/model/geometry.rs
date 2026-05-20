//! Shared, hot-updatable captured-monitor geometry for mouse handlers.
//!
//! Mouse handlers used to bake `(left, top, width, height)` of the
//! captured monitor into immutable struct fields at construction time.
//! Display reconfiguration mid-session (IDD resize, monitor add/remove,
//! resolution change) was handled by killing the connection — the
//! browser would re-establish and we'd re-query.
//!
//! That contract breaks for the upcoming pixel-perfect IDD resize flow,
//! which is a frequent event. The handlers now hold a
//! [`SharedMonitorGeometry`] instead — a small `Arc<RwLock<...>>` that
//! the worker mutates in place on `WM_DISPLAYCHANGE`, virtual display
//! `SetMode`/Attach/Detach, etc. — and read it on every injection.
//!
//! ## Why `std::sync::RwLock`
//!
//! `MouseEventHandler::handle_mouse_*` is a sync trait. The handler
//! cannot `.await` across the lock, so a `tokio::sync::RwLock` is
//! awkward. Writes are very short (four `i32`s) and very rare; reads
//! happen on every mouse event but are uncontended in practice
//! (single-writer, single-reader-per-connection).

use std::sync::{Arc, RwLock};

/// Captured monitor rectangle in virtual desktop coordinate space.
///
/// `left`/`top` may be negative (a secondary monitor dragged left of
/// the primary in Windows Display Settings) — the math in
/// `WindowsMouseEventHandler::compute_absolute` accepts that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorGeometry {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl MonitorGeometry {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }

    /// Last-resort default when no display can be enumerated (headless
    /// CI, capture-engine failure during initialisation). 1920x1080 at
    /// the origin keeps handler construction infallible and matches the
    /// fallback used by `display_geometry_for_device`.
    pub const FALLBACK: Self = Self::new(0, 0, 1920, 1080);
}

impl Default for MonitorGeometry {
    fn default() -> Self {
        Self::FALLBACK
    }
}

/// Reference-counted, hot-updatable handle to a [`MonitorGeometry`].
/// Cloned freely by the mouse handler and any geometry refresher; the
/// underlying `RwLock` serialises the rare writes against the frequent
/// reads.
pub type SharedMonitorGeometry = Arc<RwLock<MonitorGeometry>>;

/// Convenience constructor — `Arc::new(RwLock::new(g))` shows up a lot.
pub fn shared(g: MonitorGeometry) -> SharedMonitorGeometry {
    Arc::new(RwLock::new(g))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writing through one `SharedMonitorGeometry` handle must be visible
    /// to all other clones — this is the entire point of the shared
    /// wrapper. A handler holding one clone reads new values written by
    /// the worker holding another clone.
    #[test]
    fn shared_geometry_propagates_writes_across_clones() {
        let g = shared(MonitorGeometry::new(0, 0, 1280, 800));
        let clone_for_reader = Arc::clone(&g);
        *g.write().unwrap() = MonitorGeometry::new(1280, 0, 1500, 900);
        let read = *clone_for_reader.read().unwrap();
        assert_eq!(read, MonitorGeometry::new(1280, 0, 1500, 900));
    }

    /// FALLBACK is the explicit no-display sentinel — match the value
    /// the existing `display_geometry_for_device` returns when the
    /// capture engine enumerates nothing.
    #[test]
    fn fallback_is_origin_anchored_full_hd() {
        assert_eq!(
            MonitorGeometry::FALLBACK,
            MonitorGeometry::new(0, 0, 1920, 1080)
        );
    }
}
