use actix_web::{HttpResponse, get, post, web};
use desk_utils::rest::RestResponse;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::SignalingType;
use crate::model::system_settings::RemoteSystemSettings;
use crate::service::request_on_local_connection;

pub const TAG: &str = "Settings";

/// Path parameter for settings endpoints
#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct SettingsPath {
    /// Target connection id
    pub connection_id: String,
}

#[utoipa::path(
    tag = TAG,
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

    let not_found = format!("Connection {connection_id} not found");
    let response = request_on_local_connection::<()>(
        &connection_map,
        connection_id,
        SignalingType::ManagerQuerySettings,
        None,
        &not_found,
    )
    .await?;

    let settings: RemoteSystemSettings = response.get_data()?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(settings)))
}

#[utoipa::path(
    tag = TAG,
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

    let not_found = format!("Connection {connection_id} not found");
    request_on_local_connection(
        &connection_map,
        connection_id,
        SignalingType::ManagerUpdateSettings,
        Some(&settings),
        &not_found,
    )
    .await?;

    Ok(HttpResponse::Ok().json(RestResponse::succeed()))
}
