use std::sync::{Arc, LazyLock};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use actix_ws::{CloseCode, CloseReason};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::signal::{
    InitSignalingData, LcxlRTCIceServer, OfferModel, SignalingModel, SignalingSessionExt,
    SignalingState, SignalingType, WebRTConnectionState,
};
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;
use log::{error, info, warn};
use prometheus::{HistogramVec, register_histogram_vec};
use tokio::task::LocalSet;
use tokio::time::Duration;
use tokio::time::Instant;
use turn_server::config::Transport;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9};
use webrtc::data_channel::RTCDataChannel;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_H264, MediaEngine},
    },
    ice_transport::{
        ice_connection_state::RTCIceConnectionState, ice_gatherer_state::RTCIceGathererState,
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
};

use crate::model::data_channel::SignalRequestControlData;
use crate::model::video_encoder::{VideoEncoderType, VideoEncoderTypeHelper};
use crate::service::audio_capture::audio_capture_factory::{
    create_audio_capture, list_audio_capture,
};
use crate::service::audio_encoder::audio_encoder_factory::{
    create_audio_encoder, list_audio_encoder,
};
use crate::service::data_channel::handle_data_channel_event;
use crate::service::image_capture::image_capture_factory::{
    create_image_capture, list_image_capture,
};
use crate::service::video_encoder::video_encoder_factory::{
    create_video_encoder, list_video_encoder,
};
use crate::{error::DeskError, model::settings::SharedSettings};

pub static CAPTURE_SCREEN_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("capture_screen_histogram", "help", &["type"]).unwrap()
});
pub static WEBRTC_WRITE_SAMPLE_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("webrtc_write_sample_histogram", "help", &["type"]).unwrap()
});

pub async fn handle_signaling(
    settings: web::Data<SharedSettings>,
    stream: AggregatedMessageStream,
    session: Session,
    user: CurrentUser,
) -> Result<(), DeskError> {
    info!("Handling signaling");
    // Handle signaling logic here
    let mut signaling_context = SignalingContext::init_signaling(settings, session, user).await?;

    let result = do_handle_signaling(&mut signaling_context, stream).await;
    // Shutdown function must be invoked to clean up resources.
    signaling_context.shutdown().await?;
    result
}

pub async fn do_handle_signaling(
    signaling_context: &mut SignalingContext,
    mut stream: AggregatedMessageStream,
) -> Result<(), DeskError> {
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
    Ok(())
}
/// Signaling context for handling WebSocket messages.
pub struct SignalingContext {
    pub settings: web::Data<SharedSettings>,
    pub session: Session,
    pub user: CurrentUser,
    /// RTC peer connection
    pub rtc_peer_connection: Arc<RTCPeerConnection>,
    /// Capture screen thread handle
    pub capture_screen_thread: Option<std::thread::JoinHandle<()>>,
    /// Capture audio thread handle
    pub capture_audio_thread: Option<std::thread::JoinHandle<()>>,

    /// Signaling state
    pub signaling_state: Arc<tokio::sync::RwLock<SignalingState>>,
    /// Tokio watch sender for WebRTConnectionState updates
    pub update_setting_sender: Option<tokio::sync::watch::Sender<WebRTConnectionState>>,
}

impl SignalingContext {
    pub async fn init_signaling(
        settings: web::Data<SharedSettings>,
        mut session: Session,
        user: CurrentUser,
    ) -> Result<Self, DeskError> {
        let local_settings = {
            let shared_settings = settings.read().await;
            shared_settings.clone()
        };
        let mut stun_urls = Vec::<String>::new();
        let mut turn_urls = Vec::<String>::new();
        for interface in local_settings.turn.interfaces.iter() {
            if interface.transport == Transport::TCP {
                turn_urls.push(format!(
                    "turn:{}?transport=tcp",
                    interface.external.to_string()
                ));
            } else {
                if local_settings.turn.enable_stun {
                    stun_urls.push(format!("stun:{}", interface.external.to_string()));
                }
                if local_settings.turn.enable_turn {
                    turn_urls.push(format!("turn:{}", interface.external.to_string()));
                }
            }
        }
        let mut ice_servers = Vec::new();
        let mut client_ice_servers = Vec::new();

        if !stun_urls.is_empty() {
            let ice_stun_server = RTCIceServer {
                urls: stun_urls,
                ..Default::default()
            };
            ice_servers.push(ice_stun_server.clone());
            client_ice_servers.push(ice_stun_server);
        }

        if !turn_urls.is_empty() {
            let ice_turn_server = RTCIceServer {
                urls: turn_urls,
                username: local_settings.user.login_user_name.clone(),
                credential: local_settings.user.login_password.clone(),
            };
            // Only add TURN server to client configuration, not server configuration
            // forcing server to use Host candidates or STUN only.
            // This avoids "Self-Reflective Relay" (Hairpinning) issues on local machine.
            client_ice_servers.push(ice_turn_server);
        }

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

        // Prepare the configuration for Server (use only STUN/Host)
        let config = RTCConfiguration {
            ice_servers: ice_servers.clone(),
            ..Default::default()
        };

        // Create a new RTCPeerConnection
        let rtc_peer_connection = Arc::new(api.new_peer_connection(config).await?);

        // get audio device
        let audio_device_list = list_audio_capture();
        let audio_encoder_list = list_audio_encoder();
        // get video device
        let video_device_list = list_image_capture();

        let video_encoder_list = list_video_encoder();

        let init_signaling_data = InitSignalingData {
            ice_servers: client_ice_servers
                .iter()
                .map(|s| LcxlRTCIceServer::from(s.clone()))
                .collect(),
            user_name: user.name.clone(),
            audio_device_list,
            audio_encoder_list,
            video_device_list,
            video_encoder_list,
            desk_settings: local_settings.desk,
        };

        info!("Sending init signaling: {:?}", init_signaling_data);
        session
            .send_signaling(SignalingType::Init, &init_signaling_data)
            .await?;
        info!("Sent init signaling");

        Ok(Self {
            settings,
            session,
            user,
            rtc_peer_connection,
            capture_screen_thread: None,
            capture_audio_thread: None,
            signaling_state: Arc::new(tokio::sync::RwLock::new(SignalingState::default())),
            update_setting_sender: None,
        })
    }

    /// Starts the WebRTC connection
    pub async fn start_webrtc(&mut self, offer_model: &OfferModel) -> Result<(), DeskError> {
        let (ice_state_change_sender, ice_connection_state_rx) =
            tokio::sync::watch::channel(WebRTConnectionState::Init);
        let peer_state_change_sender = ice_state_change_sender.clone();
        let update_setting_sender = ice_state_change_sender.clone();
        let video_state_receiver = ice_connection_state_rx.clone();
        let audio_state_receiver = ice_connection_state_rx.clone();
        let video_mime_type = match offer_model.desk_settings.get_video_encoder_type()? {
            VideoEncoderType::H264 => MIME_TYPE_H264,
            VideoEncoderType::VP8 => MIME_TYPE_VP8,
            VideoEncoderType::VP9 => MIME_TYPE_VP9,
        };
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: video_mime_type.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        // Add this newly created track to the PeerConnection
        let rtp_sender = self
            .rtc_peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            log::info!("Start to read incoming video RTCP packets");
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
            log::info!("Finished to read incoming video RTCP packets");
            Result::<(), DeskError>::Ok(())
        });

        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "webrtc-rs".to_owned(),
        ));

        // Add this newly created track to the PeerConnection
        let rtp_sender = self
            .rtc_peer_connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        // Read incoming RTCP packets
        // Before these packets are returned they are processed by interceptors. For things
        // like NACK this needs to be called.
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            log::info!("Start to read incoming audio RTCP packets");
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
            log::info!("Finished to read incoming audio RTCP packets");
            Result::<(), DeskError>::Ok(())
        });

        let session_for_video = self.session.clone();

        let local_settings = self.settings.read().await.clone();

        // Spawn a blocking task to capture screen and send video
        let desk_settings = offer_model.desk_settings.clone();
        let signaling_state_for_screen = self.signaling_state.clone();

        let capture_screen_thread = std::thread::spawn(move || {
            let local = LocalSet::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            local.spawn_local(async move {
                let result = SignalingContext::capture_screen_task(
                    signaling_state_for_screen,
                    desk_settings,
                    video_state_receiver,
                    video_track,
                )
                .await;

                if let Err(error) = result {
                    log::error!("Capture screen task failed, error: {:?}", error);
                    session_for_video
                        .close(Some(CloseReason::from((
                            CloseCode::Abnormal,
                            format!("{:?}", error),
                        ))))
                        .await?;
                    return Err(error);
                }
                log::info!("Capture screen task completed successfully");
                return result;
            });

            // This will return once all senders are dropped and all
            // spawned tasks have returned.
            rt.block_on(local);
        });
        self.capture_screen_thread = Some(capture_screen_thread);

        let session_for_audio = self.session.clone();

        let audio_settings = local_settings.clone();
        let audio_device = offer_model.desk_settings.audio_device.clone();
        if let Some(audio_device) = audio_device {
            log::info!("Start to capture audio with device: {:?}", audio_device);

            let capture_audio_thread = std::thread::spawn(move || {
                let local = LocalSet::new();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                local.spawn_local(async move {
                    let result = SignalingContext::capture_audio_task(
                        audio_settings.desk,
                        audio_state_receiver,
                        audio_track,
                    )
                    .await;

                    if let Err(error) = result {
                        log::error!("Capture audio task failed, error: {:?}", error);
                        session_for_audio
                            .close(Some(CloseReason::from((
                                CloseCode::Abnormal,
                                format!("{:?}", error),
                            ))))
                            .await?;
                        return Err(error);
                    }
                    log::info!("Capture audio task completed");
                    return result;
                });

                // This will return once all senders are dropped and all
                // spawned tasks have returned.
                rt.block_on(local);
            });

            self.capture_audio_thread = Some(capture_audio_thread);
        } else {
            log::info!("Will not capture audio because no device is selected");
        }

        // Set the handler for ICE connection state
        // This will notify you when the peer has connected/disconnected
        self.rtc_peer_connection
            .on_ice_connection_state_change(Box::new(
                move |connection_state: RTCIceConnectionState| {
                    log::info!("RTC ice connection state has changed {connection_state}");
                    let state = WebRTConnectionState::from(&connection_state);
                    if state != WebRTConnectionState::Init {
                        if let Err(error) = ice_state_change_sender.send(state) {
                            log::error!("Failed to send connection state: {:?}", error);
                        }
                    }

                    Box::pin(async {})
                },
            ));

        // Set the handler for Peer connection state
        // This will notify you when the peer has connected/disconnected
        self.rtc_peer_connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                log::info!("Peer connection state has changed: {s}");
                let state = WebRTConnectionState::from(&s);
                if state == WebRTConnectionState::Closed {
                    if let Err(error) = peer_state_change_sender.send(state) {
                        log::error!("Failed to send connection state: {:?}", error);
                    }
                }

                Box::pin(async {})
            }));

        // Set the handler for ICE gathering state
        // This will notify you when the ICE gathering state has changed
        self.rtc_peer_connection
            .on_ice_gathering_state_change(Box::new(move |s: RTCIceGathererState| {
                info!("ICE gathering state has changed: {s}");
                Box::pin(async {})
            }));

        // Register data channel creation handling
        // Used for mouse event, keyboard event, clipboard manage, file copy, etc.
        let signaling_state_for_data_channel = self.signaling_state.clone();
        self.rtc_peer_connection
            .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
                let d_label = d.label().to_owned();
                let d_id = d.id();
                log::info!("New DataChannel {d_label} {d_id}");
                let signaling_state = signaling_state_for_data_channel.clone();
                // Register channel opening handling
                Box::pin(async move {
                    let result = handle_data_channel_event(signaling_state, d.clone()).await;
                    if let Err(error) = result {
                        log::error!("Failed to handle data channel event: {:?}", error);
                    }
                })
            }));
        self.update_setting_sender = Some(update_setting_sender);
        Ok(())
    }

    /// Shutdown the signaling context, including peer connection and capture tasks.
    pub async fn shutdown(self) -> Result<(), DeskError> {
        let result = self.rtc_peer_connection.close().await;
        info!("Signaling session ended, result={:?}", result);

        info!("Begin to shutdown capture screen&audio runtime");
        if let Some(capture_screen_thread) = self.capture_screen_thread {
            capture_screen_thread.join()?;
        }
        if let Some(capture_audio_thread) = self.capture_audio_thread {
            capture_audio_thread.join()?;
        }

        info!("End to shutdown capture screen&audio runtime");

        Ok(())
    }

    /// Start the screen capture task
    pub async fn capture_screen_task(
        signaling_state: Arc<tokio::sync::RwLock<SignalingState>>,
        desk_settings: DeskSettings,
        mut connection_state_rx: tokio::sync::watch::Receiver<WebRTConnectionState>,
        video_track: Arc<TrackLocalStaticSample>,
    ) -> Result<(), DeskError> {
        let mut desk_settings = desk_settings;
        log::info!(
            "Preparing to capture screen, desk settings: {:?}",
            desk_settings
        );
        let mut capture = create_image_capture(&desk_settings)?;
        let mut image_capture_type = capture.get_capture_type().into();
        //TODO
        let display_info = capture.get_current_output()?;
        {
            let mut signaling_state = signaling_state.write().await;
            signaling_state.display_info = display_info.clone();
            log::info!(
                "Set initial display info: {:?}",
                signaling_state.display_info
            );
        }

        let mut encoder = create_video_encoder(&desk_settings, &display_info)?;
        // Wait for connection established
        while let Ok(_) = connection_state_rx.changed().await {
            let state = connection_state_rx.borrow_and_update().clone();
            match state {
                WebRTConnectionState::Init => {
                    log::info!("current state is {}, keep wait", state);
                }
                WebRTConnectionState::Connected => {
                    log::info!("capture_screen_task: RTC is connected");
                    break;
                }
                _ => {
                    log::error!("Unexcepted state {}, exit to capture screen", state);
                    return DeskError::custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        format!("Unexcepted state {}", state),
                    );
                }
            }
        }

        log::info!("Start to capture screen and send to peer");

        // It is important to use a time.Ticker instead of time.Sleep because
        // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
        // * works around latency issues with Sleep
        let mut ticker = tokio::time::interval(desk_settings.get_duration_by_video_fps());
        loop {
            //ticker = tokio::time::interval(Duration::from_millis(3));
            // check if the connection is still alive
            tokio::select! {
             _ = ticker.tick() => {},
             _ = connection_state_rx.changed() => {
                let state = connection_state_rx.borrow_and_update().clone();
                match state {
                    WebRTConnectionState::Init => {
                        log::warn!("current state is {}, it should be happened?", state);
                    },
                    WebRTConnectionState::Connected => {
                        log::warn!("capture_screen_task: RTC is connected again?");

                    },
                    WebRTConnectionState::UpdateSettings(new_desk_setting)=> {
                        log::info!("update settings {:?}", new_desk_setting);
                        // update desk settings with new values
                        desk_settings = new_desk_setting;
                        // update ticker interval based on new settings
                        ticker = tokio::time::interval(desk_settings.get_duration_by_video_fps());
                        image_capture_type = capture.get_capture_type().into();
                    },
                    _ => {
                        log::error!("Unexcepted state {}, exit to capture screen", state);
                        break;
                    },
                }
             },
            }
            log::trace!("begin caption scrren");
            let timer = CAPTURE_SCREEN_HISTOGRAM
                .with_label_values(&[image_capture_type])
                .start_timer();
            let image_info_result = capture.capture(desk_settings.show_mouse);

            let image_info = match image_info_result {
                Ok(image_info) => {
                    timer.stop_and_record();
                    image_info
                }
                Err(err) => {
                    if let DeskError::CustomError(custom_error) = err {
                        if custom_error.error_code == DeskErrorCode::ACTION_NEED_RETRY {
                            timer.stop_and_discard();
                            continue;
                        }
                        log::error!("Failed to get nal info, custom error={}", custom_error);
                        continue;
                    }
                    log::error!("Failed to get nal info, error={:?}", err);
                    continue;
                }
            };

            let nal_info_vec = encoder.encode(image_info.as_ref())?;
            for nal_info in nal_info_vec {
                let timer = WEBRTC_WRITE_SAMPLE_HISTOGRAM
                    .with_label_values(&["video"])
                    .start_timer();
                video_track
                    .write_sample(&Sample {
                        data: nal_info.nal_bytes,
                        duration: Duration::from_secs(1),
                        ..Default::default()
                    })
                    .await?;
                timer.stop_and_record();
            }
        }
        Result::<(), DeskError>::Ok(())
    }

    /// Capture audio and send it to the remote peer
    pub async fn capture_audio_task(
        desk_settings: DeskSettings,
        mut connection_state_rx: tokio::sync::watch::Receiver<WebRTConnectionState>,
        audio_track: Arc<TrackLocalStaticSample>,
    ) -> Result<(), DeskError> {
        log::info!(
            "Preparing to capture audio, desk_settings={:?}",
            desk_settings
        );
        let mut capture = create_audio_capture(&desk_settings)?;

        //let mut opus_audio_capture = OpusAudioCapture::new(audio_device)?;

        // Wait for connection established
        while let Ok(_) = connection_state_rx.changed().await {
            let state = connection_state_rx.borrow_and_update().clone();
            match state {
                WebRTConnectionState::Init => {
                    log::info!("current state is {}, keep wait", state);
                }
                WebRTConnectionState::Connected => {
                    log::info!("capture_audio_task: RTC is connected");
                    break;
                }
                _ => {
                    log::error!("Unexcepted state {}, exit to capture audio", state);
                    return DeskError::custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        format!("Unexcepted state {}", state),
                    );
                }
            }
        }

        log::info!("Start to capture audio and send to peer");
        //opus_audio_capture.start()?;
        let wave_format = capture.start()?;
        let mut encoder = create_audio_encoder(&desk_settings, wave_format)?;
        // sleep 5ms
        let mills = 5u64;
        // It is important to use a time.Ticker instead of time.Sleep because
        // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
        // * works around latency issues with Sleep
        let mut ticker = tokio::time::interval(Duration::from_millis(mills));
        loop {
            // check if the connection is still alive
            tokio::select! {
             _ = ticker.tick() => {},
             _ = connection_state_rx.changed() => {
                let state = connection_state_rx.borrow_and_update().clone();
                match state {
                    WebRTConnectionState::Init => {
                        log::warn!("current state is {}, it should be happened?", state);
                    },
                    WebRTConnectionState::Connected => {
                        log::warn!("capture_audio_task: RTC is connected again?");
                    },
                    WebRTConnectionState::UpdateSettings(desk_setting)=> {
                        log::info!("update settings {:?}", desk_setting);
                    },
                    _ => {
                        log::error!("Unexcepted state {}, exit to capture audio", state);
                        break;
                    },
                }
             },
            }
            log::trace!("begin capture audio");
            loop {
                let start = Instant::now();
                //let buffer = opus_audio_capture.get_buffer()?;
                let result = capture.get_buffer();
                if result.is_err() {
                    if let Err(DeskError::CustomError(ref err)) = result {
                        if err.error_code == DeskErrorCode::ACTION_NEED_RETRY {
                            // recreate audio capture
                            log::warn!("Failed to get audio buffer, recreate audio capture");
                            capture = create_audio_capture(&desk_settings)?;
                            capture.start()?;
                            continue;
                        }
                    }
                    log::error!("Failed to get audio buffer, error: {:?}", result.err());
                    break;
                }

                let buffer = result?;

                let buffer = encoder.encode(buffer.as_ref())?;

                let time1 = start.elapsed();
                log::trace!(
                    "capture audio time: {} μs, buffer len: {}",
                    time1.as_micros(),
                    buffer.data.len(),
                );
                if buffer.data.is_empty() {
                    break;
                }
                let timer = WEBRTC_WRITE_SAMPLE_HISTOGRAM
                    .with_label_values(&["audio"])
                    .start_timer();
                audio_track
                    .write_sample(&Sample {
                        data: Bytes::copy_from_slice(buffer.data.as_slice()),
                        //TODO sleep 20ms
                        duration: Duration::from_millis(20),
                        ..Default::default()
                    })
                    .await?;
                timer.stop_and_record();
            }
        }
        //opus_audio_capture.stop()?;
        capture.stop()?;
        Result::<(), DeskError>::Ok(())
    }

    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskError> {
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;

        if signaling_model.signaling_data == None {
            self.session
                .send_error(
                    signaling_model.signaling_type.into(),
                    DeskErrorCode::BLANK_SIGNALING_DATA,
                    "No signaling data provided",
                )
                .await?;
            return Ok(());
        }

        match signaling_model.signaling_type {
            SignalingType::Init => {} // handle_hello(session, user),
            SignalingType::Offer => {
                self.handle_offer(&signaling_model).await?;
            }
            SignalingType::Answer => {}
            SignalingType::Canid => {}
            SignalingType::UpdateDeskSettings => {
                self.handle_update_desk_settings(&signaling_model).await?;
            }
            SignalingType::RequireControl => {
                // send back a message to client
                self.handle_request_control(&signaling_model).await?;
            }
            _ => {
                error!(
                    "Unknown signaling type: {:?}",
                    signaling_model.signaling_type
                );

                self.session
                    .send_signaling(
                        SignalingType::Unknown,
                        &format!(
                            "Failed to handle signaling type: {:?}",
                            signaling_model.signaling_type
                        ),
                    )
                    .await?;
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
        let offer_model = signaling_model.get_data::<OfferModel>()?;

        // start webrtc first
        self.start_webrtc(&offer_model).await?;
        // Set the remote SessionDescription
        self.rtc_peer_connection
            .set_remote_description(offer_model.offer)
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
        let local_desc = if let Some(local_desc) = option {
            local_desc
        } else {
            self.session
                .send_error(
                    signaling_model.signaling_type.into(),
                    DeskErrorCode::GENERATE_LOCAL_DESCRIPTION_FAILED,
                    "generate local_description failed!",
                )
                .await?;
            return Ok(());
        };

        log::info!("local description: {:?}", local_desc);

        self.session
            .send_signaling(SignalingType::Answer, &local_desc)
            .await?;
        // Save to config file
        {
            let mut settings = self.settings.write().await;
            settings.desk = offer_model.desk_settings;
            settings.save()?;
        }

        Ok(())
    }

    /// Handle update desk settings signaling
    pub async fn handle_update_desk_settings(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let desk_settings = signaling_model.get_data_with_default::<DeskSettings>()?;
        if let Some(ref sender) = self.update_setting_sender {
            log::info!("Sending update desk settings: {:?}", desk_settings);
            sender.send(WebRTConnectionState::UpdateSettings(desk_settings))?;
        } else {
            log::error!("Update setting sender is not set");
        }
        Ok(())
    }
    /// Handle request control signaling
    pub async fn handle_request_control(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        // TODO need implement more logic here
        let request_control_data =
            signaling_model.get_data_with_default::<SignalRequestControlData>()?;
        log::info!("Request control data: {:?}", request_control_data);
        self.signaling_state.write().await.accept_control = request_control_data.accept;
        let signal_type = if request_control_data.accept {
            SignalingType::AcceptControl
        } else {
            SignalingType::CloseControl
        };
        self.session
            .send_signaling(signal_type, &request_control_data)
            .await?;
        Ok(())
    }
}
