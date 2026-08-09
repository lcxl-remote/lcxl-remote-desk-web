#[cfg(target_os = "linux")]
use desk_wayland_portal::PortalInputSender;

#[cfg(target_os = "linux")]
use crate::model::data_channel::KeyboardEventData;
use crate::{error::InputError, model::data_channel::KeyboardEventHandler};

#[cfg(target_os = "linux")]
use crate::keyboard_event::linux;
#[cfg(target_os = "linux")]
use crate::keyboard_event::wayland_portal;
#[cfg(target_os = "windows")]
use crate::keyboard_event::windows;

#[cfg(target_os = "macos")]
use crate::keyboard_event::mac;

pub fn create_keyboard_event_handler(
    _wayland_control_mode: Option<&str>,
    #[cfg(target_os = "linux")] portal: Option<PortalInputSender>,
) -> Result<Box<dyn KeyboardEventHandler + Send + Sync>, InputError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsKeyboardEventHandler {}))
    }
    #[cfg(target_os = "linux")]
    {
        struct NoopKeyboardEventHandler;
        impl KeyboardEventHandler for NoopKeyboardEventHandler {
            fn handle_key_down(&mut self, _event: &KeyboardEventData) -> Result<(), InputError> {
                Ok(())
            }

            fn handle_key_up(&mut self, _event: &KeyboardEventData) -> Result<(), InputError> {
                Ok(())
            }
        }

        let mode = _wayland_control_mode.unwrap_or("none");
        log::info!("Keyboard handler: selecting frozen linux backend, mode={mode}");
        match mode {
            "none" => {
                log::info!("Keyboard handler: using noop backend");
                return Ok(Box::new(NoopKeyboardEventHandler));
            }
            "uinput" => {
                log::info!("Keyboard handler: using forced uinput backend");
                return Ok(Box::new(linux::UinputKeyboardEventHandler::new()?));
            }
            "portal" => {
                log::info!("Keyboard handler: using forced wayland portal backend");
                return Ok(Box::new(
                    wayland_portal::WaylandPortalKeyboardEventHandler::new(
                        portal.clone().ok_or_else(|| {
                            InputError::new_custom_error(
                                desk_utils::error::DeskErrorCode::FEATURE_UNAVAILABLE,
                                "Wayland Portal authorization is required on the host",
                            )
                        })?,
                    )?,
                ));
            }
            _ => {
                return Err(InputError::new_custom_error(
                    desk_utils::error::DeskErrorCode::INVALID_PARAMS,
                    "Linux input requires a frozen mode of none, uinput, or portal",
                ));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(mac::MacKeyboardEventHandler {}))
    }
}
