use std::sync::Arc;

use crate::{
    error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
    service::wayland_remote_desktop::WaylandRemoteDesktop,
};

pub struct WaylandPortalKeyboardEventHandler {
    portal: Arc<WaylandRemoteDesktop>,
}

impl WaylandPortalKeyboardEventHandler {
    pub fn new() -> Result<Self, DeskError> {
        log::info!("Wayland portal keyboard handler: creating");
        Ok(Self {
            portal: WaylandRemoteDesktop::shared()?,
        })
    }
}

impl KeyboardEventHandler for WaylandPortalKeyboardEventHandler {
    fn handle_key_down(&mut self, event: &KeyboardEventData) -> Result<(), DeskError> {
        self.portal.notify_keyboard_keycode(event.key_code as i32, 1)
    }

    fn handle_key_up(&mut self, event: &KeyboardEventData) -> Result<(), DeskError> {
        self.portal.notify_keyboard_keycode(event.key_code as i32, 0)
    }
}
