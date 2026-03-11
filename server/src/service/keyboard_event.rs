use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::{
    error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod wayland_portal;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod mac;

pub fn create_keyboard_event_handler(
    wayland_control_mode: Option<&str>,
) -> Result<Box<dyn KeyboardEventHandler + Send + Sync>, DeskError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsKeyboardEventHandler {}))
    }
    #[cfg(target_os = "linux")]
    {
        struct NoopKeyboardEventHandler;
        impl KeyboardEventHandler for NoopKeyboardEventHandler {
            fn handle_key_down(&mut self, _event: &KeyboardEventData) -> Result<(), DeskError> {
                Ok(())
            }

            fn handle_key_up(&mut self, _event: &KeyboardEventData) -> Result<(), DeskError> {
                Ok(())
            }
        }

        let mode = wayland_control_mode.unwrap_or("auto");
        log::info!(
            "Keyboard handler: selecting linux backend, mode={}, WAYLAND_DISPLAY={}",
            mode,
            std::env::var("WAYLAND_DISPLAY").is_ok()
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

        if std::env::var("WAYLAND_DISPLAY").is_ok() {
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

pub async fn handle_keyboard_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    let wayland_control_mode = {
        let state = signaling_state.read().await;
        state.wayland_control_mode.clone()
    };
    let handler = Arc::new(Mutex::new(create_keyboard_event_handler(
        wayland_control_mode.as_deref(),
    )?));
    data_channel.on_message(Box::new(move |msg| {
        let signaling_state = signaling_state.clone();
        let handler = handler.clone();

        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
        log::debug!("Keyboard event message: '{msg_str}'");
        Box::pin(async move {
            if !signaling_state.read().await.accept_control {
                log::warn!("Keyboard event rejected");
                return;
            }
            match serde_json::from_str::<KeyboardEventData>(&msg_str) {
                Ok(event) => {
                    if let Err(e) = handler.lock().await.handle_keyboard_event(&event) {
                        log::error!("Failed to handle keyboard event: {}", e);
                    }
                }
                Err(e) => {
                    log::error!("Failed to parse keyboard event message: {}", e);
                }
            }
        })
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::data_channel::KeyboardEventData;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_create_keyboard_event_handler_none_mode() {
        let mut handler = create_keyboard_event_handler(Some("none")).unwrap();
        let event = KeyboardEventData {
            event: "keydown".to_string(),
            code: "KeyA".to_string(),
            key_code: 30,
            ..Default::default()
        };
        let result = handler.handle_keyboard_event(&event);
        assert!(result.is_ok());
    }
}
