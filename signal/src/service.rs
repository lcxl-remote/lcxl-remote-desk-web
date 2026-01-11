use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_server_user::model::CurrentUser;
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::{signal::SignalingModel, version::VersionInfo};
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;

use crate::error::DeskSignalError;

pub async fn handle_signaling(
    stream: AggregatedMessageStream,
    session: Session,
    user: CurrentUser,
) -> Result<(), DeskSignalError> {
    log::info!("Handling signaling");
    // Handle signaling logic here
    let mut signaling_context = SignalingContext::new(session, user);

    let result = signaling_context.do_handle_signaling(stream).await;
    // Shutdown function must be invoked to clean up resources.
    // signaling_context.shutdown().await?;
    result
}

/// Signaling context for handling WebSocket messages.
pub struct SignalingContext {
    pub session: Session,
    pub user: CurrentUser,
    pub signal_server_version: Option<VersionInfo>,
}

impl SignalingContext {
    pub fn new(session: Session, user: CurrentUser) -> Self {
        Self {
            session,
            user,
            signal_server_version: None,
        }
    }
    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskSignalError> {
        log::debug!("Received text message: {}", text);
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            desk_signal_facade::model::signal::SignalingType::Version => {
                let version_info: VersionInfo = signaling_model.get_data()?;
                if version_info.api_version < SERVER_API_VERSION {
                    return DeskSignalError::custom_error(
                        DeskErrorCode::INVALID_STATE,
                        format!("Unsupported api version: {}", version_info.api_version),
                    );
                }
                self.signal_server_version = Some(version_info);
            }
            desk_signal_facade::model::signal::SignalingType::Init => todo!(),
            desk_signal_facade::model::signal::SignalingType::Offer => todo!(),
            desk_signal_facade::model::signal::SignalingType::Answer => todo!(),
            desk_signal_facade::model::signal::SignalingType::Canid => todo!(),
            desk_signal_facade::model::signal::SignalingType::RequireControl => todo!(),
            desk_signal_facade::model::signal::SignalingType::AcceptControl => todo!(),
            desk_signal_facade::model::signal::SignalingType::DenyControl => todo!(),
            desk_signal_facade::model::signal::SignalingType::CloseControl => todo!(),
            desk_signal_facade::model::signal::SignalingType::ChangeDisplaySettings => todo!(),
            desk_signal_facade::model::signal::SignalingType::UpdateDeskSettings => todo!(),
            desk_signal_facade::model::signal::SignalingType::ManagerFile => todo!(),
            desk_signal_facade::model::signal::SignalingType::ManagerTerminal => todo!(),
            desk_signal_facade::model::signal::SignalingType::ManagerSystemInfo => todo!(),
            desk_signal_facade::model::signal::SignalingType::ManagerSystemStatue => todo!(),
            desk_signal_facade::model::signal::SignalingType::Error => todo!(),
            desk_signal_facade::model::signal::SignalingType::Unknown => todo!(),
            _ => {
                log::error!("Unknown signaling type: {}", signaling_model.signaling_type);
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
