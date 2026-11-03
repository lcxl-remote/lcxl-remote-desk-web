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
    let snapshots: Vec<_> = {
        let connection_map = connection_map.read().await;
        connection_map
            .values()
            .filter(|s| s.model.peer.remote_desk_type == RemoteDeskTypeEnum::Server)
            .map(|state| state.model.clone())
            .collect()
    };

    let connections: Vec<ConnectionModel> = snapshots
        .into_iter()
        .map(|model| model.as_ref().clone())
        .collect();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(connections)))
}
