use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::ai_usage::query_ai_usage;
use crate::error::DeskSignalError;
use crate::usage_query::{self, Granularity};

pub const TAG: &str = "ModelUsage";

/// Time range for a usage query. Both bounds are RFC3339 timestamps; `to` is
/// exclusive. When omitted, `from` defaults to a recent window and the range is
/// clamped to the configured retention.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ModelUsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    /// Time-bucket granularity (`hour` / `day`). Omitted defaults to `hour`; a
    /// range wider than the day threshold forces `day` regardless.
    pub granularity: Option<String>,
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
    /// The effective range actually queried (after clamping to retention / now)
    /// and the granularity applied. Day buckets are UTC-0.
    pub range: usage_query::UsageRangeDto,
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
    let db = crate::db::get_db();
    // Clamp the requested range to the configured AI retention window, and resolve
    // the effective granularity (wide ranges force `day`).
    let now = chrono::Utc::now();
    let retention = crate::usage_retention::load(db).await?;
    let range = usage_query::resolve_effective_range(
        query.from.as_deref(),
        query.to.as_deref(),
        Granularity::parse(query.granularity.as_deref()),
        now,
        retention.ai_days,
    )
    .map_err(|e| {
        DeskSignalError::new_custom_error(desk_utils::error::DeskErrorCode::INVALID_PARAMS, &e)
    })?;

    let items = if range.is_empty {
        Vec::new()
    } else {
        query_ai_usage(db, range.from, range.to, range.granularity)
            .await?
            .into_iter()
            .map(|row| ModelUsageItem {
                model_name: row.model_name,
                hour_bucket: chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                    row.hour_bucket,
                    chrono::Utc,
                )
                .to_rfc3339(),
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                cache_read_tokens: row.cache_read_tokens,
                cache_write_tokens: row.cache_write_tokens,
                request_count: row.request_count,
            })
            .collect()
    };

    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(ModelUsageResult {
            items,
            range: range.to_dto(),
        })),
    )
}
