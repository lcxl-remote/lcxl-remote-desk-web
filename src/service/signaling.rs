use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytestring::ByteString;
use futures_util::StreamExt;
use log::{error, info, warn};
use webrtc::ice_transport::ice_server::RTCIceServer;

use crate::{
    desk_error::DeskError,
    model::{
        settings::SharedSettings,
        signaling::{
            InitSignalingData, SIGNALING_TYPE_CODE_ANSWER, SIGNALING_TYPE_CODE_CANID,
            SIGNALING_TYPE_CODE_INIT, SIGNALING_TYPE_CODE_OFFER, SignalingModel,
            SignalingSessionExt, SignalingType,
        },
        user::CurrentUser,
    },
};

pub async fn handle_signaling(
    settings: web::Data<SharedSettings>,
    mut stream: AggregatedMessageStream,
    mut session: Session,
    user: CurrentUser,
) -> Result<(), DeskError> {
    info!("Handling signaling");
    // Handle signaling logic here

    send_init_signaling(settings, &mut session, &user).await?;

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(AggregatedMessage::Text(text)) => {
                // echo text message
                //session.text(text).await?;
                handle_message(&mut session, &user, text).await?;
            }

            Ok(AggregatedMessage::Binary(bin)) => {
                // echo binary message
                session.binary(bin).await?;
            }

            Ok(AggregatedMessage::Ping(msg)) => {
                // respond to PING frame with PONG frame
                session.pong(&msg).await?;
            }
            Ok(AggregatedMessage::Pong(_)) => {
                // ignore PONG frames
            }
            Ok(AggregatedMessage::Close(close_reason)) => {
                warn!("WS close frame received: {:?}", close_reason);
                break;
            }
            Err(e) => {
                error!("WS error: {}", e);
                break;
            }
            _ => {}
        }
    }
    info!("Signaling session ended");
    Ok(())
}

async fn send_init_signaling(
    settings: web::Data<SharedSettings>,
    session: &mut Session,
    user: &CurrentUser,
) -> Result<(), DeskError> {
    let mut urls = Vec::<String>::new();
    {
        let shared_settings = settings.lock().await;
        for interface in shared_settings.turn.interfaces.iter() {
            urls.push(format!("turn:{}", interface.external.to_string()));
        }
    }

    let init_signaling_data = InitSignalingData {
        ice_server: RTCIceServer {
            urls: urls,
            username: "unittest".to_owned(),
            credential: "placeholder".to_owned(),
        },
        user_name: user.name.clone(),
    };
    info!("Sending init signaling");
    let hello_signaling_model =
        SignalingModel::new_json_data(SignalingType::INIT, &init_signaling_data)?;
    session.send_signaling(&hello_signaling_model).await?;
    info!("Init signaling sent");
    Ok(())
}

async fn handle_message(
    session: &mut Session,
    user: &CurrentUser,
    text: ByteString,
) -> Result<(), DeskError> {
    let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
    match signaling_model.signaling_type {
        SIGNALING_TYPE_CODE_INIT => {} // handle_hello(session, user),
        SIGNALING_TYPE_CODE_OFFER => {}
        SIGNALING_TYPE_CODE_ANSWER => {}
        SIGNALING_TYPE_CODE_CANID => {}
        _ => {
            error!("Unknown signaling type: {}", signaling_model.signaling_type);
            let error_signaling = SignalingModel::error(
                SignalingType::ERROR,
                &format!(
                    "Failed to handle signaling type: {}",
                    signaling_model.signaling_type
                ),
            );

            session.send_signaling(&error_signaling).await?;
        }
    }
    session.text(text).await?;
    Ok(())
}
