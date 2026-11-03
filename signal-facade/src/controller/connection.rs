use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::{ConnectionModel, SharedConnectionMap};
use crate::model::signal::RemoteDeskTypeEnum;

pub const TAG: &str = "Connection";

#[utoipa::path(
    tag = TAG,
    summary = "List all online desk connections",
    responses(
        (status = 200, description = "List of online desk connections", body = RestResponse<Vec<ConnectionModel>>),
    ),
)]
#[get("/connections")]
pub async fn list_connections(
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let connections: Vec<ConnectionModel> = {
        let connection_map = connection_map.read().await;
        connection_map
            .values()
            .filter(|s| s.model.version_info.remote_desk_type == RemoteDeskTypeEnum::Server)
            .map(|s| s.public_model())
            .collect()
    };

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(connections)))
}
