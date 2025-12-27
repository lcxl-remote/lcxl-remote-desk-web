use std::{sync::Arc, time::Duration};

use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::RwLock;
use webrtc::{
    data_channel::{RTCDataChannel, data_channel_message::DataChannelMessage},
    peer_connection::math_rand_alpha,
};

use crate::{
    error::DeskError,
    model::data_channel::{DATA_CHANNEL_LABEL_KEYBOARD_EVENT, DATA_CHANNEL_LABEL_MOUSE_EVENT},
    service::{keyboard_event::handle_keyboard_event, mouse_event::handle_mouse_event},
};

pub async fn handle_data_channel_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    match data_channel.label() {
        DATA_CHANNEL_LABEL_MOUSE_EVENT => {
            handle_mouse_event(signaling_state, data_channel).await?;
            return Ok(());
        }
        DATA_CHANNEL_LABEL_KEYBOARD_EVENT => {
            handle_keyboard_event(signaling_state, data_channel).await?;
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
