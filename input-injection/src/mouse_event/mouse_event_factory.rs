use crate::{
    error::InputError,
    model::{data_channel::MouseEventHandler, geometry::SharedMonitorGeometry},
};

#[cfg(target_os = "windows")]
use crate::mouse_event::windows;

#[cfg(target_os = "linux")]
use crate::mouse_event::linux;
#[cfg(target_os = "linux")]
use crate::mouse_event::wayland_portal;

#[cfg(target_os = "macos")]
use crate::mouse_event::mac;

/// Construct a platform mouse event handler bound to a hot-updatable
/// captured-monitor [`SharedMonitorGeometry`].
///
/// The geometry is shared with the worker (Arch IV `InputDispatcher`):
/// when display reconfiguration happens mid-session (`WM_DISPLAYCHANGE`,
/// IDD `SetMode`, virtual display Attach / Detach) the worker writes
/// new `(left, top, width, height)` values through its clone of the
/// `Arc<RwLock<...>>` and the handler picks them up on the next mouse
/// event. The connection no longer has to be torn down to recover from
/// a resolution change.
///
/// The Wayland portal backend honours `width` / `height` but ignores
/// `left` / `top` because the portal stream pins the output. The Linux
/// uinput backend stores the handle for interface symmetry but never
/// reads it — the kernel's `0..32767` abs range is compositor-mapped.
/// macOS and Windows consume the full rect.
pub fn create_mouse_event_handler(
    geometry: SharedMonitorGeometry,
    wayland_control_mode: Option<&str>,
) -> Result<Box<dyn MouseEventHandler + Send + Sync>, InputError> {
    #[cfg(target_os = "windows")]
    {
        let _ = wayland_control_mode;
        Ok(Box::new(windows::WindowsMouseEventHandler::new(geometry)))
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
        let (left, top, width, height) = {
            let g = geometry.read().expect("monitor geometry poisoned");
            (g.left, g.top, g.width, g.height)
        };
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
                return Ok(Box::new(linux::UinputMouseEventHandler::new(geometry)?));
            }
            "portal" => {
                log::info!("Mouse handler: using forced wayland portal backend");
                return Ok(Box::new(
                    wayland_portal::WaylandPortalMouseEventHandler::new(geometry)?,
                ));
            }
            _ => {}
        }

        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            match wayland_portal::WaylandPortalMouseEventHandler::new(geometry.clone()) {
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
        Ok(Box::new(linux::UinputMouseEventHandler::new(geometry)?))
    }
    #[cfg(target_os = "macos")]
    {
        let _ = wayland_control_mode;
        Ok(Box::new(mac::MacMouseEventHandler::new(geometry)?))
    }
}
