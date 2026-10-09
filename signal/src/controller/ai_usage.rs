//! Owner-only public usage query over the current observation store.

use crate::error::DeskSignalError;
use crate::usage_query::{self, Granularity as UsageGranularity};
use actix_session::Session;
use actix_web::{HttpResponse, get, web};
use desk_signal_facade::model::{
    model_metrics::{Granularity, MetricsQuery},
    model_usage::{ModelUsageQuery, ModelUsageRange, ModelUsageResult},
};
use desk_signal_facade::service::model_metrics::ResolvedQuery;
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

pub const TAG: &str = "ModelUsage";

#[utoipa::path(tag=TAG,summary="Query local model usage from current observations",params(ModelUsageQuery),responses((status=200,body=RestResponse<ModelUsageResult>)))]
#[get("/usage")]
pub async fn get_model_usage(
    session: Session,
    query: web::Query<ModelUsageQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    super::model_metrics::authorize(&session)?;
    let store = crate::model_metrics::runtime::store().ok_or_else(|| {
        DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, "model metrics unavailable")
    })?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let config = store.load_settings().await.map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "model metrics unavailable",
            )
        })?;
        let now = chrono::Utc::now();
        if query
            .granularity
            .as_deref()
            .is_some_and(|v| v != "hour" && v != "day")
        {
            return Err(DeskSignalError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "unsupported usage granularity",
            ));
        }
        let range = usage_query::resolve_effective_range(
            query.from.as_deref(),
            query.to.as_deref(),
            UsageGranularity::parse(query.granularity.as_deref()),
            now,
            config.hourly_days,
        )
        .map_err(|_| {
            DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, "invalid usage range")
        })?;
        if range.is_empty {
            let dto = range.to_dto();
            return Ok(ModelUsageResult {
                items: vec![],
                range: ModelUsageRange {
                    from: dto.from,
                    to: dto.to,
                    granularity: dto.granularity,
                },
                usage_source: "observations".into(),
                partial: true,
                available_from: None,
            });
        }
        if query
            .model_name
            .as_ref()
            .is_some_and(|value| value.len() > 128 || value.chars().any(char::is_control))
        {
            return Err(DeskSignalError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "invalid model selector",
            ));
        }
        let resolved = ResolvedQuery::resolve(
            &MetricsQuery {
                from: Some(range.from.to_rfc3339()),
                to: Some(range.to.to_rfc3339()),
                granularity: Some(Granularity::Hour),
                provider_id: query.provider_id.clone(),
                model_id: query.model_id.clone(),
                purpose: query.purpose.clone(),
                include_probe: Some(true),
                ..Default::default()
            },
            now.timestamp_millis(),
        )
        .map_err(|reason| {
            DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, reason)
        })?;
        store
            .model_usage(
                &resolved,
                query.model_name.as_deref(),
                range.granularity == UsageGranularity::Day,
                now.timestamp_millis(),
            )
            .await
            .map_err(|_| {
                DeskSignalError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "model metrics unavailable",
                )
            })
    })
    .await
    .map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "model metrics query timed out",
        )
    })??;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(result)))
}
