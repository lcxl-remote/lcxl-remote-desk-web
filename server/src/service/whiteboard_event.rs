use std::sync::Arc;

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::RwLock;
use webrtc::data_channel::{RTCDataChannel, data_channel_message::DataChannelMessage};

use desk_input_injection::model::host_control::WhiteboardCommand;

use crate::host_control::HostControlHub;
use crate::model::{
    security_approval::{SecurityPermissionType, check_security_permission},
    settings::SharedSettings,
};

/// Handle whiteboard events from the DataChannel.
/// Forwards drawing messages to the Tauri whiteboard overlay via the command sender.
pub async fn handle_whiteboard_event(
    _signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
    whiteboard_cmd_sender: std::sync::mpsc::Sender<WhiteboardCommand>,
    connection_id: String,
    settings: actix_web::web::Data<SharedSettings>,
    host_control_hub: Arc<HostControlHub>,
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
    let permission_cache = Arc::new(tokio::sync::RwLock::new(None::<bool>));

    data_channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let sender_for_msg = sender_for_msg.clone();
        let settings = settings.clone();
        let hub = host_control_hub.clone();
        let connection_id = connection_id.clone();
        let permission_cache = permission_cache.clone();

        Box::pin(async move {
            // Check permission first
            let mut allowed = false;
            {
                let cache = permission_cache.read().await;
                if let Some(res) = *cache {
                    allowed = res;
                } else {
                    drop(cache);
                    let mut cache_write = permission_cache.write().await;
                    if let Some(res) = *cache_write {
                        allowed = res;
                    } else {
                        let allow_whiteboard = { settings.read().await.security.allow_whiteboard };
                        let approved = check_security_permission(
                            &settings,
                            &hub,
                            allow_whiteboard,
                            SecurityPermissionType::Whiteboard,
                            Some(connection_id.clone()),
                            false,
                        )
                        .await;
                        *cache_write = Some(approved);
                        allowed = approved;
                    }
                }
            }
            if !allowed {
                log::warn!(
                    "Whiteboard message blocked by security settings or user for {}",
                    connection_id
                );
                return;
            }

            let msg_str = match String::from_utf8(msg.data.to_vec()) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("Invalid UTF-8 in whiteboard message: {}", e);
                    return;
                }
            };
            log::trace!("Whiteboard message received: {}", msg_str);

            // First, ensure the window is shown
            if let Err(e) = sender_for_msg.send(WhiteboardCommand::Show(connection_id.clone())) {
                log::error!("Failed to send whiteboard show command: {}", e);
            }

            // Forward the raw JSON message to the tauri whiteboard manager
            if let Err(e) = sender_for_msg.send(WhiteboardCommand::DrawMessage(msg_str)) {
                log::error!("Failed to send whiteboard draw command: {}", e);
            }
        })
    }));

    Ok(())
}
