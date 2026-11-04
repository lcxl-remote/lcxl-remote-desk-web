pub use desk_signal_facade::controller::connection::list_connections;

use actix_web::{HttpResponse, get, web};
use desk_signal_facade::model::connection::{
    ConnectionModel, ConnectionRegistry, SharedConnectionMap,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::sync::Arc;

use crate::{device_code_lifecycle::DeviceCodeLifecycle, entity::device_code};

/// The host's owner middleware authorizes this endpoint before directory access.
#[utoipa::path(
    tag = desk_signal_facade::controller::connection::TAG,
    summary = "Resolve the current connection for one device",
    params(("device_id" = String, Path), desk_signal_facade::model::connection::FetchConnectionsScope),
    responses((status = 200, body = RestResponse<ConnectionModel>)),
)]
#[get("/devices/{device_id}/connection")]
pub async fn get_device_connection(
    device_id: web::Path<String>,
    connection_map: web::Data<SharedConnectionMap>,
) -> HttpResponse {
    let Some(db) = crate::db::try_get_db() else {
        let model = online(&*connection_map.read().await, &device_id);
        return connection_response(model.ok_or(DeskErrorCode::SYSTEM_ERROR));
    };
    device_connection(
        db,
        &connection_map,
        crate::device_code_lifecycle::global(),
        &device_id,
    )
    .await
}

pub(crate) async fn device_connection<C: ConnectionTrait>(
    db: &C,
    connections: &SharedConnectionMap,
    lifecycle: &Arc<DeviceCodeLifecycle>,
    device_id: &str,
) -> HttpResponse {
    let model = online(&*connections.read().await, device_id);
    if let Some(model) = model {
        return connection_response(Ok(model));
    }
    let _identity = lifecycle.lock(device_id).await;
    let model = online(&*connections.read().await, device_id);
    if let Some(model) = model {
        return connection_response(Ok(model));
    }
    let code = match device_code::Entity::find()
        .filter(device_code::Column::ClientId.eq(device_id))
        .one(db)
        .await
    {
        Ok(Some(_)) => DeskErrorCode::REMOTE_DESK_OFFLINE,
        Ok(None) => DeskErrorCode::DEVICE_NOT_FOUND,
        Err(_) => DeskErrorCode::SYSTEM_ERROR,
    };
    connection_response(Err(code))
}

fn connection_response(result: Result<Arc<ConnectionModel>, DeskErrorCode>) -> HttpResponse {
    let mut response = HttpResponse::Ok();
    response.insert_header(("cache-control", "no-store"));
    match result {
        Ok(model) => response.json(RestResponse::succeed_with_data(model.as_ref().clone())),
        Err(code) => response.json(RestResponse::failed(
            code,
            match code {
                DeskErrorCode::REMOTE_DESK_OFFLINE => "Device is offline",
                DeskErrorCode::SYSTEM_ERROR => "Device directory unavailable",
                _ => "Device unavailable",
            }
            .into(),
        )),
    }
}

fn online(connections: &ConnectionRegistry, client_id: &str) -> Option<Arc<ConnectionModel>> {
    connections
        .current_oss_server(client_id)
        .filter(|state| !state.lifecycle.is_retired())
        .map(|state| state.model.clone())
}
