use actix_web::{HttpResponse, get, web};
use desk_signal_facade::model::{connection::ConnectionModel, signal::RemoteDeskTypeEnum};

use crate::model::SharedConnectionMap;


#[utoipa::path(
    summary = "List all online desk connections",
    responses(
        (status = 200, description = "List of online desk connections", body = Vec<ConnectionModel>),
    ),
)]

#[get("/connections")]
pub async fn list_connections(
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, actix_web::Error> {
    let connection_map = connection_map.read().await;
    let connections: Vec<ConnectionModel> = connection_map
        .values()
        .filter(|s| s.model.version_info.remote_desk_type == RemoteDeskTypeEnum::Server)
        .map(|s| s.model.clone())
        .collect();

    Ok(HttpResponse::Ok().json(connections))
}
