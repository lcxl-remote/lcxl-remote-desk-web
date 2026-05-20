use std::sync::Arc;

use crate::{
    error::InputError,
    model::{
        data_channel::{MouseEventData, MouseEventHandler},
        geometry::SharedMonitorGeometry,
    },
    service::wayland_remote_desktop::WaylandRemoteDesktop,
};

pub struct WaylandPortalMouseEventHandler {
    portal: Arc<WaylandRemoteDesktop>,
    /// Shared, hot-updatable monitor rect. Only `width` / `height` are
    /// consumed — the xdg-desktop-portal `NotifyPointerMotionAbsolute`
    /// call takes a `stream_id` that already pins the output and
    /// expresses `(x, y)` inside that stream's space, so applying
    /// `left` / `top` would double-shift the cursor.
    geometry: SharedMonitorGeometry,
}

impl WaylandPortalMouseEventHandler {
    /// `left` / `top` inside `geometry` are intentionally unused; see
    /// the field doc.
    pub fn new(geometry: SharedMonitorGeometry) -> Result<Self, InputError> {
        {
            let g = geometry.read().expect("monitor geometry poisoned");
            log::info!(
                "Wayland portal mouse handler: creating, width={}, height={}",
                g.width,
                g.height
            );
        }
        let portal = WaylandRemoteDesktop::shared()?;
        log::info!("Wayland portal mouse handler: ready");
        Ok(Self { portal, geometry })
    }

    fn to_absolute(&self, x: f64, y: f64) -> (f64, f64) {
        // Snapshot first so the D-Bus call below never runs with the
        // lock held.
        let (width, height) = {
            let g = self.geometry.read().expect("monitor geometry poisoned");
            (g.width, g.height)
        };
        to_absolute_in(width, height, x, y)
    }
}

/// Pure helper: clamp `(x, y) ∈ [0, 1]` into the stream's pixel space.
/// Does **not** apply `left` / `top` — the portal stream pins the
/// output. Extracted so unit tests don't need a live D-Bus session.
fn to_absolute_in(width: i32, height: i32, x: f64, y: f64) -> (f64, f64) {
    let abs_x = (x * width as f64).clamp(0.0, width as f64);
    let abs_y = (y * height as f64).clamp(0.0, height as f64);
    (abs_x, abs_y)
}

impl MouseEventHandler for WaylandPortalMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let (x, y) = self.to_absolute(event.x, event.y);
        self.portal.notify_pointer_motion_absolute(x, y)
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let button = match event.button {
            0 => 0x110, // BTN_LEFT
            1 => 0x112, // BTN_MIDDLE
            2 => 0x111, // BTN_RIGHT
            _ => return Ok(()),
        };
        self.portal.notify_pointer_button(button, 1)
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let button = match event.button {
            0 => 0x110, // BTN_LEFT
            1 => 0x112, // BTN_MIDDLE
            2 => 0x111, // BTN_RIGHT
            _ => return Ok(()),
        };
        self.portal.notify_pointer_button(button, 0)
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        self.portal
            .notify_pointer_axis(event.delta_x, event.delta_y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::geometry::{MonitorGeometry, shared};

    /// Smoke-test the per-stream coordinate mapping: even on a
    /// non-primary stream the `(x, y)` reported to the portal is
    /// expressed inside the stream's own `(width, height)`, so we
    /// deliberately do NOT add the `left` / `top` offset that Windows /
    /// macOS need. Guards against someone "fixing" the wayland backend
    /// by mirroring the Windows offset, which would double-shift the
    /// cursor.
    #[test]
    fn to_absolute_does_not_apply_virtual_desktop_offset() {
        assert_eq!(to_absolute_in(1500, 900, 0.5, 0.5), (750.0, 450.0));
        // Clamps over-the-edge inputs.
        assert_eq!(to_absolute_in(1500, 900, 2.0, -0.5), (1500.0, 0.0));
    }

    /// Hot-update path: even though the handler can't be instantiated
    /// without a live portal session, the arithmetic it executes reads
    /// the shared geometry on every call, so a worker-side mutation
    /// flows through. Also re-asserts that `left` / `top` from the
    /// updated geometry are still ignored (the portal stream pins the
    /// output).
    #[test]
    fn to_absolute_reflects_geometry_update_and_ignores_offset() {
        let geometry = shared(MonitorGeometry::new(0, 0, 1280, 800));
        let writer = std::sync::Arc::clone(&geometry);

        let (w, h) = {
            let g = geometry.read().unwrap();
            (g.width, g.height)
        };
        assert_eq!(to_absolute_in(w, h, 0.5, 0.5), (640.0, 400.0));

        // Worker-side write — note we also stuff a non-zero left/top to
        // verify they are still ignored after a hot update.
        *writer.write().unwrap() = MonitorGeometry::new(9999, 9999, 1500, 900);

        let (w, h) = {
            let g = geometry.read().unwrap();
            (g.width, g.height)
        };
        // 1500x900 → centre is (750, 450). The 9999 offsets do not
        // leak through, exactly as documented on the field.
        assert_eq!(to_absolute_in(w, h, 0.5, 0.5), (750.0, 450.0));
    }
}
