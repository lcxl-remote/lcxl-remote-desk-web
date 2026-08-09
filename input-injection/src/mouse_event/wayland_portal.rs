use desk_wayland_portal::PortalInputSender;

use crate::{
    error::InputError,
    model::{
        data_channel::{MouseEventData, MouseEventHandler},
        geometry::SharedMonitorGeometry,
    },
};

pub struct WaylandPortalMouseEventHandler {
    portal: PortalInputSender,
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
    pub fn new(
        geometry: SharedMonitorGeometry,
        portal: PortalInputSender,
    ) -> Result<Self, InputError> {
        {
            let g = geometry.read().expect("monitor geometry poisoned");
            log::info!(
                "Wayland portal mouse handler: creating, width={}, height={}",
                g.width,
                g.height
            );
        }
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

/// Pure helper: clamp `(x, y) ∈ [0, 1]` into the stream's logical space.
/// Does **not** apply `left` / `top` — the portal stream pins the
/// output. Extracted so unit tests don't need a live D-Bus session.
fn to_absolute_in(width: i32, height: i32, x: f64, y: f64) -> (f64, f64) {
    let abs_x = scale_portal_coordinate(x, width);
    let abs_y = scale_portal_coordinate(y, height);
    (abs_x, abs_y)
}

fn scale_portal_coordinate(value: f64, extent: i32) -> f64 {
    let extent = extent.max(0);
    let last_valid = extent.saturating_sub(1).max(0) as f64;
    if value.is_finite() {
        (value * extent as f64).clamp(0.0, last_valid)
    } else {
        0.0
    }
}

impl MouseEventHandler for WaylandPortalMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let (x, y) = self.to_absolute(event.x, event.y);
        self.portal
            .notify_pointer_motion_absolute(x, y)
            .map_err(portal_input_error)
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let button = match event.button {
            0 => 0x110, // BTN_LEFT
            1 => 0x112, // BTN_MIDDLE
            2 => 0x111, // BTN_RIGHT
            _ => return Ok(()),
        };
        self.portal
            .notify_pointer_button(button, 1)
            .map_err(portal_input_error)
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let button = match event.button {
            0 => 0x110, // BTN_LEFT
            1 => 0x112, // BTN_MIDDLE
            2 => 0x111, // BTN_RIGHT
            _ => return Ok(()),
        };
        self.portal
            .notify_pointer_button(button, 0)
            .map_err(portal_input_error)
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        self.portal
            .notify_pointer_axis(event.delta_x, event.delta_y)
            .map_err(portal_input_error)
    }
}

fn portal_input_error(error: desk_wayland_portal::PortalError) -> InputError {
    InputError::new_custom_error(
        desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
        &error.to_string(),
    )
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
        // Portal coordinates use an exclusive upper bound.
        assert_eq!(to_absolute_in(1500, 900, 1.0, 1.0), (1499.0, 899.0));
        assert_eq!(to_absolute_in(1500, 900, 2.0, -0.5), (1499.0, 0.0));
    }

    #[test]
    fn to_absolute_sanitizes_degenerate_and_non_finite_input() {
        assert_eq!(to_absolute_in(0, -1, 1.0, 1.0), (0.0, 0.0));
        assert_eq!(
            to_absolute_in(1280, 800, f64::NAN, f64::INFINITY),
            (0.0, 0.0)
        );
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
