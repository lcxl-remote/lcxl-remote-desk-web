use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::{
    error::DeskError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod wayland_portal;

#[cfg(target_os = "macos")]
pub mod mac;

pub fn create_mouse_event_handler(
    width: i32,
    height: i32,
    wayland_control_mode: Option<&str>,
) -> Result<Box<dyn MouseEventHandler + Send + Sync>, DeskError> {
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
            fn handle_mouse_move(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
                Ok(())
            }
            fn handle_mouse_down(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
                Ok(())
            }
            fn handle_mouse_up(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
                Ok(())
            }
            fn handle_mouse_wheel(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
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
                return Ok(Box::new(linux::UinputMouseEventHandler::new(width, height)?));
            }
            "portal" => {
                log::info!("Mouse handler: using forced wayland portal backend");
                return Ok(Box::new(wayland_portal::WaylandPortalMouseEventHandler::new(
                    width, height,
                )?));
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
        Ok(Box::new(linux::UinputMouseEventHandler::new(width, height)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(mac::MacMouseEventHandler::new(width, height)?))
    }
}

pub async fn handle_mouse_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    let (width, height) = {
        let state = signaling_state.read().await;
        let desktop_coordinates = state.display_info.desktop_coordinates;
        (desktop_coordinates.width(), desktop_coordinates.height())
    };
    let wayland_control_mode = {
        let state = signaling_state.read().await;
        state.wayland_control_mode.clone()
    };
    let handler = Arc::new(Mutex::new(create_mouse_event_handler(
        width,
        height,
        wayland_control_mode.as_deref(),
    )?));
    let last_sequence_num = Arc::new(Mutex::new(0u64));

    data_channel.on_message(Box::new(move |msg| {
        let signaling_state = signaling_state.clone();
        let handler = handler.clone();
        let last_sequence_num = last_sequence_num.clone();

        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
        // log::debug!("Mouse event message: '{msg_str}'");
        Box::pin(async move {
            if !signaling_state.read().await.accept_control {
                log::warn!("Mouse event rejected");
                return;
            }
            match serde_json::from_str::<MouseEventData>(&msg_str) {
                Ok(event) => {
                    if let Some(seq) = event.sequence_number {
                        if seq > 0 {
                            let mut last_seq = last_sequence_num.lock().await;
                            if seq < *last_seq {
                                // Discard late/expired out-of-order packets
                                return;
                            }
                            *last_seq = seq;
                        }
                    }
                    if let Err(e) = handler.lock().await.handle_mouse_event(&event) {
                        log::error!("Failed to handle mouse event: {}", e);
                    }
                }
                Err(e) => {
                    log::error!("Failed to parse mouse event message: {}", e);
                }
            }
        })
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::data_channel::MouseEventData;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_create_mouse_event_handler_none_mode() {
        let mut handler = create_mouse_event_handler(1920, 1080, Some("none")).unwrap();
        let event = MouseEventData {
            event: "mousemove".to_string(),
            x: 0.5,
            y: 0.5,
            ..Default::default()
        };
        let result = handler.handle_mouse_event(&event);
        assert!(result.is_ok());
    }
}
