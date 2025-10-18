use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::{
    desk_error::DeskError,
    model::{
        data_channel::{KeyboardEventData, KeyboardEventHandler},
        signaling::SignalingState,
    },
};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

pub fn create_keyboard_event_handler()
-> Result<Box<dyn KeyboardEventHandler + Send + Sync>, DeskError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsKeyboardEventHandler {}))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::UinputKeyboardEventHandler::new()?))
    }
}

pub async fn handle_keyboard_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    let handler = Arc::new(Mutex::new(create_keyboard_event_handler()?));
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
                        log::error!("Failed to handle keyboard event: {:?}", e);
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
