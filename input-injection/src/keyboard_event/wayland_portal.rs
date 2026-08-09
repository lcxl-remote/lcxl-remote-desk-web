use desk_wayland_portal::PortalInputSender;

use crate::{
    error::InputError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

use super::keyboard_event_to_evdev;

pub struct WaylandPortalKeyboardEventHandler {
    portal: PortalInputSender,
}

impl WaylandPortalKeyboardEventHandler {
    pub fn new(portal: PortalInputSender) -> Result<Self, InputError> {
        log::info!("Wayland portal keyboard handler: creating");
        Ok(Self { portal })
    }
}

impl KeyboardEventHandler for WaylandPortalKeyboardEventHandler {
    fn handle_key_down(&mut self, event: &KeyboardEventData) -> Result<(), InputError> {
        if let Some(evdev_code) = keyboard_event_to_evdev(event) {
            self.portal
                .notify_keyboard_keycode(i32::from(evdev_code), 1)
                .map_err(portal_input_error)?;
        }
        Ok(())
    }

    fn handle_key_up(&mut self, event: &KeyboardEventData) -> Result<(), InputError> {
        if let Some(evdev_code) = keyboard_event_to_evdev(event) {
            self.portal
                .notify_keyboard_keycode(i32::from(evdev_code), 0)
                .map_err(portal_input_error)?;
        }
        Ok(())
    }
}

fn portal_input_error(error: desk_wayland_portal::PortalError) -> InputError {
    InputError::new_custom_error(
        desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
        &error.to_string(),
    )
}
