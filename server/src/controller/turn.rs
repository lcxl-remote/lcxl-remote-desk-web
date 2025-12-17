use actix_web::{HttpResponse, delete, get, web};
use desk_turn::model::{
    TurnApiState, TurnInfo, TurnQueryParams, TurnSession, TurnSessionStatistics,
};

use crate::{desk_error::DeskError, model::turn::TurnObserver};

#[utoipa::path(
    summary = "Get turn server info",
    responses(
        (status = 200, description = "Turn server info", body = TurnInfo),
    ),
)]
#[get("/info")]
pub async fn get_turn_info(
    api_state: web::Data<TurnApiState<TurnObserver>>,
) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_info(api_state).await?;
    Ok(response)
}

#[utoipa::path(
    summary = "Get turn server session",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Turn server session", body = TurnSession),
    ),
)]
#[get("/session")]
pub async fn get_turn_session(
    api_state: web::Data<TurnApiState<TurnObserver>>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_session(api_state, query).await?;
    Ok(response)
}

#[utoipa::path(
    summary = "Get turn server session statistics",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Turn server session statistics", body = TurnSessionStatistics),
        (status = 404, description = "Turn server session not found"),
    ),
)]
#[get("/session/statistics")]
pub async fn get_turn_session_statistics(
    api_state: web::Data<TurnApiState<TurnObserver>>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_session_statistics(api_state, query).await?;
    Ok(response)
}

#[utoipa::path(
    summary = "Delete turn server session",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Deleted turn server session"),
        (status = 417, description = "Expectation failed"),
    ),
)]
#[delete("/session")]
pub async fn delete_turn_session(
    api_state: web::Data<TurnApiState<TurnObserver>>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::delete_turn_session(api_state, query).await?;
    Ok(response)
}

#[utoipa::path(
    summary = "Turn server metrics",
    responses(
        (status = 200, description = "turn server metrics", body = String),
        (status = 417, description = "Expectation failed"),
    ),
)]
#[get("/metrics")]
pub async fn get_turn_metrics() -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_metrics().await?;
    Ok(response)
}
