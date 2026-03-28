use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::{BaseUser, CurrentUser};
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::{
    error::DeskSignalFacadeError,
    model::{
        connection::{ConnectionList, ConnectionModel},
        signal::{
            ForwardSignalingSender, InitSignalingData, RemoteDeskTypeEnum,
            RequestRemoteModel, SignalingModel, SignalingType,
        },
        version::VersionInfo,
    },
};
use desk_turn::model::TurnSettings;
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json;
use std::net::{IpAddr, SocketAddr};
use tokio::{runtime::Handle, sync::RwLock};
use uuid::Uuid;

use crate::{
    error::DeskSignalError,
    model::{ConnectionState, SharedConnectionMap},
};

pub async fn handle_signaling(
    client_version_info: VersionInfo,
    stream: AggregatedMessageStream,
    connection_map: web::Data<SharedConnectionMap>,
    ws_session: Session,
    user: CurrentUser,
    ip: Option<String>,
    turn: TurnSettings,
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    let random_uuid = Uuid::new_v4();
    let connection_id = String::from(random_uuid);
    // Handle signaling logic here
    let mut signaling_context = SignalingContext::init(
        connection_id,
        client_version_info,
        connection_map,
        ws_session,
        user,
        ip,
        turn,
    )
    .await?;

    let result = signaling_context.do_handle_signaling(stream).await;
    // Shutdown function must be invoked to clean up resources.
    // signaling_context.shutdown().await?;
    result
}

/// Signaling context for handling WebSocket messages.
pub struct SignalingContext<T: BaseUser> {
    pub connection_state: ConnectionState,
    pub connection_map: web::Data<SharedConnectionMap>,
    pub user: T,
    pub turn: TurnSettings,
}

fn parse_ip_from_peer_addr(addr: &str) -> Option<IpAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Some(ip);
    }
    None
}

fn rewrite_mdns_candidate_with_ip(
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

impl ForwardSignalingSender for ConnectionState {
    async fn send_response(
        &self,
        from_connection_id: Option<String>,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::success_response(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            from_connection_id,
            Some(self.model.connection_id.clone()),
            signaling_model.get_raw_data().as_ref(),
        )?;
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;
        Ok(())
    }

    async fn send_to_peer(
        &self,
        from_connection_id: &str,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::new(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            Some(from_connection_id.to_owned()),
            Some(self.model.connection_id.clone()),
            signaling_model.get_raw_data().clone(),
            signaling_model.response_state.clone(),
        );
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;

        Ok(())
    }

    async fn request_peer_with_callback<T>(
        &self,
        signaling_type: SignalingType,
        data: Option<&T>,
        timeout: Option<Duration>,
    ) -> Result<SignalingModel, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize + Sync,
    {
        let signaling_model =
            SignalingModel::new_request(signaling_type, Some(self.model.connection_id.clone()), data)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;
        self.request_callback_map
            .write()
            .await
            .insert(signaling_model.request_id.clone(), tx);

        // TODO: timeout should be configured in the config file
        let timeout = timeout.unwrap_or(Duration::from_secs(30));
        let result = tokio::time::timeout(timeout, rx).await;
        match result {
            Ok(Ok(signaling_model)) => Ok(signaling_model),
            Ok(Err(e)) => {
                // try to remove the request callback map
                let _ = self
                    .request_callback_map
                    .write()
                    .await
                    .remove(&signaling_model.request_id);
                DeskSignalFacadeError::custom_error(DeskErrorCode::TIMEOUT, &e.to_string())
            }
            Err(e) => {
                // try to remove the request callback map
                let _ = self
                    .request_callback_map
                    .write()
                    .await
                    .remove(&signaling_model.request_id);
                DeskSignalFacadeError::custom_error(DeskErrorCode::TIMEOUT, &e.to_string())
            }
        }
    }
}

impl<T: BaseUser> Drop for SignalingContext<T> {
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

impl<T: BaseUser> SignalingContext<T> {
    /// Initialize a new SignalingContext.
    pub async fn init(
        connection_id: String,
        client_version_info: VersionInfo,
        connection_map: web::Data<SharedConnectionMap>,
        ws_session: Session,
        user: T,
        ip: Option<String>,
        turn: TurnSettings,
    ) -> Result<Self, DeskSignalError> {
        log::info!("Init new SignalingContext, connection id: {}", connection_id);
        if client_version_info.api_version > SERVER_API_VERSION {
            log::warn!(
                "Client API version({}) is higher than server's({}). This may cause compatibility issues.",
                client_version_info.api_version,
                SERVER_API_VERSION
            );
        }

        let connection_model = ConnectionModel {
            connection_id: connection_id.clone(),
            version_info: client_version_info.clone(),
            ip,
        };

        let mut device_code = None;
        if client_version_info.remote_desk_type == RemoteDeskTypeEnum::Server {
            if let Some(client_id) = &client_version_info.client_id {
                let db = crate::db::get_db();
                use crate::entity::device_code;
                use sea_orm::*;

                let db_model_opt = device_code::Entity::find()
                    .filter(device_code::Column::ClientId.eq(client_id.clone()))
                    .one(db)
                    .await?;

                if let Some(db_model) = db_model_opt {
                    device_code = Some(db_model.device_code);
                } else {
                    let new_code = desk_utils::string::generate_device_code(6);

                    let new_model = device_code::ActiveModel {
                        client_id: Set(client_id.clone()),
                        device_code: Set(new_code.clone()),
                        created_at: Set(chrono::Utc::now()),
                        updated_at: Set(chrono::Utc::now()),
                        ..Default::default()
                    };

                    if let Err(e) = new_model.insert(db).await {
                        log::error!("Failed to generate device_code: {}", e);
                    } else {
                        device_code = Some(new_code);
                    }
                }
            }
        }

        let connection_state = ConnectionState {
            model: connection_model,
            session: Arc::new(RwLock::new(ws_session)),
            terminal_connection_ids: Arc::new(RwLock::new(HashSet::new())),
            request_callback_map: Arc::new(RwLock::new(HashMap::new())),
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

    /// Send data to target peer
    pub async fn forward_to_peer(
        &self,
        signaling_model: &SignalingModel,
        ignore_connection_not_found: bool,
    ) -> Result<(), DeskSignalError> {
        // Device user restriction logic
        if self.user.get_access() == Some("device_user") {
            if let Some(target_connection) = self.user.get_target_connection_id() {
                let to_connection_id = signaling_model.check_and_get_to_connection_id()?;
                if to_connection_id != target_connection {
                    return DeskSignalError::custom_error(
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
                DeskSignalError::new_custom_error(
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
            return DeskSignalError::custom_error(
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
    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskSignalError> {
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
                        return DeskSignalError::custom_error(
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
                        return DeskSignalError::custom_error(
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
                    let ice_server = self
                        .turn
                        .get_ice_servers(&self.connection_state.model.connection_id, &client_id);

                    data.ice_servers.push(ice_server);
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
                    let ice_server = self
                        .turn
                        .get_ice_servers(&self.connection_state.model.connection_id, &client_id);

                    data.ice_servers.push(ice_server);
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

    pub async fn binary(&mut self, bin: Bytes) -> Result<(), DeskSignalError> {
        log::debug!("Received binary message: {} bytes", bin.len());
        Ok(())
    }

    pub async fn ping(&mut self, bin: Bytes) -> Result<(), DeskSignalError> {
        self.connection_state.session.write().await.pong(&bin).await?;
        Ok(())
    }

    pub async fn do_handle_signaling(
        &mut self,
        mut stream: AggregatedMessageStream,
    ) -> Result<(), DeskSignalError> {
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
