use std::collections::BTreeMap;

use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::{
    session::SessionList,
    signal::{SignalingModel, SignalingSessionExt, SignalingType},
    version::VersionInfo,
};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::error::DeskSignalError;

pub async fn handle_signaling(
    client_version_info: VersionInfo,
    stream: AggregatedMessageStream,
    session: Session,
    user: CurrentUser,
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    let random_uuid = Uuid::new_v4();
    let session_id = String::from(random_uuid);
    // Handle signaling logic here
    let mut signaling_context =
        SignalingContext::init(session_id, client_version_info, session, user).await?;

    let result = signaling_context.do_handle_signaling(stream).await;
    // Shutdown function must be invoked to clean up resources.
    // signaling_context.shutdown().await?;
    result
}

/// Signaling context for handling WebSocket messages.
pub struct SignalingContext {
    pub session_id: String,
    pub session: Session,
    pub user: CurrentUser,
    pub client_version_info: VersionInfo,
}

impl SignalingContext {
    /// Initialize a new SignalingContext. This function sends the server's version information to the client.
    pub async fn init(
        session_id: String,
        client_version_info: VersionInfo,
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
        let server_version_info = VersionInfo::new(SERVER_API_VERSION, None);
        session
            .send_signaling(&SignalingModel::new_json_data(
                SignalingType::Version,
                &server_version_info,
            )?)
            .await?;
        Ok(Self {
            session_id,
            client_version_info,
            session,
            user,
        })
    }
    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskSignalError> {
        log::debug!("Received text message: {}", text);
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            SignalingType::FetchSessions => {
                let session_list = SessionList {
                    current_session_id: self.session_id.clone(),
                    session_map: BTreeMap::new(),
                };
                let model =
                    SignalingModel::new_json_data(SignalingType::SessionList, &session_list)?;
                log::info!("Sending session list to client: {:?}", model);
                self.session.send_signaling(&model).await?;
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
