use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::error::DeskError;
use desk_input_injection::model::data_channel::MouseEventData;
use desk_input_injection::model::geometry::{MonitorGeometry, shared as shared_geometry};
use desk_input_injection::mouse_event::mouse_event_factory::create_mouse_event_handler;

// NOTE: This function is currently dead code. The
// `service/data_channel.rs::handle_data_channel_event` caller is never
// installed — every startup mode now routes data channels through
// `daemon/pc_manager.rs::on_data_channel`, which forwards mouse
// events to the worker via `ServiceToWorker::MouseInput` IPC. The
// real handler lives in `worker/input_dispatcher.rs`. This file is
// preserved only so a future cleanup can remove the entire
// `service/{data_channel,mouse_event,keyboard_event,clipboard_event,
// file_transfer,whiteboard_event}.rs` group in one go.
pub async fn handle_mouse_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    let geometry = {
        let state = signaling_state.read().await;
        let r = state.display_info.desktop_coordinates;
        shared_geometry(MonitorGeometry::new(r.left, r.top, r.width(), r.height()))
    };
    let wayland_control_mode = {
        let state = signaling_state.read().await;
        state.wayland_control_mode.clone()
    };
    let handler = Arc::new(Mutex::new(create_mouse_event_handler(
        geometry,
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
