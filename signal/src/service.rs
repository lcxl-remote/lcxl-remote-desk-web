use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::{
    error::DeskSignalFacadeError,
    model::{
        session::{SessionList, SessionModel},
        signal::{ForwardSignalingSender, SignalingModel, SignalingType},
        version::VersionInfo,
    },
};
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::{runtime::Handle, sync::RwLock};
use uuid::Uuid;

use crate::{
    error::DeskSignalError,
    model::{SessionState, SharedSessionMap},
};

pub async fn handle_signaling(
    client_version_info: VersionInfo,
    stream: AggregatedMessageStream,
    session_map: web::Data<SharedSessionMap>,
    session: Session,
    user: CurrentUser,
    ip: Option<String>,
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    let random_uuid = Uuid::new_v4();
    let session_id = String::from(random_uuid);
    // Handle signaling logic here
    let mut signaling_context = SignalingContext::init(
        session_id,
        client_version_info,
        session_map,
        session,
        user,
        ip,
    )
    .await?;

    let result = signaling_context.do_handle_signaling(stream).await;
    // Shutdown function must be invoked to clean up resources.
    // signaling_context.shutdown().await?;
    result
}

/// Signaling context for handling WebSocket messages.
pub struct SignalingContext {
    pub session_state: SessionState,
    pub session_map: web::Data<SharedSessionMap>,
    pub user: CurrentUser,
}

impl ForwardSignalingSender for SessionState {
    async fn send_response(
        &self,
        from_session_id: Option<String>,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::success_response(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            from_session_id,
            Some(self.model.session_id.clone()),
            signaling_model.get_raw_data().as_ref(),
        )?;
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;
        Ok(())
    }

    async fn forward_to_peer(
        &self,
        from_session_id: &str,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::new(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            Some(from_session_id.to_owned()),
            Some(self.model.session_id.clone()),
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
            SignalingModel::new_request(signaling_type, Some(self.model.session_id.clone()), data)?;
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

impl Drop for SignalingContext {
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
        let session_id = self.session_state.model.session_id.clone();
        let session_map = self.session_map.clone();
        let removed_value = futures::executor::block_on(async move {
            handle
                .spawn_blocking(move || session_map.blocking_write().remove(&session_id))
                .await
        });
        match removed_value {
            Ok(None) => log::error!(
                "Failed to remove session from map: session {} not found",
                self.session_state.model.session_id
            ),
            Ok(Some(session_state)) => {
                log::info!("Removed session from map: {:?}", session_state.model)
            }
            Err(err) => log::error!("Failed to remove session from map: {:?}", err),
        }
    }
}

impl SignalingContext {
    /// Initialize a new SignalingContext.
    pub async fn init(
        session_id: String,
        client_version_info: VersionInfo,
        session_map: web::Data<SharedSessionMap>,
        session: Session,
        user: CurrentUser,
        ip: Option<String>,
    ) -> Result<Self, DeskSignalError> {
        log::info!("Init new SignalingContext, session id: {}", session_id);
        if client_version_info.api_version > SERVER_API_VERSION {
            log::warn!(
                "Client API version({}) is higher than server's({}). This may cause compatibility issues.",
                client_version_info.api_version,
                SERVER_API_VERSION
            );
        }

        let session_model = SessionModel {
            session_id: session_id.clone(),
            version_info: client_version_info.clone(),
            ip,
        };

        let session_state = SessionState {
            model: session_model,
            session: Arc::new(RwLock::new(session)),
            terminal_session_ids: Arc::new(RwLock::new(HashSet::new())),
            request_callback_map: Arc::new(RwLock::new(HashMap::new())),
        };

        session_map
            .write()
            .await
            .insert(session_id.clone(), session_state.clone());
        Ok(Self {
            session_state,
            session_map,
            user,
        })
    }

    pub async fn send_request(
        &self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalError> {
        self.session_state
            .forward_to_peer(&self.session_state.model.session_id, signaling_model)
            .await?;
        Ok(())
    }

    /// Send data to target peer
    pub async fn forward_to_peer(
        &self,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalError> {
        if let Some(tx) = self
            .session_state
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
        let to_session_id = signaling_model.check_and_get_to_session_id()?;
        let session_map = self.session_map.read().await;
        let to_session_state = if let Some(session_state) = session_map.get(&to_session_id) {
            session_state
        } else {
            return DeskSignalError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("Session {} not found", to_session_id),
            );
        };
        to_session_state
            .forward_to_peer(&self.session_state.model.session_id, signaling_model)
            .await?;

        Ok(())
    }

    /// Handle incoming signaling message
    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskSignalError> {
        log::debug!("Received text message: {}", text);
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            SignalingType::FetchSessions => {
                let session_map = {
                    let session_map = self.session_map.read().await;
                    session_map
                        .iter()
                        .map(|item| (item.0.clone(), item.1.model.clone()))
                        .collect()
                };
                let session_list = SessionList {
                    current_session_id: self.session_state.model.session_id.clone(),
                    session_map,
                };

                log::info!("Sending session list to client: {:?}", session_list);
                let response = SignalingModel::success_response(
                    &signaling_model.request_id,
                    SignalingType::SessionList,
                    None,
                    None,
                    Some(&session_list),
                )?;
                self.session_state.send_response(None, &response).await?;
            }
            SignalingType::StartTerminal => {
                let to_session_id = signaling_model.check_and_get_to_session_id()?;
                self.forward_to_peer(&signaling_model).await?;
                if signaling_model.is_request() {
                    self.session_state
                        .terminal_session_ids
                        .write()
                        .await
                        .insert(to_session_id);
                }
            }
            SignalingType::CloseTerminal => {
                let to_session_id = signaling_model.check_and_get_to_session_id()?;
                self.forward_to_peer(&signaling_model).await?;
                if signaling_model.is_request() {
                    self.session_state
                        .terminal_session_ids
                        .write()
                        .await
                        .remove(&to_session_id);
                }
            }

            SignalingType::SendDataToTerminal => {
                let to_session_id = signaling_model.check_and_get_to_session_id()?;
                if signaling_model.is_request() {
                    if !self
                        .session_state
                        .terminal_session_ids
                        .read()
                        .await
                        .contains(&to_session_id)
                    {
                        return DeskSignalError::custom_error(
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!("Session {} is not a terminal", to_session_id),
                        );
                    }
                }
                self.forward_to_peer(&signaling_model).await?;
            }

            // Forwarding types
            SignalingType::RequestRemote
            | SignalingType::Init
            | SignalingType::Offer
            | SignalingType::Answer
            | SignalingType::Canid
            | SignalingType::RequireControl
            | SignalingType::AcceptControl
            | SignalingType::DenyControl
            | SignalingType::CloseControl
            | SignalingType::ChangeDisplaySettings
            | SignalingType::UpdateDeskSettings
            | SignalingType::ManagerFileList
            | SignalingType::ManagerSystemInfo
            | SignalingType::ManagerSystemStatue
            | SignalingType::ListTerminal
            | SignalingType::ReplyFromTerminal => {
                // Generic forwarding
                self.forward_to_peer(&signaling_model).await?;
            }

            SignalingType::Error => {
                log::warn!("Received error from client: {:?}", signaling_model);
            }
            SignalingType::Unknown => {
                log::warn!("Received unknown signaling type");
            }
            _ => {
                log::error!(
                    "Unsupported signaling type: {}",
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
        self.session_state.session.write().await.pong(&bin).await?;
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
                    self.handle_message(text).await?;
                }

                Ok(AggregatedMessage::Binary(bin)) => {
                    // echo binary message
                    self.binary(bin).await?;
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
