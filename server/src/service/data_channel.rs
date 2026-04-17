use std::{sync::Arc, time::Duration};

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::RwLock;
use webrtc::{
    data_channel::{RTCDataChannel, data_channel_message::DataChannelMessage},
    peer_connection::math_rand_alpha,
};

use crate::{
    error::DeskError,
    model::{
        data_channel::{
            DATA_CHANNEL_LABEL_CLIPBOARD_EVENT, DATA_CHANNEL_LABEL_FILE_TRANSFER_EVENT,
            DATA_CHANNEL_LABEL_KEYBOARD_EVENT, DATA_CHANNEL_LABEL_MOUSE_EVENT,
            DATA_CHANNEL_LABEL_MOUSE_MOVE_EVENT, DATA_CHANNEL_LABEL_WHITEBOARD_EVENT,
        },
        host_control::WhiteboardCommand,
        security_approval::SecurityApprovalSender,
        settings::SharedSettings,
    },
    service::{
        clipboard_event::handle_clipboard_event, file_transfer::handle_file_transfer_event,
        keyboard_event::handle_keyboard_event, mouse_event::handle_mouse_event,
        whiteboard_event::handle_whiteboard_event,
    },
};

/// Handle data channel event
/// connection_id: from connection id
pub async fn handle_data_channel_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
    whiteboard_cmd_sender: Option<std::sync::mpsc::Sender<WhiteboardCommand>>,
    connection_id: String,
    settings: actix_web::web::Data<SharedSettings>,
    security_approval_sender: Option<SecurityApprovalSender>,
) -> Result<(), DeskError> {
    match data_channel.label() {
        DATA_CHANNEL_LABEL_MOUSE_EVENT | DATA_CHANNEL_LABEL_MOUSE_MOVE_EVENT => {
            handle_mouse_event(signaling_state, data_channel).await?;
            return Ok(());
        }
        DATA_CHANNEL_LABEL_KEYBOARD_EVENT => {
            handle_keyboard_event(signaling_state, data_channel).await?;
            return Ok(());
        }
        DATA_CHANNEL_LABEL_FILE_TRANSFER_EVENT => {
            handle_file_transfer_event(
                data_channel,
                settings,
                security_approval_sender,
                connection_id,
            )
            .await?;
            return Ok(());
        }
        DATA_CHANNEL_LABEL_CLIPBOARD_EVENT => {
            handle_clipboard_event(signaling_state, data_channel).await?;
            return Ok(());
        }
        DATA_CHANNEL_LABEL_WHITEBOARD_EVENT => {
            if let Some(sender) = whiteboard_cmd_sender {
                handle_whiteboard_event(
                    signaling_state,
                    data_channel,
                    sender,
                    connection_id,
                    settings,
                    security_approval_sender,
                )
                .await?;
            } else {
                log::warn!("Whiteboard data channel received but no tauri whiteboard support");
            }
            return Ok(());
        }
        label => {
            log::warn!("Unknown data channel label: {}", label);
        }
    }
    let d_label = data_channel.label().to_owned();
    let data_channel_sender = Arc::clone(&data_channel);
    let d_id = data_channel.id();
    let d_label2 = d_label.clone();
    let d_id2 = d_id;

    //data_channel.close();
    data_channel.on_close(Box::new(move || {
        log::warn!("Data channel closed");
        Box::pin(async {})
    }));

    data_channel.on_open(Box::new(move || {
                    log::info!("Data channel '{d_label2}'-'{d_id2}' open. Random messages will now be sent to any connected DataChannels every 5 seconds");

                    Box::pin(async move {
                        let mut result = webrtc::error::Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(5));
                            tokio::pin!(timeout);

                            tokio::select! {
                                _ = timeout.as_mut() =>{
                                    let message = math_rand_alpha(15);
                                    log::info!("Sending '{message}'");
                                    result = data_channel_sender.send_text(message).await.map_err(Into::into);
                                }
                            };
                        }
                    })
                }));

    // Register text message handling
    let signaling_state2 = signaling_state.clone();
    data_channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
        log::debug!("Message from DataChannel '{d_label}': '{msg_str}'");
        let d_label3 = d_label.clone();
        let d_id3 = d_id.clone();
        Box::pin({
            let value = signaling_state2.clone();
            async move {
                if !value.read().await.accept_control {
                    log::warn!("Data channel '{d_label3}'-'{d_id3}' rejected");
                    return;
                }
            }
        })
    }));
    Ok(())
}
