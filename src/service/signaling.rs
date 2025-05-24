use std::{fs::File, io::BufReader, sync::Arc};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use futures_util::StreamExt;
use log::{error, info, warn};
use tokio::sync::Notify;
use tokio::time::Duration;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_H264, MediaEngine},
    },
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    media::{Sample, io::h264_reader::H264Reader},
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
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
    service::record_screen::{
        H264ScreenOutput, ScreenOutputVideoNal, ScreenRecordManager, ScreenRecordManagerArc,
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
        let notify_tx = Arc::new(Notify::new());
        let notify_video = notify_tx.clone();
        let notify_audio = notify_tx.clone();

        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
        let video_done_tx = done_tx.clone();
        let audio_done_tx = done_tx.clone();

        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        // Add this newly created track to the PeerConnection
        let rtp_sender = rtc_peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
            Result::<(), DeskError>::Ok(())
        });
        //要改
        let video_file_name = "test".to_owned();
        tokio::spawn(async move {
            let manager = ScreenRecordManager::new()?;
            let screen_output = manager.get_screen_output(0)?;

            let mut h264_screen_output = H264ScreenOutput::new(screen_output);

            // Wait for connection established
            notify_video.notified().await;

            println!("play video from disk file {video_file_name}");

            // It is important to use a time.Ticker instead of time.Sleep because
            // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
            // * works around latency issues with Sleep
            let mut ticker = tokio::time::interval(Duration::from_millis(33));
            loop {
                let nal_info = h264_screen_output.get_nal()?;

                video_track
                    .write_sample(&Sample {
                        data: nal_info.nal_bytes,
                        duration: Duration::from_secs(1),
                        ..Default::default()
                    })
                    .await?;

                let _ = ticker.tick().await;
            }

            let _ = video_done_tx.try_send(());

            Result::<(), DeskError>::Ok(())
        });
        // Set the handler for ICE connection state
        // This will notify you when the peer has connected/disconnected
        rtc_peer_connection.on_ice_connection_state_change(Box::new(
            move |connection_state: RTCIceConnectionState| {
                println!("Connection State has changed {connection_state}");
                if connection_state == RTCIceConnectionState::Connected {
                    notify_tx.notify_waiters();
                }
                Box::pin(async {})
            },
        ));

        // Set the handler for Peer connection state
        // This will notify you when the peer has connected/disconnected
        rtc_peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                println!("Peer Connection State has changed: {s}");

                if s == RTCPeerConnectionState::Failed {
                    // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                    // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                    // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                    println!("Peer Connection has gone to failed exiting");
                    let _ = done_tx.try_send(());
                }

                Box::pin(async {})
            },
        ));

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
