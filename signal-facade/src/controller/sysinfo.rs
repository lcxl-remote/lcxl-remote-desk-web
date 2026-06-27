use actix_web::{HttpResponse, get, web};

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::SignalingType;
use crate::model::system_info::SystemInfo;
use crate::service::request_on_local_connection;

pub const TAG: &str = "System";

/// Query path for sysinfo
#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct SysInfoPath {
    /// Target connection id
    pub connection_id: String,
}

/// Run the sysinfo request against a connection held in the local map and build
/// the HTTP response. Addressing is decoupled from the path so cross-instance
/// callers (the manager) reuse the same core (rule 22 dual-target parity).
pub async fn query_sysinfo_core(
    connection_map: &SharedConnectionMap,
    connection_id: &str,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let not_found = format!("Connection {connection_id} not found");
    let response = request_on_local_connection::<()>(
        connection_map,
        connection_id,
        SignalingType::ManagerSystemInfo,
        None,
        &not_found,
    )
    .await?;

    let system_info: SystemInfo = response.get_data()?;
    Ok(HttpResponse::Ok().json(system_info))
}

#[utoipa::path(
    tag = TAG,
    summary = "Get remote system information via signaling",
    params(SysInfoPath),
    responses(
        (status = 200, description = "Get system information successfully", body = SystemInfo),
    ),
)]
#[get("/sysinfo/{connection_id}")]
pub async fn query_sysinfo(
    path: web::Path<SysInfoPath>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    query_sysinfo_core(&connection_map, &path.connection_id).await
}
