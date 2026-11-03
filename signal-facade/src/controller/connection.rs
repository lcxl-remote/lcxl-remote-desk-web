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

/// The host's owner middleware authorizes this owner-plane endpoint before it
/// reaches the local identity index. Code sessions never use device discovery.
#[utoipa::path(
    tag = TAG,
    summary = "Resolve the current connection for one device",
    params(("device_id" = String, Path), crate::model::connection::FetchConnectionsScope),
    responses((status = 200, body = RestResponse<ConnectionModel>)),
)]
#[get("/devices/{device_id}/connection")]
pub async fn get_device_connection(
    device_id: web::Path<String>,
    connection_map: web::Data<SharedConnectionMap>,
) -> HttpResponse {
    let model = connection_map
        .read()
        .await
        .current_oss_server(&device_id)
        .filter(|state| !state.lifecycle.is_retired())
        .map(|state| state.model.clone());
    match model {
        Some(model) => HttpResponse::Ok()
            .insert_header(("cache-control", "no-store"))
            .json(RestResponse::succeed_with_data(model.as_ref().clone())),
        None => HttpResponse::Ok()
            .insert_header(("cache-control", "no-store"))
            .json(RestResponse::failed(
                desk_utils::error::DeskErrorCode::DEVICE_NOT_FOUND,
                "Device unavailable".into(),
            )),
    }
}
