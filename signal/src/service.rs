use actix_web::web;
use actix_ws::{AggregatedMessageStream, Session};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::{
    error::DeskSignalFacadeError,
    model::{
        connection::SharedConnectionMap,
        signal::RemoteDeskTypeEnum,
        version::VersionInfo,
    },
    service::{DeviceCodeService, SignalingHandler},
};
use desk_turn::model::TurnSettings;
use uuid::Uuid;

use crate::error::DeskSignalError;

struct SignalDeviceCodeService;

impl DeviceCodeService for SignalDeviceCodeService {
    async fn get_or_create_device_code(
        &self,
        client_id: &str,
    ) -> Result<Option<String>, DeskSignalFacadeError> {
        let db = crate::db::get_db();
        use crate::entity::device_code;
        use sea_orm::*;

        let db_model_opt = device_code::Entity::find()
            .filter(device_code::Column::ClientId.eq(client_id.to_string()))
            .one(db)
            .await
            .map_err(|e| DeskSignalFacadeError::new_custom_error(
                desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
                &e.to_string(),
            ))?;

        if let Some(db_model) = db_model_opt {
            Ok(Some(db_model.device_code))
        } else {
            let new_code = desk_utils::string::generate_device_code(6);
            let new_model = device_code::ActiveModel {
                client_id: Set(client_id.to_string()),
                device_code: Set(new_code.clone()),
                created_at: Set(chrono::Utc::now()),
                updated_at: Set(chrono::Utc::now()),
                ..Default::default()
            };

            if let Err(e) = new_model.insert(db).await {
                log::error!("Failed to generate device_code: {}", e);
                Ok(None)
            } else {
                Ok(Some(new_code))
            }
        }
    }
}

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

    let device_code_service = SignalDeviceCodeService;
    let device_code = if client_version_info.remote_desk_type == RemoteDeskTypeEnum::Server {
        if let Some(client_id) = &client_version_info.client_id {
            device_code_service.get_or_create_device_code(client_id).await?
        } else {
            None
        }
    } else {
        None
    };

    let mut handler = SignalingHandler::init(
        connection_id,
        client_version_info,
        connection_map,
        ws_session,
        user,
        ip,
        std::sync::Arc::new(turn),
        device_code,
        desk_server_version::SERVER_API_VERSION,
    )
    .await?;

    handler.do_handle_signaling(stream).await?;
    Ok(())
}

pub type SignalingContext<T> = SignalingHandler<T>;
