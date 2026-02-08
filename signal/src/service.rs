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
        signal::{ForwardSignalingSender, SignalingModel, SignalingResponseState, SignalingType},
        version::VersionInfo,
    },
};
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::runtime::Handle;
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
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    let random_uuid = Uuid::new_v4();
    let session_id = String::from(random_uuid);
    // Handle signaling logic here
    let mut signaling_context =
        SignalingContext::init(session_id, client_version_info, session_map, session, user).await?;

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
    async fn send_signaling<T>(
        &mut self,
        signaling_type: SignalingType,
        from_session_id: Option<String>,
        signaling_data: &T,
    ) -> Result<(), DeskSignalFacadeError>
    where
        T: ?Sized + Serialize + Sync,
    {
        let signaling_model = SignalingModel::success_response(
            signaling_type,
            from_session_id,
            Some(self.model.session_id.clone()),
            signaling_data,
        )?;
        self.session
            .text(serde_json::to_string(&signaling_model)?)
            .await?;
        Ok(())
    }

    async fn forward_to_peer(
        &mut self,
        signaling_type: SignalingType,
        from_session_id: &str,
        data: Option<serde_json::Value>,
        response_state: Option<SignalingResponseState>,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::new(
            signaling_type,
            Some(from_session_id.to_owned()),
            Some(self.model.session_id.clone()),
            data,
            response_state,
        );
        self.session
            .text(serde_json::to_string(&signaling_model)?)
            .await?;

        Ok(())
    }
}

impl Drop for SignalingContext {
    fn drop(&mut self) {
        let handle = Handle::current();
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
    /// Initialize a new SignalingContext. This function sends the server's version information to the client.
    pub async fn init(
        session_id: String,
        client_version_info: VersionInfo,
        session_map: web::Data<SharedSessionMap>,
        session: Session,
        user: CurrentUser,
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
        };

        let session_state = SessionState {
            model: session_model,
            session: session.clone(),
        };
        /*
        let server_version_info = VersionInfo::new(
            SERVER_API_VERSION,
            version::SIGNAL_BUILD_NUMBER,
            version::SIGNAL_COMMIT_HASH.to_string(),
            RemoteDeskTypeEnum::Signal,
        );
        session_state
            .send_signaling(
                SignalingType::Version,
                Some(session_id.clone()),
                &server_version_info,
            )
            .await?;
         */
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

    /// Send data to target peer
    pub async fn send_peer(
        &mut self,
        signaling_type: SignalingType,
        to_session_id: &str,
        data: Option<serde_json::Value>,
        response_state: Option<SignalingResponseState>,
    ) -> Result<(), DeskSignalError> {
        {
            let mut session_map = self.session_map.write().await;
            let session_state = if let Some(session_state) = session_map.get_mut(to_session_id) {
                session_state
            } else {
                return DeskSignalError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    format!("Session {} not found", to_session_id),
                );
            };
            session_state
                .forward_to_peer(
                    signaling_type,
                    &self.session_state.model.session_id,
                    data,
                    response_state,
                )
                .await?;
        }

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
                self.session_state
                    .send_signaling(SignalingType::SessionList, None, &session_list)
                    .await?;
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
            | SignalingType::ManagerFile
            | SignalingType::ManagerTerminal
            | SignalingType::ManagerSystemInfo
            | SignalingType::ManagerSystemStatue => {
                // Generic forwarding
                // We need to parse as generic serde_json::Value
                // to forward without knowing the exact inner type.
                let to_session_id = signaling_model.check_and_get_to_session_id()?;
                let data = signaling_model.signaling_data;
                let response_state = signaling_model.response_state;

                self.send_peer(
                    signaling_model.signaling_type,
                    &to_session_id,
                    data,
                    response_state,
                )
                .await?;
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
        self.session_state.session.pong(&bin).await?;
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
