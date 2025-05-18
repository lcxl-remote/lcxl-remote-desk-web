use std::sync::Arc;

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use futures_util::StreamExt;
use log::{error, info, warn};
use webrtc::{
    api::{
        APIBuilder, interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        sdp::session_description::RTCSessionDescription,
    },
};

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
    session: Session,
    user: CurrentUser,
) -> Result<(), DeskError> {
    info!("Handling signaling");
    // Handle signaling logic here
    let mut signaling_context = SignalingContext::init_signaling(settings, session, user).await?;

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(AggregatedMessage::Text(text)) => {
                // echo text message
                signaling_context.handle_message(text).await?;
            }

            Ok(AggregatedMessage::Binary(bin)) => {
                // echo binary message
                signaling_context.binary(bin).await?;
            }

            Ok(AggregatedMessage::Ping(msg)) => {
                // respond to PING frame with PONG frame
                signaling_context.ping(msg).await?;
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
        }
    }
    let result = signaling_context.rtc_peer_connection.close().await;
    info!("Signaling session ended, result={:?}", result);
    Ok(())
}

pub struct SignalingContext {
    pub settings: web::Data<SharedSettings>,
    pub session: Session,
    pub user: CurrentUser,
    pub rtc_peer_connection: Arc<RTCPeerConnection>,
}

impl SignalingContext {
    pub async fn init_signaling(
        settings: web::Data<SharedSettings>,
        mut session: Session,
        user: CurrentUser,
    ) -> Result<Self, DeskError> {
        let tmp_settings = {
            let shared_settings = settings.lock().await;
            shared_settings.clone()
        };
        let mut urls = Vec::<String>::new();
        for interface in tmp_settings.turn.interfaces.iter() {
            urls.push(format!("turn:{}", interface.external.to_string()));
        }
        let ice_server = RTCIceServer {
            urls: urls,
            username: tmp_settings.user.login_user_name.clone(),
            credential: tmp_settings.user.login_password.clone(),
        };
        let ice_servers = vec![ice_server];
        let init_signaling_data = InitSignalingData {
            ice_servers: ice_servers.clone(),
            user_name: user.name.clone(),
        };
        info!("Sending init signaling");
        let hello_signaling_model =
            SignalingModel::new_json_data(SignalingType::INIT, &init_signaling_data)?;
        session.send_signaling(&hello_signaling_model).await?;
        info!("Init signaling sent");

        // new rtc_peer_connection
        // Create a MediaEngine object to configure the supported codec
        let mut m = MediaEngine::default();

        m.register_default_codecs()?;

        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)?;

        // Create the API object with the MediaEngine
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        // Prepare the configuration
        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        // Create a new RTCPeerConnection
        let rtc_peer_connection = Arc::new(api.new_peer_connection(config).await?);

        Ok(Self {
            settings,
            session,
            user,
            rtc_peer_connection,
        })
    }

    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskError> {
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            SIGNALING_TYPE_CODE_INIT => {
                self.handle_offer(&signaling_model).await?;
            } // handle_hello(session, user),
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

                self.session.send_signaling(&error_signaling).await?;
            }
        }
        Ok(())
    }

    pub async fn binary(&mut self, bin: Bytes) -> Result<(), DeskError> {
        self.session.binary(bin).await?;
        Ok(())
    }
    pub async fn ping(&mut self, msg: Bytes) -> Result<(), DeskError> {
        self.session.pong(&msg).await?;
        Ok(())
    }

    pub async fn handle_offer(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        if signaling_model.signaling_data == None {
            self.session
                .send_signaling(&SignalingModel::error(
                    SignalingType::from(signaling_model.signaling_type),
                    "No signaling data provided",
                ))
                .await?;
            return Ok(());
        }
        let signaling_data = signaling_model.signaling_data.clone().unwrap();
        let offer = serde_json::from_str::<RTCSessionDescription>(&signaling_data)?;

        // Set the remote SessionDescription
        self.rtc_peer_connection
            .set_remote_description(offer)
            .await?;
        let answer = self.rtc_peer_connection.create_answer(None).await?;

        // Create channel that is blocked until ICE Gathering is complete
        let mut gather_complete = self.rtc_peer_connection.gathering_complete_promise().await;

        // Sets the LocalDescription, and starts our UDP listeners
        self.rtc_peer_connection
            .set_local_description(answer)
            .await?;

        // Block until ICE Gathering is complete, disabling trickle ICE
        // we do this because we only can exchange one signaling message
        // in a production application you should exchange ICE Candidates via OnICECandidate
        let _ = gather_complete.recv().await;

        // Output the answer in base64 so we can paste it in browser
        let option = self.rtc_peer_connection.local_description().await;
        if option.is_none() {
            self.session
                .send_signaling(&SignalingModel::error(
                    SignalingType::from(signaling_model.signaling_type),
                    "generate local_description failed!",
                ))
                .await?;
            return Ok(());
        }
        let local_desc = self.rtc_peer_connection.local_description().await.unwrap();
        let json_str = serde_json::to_string(&local_desc)?;
        log::info!("local description: {}", json_str);

        self.session
            .send_signaling(&SignalingModel::new_str_data(
                SignalingType::ANSWER,
                &json_str,
            ))
            .await?;

        Ok(())
    }
}
