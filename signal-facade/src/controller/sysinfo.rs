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
    let connection_id = &path.connection_id;

    let not_found = format!("Connection {connection_id} not found");
    let response = request_on_local_connection::<()>(
        &connection_map,
        connection_id,
        SignalingType::ManagerSystemInfo,
        None,
        &not_found,
    )
    .await?;

    let system_info: SystemInfo = response.get_data()?;
    Ok(HttpResponse::Ok().json(system_info))
}
