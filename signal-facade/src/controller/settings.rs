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

/// Run the settings query against a connection held in the local map and build the
/// HTTP response. Addressing is decoupled from the path so cross-instance callers
/// (the manager) reuse the same core (rule 22 dual-target parity).
pub async fn query_settings_core(
    connection_map: &SharedConnectionMap,
    connection_id: &str,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let not_found = format!("Connection {connection_id} not found");
    let response = request_on_local_connection::<()>(
        connection_map,
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
    query_settings_core(&connection_map, &path.connection_id).await
}

/// Run the settings update against a connection held in the local map. Shares the
/// addressing-decoupled core contract with [`query_settings_core`].
pub async fn update_settings_core(
    connection_map: &SharedConnectionMap,
    connection_id: &str,
    settings: &RemoteSystemSettings,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let not_found = format!("Connection {connection_id} not found");
    request_on_local_connection(
        connection_map,
        connection_id,
        SignalingType::ManagerUpdateSettings,
        Some(settings),
        &not_found,
    )
    .await?;

    Ok(HttpResponse::Ok().json(RestResponse::succeed()))
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
    update_settings_core(
        &connection_map,
        &path.connection_id,
        &request_json.into_inner(),
    )
    .await
}
