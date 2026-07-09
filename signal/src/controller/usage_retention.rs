use actix_web::{HttpResponse, get, put, web};
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;

use crate::error::DeskSignalError;
use crate::usage_retention::{self, UsageRetentionConfig};

pub const TAG: &str = "UsageRetention";

#[utoipa::path(
    tag = TAG,
    summary = "Query the usage-rollup retention windows",
    responses(
        (status = 200, description = "Current retention windows (days)", body = RestResponse<UsageRetentionConfig>),
    ),
)]
#[get("/usage-retention")]
pub async fn get_usage_retention() -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let config = usage_retention::load(db).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config)))
}

#[utoipa::path(
    tag = TAG,
    summary = "Update the usage-rollup retention windows",
    request_body = UsageRetentionConfig,
    responses(
        (status = 200, description = "Updated windows, or a business error code", body = RestResponse<UsageRetentionConfig>),
    ),
)]
#[put("/usage-retention")]
pub async fn update_usage_retention(
    body: web::Json<UsageRetentionConfig>,
) -> Result<HttpResponse, DeskSignalError> {
    let config = body.into_inner();
    // Reject out-of-range windows as a business error (single-node LWW: no revision
    // / conflict semantics, unlike the manager's cluster-shared config).
    if let Err(msg) = config.validate() {
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::INVALID_PARAMS,
            msg,
        )));
    }
    let db = crate::db::get_db();
    usage_retention::save(db, config).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config)))
}
