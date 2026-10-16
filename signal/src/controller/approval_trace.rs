//! Separate guards for private session records and platform-only aggregates.
use crate::approval_trace;
use crate::error::DeskSignalError;
use actix_session::Session;
use actix_web::{HttpResponse, get, put, web};
use desk_signal_facade::model::approval_runtime::ApprovalRuntimeSettings;
use desk_signal_facade::model::approval_trace::*;
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

pub const TAG: &str = "AiApproval";
fn authorize(session: &Session) -> Result<String, DeskSignalError> {
    crate::controller::model_metrics::authorize(session)?;
    Ok(crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string())
}
fn failure() -> DeskSignalError {
    DeskSignalError::new_custom_error(
        DeskErrorCode::SYSTEM_ERROR,
        "Approval observation unavailable",
    )
}
fn read_error(error: sea_orm::DbErr) -> DeskSignalError {
    match error {
        sea_orm::DbErr::Custom(_) => inaccessible(),
        _ => failure(),
    }
}
fn inaccessible() -> DeskSignalError {
    DeskSignalError::new_custom_error(
        DeskErrorCode::PERMISSION_ERROR,
        "Approval review not found or not accessible",
    )
}

#[utoipa::path(tag=TAG,summary="List own AI approval reviews (administrator)",params(ApprovalReviewsQuery),responses((status=200,body=RestResponse<ApprovalReviewsDto>)))]
#[get("/my/ai-assistant-session/approval-reviews")]
pub async fn list_approval_reviews(
    session: Session,
    query: web::Query<ApprovalReviewsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let owner = authorize(&session)?;
    let db = crate::db::get_db().clone();
    let data = approval_trace::Store::new(db)
        .list(&owner, &query)
        .await
        .map_err(read_error)?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "private, no-store"))
        .json(RestResponse::succeed_with_data(data)))
}
#[utoipa::path(tag=TAG,summary="Get own AI approval review (administrator)",params(ApprovalReviewQuery),responses((status=200,body=RestResponse<ApprovalReviewDto>)))]
#[get("/my/ai-assistant-session/approval-review")]
pub async fn get_approval_review(
    session: Session,
    query: web::Query<ApprovalReviewQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let owner = authorize(&session)?;
    let db = crate::db::get_db().clone();
    let data = approval_trace::Store::new(db)
        .detail(&owner, &query)
        .await
        .map_err(read_error)?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "private, no-store"))
        .json(RestResponse::succeed_with_data(data)))
}
#[utoipa::path(tag=TAG,summary="Read own approval content parts (administrator)",params(ApprovalPartsQuery),responses((status=200,body=RestResponse<ApprovalTracePartsDto>)))]
#[get("/my/ai-assistant-session/approval-review/parts")]
pub async fn get_approval_review_parts(
    session: Session,
    query: web::Query<ApprovalPartsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let owner = authorize(&session)?;
    let db = crate::db::get_db().clone();
    let data = approval_trace::Store::new(db)
        .parts(&owner, &query)
        .await
        .map_err(read_error)?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "private, no-store"))
        .json(RestResponse::succeed_with_data(data)))
}
#[utoipa::path(tag=TAG,summary="Get approval detail retention settings",responses((status=200,body=RestResponse<ApprovalTraceSettings>)))]
#[get("/admin/approval-trace-settings")]
pub async fn get_approval_trace_settings(
    session: Session,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let db = crate::db::get_db().clone();
    let data = approval_trace::load_settings(&db)
        .await
        .map_err(|_| failure())?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "private, no-store"))
        .json(RestResponse::succeed_with_data(data)))
}
#[utoipa::path(tag=TAG,summary="Update approval detail retention settings",request_body=ApprovalTraceSettings,responses((status=200,body=RestResponse<ApprovalTraceSettings>)))]
#[put("/admin/approval-trace-settings")]
pub async fn update_approval_trace_settings(
    session: Session,
    body: web::Json<ApprovalTraceSettings>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let db = crate::db::get_db().clone();
    body.validate().map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "Invalid approval detail settings",
        )
    })?;
    let saved = approval_trace::save_settings(&db, body.into_inner())
        .await
        .map_err(|_| failure())?;
    match saved {
        Some(settings) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "private, no-store"))
            .json(RestResponse::succeed_with_data(settings))),
        None => {
            let current = approval_trace::load_settings(&db)
                .await
                .map_err(|_| failure())?;
            Ok(HttpResponse::Ok()
                .insert_header(("Cache-Control", "private, no-store"))
                .json(RestResponse::failed_with_data(
                    DeskErrorCode::REVISION_CONFLICT,
                    Some("Approval settings changed; reload and retry".into()),
                    Some(current),
                )))
        }
    }
}
#[utoipa::path(tag=TAG,summary="Aggregate platform AI approval statistics (administrator)",params(ApprovalMetricsQuery),responses((status=200,body=RestResponse<ApprovalMetricsOverviewDto>)))]
#[get("/model/metrics/approval/overview")]
pub async fn get_approval_metrics_overview(
    session: Session,
    query: web::Query<ApprovalMetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let store = crate::model_metrics::runtime::store().ok_or_else(failure)?;
    let data = store
        .approval_overview(&query, chrono::Utc::now().timestamp_millis())
        .await
        .map_err(|_| failure())?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "private, no-store"))
        .json(RestResponse::succeed_with_data(data)))
}

#[cfg(test)]
mod tests;

#[utoipa::path(tag=TAG,summary="Get approval execution settings",responses((status=200,body=RestResponse<ApprovalRuntimeSettings>)))]
#[get("/admin/approval-runtime-settings")]
pub async fn get_approval_runtime_settings(
    session: Session,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let db = crate::db::get_db().clone();
    let data = crate::approval_runtime::load(&db)
        .await
        .map_err(|_| failure())?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "private, no-store"))
        .json(RestResponse::succeed_with_data(data)))
}
#[utoipa::path(tag=TAG,summary="Update approval execution settings",request_body=ApprovalRuntimeSettings,responses((status=200,body=RestResponse<ApprovalRuntimeSettings>)))]
#[put("/admin/approval-runtime-settings")]
pub async fn update_approval_runtime_settings(
    session: Session,
    body: web::Json<ApprovalRuntimeSettings>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let db = crate::db::get_db().clone();
    body.validate()
        .map_err(|e| DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, e))?;
    match crate::approval_runtime::save(&db, body.into_inner())
        .await
        .map_err(|_| failure())?
    {
        Some(data) => Ok(HttpResponse::Ok()
            .insert_header(("Cache-Control", "private, no-store"))
            .json(RestResponse::succeed_with_data(data))),
        None => {
            let data = crate::approval_runtime::load(&db)
                .await
                .map_err(|_| failure())?;
            Ok(HttpResponse::Ok()
                .insert_header(("Cache-Control", "private, no-store"))
                .json(RestResponse::failed_with_data(
                    DeskErrorCode::REVISION_CONFLICT,
                    Some("Approval runtime settings changed; reload before saving".into()),
                    Some(data),
                )))
        }
    }
}
