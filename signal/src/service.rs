use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::{
    session::{SessionList, SessionModel},
    signal::{RemoteDeskTypeEnum, SignalingModel, SignalingSessionExt, SignalingType},
    version::VersionInfo,
};
use futures_util::StreamExt;
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
    pub session_id: String,
    pub session_map: web::Data<SharedSessionMap>,
    pub session: Session,
    pub user: CurrentUser,
    pub client_version_info: VersionInfo,
}

impl Drop for SignalingContext {
    fn drop(&mut self) {
        let handle = Handle::current();
        let session_id = self.session_id.clone();
        let session_map = self.session_map.clone();
        let removed_value = futures::executor::block_on(async move {
            handle
                .spawn_blocking(move || session_map.blocking_write().remove(&session_id))
                .await
        });
        match removed_value {
            Ok(None) => log::error!(
                "Failed to remove session from map: session {} not found",
                self.session_id
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
        mut session: Session,
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
        let server_version_info =
            VersionInfo::new(SERVER_API_VERSION, None, RemoteDeskTypeEnum::Signal);
        let session_model = SessionModel {
            session_id: session_id.clone(),
            version_info: client_version_info.clone(),
        };
        session
            .send_signaling(SignalingType::Version, &server_version_info)
            .await?;
        let session_state = SessionState {
            model: session_model,
            session: session.clone(),
        };
        session_map
            .write()
            .await
            .insert(session_id.clone(), session_state);
        Ok(Self {
            session_id,
            client_version_info,
            session_map,
            session,
            user,
        })
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
                    current_session_id: self.session_id.clone(),
                    session_map,
                };

                log::info!("Sending session list to client: {:?}", session_list);
                self.session
                    .send_signaling(SignalingType::SessionList, &session_list)
                    .await?;
            }
            SignalingType::Init => todo!(),
            SignalingType::Offer => todo!(),
            SignalingType::Answer => todo!(),
            SignalingType::Canid => todo!(),
            SignalingType::RequireControl => todo!(),
            SignalingType::AcceptControl => todo!(),
            SignalingType::DenyControl => todo!(),
            SignalingType::CloseControl => todo!(),
            SignalingType::ChangeDisplaySettings => todo!(),
            SignalingType::UpdateDeskSettings => todo!(),
            SignalingType::ManagerFile => todo!(),
            SignalingType::ManagerTerminal => todo!(),
            SignalingType::ManagerSystemInfo => todo!(),
            SignalingType::ManagerSystemStatue => todo!(),
            SignalingType::Error => todo!(),
            SignalingType::Unknown => todo!(),
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
        self.session.pong(&bin).await?;
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
