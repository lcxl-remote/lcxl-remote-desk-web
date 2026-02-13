use actix_web::{HttpResponse, get, web};
use desk_signal_facade::model::{session::SessionModel, signal::RemoteDeskTypeEnum};

use crate::model::SharedSessionMap;


#[utoipa::path(
    summary = "List all online desk sessions",
    responses(
        (status = 200, description = "List of online desk sessions", body = Vec<SessionModel>),
    ),
)]

#[get("/sessions")]
pub async fn list_sessions(
    session_map: web::Data<SharedSessionMap>,
) -> Result<HttpResponse, actix_web::Error> {
    let session_map = session_map.read().await;
    let sessions: Vec<SessionModel> = session_map
        .values()
        .filter(|s| s.model.version_info.remote_desk_type == RemoteDeskTypeEnum::Server)
        .map(|s| s.model.clone())
        .collect();

    Ok(HttpResponse::Ok().json(sessions))
}
