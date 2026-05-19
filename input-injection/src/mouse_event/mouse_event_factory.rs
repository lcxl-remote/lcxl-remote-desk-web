use crate::{error::InputError, model::data_channel::MouseEventHandler};

#[cfg(target_os = "windows")]
use crate::mouse_event::windows;

#[cfg(target_os = "linux")]
use crate::mouse_event::linux;
#[cfg(target_os = "linux")]
use crate::mouse_event::wayland_portal;

#[cfg(target_os = "macos")]
use crate::mouse_event::mac;

/// Construct a platform mouse event handler bound to the captured
/// monitor's rectangle in virtual desktop space.
///
/// `left` / `top` is the top-left corner of the captured monitor's
/// rectangle (taken from `DisplayInfo::desktop_coordinates`). The
/// Windows and macOS backends use it to translate the browser's
/// normalised `(x, y)` into absolute global cursor coordinates so the
/// cursor lands on the captured monitor rather than the primary. The
/// Wayland portal and Linux uinput backends do not — see the per-file
/// docs on why.
pub fn create_mouse_event_handler(
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    wayland_control_mode: Option<&str>,
) -> Result<Box<dyn MouseEventHandler + Send + Sync>, InputError> {
    #[cfg(target_os = "windows")]
    {
        let _ = wayland_control_mode;
        Ok(Box::new(windows::WindowsMouseEventHandler::new(
            left, top, width, height,
        )))
    }
    #[cfg(target_os = "linux")]
    {
        use crate::model::data_channel::MouseEventData;
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
            "Mouse handler: selecting linux backend, mode={}, rect=({},{},{}x{}), WAYLAND_DISPLAY={}",
            mode,
            left,
            top,
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
                    left, top, width, height,
                )?));
            }
            "portal" => {
                log::info!("Mouse handler: using forced wayland portal backend");
                return Ok(Box::new(
                    wayland_portal::WaylandPortalMouseEventHandler::new(
                        left, top, width, height,
                    )?,
                ));
            }
            _ => {}
        }

        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            match wayland_portal::WaylandPortalMouseEventHandler::new(
                left, top, width, height,
            ) {
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
            left, top, width, height,
        )?))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = wayland_control_mode;
        Ok(Box::new(mac::MacMouseEventHandler::new(
            left, top, width, height,
        )?))
    }
}
