use actix_web::{HttpResponse, get, post, web};
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::{ForwardSignalingSender, SignalingType};
use crate::model::system_settings::RemoteSystemSettings;

/// Path parameter for settings endpoints
#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct SettingsPath {
    /// Target connection id
    pub connection_id: String,
}

#[utoipa::path(
    summary = "Query remote system settings via signaling",
    params(SettingsPath),
    responses(
        (status = 200, description = "Query settings successfully", body = RestResponse<RemoteSystemSettings>),
    ),
)]
#[get("/settings/{connection_id}")]
pub async fn query_settings(
    path: web::Path<SettingsPath>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let connection_id = &path.connection_id;

    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(connection_id) {
            connection
                .request_peer_with_callback::<()>(SignalingType::ManagerQuerySettings, None, None)
                .await?
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Connection {} not found", connection_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0 {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }

    let settings: RemoteSystemSettings = response.get_data()?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(settings)))
}

#[utoipa::path(
    summary = "Update remote system settings via signaling",
    params(SettingsPath),
    request_body(content = RemoteSystemSettings),
    responses(
        (status = 200, description = "Update settings successfully"),
    ),
)]
#[post("/settings/{connection_id}")]
pub async fn update_settings(
    path: web::Path<SettingsPath>,
    request_json: web::Json<RemoteSystemSettings>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let connection_id = &path.connection_id;
    let settings = request_json.into_inner();

    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(connection_id) {
            connection
                .request_peer_with_callback(
                    SignalingType::ManagerUpdateSettings,
                    Some(&settings),
                    None,
                )
                .await?
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Connection {} not found", connection_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0 {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }

    Ok(HttpResponse::Ok().json(RestResponse::succeed()))
}
