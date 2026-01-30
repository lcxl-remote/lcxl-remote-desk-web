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

#[cfg(target_os = "macos")]
pub mod mac;

pub fn create_mouse_event_handler(
    width: i32,
    height: i32,
) -> Result<Box<dyn MouseEventHandler + Send + Sync>, DeskError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsMouseEventHandler::new(
            width, height,
        )))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::UinputMouseEventHandler::new(
            width, height,
        )?))
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
        let desktop_coordinates = signaling_state
            .read()
            .await
            .display_info
            .desktop_coordinates;
        (desktop_coordinates.width(), desktop_coordinates.height())
    };
    let handler = Arc::new(Mutex::new(create_mouse_event_handler(width, height)?));
    data_channel.on_message(Box::new(move |msg| {
        let signaling_state = signaling_state.clone();
        let handler = handler.clone();

        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
        log::debug!("Mouse event message: '{msg_str}'");
        Box::pin(async move {
            if !signaling_state.read().await.accept_control {
                log::warn!("Mouse event rejected");
                return;
            }
            match serde_json::from_str::<MouseEventData>(&msg_str) {
                Ok(event) => {
                    if let Err(e) = handler.lock().await.handle_mouse_event(&event) {
                        log::error!("Failed to handle mouse event: {:?}", e);
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
