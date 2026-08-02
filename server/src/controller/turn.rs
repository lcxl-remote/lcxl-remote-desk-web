use actix_web::{HttpResponse, get, web};
use desk_turn::model::{TurnQueryParams, TurnRuntimeInfo, TurnSessionStatistics};
use desk_turn::runtime::TurnRuntimeView;
use desk_utils::rest::RestResponse;

use crate::error::DeskError;

pub const TAG: &str = "Turn";

#[utoipa::path(
    tag = TAG,
    summary = "Get TURN runtime status",
    responses(
        (status = 200, description = "TURN runtime status", body = RestResponse<TurnRuntimeInfo>),
    ),
)]
#[get("/info")]
pub async fn get_turn_info(view: web::Data<TurnRuntimeView>) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_info(view).await?;
    Ok(response)
}

#[utoipa::path(
    tag = TAG,
    summary = "Get turn server session statistics",
    params(TurnQueryParams),
    responses(
        (status = 200, description = "Turn server session statistics, or the reason there are none",
         body = RestResponse<TurnSessionStatistics>),
    ),
)]
#[get("/session/statistics")]
pub async fn get_turn_session_statistics(
    view: web::Data<TurnRuntimeView>,
    query: web::Query<TurnQueryParams>,
) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_session_statistics(view, query).await?;
    Ok(response)
}

#[utoipa::path(
    tag = TAG,
    summary = "Turn server metrics",
    responses(
        (status = 200, description = "turn server metrics", body = String),
        (status = 503, description = "No TURN runtime is serving on this host", body = String),
    ),
)]
#[get("/metrics")]
pub async fn get_turn_metrics(view: web::Data<TurnRuntimeView>) -> Result<HttpResponse, DeskError> {
    let response = desk_turn::controller::get_turn_metrics(view).await?;
    Ok(response)
}
