use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::error::DeskError;
use desk_input_injection::model::data_channel::MouseEventData;
use desk_input_injection::mouse_event::mouse_event_factory::create_mouse_event_handler;

pub async fn handle_mouse_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    // The captured monitor may sit at a non-zero (left, top) in virtual
    // desktop space (any secondary monitor or IDD virtual display
    // attached to the right or below the primary). `SetCursorPos` /
    // `CGEvent::post` both speak that global coordinate space, so the
    // mouse handler needs the full rectangle — not just width/height —
    // to land the cursor on the correct surface.
    let (left, top, width, height) = {
        let state = signaling_state.read().await;
        let r = state.display_info.desktop_coordinates;
        (r.left, r.top, r.width(), r.height())
    };
    let wayland_control_mode = {
        let state = signaling_state.read().await;
        state.wayland_control_mode.clone()
    };
    let handler = Arc::new(Mutex::new(create_mouse_event_handler(
        left,
        top,
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
                    if let Some(seq) = event.sequence_number
                        && seq > 0
                    {
                        let mut last_seq = last_sequence_num.lock().await;
                        if seq < *last_seq {
                            // Discard late/expired out-of-order packets
                            return;
                        }
                        *last_seq = seq;
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
