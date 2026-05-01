use crate::{error::InputError, model::data_channel::MouseEventHandler};

#[cfg(target_os = "windows")]
use crate::mouse_event::windows;

#[cfg(target_os = "linux")]
use crate::mouse_event::linux;
#[cfg(target_os = "linux")]
use crate::mouse_event::wayland_portal;

#[cfg(target_os = "macos")]
use crate::mouse_event::mac;

pub fn create_mouse_event_handler(
    width: i32,
    height: i32,
    wayland_control_mode: Option<&str>,
) -> Result<Box<dyn MouseEventHandler + Send + Sync>, InputError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsMouseEventHandler::new(
            width, height,
        )))
    }
    #[cfg(target_os = "linux")]
    {
        struct NoopMouseEventHandler;
        impl MouseEventHandler for NoopMouseEventHandler {
            fn handle_mouse_move(&mut self, _event: &MouseEventData) -> Result<(), InputError> {
                Ok(())
            }
            fn handle_mouse_down(&mut self, _event: &MouseEventData) -> Result<(), InputError> {
                Ok(())
            }
            fn handle_mouse_up(&mut self, _event: &MouseEventData) -> Result<(), InputError> {
                Ok(())
            }
            fn handle_mouse_wheel(&mut self, _event: &MouseEventData) -> Result<(), InputError> {
                Ok(())
            }
        }

        let mode = wayland_control_mode.unwrap_or("auto");
        log::info!(
            "Mouse handler: selecting linux backend, mode={}, width={}, height={}, WAYLAND_DISPLAY={}",
            mode,
            width,
            height,
            std::env::var("WAYLAND_DISPLAY").is_ok()
        );
        match mode {
            "none" => {
                log::info!("Mouse handler: using noop backend");
                return Ok(Box::new(NoopMouseEventHandler));
            }
            "uinput" => {
                log::info!("Mouse handler: using forced uinput backend");
                return Ok(Box::new(linux::UinputMouseEventHandler::new(
                    width, height,
                )?));
            }
            "portal" => {
                log::info!("Mouse handler: using forced wayland portal backend");
                return Ok(Box::new(
                    wayland_portal::WaylandPortalMouseEventHandler::new(width, height)?,
                ));
            }
            _ => {}
        }

        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            match wayland_portal::WaylandPortalMouseEventHandler::new(width, height) {
                Ok(handler) => {
                    log::info!("Mouse handler: auto selected wayland portal backend");
                    return Ok(Box::new(handler));
                }
                Err(e) => {
                    log::warn!("Wayland portal mouse handler init failed, fallback to uinput: {e}");
                }
            }
        }
        log::info!("Mouse handler: fallback to uinput backend");
        Ok(Box::new(linux::UinputMouseEventHandler::new(
            width, height,
        )?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(mac::MacMouseEventHandler::new(width, height)?))
    }
}
