use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use actix_web::web;
use awc::{Client, Connector};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::signal::{
    InitSignalingData, LcxlRTCIceServer, OfferModel, PeerSignalingSender, RemoteDeskTypeEnum,
    SignalingModel, SignalingState, SignalingType, WebRTConnectionState,
};
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalOutputData, TerminalResizeData,
};
use desk_signal_facade::{error::DeskSignalFacadeError, model::version::VersionInfo};
use desk_utils::error::{CustomDeskError, DeskErrorCode};

use futures_util::{SinkExt, StreamExt};

use log::{error, info, warn};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use prometheus::{HistogramVec, register_histogram_vec};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tokio::time::Instant;
use turn_server::config::Transport;
use url::Url;
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
use crate::model::login::{LoginParams, LoginResult};
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
use crate::service::terminal::fetch_terminal_list;
use crate::service::video_encoder::video_encoder_factory::{
    create_video_encoder, list_video_encoder,
};
use crate::version;
use crate::{error::DeskError, model::settings::SharedSettings};
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams};

pub static CAPTURE_SCREEN_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("capture_screen_histogram", "help", &["type"]).unwrap()
});
pub static WEBRTC_WRITE_SAMPLE_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("webrtc_write_sample_histogram", "help", &["type"]).unwrap()
});

#[derive(Debug)]
pub enum DeskSessionMessage {
    Text(ByteString),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close,
}

#[derive(Clone)]
pub struct DeskSessionSender {
    sender: mpsc::UnboundedSender<DeskSessionMessage>,
}

impl PeerSignalingSender for DeskSessionSender {
    async fn send_response<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_session_id: Option<String>,
        signaling_data: &T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: ?Sized + Serialize + Sync,
    {
        let signaling_model = SignalingModel::success_response(
            request_id,
            signaling_type,
            None,
            to_session_id,
            Some(signaling_data),
        )?;
        let text = serde_json::to_string(&signaling_model)?;
        self.sender
            .send(DeskSessionMessage::Text(ByteString::from(text)))
            .map_err(|e| {
                DeskSignalFacadeError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to send signaling message: {:?}", e),
                ))
            })?;
        Ok(())
    }

    async fn send_error(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_session_id: Option<String>,
        error_code: DeskErrorCode,
        error_message: &str,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::error(
            request_id,
            signaling_type,
            None,
            to_session_id,
            error_code,
            error_message,
        )?;
        let text = serde_json::to_string(&signaling_model)?;
        self.sender
            .send(DeskSessionMessage::Text(ByteString::from(text)))
            .map_err(|e| {
                DeskSignalFacadeError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to send error message: {:?}", e),
                ))
            })?;
        Ok(())
    }

    async fn send_to_peer<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_session_id: &str,
        data: T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: Serialize + Sync + Send,
    {
        self.send_response(
            request_id,
            signaling_type,
            Some(to_session_id.to_owned()),
            &data,
        )
        .await
    }
}

/// Handle incoming websocket message
async fn handle_incoming_ws_message(
    msg: Option<Result<awc::ws::Frame, awc::error::WsProtocolError>>,
    desk_session: &mut DeskSession,
    tx: &mpsc::UnboundedSender<DeskSessionMessage>,
) -> Result<bool, DeskError> {
    match msg {
        Some(Ok(frame)) => match frame {
            awc::ws::Frame::Text(text) => {
                let text_str = match std::str::from_utf8(&text) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Invalid UTF-8 text: {:?}", e);
                        return Ok(false);
                    }
                };
                let signaling_model = serde_json::from_str::<SignalingModel>(text_str)?;
                if let Err(e) = desk_session.handle_message(&signaling_model).await {
                    log::warn!(
                        "Error handling message, request_id: {}, signaling_type: {}, from_session_id: {:?}, to_session_id: {:?}, e: {:?}",
                        signaling_model.request_id,
                        signaling_model.signaling_type,
                        signaling_model.from_session_id,
                        signaling_model.to_session_id,
                        e
                    );

                    desk_session
                        .session
                        .send_error(
                            &signaling_model.request_id,
                            signaling_model.signaling_type.into(),
                            signaling_model.from_session_id.clone(),
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!("Error handling message: {:?}", e),
                        )
                        .await?;
                }
            }
            awc::ws::Frame::Binary(bin) => {
                if let Err(e) = desk_session.binary(bin).await {
                    error!("Error handling binary: {:?}", e);
                }
            }
            awc::ws::Frame::Ping(msg) => {
                let _ = tx.send(DeskSessionMessage::Pong(msg));
            }
            awc::ws::Frame::Pong(_) => {}
            awc::ws::Frame::Close(reason) => {
                warn!("WS close frame received: {:?}", reason);
                return Ok(true);
            }
            awc::ws::Frame::Continuation(_) => {}
        },
        Some(Err(e)) => {
            error!("WS error: {:?}", e);
            return Ok(true);
        }
        None => {
            warn!("WS stream closed");
            return Ok(true);
        }
    }
    Ok(false)
}

/// Handle outgoing websocket message
async fn handle_outgoing_channel_message<S>(msg: Option<DeskSessionMessage>, sink: &mut S) -> bool
where
    S: SinkExt<awc::ws::Message, Error = awc::error::WsProtocolError> + Unpin,
{
    match msg {
        Some(DeskSessionMessage::Text(text)) => {
            if let Err(e) = sink.send(awc::ws::Message::Text(text)).await {
                error!("Failed to send text: {:?}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Binary(bin)) => {
            if let Err(e) = sink.send(awc::ws::Message::Binary(bin)).await {
                error!("Failed to send binary: {:?}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Ping(msg)) => {
            if let Err(e) = sink.send(awc::ws::Message::Ping(msg)).await {
                error!("Failed to send ping: {:?}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Pong(msg)) => {
            if let Err(e) = sink.send(awc::ws::Message::Pong(msg)).await {
                error!("Failed to send pong: {:?}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Close) => {
            let _ = sink.close().await;
            return true;
        }
        None => return true,
    }
    false
}

pub async fn start_desk_session(settings: web::Data<SharedSettings>) -> Result<(), DeskError> {
    let signaling_url = {
        let settings = settings.read().await;
        if let Some(url) = &settings.system.signaling_url {
            url.clone()
        } else if settings.system.enable_ipv6 {
            format!("ws://[::1]:{}/api/desk/signaling", settings.system.port)
        } else {
            format!("ws://127.0.0.1:{}/api/desk/signaling", settings.system.port)
        }
    };
    // determine the root url from signaling url
    let parsed_url = Url::parse(&signaling_url)?;
    let root_url = parsed_url.origin().ascii_serialization();

    let login_url = format!("{}/api/login/account", root_url);
    // http => http, ws => http, wss => https
    let login_url = if login_url.starts_with("ws://") {
        login_url.replace("ws://", "http://")
    } else if login_url.starts_with("wss://") {
        login_url.replace("wss://", "https://")
    } else {
        login_url
    };

    let display_name = {
        let settings = settings.read().await;
        settings.desk.display_name.clone()
    };

    let display_name = if display_name.is_some() {
        display_name
    } else {
        sysinfo::System::host_name()
    };

    let version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        version::SERVER_BUILD_NUMBER,
        version::SERVER_COMMIT_HASH.to_string(),
        RemoteDeskTypeEnum::Server,
        display_name,
    );
    let version_query = serde_urlencoded::to_string(&version_info).unwrap();

    loop {
        // Create awc client
        let mut root_store = RootCertStore::empty();
        for cert in load_native_certs().expect("could not load platform certs") {
            root_store.add(cert).unwrap();
        }

        let client = Client::builder()
            .connector(
                Connector::new()
                    .timeout(Duration::from_secs(10))
                    .rustls_0_23(Arc::new(
                        ClientConfig::builder()
                            .with_root_certificates(Arc::new(root_store))
                            .with_no_client_auth(),
                    )),
            )
            .finish();

        info!("Connecting to signaling server: {}", signaling_url);
        // Login first
        let (username, password) = {
            let settings = settings.read().await;
            (
                settings.user.login_user_name.clone(),
                settings.user.login_password.clone(),
            )
        };

        let login_params = LoginParams {
            username: username.clone(),
            password: password.clone(),
            login_type: "account".to_string(), // TODO: use enum
            auto_login: true,
        };

        let mut login_response = match client.post(&login_url).send_json(&login_params).await {
            Ok(res) => res,
            Err(e) => {
                error!("Failed to login: {:?}", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        if !login_response.status().is_success() {
            error!("Login failed: {:?}", login_response.status());
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let cookie = if let Some(cookie) = login_response.cookie("id") {
            cookie
        } else {
            error!("Login failed: no cookie");
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };

        let login_result = login_response.json::<LoginResult>().await?;
        if login_result.status != "ok" {
            // it should not happen, just for safety
            error!("Login failed: {}", login_result.status);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        if login_result.api_version < desk_server_version::SERVER_API_VERSION {
            error!(
                "Login failed: api version of signaling/manage server is too old, please upgrade server"
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }
        // Connect to websocket
        let connect_url = format!("{}?{}", signaling_url, version_query);
        let (response, framed) = match client.ws(&connect_url).cookie(cookie).connect().await {
            Ok(res) => res,
            Err(e) => {
                error!(
                    "Failed to connect to signaling server: {:?}, url: {}",
                    e, connect_url
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        info!("Connected to signaling server: {:?}", response);

        let (mut sink, mut stream) = framed.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<DeskSessionMessage>();
        let session_sender = DeskSessionSender { sender: tx.clone() };

        let mut desk_session = match DeskSession::new(
            settings.clone(),
            session_sender,
            CurrentUser::new_admin(&username),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to init desk session: {:?}", e);
                break Ok(());
            }
        };

        // Main loop
        loop {
            tokio::select! {
                msg = stream.next() => {
                    if handle_incoming_ws_message(msg, &mut desk_session, &tx).await? {
                        break;
                    }
                }
                msg = rx.recv() => {
                     if handle_outgoing_channel_message(msg, &mut sink).await {
                        break;
                    }
                }
            }
        }

        info!("Desk session ended, cleaning up...");

        if let Err(e) = desk_session.shutdown().await {
            error!("Error shutdown desk session: {:?}", e);
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
        info!("Reconnecting...");
    }
}
/// Peer connection for handling WebRTC connections.
pub struct PeerConnection {
    /// RTC peer connection
    pub rtc_peer_connection: RTCPeerConnection,
    /// Capture screen thread handle
    pub capture_screen_thread: Option<std::thread::JoinHandle<()>>,
    /// Capture audio thread handle
    pub capture_audio_thread: Option<std::thread::JoinHandle<()>>,
    /// Signaling state
    pub signaling_state: Arc<tokio::sync::RwLock<SignalingState>>,
}

impl PeerConnection {
    /// Shutdown the signaling context, including peer connection and capture tasks.
    pub async fn shutdown(&self) -> Result<(), DeskError> {
        let result = self.rtc_peer_connection.close().await;
        info!("Signaling session ended, result={:?}", result);

        Ok(())
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        info!("Begin to shutdown capture screen&audio runtime");
        if let Some(capture_screen_thread) = self.capture_screen_thread.take() {
            capture_screen_thread.join().unwrap();
        }
        if let Some(capture_audio_thread) = self.capture_audio_thread.take() {
            capture_audio_thread.join().unwrap();
        }

        info!("End to shutdown capture screen&audio runtime");
    }
}

/// Signaling context for handling WebSocket messages.
pub struct DeskSession {
    pub settings: web::Data<SharedSettings>,
    pub session: DeskSessionSender,
    pub user: CurrentUser,
    /// RTC peer connection map, key is from_session_id
    pub rtc_peer_connection_map: HashMap<String, Arc<tokio::sync::RwLock<PeerConnection>>>,
    /// Tokio watch sender for WebRTConnectionState updates
    pub update_setting_sender: Option<tokio::sync::watch::Sender<WebRTConnectionState>>,
    /// Terminal map: from_session_id -> (MasterPty, Child)
    pub terminal_map: HashMap<String, (Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>)>,
}

impl DeskSession {
    pub async fn new(
        settings: web::Data<SharedSettings>,
        session: DeskSessionSender,
        user: CurrentUser,
    ) -> Result<Self, DeskError> {
        Ok(Self {
            settings,
            session,
            user,
            rtc_peer_connection_map: HashMap::new(),
            update_setting_sender: None,
            terminal_map: HashMap::new(),
        })
    }
    pub async fn init_ptc_peer_connection(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;

        if self.rtc_peer_connection_map.contains_key(&from_session_id) {
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Peer connection already exists",
            );
        }

        let local_settings = {
            let shared_settings = self.settings.read().await;
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
        let rtc_peer_connection = api.new_peer_connection(config).await?;

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
            user_name: self.user.name.clone(),
            audio_device_list,
            audio_encoder_list,
            video_device_list,
            video_encoder_list,
            desk_settings: local_settings.desk,
        };

        info!("Sending init signaling: {:?}", init_signaling_data);
        self.session
            .send_to_peer(
                &signaling_model.request_id,
                SignalingType::Init,
                &from_session_id,
                init_signaling_data,
            )
            .await?;
        info!("Sent init signaling");

        self.rtc_peer_connection_map.insert(
            from_session_id,
            Arc::new(tokio::sync::RwLock::new(PeerConnection {
                rtc_peer_connection,
                capture_screen_thread: None,
                capture_audio_thread: None,
                signaling_state: Arc::new(tokio::sync::RwLock::new(SignalingState::default())),
            })),
        );
        Ok(())
    }

    /// Get the RTC peer connection, if not initialized, return error
    pub fn get_rtc_peer_connection(
        &self,
        from_session_id: &str,
    ) -> Result<Arc<tokio::sync::RwLock<PeerConnection>>, DeskError> {
        if let Some(rtc_peer_connection) = self.rtc_peer_connection_map.get(from_session_id) {
            Ok(rtc_peer_connection.clone())
        } else {
            DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "RTC peer connection not initialized",
            )
        }
    }

    /// Starts the WebRTC connection
    pub async fn start_webrtc(
        &mut self,
        offer_model: &OfferModel,
        peer_connection: &mut PeerConnection,
    ) -> Result<(), DeskError> {
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
        let rtp_sender = peer_connection
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
        let rtp_sender = peer_connection
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

        let _session_for_video = self.session.clone();

        let local_settings = self.settings.read().await.clone();

        // Spawn a blocking task to capture screen and send video
        let desk_settings = offer_model.desk_settings.clone();
        let signaling_state_for_screen = peer_connection.signaling_state.clone();

        let capture_screen_thread = std::thread::spawn(move || {
            let local = LocalSet::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            local.spawn_local(async move {
                let result = DeskSession::capture_screen_task(
                    signaling_state_for_screen,
                    desk_settings,
                    video_state_receiver,
                    video_track,
                )
                .await;

                if let Err(error) = result {
                    log::error!("Capture screen task failed, error: {:?}", error);
                    // session_for_video.close(); // TODO: Implement close
                    return Err(error);
                }
                log::info!("Capture screen task completed successfully");
                return result;
            });

            // This will return once all senders are dropped and all
            // spawned tasks have returned.
            rt.block_on(local);
        });
        peer_connection.capture_screen_thread = Some(capture_screen_thread);

        let _session_for_audio = self.session.clone();

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
                    let result = DeskSession::capture_audio_task(
                        audio_settings.desk,
                        audio_state_receiver,
                        audio_track,
                    )
                    .await;

                    if let Err(error) = result {
                        log::error!("Capture audio task failed, error: {:?}", error);
                        // session_for_audio.close(); // TODO: Implement close
                        return Err(error);
                    }
                    log::info!("Capture audio task completed");
                    return result;
                });

                // This will return once all senders are dropped and all
                // spawned tasks have returned.
                rt.block_on(local);
            });

            peer_connection.capture_audio_thread = Some(capture_audio_thread);
        } else {
            log::info!("Will not capture audio because no device is selected");
        }

        // Set the handler for ICE connection state
        // This will notify you when the peer has connected/disconnected
        peer_connection
            .rtc_peer_connection
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
        peer_connection
            .rtc_peer_connection
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
        peer_connection
            .rtc_peer_connection
            .on_ice_gathering_state_change(Box::new(move |s: RTCIceGathererState| {
                info!("ICE gathering state has changed: {s}");
                Box::pin(async {})
            }));

        // Register data channel creation handling
        // Used for mouse event, keyboard event, clipboard manage, file copy, etc.
        let signaling_state_for_data_channel = peer_connection.signaling_state.clone();
        peer_connection
            .rtc_peer_connection
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
        // shutdown rtc peer connection
        for peer_connection in self.rtc_peer_connection_map.values() {
            let result = peer_connection.write().await.shutdown().await;
            info!("Signaling session ended, result={:?}", result);
        }
        // shutdown terminal
        for mut terminal in self.terminal_map.into_values() {
            let result = terminal.1.kill();
            info!("Terminal session ended, result={:?}", result);
        }
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
                        &format!("Unexcepted state {}", state),
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
                        &format!("Unexcepted state {}", state),
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

    /// Handle a signaling message
    pub async fn handle_message(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        match signaling_model.signaling_type {
            SignalingType::RequestRemote => {
                // Init PTC peer connection
                self.init_ptc_peer_connection(&signaling_model).await?;
            }
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
            SignalingType::CloseControl => {
                let from_session_id = signaling_model.check_and_get_from_session_id()?;
                if let Some(peer_connection) = self.rtc_peer_connection_map.remove(&from_session_id)
                {
                    info!(
                        "Received CloseControl from session {}, shutting down peer connection",
                        from_session_id
                    );
                    let peer_connection = peer_connection.read().await;
                    peer_connection.shutdown().await?;
                } else {
                    warn!(
                        "Received CloseControl from session {} but no peer connection found",
                        from_session_id
                    );
                }
            }
            SignalingType::ManagerFileList => {
                self.handle_manager_file_list(signaling_model).await?;
            }
            SignalingType::ManagerFileDelete => {
                self.handle_manager_file_delete(signaling_model).await?;
            }
            SignalingType::StartTerminal => {
                self.handle_manager_terminal_start(signaling_model).await?;
            }
            SignalingType::SendDataToTerminal => {
                self.handle_manager_terminal_data(signaling_model).await?;
            }
            SignalingType::ResizeTerminal => {
                self.handle_manager_terminal_resize(signaling_model).await?;
            }
            SignalingType::CloseTerminal => {
                self.handle_manager_terminal_close(signaling_model).await?;
            }
            SignalingType::ListTerminal => {
                self.handle_list_terminals(signaling_model).await?;
            }
            /*
            SignalingType::Version => {
                // send back a message to client
                self.handle_version(&signaling_model).await?;
            }
             */
            _ => {
                error!(
                    "Unknown signaling type: {:?}",
                    signaling_model.signaling_type
                );

                self.session
                    .send_error(
                        &signaling_model.request_id,
                        signaling_model.signaling_type.into(),
                        signaling_model.from_session_id.clone(),
                        DeskErrorCode::UNKNOWN_SIGNALING_TYPE,
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
        self.session
            .sender
            .send(DeskSessionMessage::Binary(bin))
            .ok();
        Ok(())
    }
    pub async fn ping(&mut self, msg: Bytes) -> Result<(), DeskError> {
        self.session.sender.send(DeskSessionMessage::Pong(msg)).ok();
        Ok(())
    }

    pub async fn handle_offer(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        let rtc_peer_connection = self.get_rtc_peer_connection(&from_session_id)?;
        let offer_model = signaling_model.get_data::<OfferModel>()?;

        // start webrtc first
        let mut rwlock_peer_connection = rtc_peer_connection.write().await;
        let peer_connection = rwlock_peer_connection.deref_mut();
        self.start_webrtc(&offer_model, peer_connection).await?;
        // Set the remote SessionDescription
        peer_connection
            .rtc_peer_connection
            .set_remote_description(offer_model.offer)
            .await?;
        let answer = peer_connection
            .rtc_peer_connection
            .create_answer(None)
            .await?;
        let mut gather_complete = peer_connection
            .rtc_peer_connection
            .gathering_complete_promise()
            .await;

        peer_connection
            .rtc_peer_connection
            .set_local_description(answer)
            .await?;

        // wait for ice gathering complete
        let _ = gather_complete.recv().await;

        if let Some(local_desc) = peer_connection
            .rtc_peer_connection
            .local_description()
            .await
        {
            info!("Sending answer signaling, local_desc: {:?}", local_desc);
            self.session
                .send_to_peer(
                    &signaling_model.request_id,
                    SignalingType::Answer,
                    &from_session_id,
                    local_desc,
                )
                .await?;
        }

        Ok(())
    }

    pub async fn handle_update_desk_settings(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let desk_settings = signaling_model.get_data::<DeskSettings>()?;
        info!("Receive update desk settings: {:?}", desk_settings);

        // notify the new desk settings to the capture screen task
        if let Some(sender) = &self.update_setting_sender {
            if let Err(e) = sender.send(WebRTConnectionState::UpdateSettings(desk_settings)) {
                error!("Failed to send update settings: {:?}", e);
            }
        }

        Ok(())
    }

    pub async fn handle_request_control(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        let rtc_peer_connection = self.get_rtc_peer_connection(&from_session_id)?;

        let _ = signaling_model.get_data::<SignalRequestControlData>()?;

        // auto accept handle request control
        let peer_connection = rtc_peer_connection.read().await;
        let mut signaling_state = peer_connection.signaling_state.write().await;
        signaling_state.accept_control = true;

        Ok(())
    }

    /*
    async fn handle_version(&self, signaling_model: &SignalingModel) -> Result<(), DeskError> {
        let version_info = signaling_model.get_data::<VersionInfo>()?;
        info!("Receive signal server version info: {:?}", version_info);
        Ok(())
    } */

    pub async fn handle_manager_terminal_start(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        // The from_session_id IS the terminal_session_id generated by the controller.
        let start_terminal_session = signaling_model.get_data::<StartTerminalSession>()?;
        let command = start_terminal_session.command;
        if command.is_empty() {
            return DeskError::custom_error(DeskErrorCode::INVALID_PARAMS, "Missing command");
        }

        let terminal_command_list: Vec<&str> = command.split(",").collect();
        let execute_file_path = terminal_command_list[0];
        let args_list = &terminal_command_list[1..];

        // PTY setup
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| {
                DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
            })?;

        let mut cmd = CommandBuilder::new(execute_file_path);
        cmd.args(args_list);

        let child = pair.slave.spawn_command(cmd).map_err(|e| {
            DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
        })?;

        // Spawn reader
        let mut reader = pair.master.try_clone_reader().map_err(|e| {
            DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
        })?;
        let session_sender = self.session.clone();
        let terminal_session_id = from_session_id.clone();
        // We need to know who to send TO. The controller put desk_session_id as `to_session_id`.
        // When we reply, `to_session_id` should be `terminal_session_id` (so Signal server can route it).

        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(n) => {
                        if n == 0 {
                            break;
                        }
                        let content = String::from_utf8_lossy(&buf[..n]).to_string();
                        let data = TerminalOutputData { content };
                        let model = SignalingModel::new_request(
                            SignalingType::ReplyFromTerminal,
                            Some(terminal_session_id.clone()),
                            Some(&data),
                        );
                        if let Ok(model) = model {
                            if let Ok(text) = serde_json::to_string(&model) {
                                let _ = session_sender.sender.send(
                                    crate::service::signaling::DeskSessionMessage::Text(
                                        bytestring::ByteString::from(text),
                                    ),
                                );
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            // Send close message
            let data = TerminalOutputData {
                content: "\r\n\x1b[33m[Process exited]\x1b[0m\r\n".to_string(),
            };

            let model = SignalingModel::new_request(
                SignalingType::ReplyFromTerminal,
                Some(terminal_session_id.clone()),
                Some(&data),
            );

            if let Ok(model) = model {
                if let Ok(text) = serde_json::to_string(&model) {
                    let _ = session_sender.sender.send(
                        crate::service::signaling::DeskSessionMessage::Text(
                            bytestring::ByteString::from(text),
                        ),
                    );
                }
            }
        });

        self.terminal_map
            .insert(from_session_id, (pair.master, child));
        Ok(())
    }

    pub async fn handle_manager_terminal_data(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        let data_value =
            if let Some(v) = signaling_model.get_data_with_type::<TerminalInputData>()? {
                v
            } else {
                return Ok(()); // Ignore empty
            };

        if let Some(pair) = self.terminal_map.get_mut(&from_session_id) {
            if let Ok(mut writer) = pair.0.take_writer() {
                if let Err(e) = writer.write_all(data_value.content.as_bytes()) {
                    warn!("Failed to write to pty: {}", e);
                }
            } else {
                warn!("Failed to get pty writer");
            }
        }
        Ok(())
    }

    pub async fn handle_manager_terminal_resize(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        let data_value =
            if let Some(v) = signaling_model.get_data_with_type::<TerminalResizeData>()? {
                v
            } else {
                return Ok(());
            };

        if let Some(pair) = self.terminal_map.get_mut(&from_session_id) {
            let rows = data_value.rows;
            let cols = data_value.cols;
            if let Err(e) = pair.0.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                warn!("Failed to resize pty: {}", e);
            }
        }
        Ok(())
    }

    pub async fn handle_manager_terminal_close(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        if let Some(mut pair) = self.terminal_map.remove(&from_session_id) {
            let _ = pair.1.kill();
        }
        Ok(())
    }

    pub async fn handle_list_terminals(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        let terminals = fetch_terminal_list(self.settings.clone()).await?;
        self.session
            .send_response(
                &signaling_model.request_id,
                signaling_model.signaling_type.into(),
                Some(from_session_id),
                &terminals,
            )
            .await?;
        Ok(())
    }

    pub async fn handle_manager_file_list(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        // ManagerFileList is a request from the http api, so it may not have a from_session_id
        let from_session_id = signaling_model.from_session_id.clone();

        let params = signaling_model.get_data::<FileListParams>()?;
        match crate::service::file_manager::list_files(params).await {
            Ok(response) => {
                self.session
                    .send_response(
                        &signaling_model.request_id,
                        SignalingType::ManagerFileList,
                        from_session_id,
                        &response,
                    )
                    .await?;
            }
            Err(e) => {
                self.session
                    .send_error(
                        &signaling_model.request_id,
                        SignalingType::ManagerFileList,
                        from_session_id,
                        e.to_error_code(),
                        &e.to_string(),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn handle_manager_file_delete(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_session_id = signaling_model.check_and_get_from_session_id()?;
        let params = signaling_model.get_data::<DeleteFileRequest>()?;

        match crate::service::file_manager::delete_file(params).await {
            Ok(_) => {
                self.session
                    .send_response(
                        &signaling_model.request_id,
                        SignalingType::ManagerFileDelete,
                        Some(from_session_id),
                        &serde_json::json!({}),
                    )
                    .await?;
            }
            Err(e) => {
                self.session
                    .send_error(
                        &signaling_model.request_id,
                        SignalingType::ManagerFileDelete,
                        Some(from_session_id),
                        e.to_error_code(),
                        &e.to_string(),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}
