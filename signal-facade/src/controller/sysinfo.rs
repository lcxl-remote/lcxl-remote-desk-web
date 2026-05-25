use actix_web::{HttpResponse, get, web};
use desk_utils::error::DeskErrorCode;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::signal::{ForwardSignalingSender, SignalingType};
use crate::model::system_info::SystemInfo;

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

    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(connection_id) {
            connection
                .request_peer_with_callback::<()>(SignalingType::ManagerSystemInfo, None, None)
                .await?
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Connection {} not found", connection_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0
    {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::new(response_state.error_code),
            &response_state.message.clone().unwrap_or_default(),
        );
    }

    let system_info: SystemInfo = response.get_data()?;
    Ok(HttpResponse::Ok().json(system_info))
}
