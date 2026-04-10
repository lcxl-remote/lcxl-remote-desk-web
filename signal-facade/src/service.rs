use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;
use serde_json;
use tokio::{runtime::Handle, sync::RwLock};

use crate::{
    error::DeskSignalFacadeError,
    model::{
        connection::{ConnectionList, ConnectionModel, ConnectionState, SharedConnectionMap},
        signal::{
            ForwardSignalingSender, InitSignalingData,
            RequestRemoteModel, SignalingModel, SignalingType, SignalingUser,
            TurnProvider,
        },
        version::VersionInfo,
    },
};

// ====== DeviceCodeService trait ======

/// Trait for device code operations.  
/// Signal implements this with SQLite DB.
/// Manager can return None (no device codes in manager).
pub trait DeviceCodeService: Send + Sync {
    fn get_or_create_device_code(
        &self,
        client_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, DeskSignalFacadeError>> + Send;
}

/// A no-op implementation that always returns None
pub struct NoOpDeviceCodeService;

impl DeviceCodeService for NoOpDeviceCodeService {
    async fn get_or_create_device_code(
        &self,
        _client_id: &str,
    ) -> Result<Option<String>, DeskSignalFacadeError> {
        Ok(None)
    }
}

// ====== NodeTokenValidator trait ======

/// Trait for validating node tokens (e.g. manager API tokens).
pub trait NodeTokenValidator: Send + Sync {
    fn validate_node_token<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
}

// ====== 通用工具函数 ======

pub fn parse_ip_from_peer_addr(addr: &str) -> Option<IpAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Some(ip);
    }
    None
}

pub fn rewrite_mdns_candidate_with_ip(
    signaling_model: &SignalingModel,
    fallback_ip: IpAddr,
) -> Option<SignalingModel> {
    let data = match signaling_model.get_raw_data() {
        Some(d) => d.clone(),
        None => return None,
    };
    let mut obj = match data.as_object() {
        Some(o) => o.clone(),
        None => return None,
    };

    let candidate_value = match obj.get("candidate") {
        Some(v) => v,
        None => return None,
    };
    let candidate_str = match candidate_value.as_str() {
        Some(s) => s,
        None => return None,
    };

    if !candidate_str.contains(".local") {
        return None;
    }

    let mut parts = candidate_str
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if parts.len() < 6 {
        return None;
    }

    let host = parts[4].clone();
    if !host.ends_with(".local") {
        return None;
    }

    parts[4] = fallback_ip.to_string();
    let new_candidate = parts.join(" ");
    obj.insert(
        "candidate".to_string(),
        serde_json::Value::String(new_candidate.clone()),
    );

    log::info!(
        "Rewrote mDNS ICE candidate using signaling peer IP {}: {} -> {}",
        fallback_ip,
        host,
        new_candidate
    );

    Some(SignalingModel::new(
        &signaling_model.request_id,
        signaling_model.signaling_type,
        signaling_model.from_connection_id.clone(),
        signaling_model.to_connection_id.clone(),
        Some(serde_json::Value::Object(obj)),
        signaling_model.response_state.clone(),
    ))
}

// ====== SignalingHandler ======

/// Generic signaling handler. Usable by both signal server and manager.
pub struct SignalingHandler<U: SignalingUser> {
    pub connection_state: ConnectionState,
    pub connection_map: web::Data<SharedConnectionMap>,
    pub user: U,
    pub turn: Option<Arc<dyn TurnProvider>>,
}

impl<U: SignalingUser> Drop for SignalingHandler<U> {
    fn drop(&mut self) {
        let handle = match Handle::try_current() {
            Ok(handle) => handle,
            Err(e) => {
                log::warn!(
                    "Failed to get tokio handle in SignalingContext::drop: {}",
                    e
                );
                return;
            }
        };
        let connection_id = self.connection_state.model.connection_id.clone();
        let connection_map = self.connection_map.clone();
        let removed_value = futures::executor::block_on(async move {
            handle
                .spawn_blocking(move || connection_map.blocking_write().remove(&connection_id))
                .await
        });
        match removed_value {
            Ok(None) => log::error!(
                "Failed to remove connection from map: connection {} not found",
                self.connection_state.model.connection_id
            ),
            Ok(Some(connection_state)) => {
                log::info!("Removed connection from map: {:?}", connection_state.model)
            }
            Err(err) => log::error!("Failed to remove connection from map: {:?}", err),
        }
    }
}

impl<U: SignalingUser> SignalingHandler<U> {
    /// Initialize a new SignalingHandler.
    pub async fn init(
        connection_id: String,
        client_version_info: VersionInfo,
        connection_map: web::Data<SharedConnectionMap>,
        ws_session: Session,
        user: U,
        ip: Option<String>,
        turn: Option<Arc<dyn TurnProvider>>,
        device_code: Option<String>,
        server_api_version: i32,
    ) -> Result<Self, DeskSignalFacadeError> {
        log::info!("Init new SignalingContext, connection id: {}", connection_id);
        if client_version_info.api_version > server_api_version {
            log::warn!(
                "Client API version({}) is higher than server's({}). This may cause compatibility issues.",
                client_version_info.api_version,
                server_api_version
            );
        }

        let connection_model = ConnectionModel {
            connection_id: connection_id.clone(),
            version_info: client_version_info.clone(),
            ip,
        };

        let connection_state = ConnectionState {
            model: connection_model,
            session: Arc::new(RwLock::new(ws_session)),
            terminal_connection_ids: Arc::new(RwLock::new(std::collections::HashSet::new())),
            request_callback_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
            device_code,
        };

        connection_map
            .write()
            .await
            .insert(connection_id.clone(), connection_state.clone());
        Ok(Self {
            connection_state,
            connection_map,
            user,
            turn,
        })
    }

    /// Forward a signaling message to target peer
    pub async fn forward_to_peer(
        &self,
        signaling_model: &SignalingModel,
        ignore_connection_not_found: bool,
    ) -> Result<(), DeskSignalFacadeError> {
        // Device user restriction logic
        if self.user.get_access() == Some("device_user") {
            if let Some(target_connection) = self.user.get_target_connection_id() {
                let to_connection_id = signaling_model.check_and_get_to_connection_id()?;
                if to_connection_id != target_connection {
                    return DeskSignalFacadeError::custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        &format!(
                            "Permission denied: cannot send message to {}",
                            to_connection_id
                        ),
                    );
                }
            }
        }

        if let Some(tx) = self
            .connection_state
            .request_callback_map
            .write()
            .await
            .remove(&signaling_model.request_id)
        {
            tx.send(signaling_model.clone()).map_err(|_| {
                DeskSignalFacadeError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Failed to send response to peer",
                )
            })?;
            return Ok(());
        }
        let to_connection_id = signaling_model.check_and_get_to_connection_id()?;
        let connection_map = self.connection_map.read().await;
        let to_connection_state = if let Some(connection_state) = connection_map.get(to_connection_id) {
            connection_state
        } else {
            if ignore_connection_not_found {
                log::warn!(
                    "Connection {} is not found to forward signaling, ignore it: {:?}",
                    to_connection_id,
                    signaling_model
                );
                return Ok(());
            }
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::SESSION_NOT_FOUND,
                &format!(
                    "Connection {} is not found to forward signaling: {:?}",
                    to_connection_id, signaling_model
                ),
            );
        };
        to_connection_state
            .send_to_peer(&self.connection_state.model.connection_id, signaling_model)
            .await?;

        Ok(())
    }

    /// Handle incoming signaling message
    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskSignalFacadeError> {
        log::debug!("Received text message: {}", text);
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            SignalingType::Heartbeat => {
                // Respond to heartbeat immediately to keep connection alive
                let response = SignalingModel::success_response::<()>(
                    &signaling_model.request_id,
                    SignalingType::Heartbeat,
                    None,
                    None,
                    None,
                )?;
                self.connection_state.send_response(None, &response).await?;
            }
            SignalingType::FetchConnections => {
                let connection_map = {
                    let connection_map = self.connection_map.read().await;
                    connection_map
                        .iter()
                        .map(|item| (item.0.clone(), item.1.model.clone()))
                        .collect()
                };
                let connection_list = ConnectionList {
                    current_connection_id: self.connection_state.model.connection_id.clone(),
                    connection_map,
                };

                log::info!("Sending connection list to client: {:?}", connection_list);
                let response = SignalingModel::success_response(
                    &signaling_model.request_id,
                    SignalingType::ConnectionList,
                    None,
                    None,
                    Some(&connection_list),
                )?;
                self.connection_state.send_response(None, &response).await?;
            }
            SignalingType::ConnectionList => {
                log::warn!(
                    "Received connection list signaling type: {}, it should not be received",
                    signaling_model.signaling_type
                );
            }
            SignalingType::SendDataToTerminal => {
                let from_connection_id = &self.connection_state.model.connection_id;
                if signaling_model.is_request() {
                    if !self
                        .connection_state
                        .terminal_connection_ids
                        .read()
                        .await
                        .contains(from_connection_id)
                    {
                        return DeskSignalFacadeError::custom_error(
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!(
                                "Connection {} is not a terminal, can not send data to terminal",
                                from_connection_id
                            ),
                        );
                    }
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }
            SignalingType::ResizeTerminal => {
                let from_connection_id = &self.connection_state.model.connection_id;
                if signaling_model.is_request() {
                    if !self
                        .connection_state
                        .terminal_connection_ids
                        .read()
                        .await
                        .contains(from_connection_id)
                    {
                        return DeskSignalFacadeError::custom_error(
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!(
                                "Connection {} is not a terminal, can not resize terminal",
                                from_connection_id
                            ),
                        );
                    }
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            SignalingType::ReplyFromTerminal | SignalingType::TerminalClosed => {
                self.forward_to_peer(&signaling_model, true).await?;
            }

            SignalingType::Canid => {
                let fallback_ip = self
                    .connection_state
                    .model
                    .ip
                    .as_deref()
                    .and_then(parse_ip_from_peer_addr);
                if let Some(ip) = fallback_ip.map(|ip| {
                    if ip.is_ipv6() && ip.is_loopback() {
                        IpAddr::from([127, 0, 0, 1])
                    } else {
                        ip
                    }
                }) {
                    if let Some(rewritten) = rewrite_mdns_candidate_with_ip(&signaling_model, ip) {
                        self.forward_to_peer(&rewritten, false).await?;
                        return Ok(());
                    }
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            SignalingType::RequestRemote => {
                let mut data = signaling_model.get_data_with_default::<RequestRemoteModel>()?;
                // TODO need to support static auth secret
                // ice servers
                // username is connection_id
                // password is client id
                let client_id_opt = self.connection_state.model.version_info.client_id.clone();
                if let Some(client_id) = client_id_opt {
                    if let Some(turn) = &self.turn {
                        let ice_server =
                            turn.get_ice_servers(&self.connection_state.model.connection_id, &client_id);
                        if !ice_server.urls.is_empty() {
                            data.ice_servers.push(ice_server);
                        } else {
                            log::warn!(
                                "Skipping empty TURN ICE servers for connection {}",
                                self.connection_state.model.connection_id
                            );
                        }
                    } else {
                        log::warn!(
                            "TURN settings unavailable, skip injecting TURN ICE for connection {}",
                            self.connection_state.model.connection_id
                        );
                    }
                }
                let data = Some(serde_json::to_value(data)?);
                let new_signaling_model = SignalingModel::new(
                    signaling_model.request_id.as_str(),
                    signaling_model.signaling_type,
                    signaling_model.from_connection_id,
                    signaling_model.to_connection_id,
                    data,
                    signaling_model.response_state,
                );
                self.forward_to_peer(&new_signaling_model, false).await?;
            }

            SignalingType::Init => {
                let mut data = signaling_model.get_data::<InitSignalingData>()?;
                // TODO need to support static auth secret
                // ice servers
                // username is connection_id
                // password is client id
                let client_id_opt = self.connection_state.model.version_info.client_id.clone();
                if let Some(client_id) = client_id_opt {
                    if let Some(turn) = &self.turn {
                        let ice_server =
                            turn.get_ice_servers(&self.connection_state.model.connection_id, &client_id);
                        if !ice_server.urls.is_empty() {
                            data.ice_servers.push(ice_server);
                        } else {
                            log::warn!(
                                "Skipping empty TURN ICE servers for connection {}",
                                self.connection_state.model.connection_id
                            );
                        }
                    } else {
                        log::warn!(
                            "TURN settings unavailable, skip injecting TURN ICE for connection {}",
                            self.connection_state.model.connection_id
                        );
                    }
                }
                let data = Some(serde_json::to_value(data)?);
                let new_signaling_model = SignalingModel::new(
                    signaling_model.request_id.as_str(),
                    signaling_model.signaling_type,
                    signaling_model.from_connection_id,
                    signaling_model.to_connection_id,
                    data,
                    signaling_model.response_state,
                );
                self.forward_to_peer(&new_signaling_model, false).await?;
            }
            // Forwarding types
            SignalingType::Offer
            | SignalingType::Answer
            | SignalingType::RequireControl
            | SignalingType::AcceptControl
            | SignalingType::DenyControl
            | SignalingType::CloseControl
            | SignalingType::ChangeDisplaySettings
            | SignalingType::UpdateDeskSettings
            | SignalingType::ManagerFileList
            | SignalingType::ManagerFileDelete
            | SignalingType::ManagerSystemInfo
            | SignalingType::ManagerSystemStatue
            | SignalingType::ManagerQuerySettings
            | SignalingType::ManagerUpdateSettings
            | SignalingType::ListTerminal
            | SignalingType::EnablePrivateScreen
            | SignalingType::PrivateScreenStateChanged
            | SignalingType::TerminalStarted
            | SignalingType::AudioPlaybackError => {
                // Generic forwarding
                self.forward_to_peer(&signaling_model, false).await?;
            }

            SignalingType::Error => {
                log::warn!("Received error from client: {:?}", signaling_model);
            }
            SignalingType::Unknown => {
                log::warn!("Received unknown signaling type");
            }
            SignalingType::StartTerminal | SignalingType::CloseTerminal => {
                // This not send by client, it is send by signal server, so it is not need to handle, and should not be received
                log::warn!(
                    "Received start/close terminal signaling type: {}, it should not be received",
                    signaling_model.signaling_type
                );
            }
        }
        Ok(())
    }

    pub async fn binary(&mut self, _bin: Bytes) -> Result<(), DeskSignalFacadeError> {
        log::debug!("Received binary message: {} bytes", _bin.len());
        Ok(())
    }

    pub async fn ping(&mut self, bin: Bytes) -> Result<(), DeskSignalFacadeError> {
        self.connection_state.session.write().await.pong(&bin).await?;
        Ok(())
    }

    pub async fn do_handle_signaling(
        &mut self,
        mut stream: AggregatedMessageStream,
    ) -> Result<(), DeskSignalFacadeError> {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(AggregatedMessage::Text(text)) => {
                    // echo text message
                    if let Err(e) = self.handle_message(text).await {
                        log::error!("Error handling signaling message: {}", e);
                    }
                }

                Ok(AggregatedMessage::Binary(bin)) => {
                    // echo binary message
                    if let Err(e) = self.binary(bin).await {
                        log::error!("Error handling binary message: {}", e);
                    }
                }

                Ok(AggregatedMessage::Ping(msg)) => {
                    // respond to PING frame with PONG frame
                    self.ping(msg).await?;
                }
                Ok(AggregatedMessage::Pong(_)) => {
                    // ignore PONG frames
                }
                Ok(AggregatedMessage::Close(close_reason)) => {
                    log::warn!("WS close frame received: {:?}", close_reason);
                    break;
                }
                Err(e) => {
                    log::error!("WS error: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }
}
