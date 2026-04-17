use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::{Arc, LazyLock, atomic::AtomicBool};
use std::time::Duration;

use actix_web::web;
use awc::{Client, Connector};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::private_screen::{
    EnablePrivateScreenData, PrivateScreenStateChangedData,
};
use desk_signal_facade::model::signal::{
    InitSignalingData, OfferModel, PeerSignalingSender, RemoteDeskTypeEnum, RequestRemoteModel,
    SignalingModel, SignalingState, SignalingType, TurnTransport, WebRTConnectionState,
};
use desk_signal_facade::{error::DeskSignalFacadeError, model::version::VersionInfo};
use desk_utils::error::{CustomDeskError, DeskErrorCode};

use futures_util::{SinkExt, StreamExt};

use log::{debug, error, info, warn};
use once_cell::sync::OnceCell;
use prometheus::{HistogramVec, register_histogram_vec};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use serde::Serialize;
use std::net::IpAddr;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tokio::time::Instant;

use webrtc::api::media_engine::{MIME_TYPE_AV1, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_H264, MediaEngine},
        setting_engine::{SctpMaxMessageSize, SettingEngine},
    },
    ice_transport::{
        ice_connection_state::RTCIceConnectionState, ice_gatherer_state::RTCIceGathererState,
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
use webrtc_mdns::{config::Config as MdnsConfig, conn::DnsConn};

use crate::model::data_channel::SignalRequestControlData;
use crate::model::host_control::{HostControlEventType, HostControlHelper, WhiteboardCommand};
use crate::model::security_approval::{
    SecurityApprovalSender, SecurityPermissionType, check_security_permission,
};
use crate::model::settings::{StartupMode, TraversalMode};
use crate::model::video_encoder::{VideoEncoderType, VideoEncoderTypeHelper};
use crate::service::audio_capture::audio_capture_factory::{
    create_audio_capture, list_audio_capture,
};
use crate::service::audio_encoder::audio_encoder_factory::{
    create_audio_encoder, list_audio_encoder,
};
use crate::service::audio_playback::start_audio_playback;
use crate::service::data_channel::handle_data_channel_event;
use crate::service::file_manager::{handle_manager_file_delete, handle_manager_file_list};
use crate::service::host_control::host_control_factory::create_host_control_helper;
use crate::service::image_capture::image_capture_factory::{
    create_image_capture, list_image_capture_async,
};
use crate::service::terminal::{
    RunningTerminal, force_kill_terminal_process, handle_list_terminals,
    handle_manager_terminal_close, handle_manager_terminal_data, handle_manager_terminal_resize,
    handle_manager_terminal_start,
};
use crate::service::video_encoder::video_encoder_factory::{
    create_video_encoder, list_video_encoder,
};
use crate::version;
use crate::{error::DeskError, model::settings::SharedSettings};
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
    WebRTCDropped(String),
}

#[derive(Clone)]
pub struct DeskSessionSender {
    pub sender: mpsc::UnboundedSender<DeskSessionMessage>,
}

impl PeerSignalingSender for DeskSessionSender {
    async fn send_response<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        signaling_data: &T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: ?Sized + Serialize + Sync,
    {
        let signaling_model = SignalingModel::success_response(
            request_id,
            signaling_type,
            None,
            to_connection_id,
            Some(signaling_data),
        )?;
        let text = serde_json::to_string(&signaling_model)?;
        self.sender
            .send(DeskSessionMessage::Text(ByteString::from(text)))
            .map_err(|e| {
                DeskSignalFacadeError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to send signaling message: {}", e),
                ))
            })?;
        Ok(())
    }

    async fn send_error(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        error_code: DeskErrorCode,
        error_message: &str,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::error(
            request_id,
            signaling_type,
            None,
            to_connection_id,
            error_code,
            error_message,
        )?;
        let text = serde_json::to_string(&signaling_model)?;
        self.sender
            .send(DeskSessionMessage::Text(ByteString::from(text)))
            .map_err(|e| {
                DeskSignalFacadeError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to send error message: {}", e),
                ))
            })?;
        Ok(())
    }

    async fn send_to_peer<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: &str,
        data: T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: Serialize + Sync + Send,
    {
        self.send_response(
            request_id,
            signaling_type,
            Some(to_connection_id.to_owned()),
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
                        error!("Invalid UTF-8 text: {}", e);
                        return Ok(false);
                    }
                };
                let signaling_model = serde_json::from_str::<SignalingModel>(text_str)?;
                if let Err(e) = desk_session.handle_message(&signaling_model).await {
                    log::warn!(
                        "Error handling message, request_id: {}, signaling_type: {}, from_connection_id: {:?}, to_connection_id: {:?}, e: {}",
                        signaling_model.request_id,
                        signaling_model.signaling_type,
                        signaling_model.from_connection_id,
                        signaling_model.to_connection_id,
                        e
                    );

                    desk_session
                        .session
                        .send_error(
                            &signaling_model.request_id,
                            signaling_model.signaling_type.into(),
                            signaling_model.from_connection_id.clone(),
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!("Error handling message: {}", e),
                        )
                        .await?;
                }
            }
            awc::ws::Frame::Binary(bin) => {
                if let Err(e) = desk_session.binary(bin).await {
                    error!("Error handling binary: {}", e);
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
            error!("WS error: {}", e);
            return Ok(true);
        }
        None => {
            warn!("WS stream closed");
            return Ok(true);
        }
    }
    Ok(false)
}

async fn handle_outgoing_channel_message<S>(
    msg: Option<DeskSessionMessage>,
    sink: &mut S,
    desk_session: &mut DeskSession,
) -> bool
where
    S: SinkExt<awc::ws::Message, Error = awc::error::WsProtocolError> + Unpin,
{
    match msg {
        Some(DeskSessionMessage::Text(text)) => {
            if let Err(e) = sink.send(awc::ws::Message::Text(text)).await {
                error!("Failed to send text: {}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Binary(bin)) => {
            if let Err(e) = sink.send(awc::ws::Message::Binary(bin)).await {
                error!("Failed to send binary: {}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Ping(msg)) => {
            if let Err(e) = sink.send(awc::ws::Message::Ping(msg)).await {
                error!("Failed to send ping: {}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Pong(msg)) => {
            if let Err(e) = sink.send(awc::ws::Message::Pong(msg)).await {
                error!("Failed to send pong: {}", e);
                return true;
            }
        }
        Some(DeskSessionMessage::Close) => {
            let _ = sink.close().await;
            return true;
        }
        Some(DeskSessionMessage::WebRTCDropped(from_connection_id)) => {
            info!(
                "Received WebRTCDropped from session {}, shutting down peer connection and private screen",
                from_connection_id
            );
            if let Some(peer_connection) = desk_session
                .rtc_peer_connection_map
                .remove(&from_connection_id)
            {
                let peer_connection = peer_connection.read().await;
                if let Err(e) = peer_connection.shutdown().await {
                    error!("Failed to shutdown peer connection: {}", e);
                }
            }
            let _ = desk_session
                .host_control_helper
                .enable_private_screen(&from_connection_id, false);
        }
        None => return true,
    }
    false
}

use desk_signal_facade::service::NodeTokenValidator;

pub struct LocalNodeTokenValidator {
    pub settings: web::Data<SharedSettings>,
}

impl NodeTokenValidator for LocalNodeTokenValidator {
    fn validate_node_token<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let token = token.to_string();
        let settings = self.settings.clone();
        Box::pin(async move {
            // Reject empty tokens immediately
            if token.is_empty() {
                return false;
            }
            let local_signaling_token = settings.read().await.system.local_signaling_token.clone();
            if let Some(local_token) = local_signaling_token {
                if !local_token.is_empty() {
                    return crate::constant_time_eq(local_token.as_bytes(), token.as_bytes());
                }
            }
            false
        })
    }
}

pub async fn start_desk_session(
    settings: web::Data<SharedSettings>,
    mut channels: crate::ExternalChannels,
    startup_mode: crate::model::settings::StartupMode,
) -> Result<(), DeskError> {
    // Take the Tauri privacy screen receiver and broadcast it
    let broadcast_tx = if let Some(mut rx) = channels.private_screen_state_receiver.take() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let tx_clone = tx.clone();
        actix_web::rt::spawn(async move {
            while let Some(event) = rx.recv().await {
                let _ = tx_clone.send(event);
            }
        });
        Some(tx)
    } else {
        None
    };

    // ===== Loop 1: Local Signaling Connection =====
    // Only in Default mode (signaling server and desk server co-exist in same process)
    if startup_mode == crate::model::settings::StartupMode::Default {
        let local_settings = settings.clone();
        let local_broadcast_tx = broadcast_tx.clone();
        let local_channels_clone = crate::ExternalChannels {
            private_screen_cmd_sender: channels.private_screen_cmd_sender.clone(),
            private_screen_state_receiver: None,
            tauri_login_token: channels.tauri_login_token.clone(),
            whiteboard_cmd_sender: channels.whiteboard_cmd_sender.clone(),
            security_approval_sender: channels.security_approval_sender.clone(),
        };

        actix_web::rt::spawn(async move {
            loop {
                let (port, enable_ipv6, local_token) = {
                    let s = local_settings.read().await;
                    (
                        s.system.port,
                        s.system.enable_ipv6,
                        s.system.local_signaling_token.clone().unwrap_or_default(),
                    )
                };

                if local_token.is_empty() {
                    log::warn!(
                        "local_signaling_token is not set, skipping local signaling connection"
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                let local_url = if enable_ipv6 {
                    format!("ws://[::1]:{}/api/desk/signaling", port)
                } else {
                    format!("ws://127.0.0.1:{}/api/desk/signaling", port)
                };

                let mut channels_for_loop = crate::ExternalChannels {
                    private_screen_cmd_sender: local_channels_clone
                        .private_screen_cmd_sender
                        .clone(),
                    private_screen_state_receiver: None,
                    tauri_login_token: local_channels_clone.tauri_login_token.clone(),
                    whiteboard_cmd_sender: local_channels_clone.whiteboard_cmd_sender.clone(),
                    security_approval_sender: local_channels_clone.security_approval_sender.clone(),
                };

                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                channels_for_loop.private_screen_state_receiver = Some(rx);
                if let Some(btx) = &local_broadcast_tx {
                    let mut brx = btx.subscribe();
                    actix_web::rt::spawn(async move {
                        while let Ok(event) = brx.recv().await {
                            if tx.send(event).is_err() {
                                break; // rx dropped, exit to prevent task leak
                            }
                        }
                    });
                }

                let _ = maintain_signaling_connection(
                    local_settings.clone(),
                    channels_for_loop,
                    local_url,
                    local_token,
                )
                .await;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    // ===== Loop 2: Remote Signaling Server Connection =====
    // In Default and DeskServer modes
    {
        let remote_sig_settings = settings.clone();
        let remote_sig_broadcast_tx = broadcast_tx.clone();
        let remote_sig_channels = crate::ExternalChannels {
            private_screen_cmd_sender: channels.private_screen_cmd_sender.clone(),
            private_screen_state_receiver: None,
            tauri_login_token: channels.tauri_login_token.clone(),
            whiteboard_cmd_sender: channels.whiteboard_cmd_sender.clone(),
            security_approval_sender: channels.security_approval_sender.clone(),
        };

        actix_web::rt::spawn(async move {
            loop {
                let (signaling_url, signaling_token) = {
                    let s = remote_sig_settings.read().await;
                    (
                        s.system.signaling_url.clone(),
                        s.system.signaling_token.clone(),
                    )
                };
                if let (Some(url), Some(token)) = (signaling_url, signaling_token) {
                    if !url.is_empty() && !token.is_empty() {
                        let mut channels_for_loop = crate::ExternalChannels {
                            private_screen_cmd_sender: remote_sig_channels
                                .private_screen_cmd_sender
                                .clone(),
                            private_screen_state_receiver: None,
                            tauri_login_token: remote_sig_channels.tauri_login_token.clone(),
                            whiteboard_cmd_sender: remote_sig_channels
                                .whiteboard_cmd_sender
                                .clone(),
                            security_approval_sender: remote_sig_channels
                                .security_approval_sender
                                .clone(),
                        };

                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        channels_for_loop.private_screen_state_receiver = Some(rx);
                        if let Some(btx) = &remote_sig_broadcast_tx {
                            let mut brx = btx.subscribe();
                            actix_web::rt::spawn(async move {
                                while let Ok(event) = brx.recv().await {
                                    if tx.send(event).is_err() {
                                        break;
                                    }
                                }
                            });
                        }

                        let _ = maintain_signaling_connection(
                            remote_sig_settings.clone(),
                            channels_for_loop,
                            url,
                            token,
                        )
                        .await;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    // ===== Loop 3: Remote Manager Server Connection =====
    // In Default and DeskServer modes
    {
        let remote_mgr_settings = settings.clone();
        let remote_mgr_broadcast_tx = broadcast_tx.clone();
        let remote_mgr_channels = crate::ExternalChannels {
            private_screen_cmd_sender: channels.private_screen_cmd_sender.clone(),
            private_screen_state_receiver: None,
            tauri_login_token: channels.tauri_login_token.clone(),
            whiteboard_cmd_sender: channels.whiteboard_cmd_sender.clone(),
            security_approval_sender: channels.security_approval_sender.clone(),
        };

        actix_web::rt::spawn(async move {
            loop {
                let (manager_url, manager_api_token) = {
                    let s = remote_mgr_settings.read().await;
                    (
                        s.system.manager_url.clone(),
                        s.system.manager_api_token.clone(),
                    )
                };
                if let (Some(url), Some(token)) = (manager_url, manager_api_token) {
                    if !url.is_empty() && !token.is_empty() {
                        let mut channels_for_loop = crate::ExternalChannels {
                            private_screen_cmd_sender: remote_mgr_channels
                                .private_screen_cmd_sender
                                .clone(),
                            private_screen_state_receiver: None,
                            tauri_login_token: remote_mgr_channels.tauri_login_token.clone(),
                            whiteboard_cmd_sender: remote_mgr_channels
                                .whiteboard_cmd_sender
                                .clone(),
                            security_approval_sender: remote_mgr_channels
                                .security_approval_sender
                                .clone(),
                        };

                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        channels_for_loop.private_screen_state_receiver = Some(rx);
                        if let Some(btx) = &remote_mgr_broadcast_tx {
                            let mut brx = btx.subscribe();
                            actix_web::rt::spawn(async move {
                                while let Ok(event) = brx.recv().await {
                                    if tx.send(event).is_err() {
                                        break;
                                    }
                                }
                            });
                        }

                        let _ = maintain_signaling_connection(
                            remote_mgr_settings.clone(),
                            channels_for_loop,
                            url,
                            token,
                        )
                        .await;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    Ok(())
}

async fn maintain_signaling_connection(
    settings: web::Data<SharedSettings>,
    mut channels: crate::ExternalChannels,
    signaling_url: String,
    auth_token: String,
) -> Result<(), DeskError> {
    let display_name = {
        let settings = settings.read().await;
        settings.desk.display_name.clone()
    };

    let display_name = if display_name.is_some() {
        display_name
    } else {
        sysinfo::System::host_name()
    };

    let client_id = {
        let settings = settings.read().await;
        settings.system.get_client_id()?
    };

    let mut version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        version::SERVER_BUILD_NUMBER,
        version::SERVER_COMMIT_HASH.to_string(),
        RemoteDeskTypeEnum::Server,
        display_name,
        Some(client_id),
    );
    version_info.token = Some(auth_token.clone());
    let version_query = serde_urlencoded::to_string(&version_info).unwrap();

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

    let signaling_url_clean = signaling_url.trim().trim_matches(|c: char| c.is_control());
    let connect_url = if signaling_url_clean.contains('?') {
        format!("{}&{}", signaling_url_clean, version_query)
    } else {
        format!("{}?{}", signaling_url_clean, version_query)
    };

    debug!("Full connection URL: {}", connect_url);

    let (response, framed) = match client.ws(&connect_url).connect().await {
        Ok(res) => res,
        Err(e) => {
            error!(
                "Failed to connect to signaling server: {:?}, url: {}",
                e, connect_url
            );
            return Err(DeskError::AnyhowError(anyhow::anyhow!("Connection failed")));
        }
    };

    info!("Connected to signaling server: {:?}", response);

    let (mut sink, mut stream) = framed.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<DeskSessionMessage>();
    let session_sender = DeskSessionSender { sender: tx.clone() };

    let mut desk_session = match DeskSession::new(
        settings.clone(),
        session_sender,
        CurrentUser::new_admin("server_node"),
        &mut channels,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to init desk session: {}", e);
            return Err(e);
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
                if handle_outgoing_channel_message(msg, &mut sink, &mut desk_session).await {
                    break;
                }
            }
        }
    }

    info!("Desk session ended, cleaning up...");

    if let Err(e) = desk_session.shutdown().await {
        error!("Error shutdown desk session: {}", e);
    }

    Ok(())
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
    /// Cursor data channel
    pub cursor_data_channel: Arc<tokio::sync::RwLock<Option<Arc<RTCDataChannel>>>>,
}

impl PeerConnection {
    /// Shutdown the signaling context, including peer connection and capture tasks.
    pub async fn shutdown(&self) -> Result<(), DeskError> {
        let result = self.rtc_peer_connection.close().await;
        info!("Signaling session ended, result={:?}", result);

        Ok(())
    }
}

/// Signaling context for handling WebSocket messages.
pub struct DeskSession {
    pub settings: web::Data<SharedSettings>,
    pub session: DeskSessionSender,
    pub user: CurrentUser,
    /// RTC peer connection map, key is from_connection_id
    pub rtc_peer_connection_map: HashMap<String, Arc<tokio::sync::RwLock<PeerConnection>>>,
    /// Tokio watch sender for WebRTConnectionState updates
    pub update_setting_sender: Option<tokio::sync::watch::Sender<WebRTConnectionState>>,
    /// Terminal map: from_connection_id -> RunningTerminal
    pub terminal_map: HashMap<String, RunningTerminal>,
    /// System setting helper
    pub host_control_helper: Box<dyn HostControlHelper + Send + Sync>,
    /// Whiteboard command sender (available when Tauri is present)
    pub whiteboard_cmd_sender: Option<std::sync::mpsc::Sender<WhiteboardCommand>>,
    /// Security approval channel sender (available when Tauri is present)
    pub security_approval_sender: Option<SecurityApprovalSender>,
}

static MDNS_CONN: OnceCell<std::sync::Arc<DnsConn>> = OnceCell::new();

async fn get_mdns_conn() -> Result<std::sync::Arc<DnsConn>, webrtc_mdns::Error> {
    if let Some(conn) = MDNS_CONN.get() {
        return Ok(conn.clone());
    }

    let mut cfg = MdnsConfig::default();
    cfg.query_interval = Duration::from_millis(200);

    // Bind to an ephemeral port to avoid conflicts with any existing mDNS listener.
    let conn = DnsConn::server("0.0.0.0:0".parse().expect("valid mdns bind"), cfg)?;
    let conn = std::sync::Arc::new(conn);
    let _ = MDNS_CONN.set(conn.clone());
    Ok(conn)
}

async fn resolve_mdns_host(host: &str) -> Option<IpAddr> {
    let conn = get_mdns_conn().await.ok()?;
    let (_close_tx, close_rx) = mpsc::channel(1);

    // Timeout quickly to avoid blocking ICE too long.
    match tokio::time::timeout(Duration::from_millis(800), conn.query(host, close_rx)).await {
        Ok(Ok((_answer, addr))) => Some(addr.ip()),
        Ok(Err(e)) => {
            log::warn!("mDNS query failed for {}: {:?}", host, e);
            None
        }
        Err(_) => {
            log::warn!("mDNS query timed out for {}", host);
            None
        }
    }
}

enum ConnectionStateChangeResult {
    NoChange,
    Exit,
    UpdateSettings(DeskSettings),
}

fn handle_connection_state_change(
    state: &WebRTConnectionState,
    task_name: &str,
) -> ConnectionStateChangeResult {
    match state {
        WebRTConnectionState::Init => {
            log::warn!(
                "{} current state is Init, it should be happened?",
                task_name
            );
            ConnectionStateChangeResult::NoChange
        }
        WebRTConnectionState::Connected => {
            log::warn!("{}: RTC is connected again?", task_name);
            ConnectionStateChangeResult::NoChange
        }
        WebRTConnectionState::UpdateSettings(new_desk_setting) => {
            log::info!("{} update settings {:?}", task_name, new_desk_setting);
            ConnectionStateChangeResult::UpdateSettings(new_desk_setting.clone())
        }
        _ => {
            log::error!("{} unexpected state {:?}, exit", task_name, state);
            ConnectionStateChangeResult::Exit
        }
    }
}

impl DeskSession {
    pub async fn new(
        settings: web::Data<SharedSettings>,
        session: DeskSessionSender,
        user: CurrentUser,
        channels: &mut crate::ExternalChannels,
    ) -> Result<Self, DeskError> {
        let desk_settings = settings.read().await.clone().desk;

        let cmd_sender = channels.private_screen_cmd_sender.clone();
        let helper = create_host_control_helper(&desk_settings, cmd_sender)?;

        // If started by Tauri, there might be a state_receiver we need to listen to

        if let Some(mut rx) = channels.private_screen_state_receiver.take() {
            let session_clone = session.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    match event {
                        HostControlEventType::PrivateScreenVisibleChanged(
                            from_connection_id,
                            visible,
                        ) => {
                            let sender = session_clone.clone();
                            let data = PrivateScreenStateChangedData {
                                visible,
                                is_supported: true,
                                error_msg: None,
                            };
                            if let Ok(model) = SignalingModel::new_request(
                                SignalingType::PrivateScreenStateChanged,
                                Some(from_connection_id),
                                Some(&data),
                            ) {
                                if let Ok(text) = serde_json::to_string(&model) {
                                    let _ = sender.sender.send(DeskSessionMessage::Text(
                                        bytestring::ByteString::from(text),
                                    ));
                                }
                            }
                        }
                        HostControlEventType::PrivateScreenInited(_) => {
                            // Ignored or handle appropriately
                        }
                        HostControlEventType::PrivateScreenClosed => {
                            let sender = session_clone.clone();
                            let data = PrivateScreenStateChangedData {
                                visible: false,
                                is_supported: true,
                                error_msg: None,
                            };
                            if let Ok(model) = SignalingModel::new_request(
                                SignalingType::PrivateScreenStateChanged,
                                None,
                                Some(&data),
                            ) {
                                if let Ok(text) = serde_json::to_string(&model) {
                                    let _ = sender.sender.send(DeskSessionMessage::Text(
                                        bytestring::ByteString::from(text),
                                    ));
                                }
                            }
                        }
                        HostControlEventType::PrivateScreenUnknownError(
                            from_connection_id_opt,
                            e,
                        ) => {
                            log::error!("Private screen error: {}", e);
                            // We shouldn't send PrivateScreenStateChanged directly to the channel queue,
                            // Instead, we should construct a Modeling message if we want to send it to the frontend.
                            // But here we only have session_clone which is DeskSessionSender.
                            // Let's add a method on DeskSessionMessage or handle in that loop.
                            // I'll just change send() to send a Text message with SignalingModel.

                            if let Some(from_connection_id) = from_connection_id_opt {
                                let sender = session_clone.clone();
                                let data = PrivateScreenStateChangedData {
                                    visible: false,
                                    is_supported: true, // or whatever
                                    error_msg: Some(e),
                                };
                                if let Ok(model) = SignalingModel::new_request(
                                    SignalingType::PrivateScreenStateChanged,
                                    Some(from_connection_id),
                                    Some(&data),
                                ) {
                                    if let Ok(text) = serde_json::to_string(&model) {
                                        let _ = sender.sender.send(DeskSessionMessage::Text(
                                            bytestring::ByteString::from(text),
                                        ));
                                    }
                                }
                            }
                        }
                        HostControlEventType::PrivateScreenHotkeyRegisterError => {
                            log::error!("Private screen hotkey register error");
                        }
                    }
                }
            });
        }

        let whiteboard_cmd_sender = channels.whiteboard_cmd_sender.clone();
        let security_approval_sender = channels.security_approval_sender.clone();

        Ok(Self {
            settings,
            session,
            user,
            rtc_peer_connection_map: HashMap::new(),
            update_setting_sender: None,
            terminal_map: HashMap::new(),
            host_control_helper: helper,
            whiteboard_cmd_sender,
            security_approval_sender,
        })
    }
}

impl DeskSession {
    pub async fn init_ptc_peer_connection(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
        let request_remote_model = signaling_model.get_data::<RequestRemoteModel>()?;

        if self
            .rtc_peer_connection_map
            .contains_key(from_connection_id)
        {
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Peer connection already exists",
            );
        }

        let local_settings = {
            let shared_settings = self.settings.read().await;
            shared_settings.clone()
        };

        // Create the SettingEngine
        let mut setting_engine = SettingEngine::default();
        // Allow unbounded SCTP message size to improve throughput on localhost
        setting_engine.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Unbounded);

        let mut ice_servers = Vec::new();
        for ice_server in request_remote_model.ice_servers.iter() {
            match ice_server.transport() {
                Some(TurnTransport::Stun) => {
                    // check if traversal mode is stun or turn
                    if local_settings.turn_client.traversal_mode == TraversalMode::Stun
                        || local_settings.turn_client.traversal_mode == TraversalMode::Turn
                    {
                        ice_servers.push(ice_server.clone());
                    }
                }
                Some(TurnTransport::Turn) => {
                    if local_settings.turn_client.traversal_mode == TraversalMode::Turn
                        && self.settings.read().await.args.startup_mode == StartupMode::DeskServer
                    {
                        ice_servers.push(ice_server.clone());
                    }
                }
                None => {
                    log::warn!(
                        "Ignoring ICE server with invalid/empty transport for connection {}: {:?}",
                        from_connection_id,
                        ice_server
                    );
                    continue;
                }
            }
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
            .with_setting_engine(setting_engine)
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        // Prepare the configuration for Server (use only STUN/Host)
        let config = RTCConfiguration {
            ice_servers: ice_servers
                .iter()
                .map(|ice_server| ice_server.into())
                .collect(),
            ..Default::default()
        };

        // Create a new RTCPeerConnection
        let rtc_peer_connection = api.new_peer_connection(config).await?;

        // get audio device
        let audio_device_list = list_audio_capture();
        let audio_encoder_list = list_audio_encoder();
        // get video device
        let video_device_list = list_image_capture_async().await;

        let video_encoder_list = list_video_encoder();

        let init_signaling_data = InitSignalingData {
            // signal server will fill ice_servers
            ice_servers: vec![],
            user_name: self.user.name.clone(),
            audio_device_list,
            audio_encoder_list,
            video_device_list,
            video_encoder_list,
            desk_settings: local_settings.desk,
            has_tauri: self.whiteboard_cmd_sender.is_some(),
            is_admin: desk_utils::permission::is_admin(),
        };

        info!("Sending init signaling: {:?}", init_signaling_data);
        self.session
            .send_to_peer(
                &signaling_model.request_id,
                SignalingType::Init,
                from_connection_id,
                init_signaling_data,
            )
            .await?;
        info!("Sent init signaling");

        self.rtc_peer_connection_map.insert(
            from_connection_id.to_owned(),
            Arc::new(tokio::sync::RwLock::new(PeerConnection {
                rtc_peer_connection,
                capture_screen_thread: None,
                capture_audio_thread: None,
                signaling_state: Arc::new(tokio::sync::RwLock::new(SignalingState::default())),
                cursor_data_channel: Arc::new(tokio::sync::RwLock::new(None)),
            })),
        );
        Ok(())
    }

    /// Get the RTC peer connection, if not initialized, return error
    pub fn get_rtc_peer_connection(
        &self,
        from_connection_id: &str,
    ) -> Result<Arc<tokio::sync::RwLock<PeerConnection>>, DeskError> {
        if let Some(rtc_peer_connection) = self.rtc_peer_connection_map.get(from_connection_id) {
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
        request_id: &str,
        from_connection_id: &str,
        offer_model: &OfferModel,
        peer_connection: &mut PeerConnection,
    ) -> Result<(), DeskError> {
        {
            let mut signaling_state = peer_connection.signaling_state.write().await;
            signaling_state.wayland_control_mode =
                offer_model.desk_settings.wayland_control_mode.clone();
        }

        let (ice_state_change_sender, ice_connection_state_rx) =
            tokio::sync::watch::channel(WebRTConnectionState::Init);
        let peer_state_change_sender = ice_state_change_sender.clone();
        let update_setting_sender = ice_state_change_sender.clone();

        // Check if the SDP offer contains video/audio media sections.
        // If not, it's a data-only connection (e.g., file transfer) — skip media tracks.
        let sdp_str = &offer_model.offer.sdp;
        let has_video = sdp_str.contains("m=video");
        let has_audio = sdp_str.contains("m=audio");
        log::info!(
            "SDP media detection: has_video={}, has_audio={}",
            has_video,
            has_audio
        );

        let is_desktop_mode = has_video || has_audio;
        if is_desktop_mode {
            log::info!("SDP offer contains media tracks, setting up video/audio capture");
            let video_state_receiver = ice_connection_state_rx.clone();
            // Shared flag for PLI/FIR keyframe requests (RTCP reader -> capture loop)
            let keyframe_requested = Arc::new(AtomicBool::new(false));
            let audio_state_receiver = ice_connection_state_rx.clone();
            let video_mime_type = match offer_model.desk_settings.get_video_encoder_type()? {
                VideoEncoderType::H264 | VideoEncoderType::X264 => MIME_TYPE_H264,
                VideoEncoderType::VP8 => MIME_TYPE_VP8,
                VideoEncoderType::VP9 => MIME_TYPE_VP9,
                VideoEncoderType::AV1 => MIME_TYPE_AV1,
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

            // Read incoming RTCP packets, detect PLI/FIR for keyframe requests
            let keyframe_flag_for_rtcp = keyframe_requested.clone();
            tokio::spawn(async move {
                let mut rtcp_buf = vec![0u8; 1500];
                log::info!("Start to read incoming video RTCP packets");
                while let Ok((pkts, _)) = rtp_sender.read(&mut rtcp_buf).await {
                    // Parse RTCP packets and detect Picture Loss Indication / Full Intra Request
                    {
                        for pkt in pkts {
                            if pkt
                                .as_any()
                                .downcast_ref::<rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                                .is_some()
                                || pkt
                                    .as_any()
                                    .downcast_ref::<rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
                                    .is_some()
                            {
                                log::info!("Received PLI/FIR, requesting keyframe");
                                keyframe_flag_for_rtcp
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                }
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
            let cursor_data_channel_for_screen = peer_connection.cursor_data_channel.clone();

            if peer_connection.capture_screen_thread.is_none() {
                let capture_screen_thread = std::thread::spawn(move || {
                    let local = LocalSet::new();
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();

                    local.spawn_local(async move {
                        let result = DeskSession::capture_screen_task(
                            signaling_state_for_screen,
                            cursor_data_channel_for_screen,
                            desk_settings,
                            video_state_receiver,
                            video_track,
                            keyframe_requested,
                        )
                        .await;

                        if let Err(error) = result {
                            log::error!("Capture screen task failed, error: {}", error);
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
            } else {
                log::info!("Screen capture thread already exists, skipping creation");
            }

            let _session_for_audio = self.session.clone();

            let audio_settings = local_settings.clone();
            let audio_device = offer_model.desk_settings.audio_device.clone();
            if let Some(audio_device) = audio_device {
                log::info!("Start to capture audio with device: {:?}", audio_device);

                if peer_connection.capture_audio_thread.is_none() {
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
                                log::error!("Capture audio task failed, error: {}", error);
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
                    log::info!("Audio capture thread already exists, skipping creation");
                }
            } else {
                log::info!("Will not capture audio because no device is selected");
            }
        } else {
            log::info!("SDP offer is data-only (no video/audio), skipping media track setup");
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
                            log::error!("Failed to send connection state: {}", error);
                        }
                    }

                    Box::pin(async {})
                },
            ));

        // Set the handler for Peer connection state
        // This will notify you when the peer has connected/disconnected
        let peer_state_change_sender_for_drop = self.session.clone();
        let from_connection_id_for_drop = from_connection_id.to_string();
        peer_connection
            .rtc_peer_connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                log::info!("Peer connection state has changed: {s}");
                let state = WebRTConnectionState::from(&s);
                if state == WebRTConnectionState::Closed {
                    if let Err(error) = peer_state_change_sender.send(state.clone()) {
                        log::error!("Failed to send connection state: {}", error);
                    }
                }

                if s == RTCPeerConnectionState::Closed
                    || s == RTCPeerConnectionState::Failed
                    || s == RTCPeerConnectionState::Disconnected
                {
                    let _ = peer_state_change_sender_for_drop.sender.send(
                        DeskSessionMessage::WebRTCDropped(from_connection_id_for_drop.clone()),
                    );
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

        // Set the handler for ICE candidate
        let session_for_candidate = self.session.clone();
        let request_id_for_candidate = request_id.to_string();
        let from_connection_id_for_candidate = from_connection_id.to_string();

        peer_connection
            .rtc_peer_connection
            .on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
                let mut session_sender = session_for_candidate.clone();
                let request_id = request_id_for_candidate.clone();
                let from_connection_id = from_connection_id_for_candidate.clone();

                Box::pin(async move {
                    if let Some(candidate) = c {
                        // send via signaling
                        match candidate.to_json() {
                            Ok(json) => {
                                info!("Sending candidate signaling, candidate: {:?}", json);
                                let result = session_sender
                                    .send_to_peer(
                                        &request_id,
                                        SignalingType::Canid,
                                        &from_connection_id,
                                        json,
                                    )
                                    .await;
                                if let Err(error) = result {
                                    log::error!("Failed to send candidate signaling: {}", error);
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to get json from candidate: {}", e);
                            }
                        }
                    }
                })
            }));

        // Register data channel creation handling
        // Used for mouse event, keyboard event, clipboard manage, file copy, whiteboard, etc.
        let signaling_state_for_data_channel = peer_connection.signaling_state.clone();
        let whiteboard_sender_for_dc = self.whiteboard_cmd_sender.clone();
        let from_connection_id_for_dc = from_connection_id.to_string();
        let settings_for_dc = self.settings.clone();
        let security_sender_for_dc = self.security_approval_sender.clone();
        let cursor_data_channel_for_dc = peer_connection.cursor_data_channel.clone();
        peer_connection
            .rtc_peer_connection
            .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
                let d_label = d.label().to_owned();
                let d_id = d.id();
                log::info!("New DataChannel {d_label} {d_id}");
                let signaling_state = signaling_state_for_data_channel.clone();
                let wb_sender = whiteboard_sender_for_dc.clone();
                let sid = from_connection_id_for_dc.clone();
                let settings = settings_for_dc.clone();
                let security_sender = security_sender_for_dc.clone();
                let cursor_data_channel = cursor_data_channel_for_dc.clone();
                // Register channel opening handling
                Box::pin(async move {
                    if d_label == crate::model::data_channel::DATA_CHANNEL_LABEL_CURSOR_SYNC_EVENT {
                        log::info!("Received CursorSyncDataChannel");
                        let mut channel = cursor_data_channel.write().await;
                        *channel = Some(d.clone());
                        return;
                    }
                    let result = handle_data_channel_event(
                        signaling_state,
                        d.clone(),
                        wb_sender,
                        sid,
                        settings,
                        security_sender,
                    )
                    .await;
                    if let Err(error) = result {
                        log::error!("Failed to handle data channel event: {}", error);
                    }
                })
            }));

        // Register track handler for incoming audio from browser
        let session_for_audio = self.session.clone();
        let request_id_for_audio = request_id.to_string();
        let from_connection_id_for_audio = from_connection_id.to_string();

        peer_connection.rtc_peer_connection.on_track(Box::new(
            move |track, _receiver, _transceiver| {
                let track_kind = track.kind().to_string();
                let track_id = track.id().to_string();
                log::info!(
                    "Received remote track: kind={}, id={}",
                    track_kind,
                    track_id
                );

                if track_kind == "audio" {
                    log::info!("Starting audio playback for remote audio track");
                    let mut session_sender = session_for_audio.clone();
                    let req_id = request_id_for_audio.clone();
                    let from_connection = from_connection_id_for_audio.clone();

                    // Capture the tokio handle so we can spawn from the std::thread inside audio_playback
                    let handle = match tokio::runtime::Handle::try_current() {
                        Ok(h) => Some(h),
                        Err(e) => {
                            log::error!(
                                "Failed to get tokio handle for audio playback error reporting: {}",
                                e
                            );
                            None
                        }
                    };

                    start_audio_playback(track, move |err_msg| {
                        log::warn!("Audio playback failed, notifying frontend: {}", err_msg);

                        if let Some(rt_handle) = handle {
                            rt_handle.spawn(async move {
                                let error_data = serde_json::json!({
                                    "error": err_msg
                                });
                                let res = session_sender
                                    .send_to_peer(
                                        &req_id,
                                        SignalingType::AudioPlaybackError,
                                        &from_connection,
                                        error_data,
                                    )
                                    .await;
                                if let Err(e) = res {
                                    log::error!("Failed to send AudioPlaybackError signal: {}", e);
                                }
                            });
                        } else {
                            log::error!(
                                "Cannot send AudioPlaybackError signal: no tokio handle available"
                            );
                        }
                    });
                } else {
                    log::info!("Ignoring non-audio track: {}", track_kind);
                }

                Box::pin(async {})
            },
        ));

        self.update_setting_sender = Some(update_setting_sender);
        if is_desktop_mode {
            let mut settings = self.settings.write().await;
            settings.desk = offer_model.desk_settings.clone();
            log::info!("Desk settings updated: {:?}", settings.desk);
        }
        Ok(())
    }

    /// Shutdown the signaling context, including peer connection and capture tasks.
    pub async fn shutdown(self) -> Result<(), DeskError> {
        // shutdown rtc peer connection
        for (connection_id, peer_connection) in self.rtc_peer_connection_map.iter() {
            let result = peer_connection.write().await.shutdown().await;
            info!("Signaling session ended, result={:?}", result);
            let _ = self
                .host_control_helper
                .enable_private_screen(connection_id, false);
        }
        // shutdown terminal
        for terminal in self.terminal_map.into_values() {
            let child_arc = terminal.child.clone();
            drop(terminal);
            if let Ok(mut child) = child_arc.lock() {
                if let Some(pid) = child.process_id() {
                    force_kill_terminal_process(pid);
                }
                let result = child.kill();
                info!("Terminal session ended, result={:?}", result);
            }
        }
        Ok(())
    }

    /// Start the screen capture task
    pub async fn capture_screen_task(
        signaling_state: Arc<tokio::sync::RwLock<SignalingState>>,
        cursor_data_channel: Arc<tokio::sync::RwLock<Option<Arc<RTCDataChannel>>>>,
        desk_settings: DeskSettings,
        mut connection_state_rx: tokio::sync::watch::Receiver<WebRTConnectionState>,
        video_track: Arc<TrackLocalStaticSample>,
        keyframe_requested: Arc<AtomicBool>,
    ) -> Result<(), DeskError> {
        let mut desk_settings = desk_settings;
        log::info!(
            "Preparing to capture screen, desk settings: {:?}",
            desk_settings
        );
        log::info!("Capture screen task: creating image capture backend");
        let mut capture = create_image_capture(&desk_settings)?;
        let mut image_capture_type = capture.get_capture_type().into();
        log::info!(
            "Capture screen task: image capture backend created, type={}",
            image_capture_type
        );
        //TODO
        log::info!("Capture screen task: querying current display info");
        let display_info = capture.get_current_output()?;
        {
            let mut signaling_state = signaling_state.write().await;
            signaling_state.display_info = display_info.clone();
            log::info!(
                "Set initial display info: {:?}",
                signaling_state.display_info
            );
        }

        log::info!("Capture screen task: creating video encoder");
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
        let mut last_cursor_shape_id: Option<u64> = None;
        loop {
            //ticker = tokio::time::interval(Duration::from_millis(3));
            // check if the connection is still alive
            tokio::select! {
             _ = ticker.tick() => {},
             res = connection_state_rx.changed() => {
                if let Err(err) = res {
                    log::info!("connection_state_tx dropped, err={}, exit capture screen task", err);
                    break;
                }
                let state = connection_state_rx.borrow_and_update().clone();
                match handle_connection_state_change(&state, "capture_screen_task") {
                    ConnectionStateChangeResult::Exit => break,
                    ConnectionStateChangeResult::UpdateSettings(new_setting) => {
                        desk_settings = new_setting;
                        ticker = tokio::time::interval(desk_settings.get_duration_by_video_fps());
                        image_capture_type = capture.get_capture_type().into();
                    },
                    ConnectionStateChangeResult::NoChange => {},
                }
             },
            }
            log::trace!("begin caption scrren");
            let timer = CAPTURE_SCREEN_HISTOGRAM
                .with_label_values(&[image_capture_type])
                .start_timer();

            let supports_native_cursor = match capture.get_capture_type() {
                crate::model::image_capture::ImageCaptureType::GDI => true,
                crate::model::image_capture::ImageCaptureType::DXGI => true,
                _ => false,
            };

            let is_controlling = signaling_state.read().await.accept_control;
            let show_mouse = if desk_settings.show_mouse {
                if supports_native_cursor {
                    !is_controlling
                } else {
                    true
                }
            } else {
                false
            };

            let image_info_result = capture.capture(show_mouse);

            if is_controlling && supports_native_cursor {
                if let Ok(Some(cursor_data)) = capture.capture_cursor(last_cursor_shape_id) {
                    if last_cursor_shape_id != Some(cursor_data.shape_id) {
                        log::info!("Cursor update: visible={}, shape_id={}, screen_width={}, screen_height={}", cursor_data.visible, cursor_data.shape_id, cursor_data.screen_width, cursor_data.screen_height);
                        last_cursor_shape_id = Some(cursor_data.shape_id);
                        let channel_opt = cursor_data_channel.read().await.clone();
                        if let Some(channel) = channel_opt {
                            if channel.ready_state() == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
                                if let Ok(json) = serde_json::to_string(&cursor_data) {
                                    let _ = channel.send_text(json).await;
                                }
                            }
                        }
                    }
                }
            } else {
                last_cursor_shape_id = None;
            }

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
                    log::error!("Failed to get nal info, error={}", err);
                    continue;
                }
            };

            // Check if a keyframe was requested via RTCP PLI/FIR
            if keyframe_requested.swap(false, std::sync::atomic::Ordering::Relaxed) {
                log::info!("Keyframe requested via PLI/FIR, recreating encoder");
                // TODO: Implement native request_keyframe() for each encoder to avoid
                // the overhead of full encoder recreation. Currently using recreation as
                // a universal fallback since PLI is a low-frequency event.
                // - H264 (OpenH264): use ForceIntraFrame(true) via raw API
                // - VP8/VP9: extend vpx-encode fork to support VPX_EFLAG_FORCE_KF flag
                // - X264: set x264_picture_t.i_type = X264_TYPE_IDR via raw API
                // - AV1 (rav1e): flush + recreate Context (no native force-keyframe API)
                let display_info = {
                    let state = signaling_state.read().await;
                    state.display_info.clone()
                };
                encoder = create_video_encoder(&desk_settings, &display_info)?;
            }

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
             res = connection_state_rx.changed() => {
                if let Err(err) = res {
                    log::info!("connection_state_tx dropped, err={}, exit capture audio task", err);
                    break;
                }
                let state = connection_state_rx.borrow_and_update().clone();
                match handle_connection_state_change(&state, "capture_audio_task") {
                    ConnectionStateChangeResult::Exit => break,
                    ConnectionStateChangeResult::UpdateSettings(_new_setting) => {
                        // TODO: No audio configuration updates for now
                    },
                    ConnectionStateChangeResult::NoChange => {},
                }
             },
            }
            log::trace!("begin capture audio");
            loop {
                let start = Instant::now();
                //let buffer = opus_audio_capture.get_buffer()?;
                let result = capture.get_buffer();
                if let Err(error) = &result {
                    if let DeskError::CustomError(err) = error {
                        if err.error_code == DeskErrorCode::ACTION_NEED_RETRY {
                            // recreate audio capture
                            log::warn!("Failed to get audio buffer, recreate audio capture");
                            capture = create_audio_capture(&desk_settings)?;
                            capture.start()?;
                            continue;
                        }
                    }
                    log::error!("Failed to get audio buffer, error: {}", error);
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
            SignalingType::Canid => {
                let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
                let rtc_peer_connection = self.get_rtc_peer_connection(&from_connection_id)?;

                use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
                if let Some(mut candidate_init) =
                    signaling_model.get_data_with_type::<RTCIceCandidateInit>()?
                {
                    let to_connection_id = signaling_model
                        .to_connection_id
                        .as_deref()
                        .unwrap_or("<none>");
                    log::info!(
                        "Received ICE candidate from {} to {}: candidate=\"{}\" sdp_mid={:?} sdp_mline_index={:?} ufrag={:?}",
                        from_connection_id,
                        to_connection_id,
                        candidate_init.candidate,
                        candidate_init.sdp_mid,
                        candidate_init.sdp_mline_index,
                        candidate_init.username_fragment
                    );
                    if candidate_init.candidate.contains(".local") {
                        let mut parts = candidate_init
                            .candidate
                            .split_whitespace()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>();
                        if parts.len() >= 6 {
                            let host = parts[4].to_string();
                            if host.ends_with(".local") {
                                if let Some(ip) = resolve_mdns_host(&host).await {
                                    log::info!("Resolved mDNS host {} -> {}", host, ip);
                                    parts[4] = ip.to_string();
                                    candidate_init.candidate = parts.join(" ");
                                    log::info!(
                                        "Rewritten ICE candidate after mDNS resolution: {}",
                                        candidate_init.candidate
                                    );
                                } else {
                                    log::warn!(
                                        "Failed to resolve mDNS host {}. ICE may fail unless client disables mDNS.",
                                        host
                                    );
                                }
                            }
                        } else {
                            log::warn!(
                                "Malformed ICE candidate (too few parts) for mDNS handling: {}",
                                candidate_init.candidate
                            );
                        }
                    }
                    let peer_connection = rtc_peer_connection.read().await;
                    if let Err(e) = peer_connection
                        .rtc_peer_connection
                        .add_ice_candidate(candidate_init)
                        .await
                    {
                        log::warn!("Failed to add ice candidate: {}", e);
                    }
                }
            }
            SignalingType::UpdateDeskSettings => {
                self.handle_update_desk_settings(&signaling_model).await?;
            }
            SignalingType::RequireControl => {
                // send back a message to client
                self.handle_request_control(&signaling_model).await?;
            }
            SignalingType::CloseControl => {
                let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
                if let Some(peer_connection) =
                    self.rtc_peer_connection_map.remove(from_connection_id)
                {
                    info!(
                        "Received CloseControl from session {}, shutting down peer connection",
                        from_connection_id
                    );
                    let peer_connection = peer_connection.read().await;
                    peer_connection.shutdown().await?;
                } else {
                    warn!(
                        "Received CloseControl from session {} but no peer connection found",
                        from_connection_id
                    );
                }
                let _ = self
                    .host_control_helper
                    .enable_private_screen(from_connection_id, false);
            }
            SignalingType::EnablePrivateScreen => {
                let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
                if let Some(data) =
                    signaling_model.get_data_with_type::<EnablePrivateScreenData>()?
                {
                    if data.enable {
                        let allow_private_screen =
                            { self.settings.read().await.security.allow_private_screen };
                        let approved = check_security_permission(
                            &self.settings,
                            self.security_approval_sender.as_ref(),
                            allow_private_screen,
                            SecurityPermissionType::PrivateScreen,
                            Some(from_connection_id.to_string()),
                        )
                        .await;

                        if !approved {
                            log::warn!(
                                "Enable private screen denied by security settings or user for {}",
                                from_connection_id
                            );
                            self.session
                                .send_error(
                                    &signaling_model.request_id,
                                    signaling_model.signaling_type.into(),
                                    Some(from_connection_id.to_string()),
                                    DeskErrorCode::PERMISSION_ERROR,
                                    "Private screen access denied",
                                )
                                .await?;
                            return Ok(());
                        }
                    }

                    let _ = self
                        .host_control_helper
                        .enable_private_screen(&from_connection_id, data.enable);
                }
            }
            SignalingType::ManagerFileList => {
                handle_manager_file_list(self, signaling_model).await?;
            }
            SignalingType::ManagerFileDelete => {
                handle_manager_file_delete(self, signaling_model).await?;
            }
            SignalingType::StartTerminal => {
                // let's assume it was receiving &signaling_model in the codebase because that's the only one available
                handle_manager_terminal_start(self, signaling_model).await?;
            }
            SignalingType::SendDataToTerminal => {
                handle_manager_terminal_data(self, signaling_model).await?;
            }
            SignalingType::ResizeTerminal => {
                handle_manager_terminal_resize(self, signaling_model).await?;
            }
            SignalingType::CloseTerminal => {
                handle_manager_terminal_close(self, signaling_model).await?;
            }
            SignalingType::ListTerminal => {
                handle_list_terminals(self, signaling_model).await?;
            }
            SignalingType::ManagerSystemInfo => {
                // Respond with local system information
                let mut sys = sysinfo::System::new_all();
                sys.refresh_all();
                let mut system_info = crate::model::info::SystemInfo::from(&sys);
                let startup_mode = { self.settings.read().await.args.startup_mode.clone() };
                system_info.startup_mode = startup_mode.clone();
                system_info.is_admin =
                    if startup_mode != crate::model::settings::StartupMode::Signaling {
                        Some(desk_utils::permission::is_admin())
                    } else {
                        None
                    };
                let facade_info = system_info.to_facade();
                self.session
                    .send_response(
                        &signaling_model.request_id,
                        SignalingType::ManagerSystemInfo,
                        signaling_model.from_connection_id.clone(),
                        &facade_info,
                    )
                    .await?;
            }
            SignalingType::ManagerQuerySettings => {
                // Respond with remote-accessible system settings
                let remote_settings = {
                    let settings = self.settings.read().await;
                    desk_signal_facade::model::system_settings::RemoteSystemSettings {
                        enable_ipv6: settings.system.enable_ipv6,
                        port: settings.system.port,
                        listen_addr_ipv4: settings.system.listen_addr_ipv4.clone(),
                        listen_addr_ipv6: settings.system.listen_addr_ipv6.clone(),
                        locale: settings.system.locale.clone(),
                        signaling_url: settings.system.signaling_url.clone(),
                        signaling_token: settings.system.signaling_token.clone(),
                        manager_url: settings.system.manager_url.clone(),
                        auto_start: settings.system.auto_start,
                        manager_api_token: settings.system.manager_api_token.clone(),
                    }
                };
                self.session
                    .send_response(
                        &signaling_model.request_id,
                        SignalingType::ManagerQuerySettings,
                        signaling_model.from_connection_id.clone(),
                        &remote_settings,
                    )
                    .await?;
            }
            SignalingType::ManagerUpdateSettings => {
                // Update system settings from remote request
                let remote_settings = signaling_model
                    .get_data::<desk_signal_facade::model::system_settings::RemoteSystemSettings>()?;
                {
                    let mut settings = self.settings.write().await;
                    settings.system.enable_ipv6 = remote_settings.enable_ipv6;
                    settings.system.port = remote_settings.port;
                    settings.system.listen_addr_ipv4 = remote_settings.listen_addr_ipv4;
                    settings.system.listen_addr_ipv6 = remote_settings.listen_addr_ipv6;
                    settings.system.locale = remote_settings.locale;
                    settings.system.signaling_url = remote_settings.signaling_url;
                    settings.system.signaling_token = remote_settings.signaling_token;
                    settings.system.manager_url = remote_settings.manager_url;
                    settings.system.auto_start = remote_settings.auto_start;
                    settings.system.manager_api_token = remote_settings.manager_api_token;
                    settings.save()?;
                }
                self.session
                    .send_response(
                        &signaling_model.request_id,
                        SignalingType::ManagerUpdateSettings,
                        signaling_model.from_connection_id.clone(),
                        &(),
                    )
                    .await?;
            }
            /*
            SignalingType::Version => {
                // send back a message to client
                self.handle_version(&signaling_model).await?;
            }
             */
            _ => {
                error!("Unknown signaling type: {}", signaling_model.signaling_type);

                self.session
                    .send_error(
                        &signaling_model.request_id,
                        signaling_model.signaling_type.into(),
                        signaling_model.from_connection_id.clone(),
                        DeskErrorCode::UNKNOWN_SIGNALING_TYPE,
                        &format!(
                            "Failed to handle signaling type: {}",
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
        let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
        let rtc_peer_connection = self.get_rtc_peer_connection(&from_connection_id)?;
        let offer_model = signaling_model.get_data::<OfferModel>()?;

        // start webrtc first
        let mut rwlock_peer_connection = rtc_peer_connection.write().await;
        let peer_connection = rwlock_peer_connection.deref_mut();
        self.start_webrtc(
            &signaling_model.request_id,
            &from_connection_id,
            &offer_model,
            peer_connection,
        )
        .await?;
        // Set the remote SessionDescription
        peer_connection
            .rtc_peer_connection
            .set_remote_description(offer_model.offer)
            .await?;
        let answer = peer_connection
            .rtc_peer_connection
            .create_answer(None)
            .await?;

        peer_connection
            .rtc_peer_connection
            .set_local_description(answer)
            .await?;

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
                    &from_connection_id,
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

        if let Some(from_connection_id) = &signaling_model.from_connection_id {
            if let Some(peer_connection) = self.rtc_peer_connection_map.get(from_connection_id) {
                let peer_connection = peer_connection.read().await;
                let mut signaling_state = peer_connection.signaling_state.write().await;
                signaling_state.wayland_control_mode = desk_settings.wayland_control_mode.clone();
            }
        }

        // notify the new desk settings to the capture screen task
        if let Some(sender) = &self.update_setting_sender {
            if let Err(e) = sender.send(WebRTConnectionState::UpdateSettings(desk_settings)) {
                error!("Failed to send update settings: {}", e);
            }
        }

        Ok(())
    }

    pub async fn handle_request_control(
        &mut self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskError> {
        let from_connection_id = signaling_model.check_and_get_from_connection_id()?;
        let rtc_peer_connection = self.get_rtc_peer_connection(&from_connection_id)?;

        let control_data = signaling_model.get_data::<SignalRequestControlData>()?;
        log::info!(
            "Received RequireControl signaling from {}, control_data: {:?}",
            from_connection_id,
            control_data
        );

        // check security permission!
        let allow_control = { self.settings.read().await.security.allow_remote_control };
        let allow_clipboard = { self.settings.read().await.security.allow_clipboard_sync };

        let control_approved = check_security_permission(
            &self.settings,
            self.security_approval_sender.as_ref(),
            allow_control,
            SecurityPermissionType::RemoteControl,
            Some(from_connection_id.to_string()),
        )
        .await;

        if !control_approved {
            log::warn!(
                "Remote control request denied by security settings or user for {}",
                from_connection_id
            );
            self.session
                .send_to_peer(
                    &signaling_model.request_id,
                    SignalingType::DenyControl,
                    &from_connection_id,
                    (),
                )
                .await?;
            return Ok(());
        }

        let clipboard_approved = if control_data.accept_clipboard_sync {
            check_security_permission(
                &self.settings,
                self.security_approval_sender.as_ref(),
                allow_clipboard,
                SecurityPermissionType::ClipboardSync,
                Some(from_connection_id.to_string()),
            )
            .await
        } else {
            false
        };

        let peer_connection = rtc_peer_connection.read().await;
        let mut signaling_state = peer_connection.signaling_state.write().await;

        let reply_type = if control_data.accept {
            signaling_state.accept_control = true;
            signaling_state.accept_clipboard_sync = clipboard_approved;
            log::info!(
                "Auto accepting control request from {}, sending AcceptControl signaling",
                from_connection_id
            );
            SignalingType::AcceptControl
        } else {
            signaling_state.accept_control = false;
            signaling_state.accept_clipboard_sync = false;
            let _ = self
                .host_control_helper
                .enable_private_screen(from_connection_id, false);
            log::info!(
                "Releasing control request from {}, sending CloseControl signaling (also disabling private screen if any)",
                from_connection_id
            );
            SignalingType::CloseControl
        };

        self.session
            .send_to_peer(
                &signaling_model.request_id,
                reply_type,
                &from_connection_id,
                (),
            )
            .await?;

        Ok(())
    }
}
