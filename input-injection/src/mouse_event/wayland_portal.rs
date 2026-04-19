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
    pub fn new(width: i32, height: i32) -> Result<Self, InputError> {
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
