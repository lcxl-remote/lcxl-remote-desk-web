use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::error::DeskError;
use desk_input_injection::model::data_channel::KeyboardEventData;
use desk_input_injection::keyboard_event::keyboard_event_factory::create_keyboard_event_handler;

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
