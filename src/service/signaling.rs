use std::sync::Arc;

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use actix_ws::{CloseCode, CloseReason};
use bytes::Bytes;
use bytestring::ByteString;
use futures_util::StreamExt;
use log::{error, info, warn};
use tokio::time::Duration;
use tokio::time::Instant;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_H264, MediaEngine},
    },
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
};

use crate::model::common::ErrorCode;
use crate::model::settings::Settings;
use crate::model::signaling::WebRTConnectionState;
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
    /// capture screen task runtime
    pub capture_screen_runtime: tokio::runtime::Runtime,
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

        let (ice_connection_state_tx, ice_connection_state_rx) =
            tokio::sync::watch::channel(WebRTConnectionState::Init);
        let ice_connection_state_tx_2 = ice_connection_state_tx.clone();
        let video_ice_connection_state_rx = ice_connection_state_rx.clone();
        let audio_ice_connection_state_rx = ice_connection_state_rx.clone();

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
            log::info!("Start to read incoming RTCP packets");
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
            log::info!("Finished to read incoming RTCP packets");
            Result::<(), DeskError>::Ok(())
        });

        let session2 = session.clone();
        let capture_screen_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .thread_name("capture_screen_task")
            .build()?;
        // Spawn a blocking task to capture screen and send video
        capture_screen_runtime.spawn(async move {
            let result = SignalingContext::capture_screen_task(
                tmp_settings,
                video_ice_connection_state_rx,
                video_track,
            )
            .await;

            if let Err(error) = result {
                log::error!("Capture screen task failed, error: {:?}", error);
                session2
                    .close(Some(CloseReason::from((
                        CloseCode::Abnormal,
                        error.to_string(),
                    ))))
                    .await?;
                return Err(error);
            }

            return result;
        });
        // Set the handler for ICE connection state
        // This will notify you when the peer has connected/disconnected
        rtc_peer_connection.on_ice_connection_state_change(Box::new(
            move |connection_state: RTCIceConnectionState| {
                log::info!("RTC ice connection State has changed {connection_state}");
                let state = WebRTConnectionState::from(&connection_state);
                if state != WebRTConnectionState::Init {
                    if let Err(error) = ice_connection_state_tx.send(state) {
                        log::error!("Failed to send connection state: {:?}", error)
                    }
                }

                Box::pin(async {})
            },
        ));

        // Set the handler for Peer connection state
        // This will notify you when the peer has connected/disconnected
        rtc_peer_connection.on_peer_connection_state_change(Box::new(
            move |s: RTCPeerConnectionState| {
                log::info!("Peer Connection State has changed: {s}");
                let state = WebRTConnectionState::from(&s);
                if state == WebRTConnectionState::Closed {
                    if let Err(error) = ice_connection_state_tx_2.send(state) {
                        log::error!("Failed to send connection state: {:?}", error)
                    }
                }

                Box::pin(async {})
            },
        ));

        Ok(Self {
            settings,
            session,
            user,
            rtc_peer_connection,
            capture_screen_runtime,
        })
    }

    pub async fn capture_screen_task(
        settings: Settings,
        mut video_ice_connection_state_rx: tokio::sync::watch::Receiver<WebRTConnectionState>,
        video_track: Arc<TrackLocalStaticSample>,
    ) -> Result<(), DeskError> {
        log::info!("Preparing to capture screen...");
        let manager = ScreenRecordManager::new(&settings)?;

        let mut h264_screen_output = H264ScreenOutput::new(manager, 0);
        // Wait for connection established
        while let Ok(_) = video_ice_connection_state_rx.changed().await {
            let state = *video_ice_connection_state_rx.borrow_and_update();
            match state {
                WebRTConnectionState::Init => {
                    log::info!("current state is {}, keep wait", state);
                }
                WebRTConnectionState::Connected => {
                    log::info!("RTC is connected");
                    break;
                }
                _ => {
                    log::error!("Unexcepted state {}, exit to capture screen", state);
                    return DeskError::custom_error(
                        ErrorCode::SYSTEM_ERROR,
                        format!("Unexcepted state {}", state),
                    );
                }
            }
        }

        log::info!("Start to capture screen and send to peer");

        // It is important to use a time.Ticker instead of time.Sleep because
        // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
        // * works around latency issues with Sleep
        let mut ticker = tokio::time::interval(Duration::from_millis(3));
        loop {
            log::debug!("begin caption scrren");
            let start = Instant::now();
            let nal_info_result = h264_screen_output.get_nal();
            if nal_info_result.is_err() {
                if let Err(DeskError::CustomError(err)) = nal_info_result {
                    if err.error_code == ErrorCode::CAPTURE_SCREEN_TIMEOUT_ERROR {
                        continue;
                    }
                    log::error!("Failed to get nal info, custom error={}", err);
                    continue;
                }
                log::error!(
                    "Failed to get nal info, error={}",
                    nal_info_result.err().unwrap()
                );
                continue;
            }
            let nal_info = nal_info_result.unwrap();

            let time1 = start.elapsed();
            log::debug!("caption scrren time: {} μs", time1.as_micros(),);
            video_track
                .write_sample(&Sample {
                    data: nal_info.nal_bytes,
                    duration: Duration::from_secs(1),
                    ..Default::default()
                })
                .await?;
            let time2 = start.elapsed();
            log::debug!(
                "write sample time: {} μs",
                time2.as_micros() - time1.as_micros(),
            );
            tokio::select! {
             _ = ticker.tick() => {},
             _ = video_ice_connection_state_rx.changed() => {
                let state = *video_ice_connection_state_rx.borrow_and_update();
                match state {
                    WebRTConnectionState::Init => {
                        log::warn!("current state is {}, it should be happened?", state);
                    },
                    WebRTConnectionState::Connected => {
                        log::warn!("RTC is connected");

                    },
                    _ => {
                        log::error!("Unexcepted state {}, exit to capture screen", state);
                        break;
                    },
                }
             },
            }
        }
        Result::<(), DeskError>::Ok(())
    }

    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskError> {
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            SIGNALING_TYPE_CODE_INIT => {} // handle_hello(session, user),
            SIGNALING_TYPE_CODE_OFFER => {
                self.handle_offer(&signaling_model).await?;
            }
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
        log::info!("Received offer: {}", signaling_data);
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
        let local_desc = option.unwrap();
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
