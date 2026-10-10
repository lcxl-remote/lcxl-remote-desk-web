//! Current observation API. Authorization precedes every metrics access.

use crate::config::ConfigConnection;
use crate::error::DeskSignalError;
use actix_session::Session;
use actix_web::{HttpResponse, get, put, web};
use desk_signal_facade::{model::model_metrics::*, service::model_metrics::ResolvedQuery};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use std::{future::Future, time::Duration};

pub const TAG: &str = "ModelMetrics";

pub(crate) fn authorize(session: &Session) -> Result<(), DeskSignalError> {
    use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
    let owner = session.get_current_user::<CurrentUser>().map_err(|_| {
        DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "owner session unavailable")
    })?;
    if !owner.is_some_and(|user| {
        user.access.as_deref() == Some(desk_server_user::model::USER_ADMIN)
            && user.target_connection_id.is_none()
    }) {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PERMISSION_ERROR,
            "owner access required",
        ));
    }
    Ok(())
}

fn store() -> Result<&'static crate::model_metrics::store::Store, DeskSignalError> {
    crate::model_metrics::runtime::store().ok_or_else(|| {
        DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "model metrics unavailable")
    })
}

fn resolve(
    query: &MetricsQuery,
    groups: bool,
    calls: bool,
) -> Result<ResolvedQuery, DeskSignalError> {
    if query.category.is_some()
        || query.definition.is_some()
        || (!groups && query.group_sort.is_some())
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "runtime and group filters require their corresponding endpoints",
        ));
    }
    let resolved =
        ResolvedQuery::resolve(query, chrono::Utc::now().timestamp_millis()).map_err(|reason| {
            DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
        })?;
    if !calls {
        resolved.validate_aggregate_filters().map_err(|reason| {
            DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
        })?;
    }
    Ok(resolved)
}

async fn read<T>(
    operation: impl Future<Output = Result<T, sea_orm::DbErr>>,
) -> Result<T, DeskSignalError> {
    tokio::time::timeout(Duration::from_secs(3), operation)
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "model metrics query timed out",
            )
        })?
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "model metrics unavailable",
            )
        })
}

#[utoipa::path(tag = TAG, summary = "Get model observation health", responses((status = 200, body = RestResponse<MetricsStatus>)))]
#[get("/model/metrics/status")]
pub async fn get_model_metrics_status(session: Session) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let now = chrono::Utc::now().timestamp_millis();
    let status = match crate::model_metrics::runtime::store() {
        Some(store) => read(store.status(now))
            .await
            .unwrap_or_else(|_| MetricsStatus::unavailable(ComponentState::Unavailable, now)),
        None => MetricsStatus::unavailable(crate::model_metrics::runtime::state(), now),
    };
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(status)))
}

#[utoipa::path(tag = TAG, summary = "Query model observation overview", params(MetricsQuery), responses((status = 200, body = RestResponse<MetricsOverview>)))]
#[get("/model/metrics/overview")]
pub async fn get_model_metrics_overview(
    session: Session,
    query: web::Query<MetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let query = resolve(&query, false, false)?;
    let now = chrono::Utc::now().timestamp_millis();
    let data = read(store()?.overview(&query, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Query model observation series", params(MetricsQuery), responses((status = 200, body = RestResponse<MetricsSeries>)))]
#[get("/model/metrics/series")]
pub async fn get_model_metrics_series(
    session: Session,
    query: web::Query<MetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let query = resolve(&query, false, false)?;
    let now = chrono::Utc::now().timestamp_millis();
    let data = read(store()?.series(&query, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Query model observation models", params(MetricsQuery), responses((status = 200, body = RestResponse<MetricsGroups>)))]
#[get("/model/metrics/models")]
pub async fn get_model_metrics_models(
    session: Session,
    query: web::Query<MetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let query = resolve(&query, true, false)?;
    query.validate_group_sort(false).map_err(|reason| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
    })?;
    let now = chrono::Utc::now().timestamp_millis();
    let data = read(store()?.groups(&query, false, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Query model observation tools", params(MetricsQuery), responses((status = 200, body = RestResponse<MetricsGroups>)))]
#[get("/model/metrics/tools")]
pub async fn get_model_metrics_tools(
    session: Session,
    query: web::Query<MetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let query = resolve(&query, true, false)?;
    query.validate_group_sort(true).map_err(|reason| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
    })?;
    let now = chrono::Utc::now().timestamp_millis();
    let data = read(store()?.groups(&query, true, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Query model observation calls", params(MetricsQuery), responses((status = 200, body = RestResponse<MetricsCalls>)))]
#[get("/model/metrics/calls")]
pub async fn get_model_metrics_calls(
    session: Session,
    query: web::Query<MetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let query = resolve(&query, false, true)?;
    query.validate_call_filters().map_err(|reason| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
    })?;
    let now = chrono::Utc::now().timestamp_millis();
    query.call_boundary(false, now).map_err(|reason| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
    })?;
    let data = read(store()?.calls(&query, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Get content-free model observation details", params(("observation_id" = String, Path, description = "Server-issued observation id")), responses((status = 200, body = RestResponse<MetricsCallDetail>)))]
#[get("/model/metrics/calls/{observation_id}")]
pub async fn get_model_metrics_call(
    session: Session,
    id: web::Path<String>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    if id.is_empty()
        || id.len() > 192
        || !id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || b"_-.:".contains(&value))
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "invalid observation id",
        ));
    }
    let data = read(store()?.call_detail(&id)).await?.ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_STATE,
            "observation details unavailable or expired",
        )
    })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Get shared model observation settings", responses((status = 200, body = RestResponse<MetricsSettings>)))]
#[get("/model/metrics/settings")]
pub async fn get_model_metrics_settings(session: Session) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let db = crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "OSS file configuration is unavailable",
        )
    })?;
    let settings = db.config_read().await.model_metrics.clone();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(settings)))
}

#[utoipa::path(tag = TAG, summary = "Update shared model observation settings with revision CAS", request_body = MetricsSettings, responses((status = 200, body = RestResponse<MetricsSettings>)))]
#[put("/model/metrics/settings")]
pub async fn update_model_metrics_settings(
    session: Session,
    body: web::Json<MetricsSettings>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let requested = body.into_inner();
    requested.validate().map_err(|reason| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
    })?;
    let db = crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "OSS file configuration is unavailable",
        )
    })?;
    let settings = read(crate::config::save_metrics(db.config_context(), requested))
        .await?
        .ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::REVISION_CONFLICT,
                "model metrics settings changed; refresh before saving",
            )
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(settings)))
}

#[utoipa::path(tag = TAG, summary = "Query typed runtime observations", params(MetricsQuery), responses((status = 200, body = RestResponse<MetricsRuntimeGroups>)))]
#[get("/model/metrics/runtime")]
pub async fn get_model_metrics_runtime(
    session: Session,
    query: web::Query<MetricsQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let (category, definition) =
        desk_signal_facade::service::model_metrics::runtime_filters(&query).map_err(|reason| {
            DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
        })?;
    let query = ResolvedQuery::resolve(&query, chrono::Utc::now().timestamp_millis()).map_err(
        |reason| DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason),
    )?;
    if !query.calls.is_empty()
        || query.cursor.is_some()
        || query.group_sort.is_some()
        || query.provider_id.is_some()
        || query.model_id.is_some()
        || query.tool.is_some()
        || query.error.is_some()
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "runtime observations do not support model, tool, or request-error filters",
        ));
    }
    let now = chrono::Utc::now().timestamp_millis();
    let data = read(store()?.runtime_groups(&query, category, definition, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[utoipa::path(tag = TAG, summary = "Query content-free facts missing original attribution or dispatch start", params(UnassociatedQuery), responses((status = 200, body = RestResponse<MetricsUnassociated>)))]
#[get("/model/metrics/unassociated")]
pub async fn get_model_metrics_unassociated(
    session: Session,
    request: actix_web::HttpRequest,
) -> Result<HttpResponse, DeskSignalError> {
    authorize(&session)?;
    let query =
        web::Query::<UnassociatedQuery>::from_query(request.query_string()).map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "unassociated facts accept only a time range, cursor and limit",
            )
        })?;
    let now = chrono::Utc::now().timestamp_millis();
    let query=desk_signal_facade::service::model_metrics::unassociated::ResolvedUnassociatedQuery::resolve(&query,now)
        .map_err(|reason|DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS,reason))?;
    query.boundary(false).map_err(|reason| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
    })?;
    let data = read(store()?.unassociated(&query, now)).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(data)))
}

#[cfg(test)]
mod tests;
