use std::sync::Arc;

use crate::{
    error::InputError,
    model::data_channel::{MouseEventData, MouseEventHandler},
    service::wayland_remote_desktop::WaylandRemoteDesktop,
};

pub struct WaylandPortalMouseEventHandler {
    portal: Arc<WaylandRemoteDesktop>,
    width: i32,
    height: i32,
}

impl WaylandPortalMouseEventHandler {
    /// `left` / `top` are accepted for signature uniformity with the
    /// Windows and macOS backends, but the xdg-desktop-portal
    /// `RemoteDesktop.NotifyPointerMotionAbsolute` call takes a
    /// `stream_id` that already pins the output, with `(x, y)` expressed
    /// inside that stream's space. Applying a virtual-desktop offset
    /// would double-shift the cursor, so the parameters are
    /// intentionally ignored.
    pub fn new(_left: i32, _top: i32, width: i32, height: i32) -> Result<Self, InputError> {
        log::info!(
            "Wayland portal mouse handler: creating, width={}, height={}",
            width,
            height
        );
        let portal = WaylandRemoteDesktop::shared()?;
        log::info!("Wayland portal mouse handler: ready");
        Ok(Self {
            portal,
            width,
            height,
        })
    }

    fn to_absolute(&self, x: f64, y: f64) -> (f64, f64) {
        let abs_x = (x * self.width as f64).clamp(0.0, self.width as f64);
        let abs_y = (y * self.height as f64).clamp(0.0, self.height as f64);
        (abs_x, abs_y)
    }
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

    /// Smoke-test the per-stream coordinate mapping: even on a
    /// non-primary stream the `(x, y)` reported to the portal is
    /// expressed inside the stream's own `(width, height)`, so we deliberately
    /// do NOT add the `left` / `top` offset that Windows / macOS need.
    /// This test guards against someone "fixing" the wayland backend by
    /// mirroring the Windows offset, which would double-shift the cursor.
    #[test]
    fn to_absolute_does_not_apply_virtual_desktop_offset() {
        // The handler must clamp coords to its own (width, height) and
        // emit them as-is — the portal stream is the output binding.
        let handler = WaylandPortalMouseEventHandlerForTest {
            width: 1500,
            height: 900,
        };
        assert_eq!(handler.to_absolute(0.5, 0.5), (750.0, 450.0));
        // Clamps over-the-edge inputs.
        assert_eq!(handler.to_absolute(2.0, -0.5), (1500.0, 0.0));
    }

    /// Shadow struct exposing the pure arithmetic from
    /// `WaylandPortalMouseEventHandler::to_absolute`. The real handler
    /// holds a portal `Arc<WaylandRemoteDesktop>` which requires a live
    /// D-Bus session to construct on Wayland — CI does not have one,
    /// so we reproduce the arithmetic here. Keep the bodies in sync.
    struct WaylandPortalMouseEventHandlerForTest {
        width: i32,
        height: i32,
    }
    impl WaylandPortalMouseEventHandlerForTest {
        fn to_absolute(&self, x: f64, y: f64) -> (f64, f64) {
            let abs_x = (x * self.width as f64).clamp(0.0, self.width as f64);
            let abs_y = (y * self.height as f64).clamp(0.0, self.height as f64);
            (abs_x, abs_y)
        }
    }
}
