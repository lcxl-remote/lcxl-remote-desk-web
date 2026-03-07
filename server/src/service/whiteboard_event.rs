use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::RwLock;
use webrtc::data_channel::{RTCDataChannel, data_channel_message::DataChannelMessage};

use crate::model::host_control::WhiteboardCommand;

/// Handle whiteboard events from the DataChannel.
/// Forwards drawing messages to the Tauri whiteboard overlay via the command sender.
pub async fn handle_whiteboard_event(
    _signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
    whiteboard_cmd_sender: std::sync::mpsc::Sender<WhiteboardCommand>,
    session_id: String,
) -> Result<(), crate::error::DeskError> {
    let d_label = data_channel.label().to_owned();
    let d_id = data_channel.id();

    let sender_for_close = whiteboard_cmd_sender.clone();
    data_channel.on_close(Box::new(move || {
        log::info!("Whiteboard data channel '{d_label}' closed, hiding overlay");
        // Only hide the overlay on channel close, do NOT send Quit
        // The manager must stay alive for the lifetime of the app
        let _ = sender_for_close.send(WhiteboardCommand::Hide("channel_closed".to_string()));
        Box::pin(async {})
    }));

    let d_label2 = data_channel.label().to_owned();
    data_channel.on_open(Box::new(move || {
        log::info!("Whiteboard data channel '{d_label2}'-'{d_id}' opened");
        Box::pin(async {})
    }));

    let sender_for_msg = whiteboard_cmd_sender.clone();
    data_channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let msg_str = match String::from_utf8(msg.data.to_vec()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Invalid UTF-8 in whiteboard message: {}", e);
                return Box::pin(async {});
            }
        };
        log::trace!("Whiteboard message received: {}", msg_str);

        // First, ensure the window is shown
        if let Err(e) = sender_for_msg.send(WhiteboardCommand::Show(session_id.clone())) {
            log::error!("Failed to send whiteboard show command: {}", e);
        }

        // Forward the raw JSON message to the tauri whiteboard manager
        if let Err(e) = sender_for_msg.send(WhiteboardCommand::DrawMessage(msg_str)) {
            log::error!("Failed to send whiteboard draw command: {}", e);
        }

        Box::pin(async {})
    }));

    Ok(())
}
