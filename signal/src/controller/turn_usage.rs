use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::error::DeskSignalError;
use crate::turn_usage::query_turn_usage;

pub const TAG: &str = "TurnUsage";

/// Time range for a usage query. Both bounds are RFC3339 timestamps; `to` is
/// exclusive. When omitted, defaults to the last 24 hours.
#[derive(Debug, Deserialize, IntoParams)]
pub struct TurnUsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

/// One per-device hourly usage row, projected for the frontend chart.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsageItem {
    pub device_code: String,
    pub hour_bucket: String,
    pub received_bytes: i64,
    pub sent_bytes: i64,
    pub received_pkts: i64,
    pub sent_pkts: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TurnUsageResult {
    pub items: Vec<TurnUsageItem>,
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
    summary = "Query local per-device TURN usage",
    params(TurnUsageQuery),
    responses(
        (status = 200, description = "Per-device hourly TURN usage", body = RestResponse<TurnUsageResult>),
    ),
)]
#[get("/usage")]
pub async fn get_turn_usage(
    query: web::Query<TurnUsageQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let now = chrono::Utc::now();
    let from = parse_or_default(&query.from, now - chrono::Duration::hours(24))?;
    let to = parse_or_default(&query.to, now)?;

    let db = crate::db::get_db();
    let rows = query_turn_usage(db, from, to).await?;

    let items = rows
        .into_iter()
        .map(|row| TurnUsageItem {
            device_code: row.device_code,
            hour_bucket: row.hour_bucket.to_rfc3339(),
            received_bytes: row.received_bytes,
            sent_bytes: row.sent_bytes,
            received_pkts: row.received_pkts,
            sent_pkts: row.sent_pkts,
        })
        .collect();

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(TurnUsageResult { items })))
}
