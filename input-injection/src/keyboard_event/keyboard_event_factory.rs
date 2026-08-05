#[cfg(target_os = "linux")]
use desk_utils::linux_display::{LinuxDisplayServer, detect_linux_display_environment};

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

        let mode = _wayland_control_mode.unwrap_or("auto");
        log::info!(
            "Keyboard handler: selecting linux backend, mode={}, WAYLAND_DISPLAY={}",
            mode,
            detect_linux_display_environment().wayland_present
        );
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
                    wayland_portal::WaylandPortalKeyboardEventHandler::new()?,
                ));
            }
            _ => {}
        }

        if detect_linux_display_environment().active_server() == LinuxDisplayServer::Wayland {
            match wayland_portal::WaylandPortalKeyboardEventHandler::new() {
                Ok(handler) => {
                    log::info!("Keyboard handler: auto selected wayland portal backend");
                    return Ok(Box::new(handler));
                }
                Err(e) => {
                    log::warn!(
                        "Wayland portal keyboard handler init failed, fallback to uinput: {e}"
                    );
                }
            }
        }
        log::info!("Keyboard handler: fallback to uinput backend");
        Ok(Box::new(linux::UinputKeyboardEventHandler::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(mac::MacKeyboardEventHandler {}))
    }
}
