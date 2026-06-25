use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::ai_usage::query_ai_usage;
use crate::error::DeskSignalError;

pub const TAG: &str = "ModelUsage";

/// Time range for a usage query. Both bounds are RFC3339 timestamps; `to` is
/// exclusive. When omitted, defaults to the last 24 hours.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ModelUsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

/// One per-model hourly usage row, projected for the frontend chart.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageItem {
    pub model_name: String,
    pub hour_bucket: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub request_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ModelUsageResult {
    pub items: Vec<ModelUsageItem>,
}

fn parse_or_default(
    value: &Option<String>,
    default: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>, DeskSignalError> {
    match value {
        None => Ok(default),
        Some(raw) => chrono::DateTime::parse_from_rfc3339(raw)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .map_err(|e| {
                DeskSignalError::new_custom_error(
                    desk_utils::error::DeskErrorCode::INVALID_PARAMS,
                    &format!("invalid timestamp '{raw}': {e}"),
                )
            }),
    }
}

#[utoipa::path(
    tag = TAG,
    summary = "Query local per-model AI gateway token usage",
    params(ModelUsageQuery),
    responses(
        (status = 200, description = "Per-model hourly token usage", body = RestResponse<ModelUsageResult>),
    ),
)]
#[get("/usage")]
pub async fn get_model_usage(
    query: web::Query<ModelUsageQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let now = chrono::Utc::now();
    let from = parse_or_default(&query.from, now - chrono::Duration::hours(24))?;
    let to = parse_or_default(&query.to, now)?;

    let db = crate::db::get_db();
    let rows = query_ai_usage(db, from, to).await?;

    let items = rows
        .into_iter()
        .map(|row| ModelUsageItem {
            model_name: row.model_name,
            hour_bucket: row.hour_bucket.to_rfc3339(),
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_write_tokens: row.cache_write_tokens,
            request_count: row.request_count,
        })
        .collect();

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(ModelUsageResult { items })))
}
