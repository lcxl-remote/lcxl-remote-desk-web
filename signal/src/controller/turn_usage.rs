use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::error::DeskSignalError;
use crate::turn_usage::query_turn_usage;
use crate::usage_query::{self, Granularity};

pub const TAG: &str = "TurnUsage";

/// Time range for a usage query. Both bounds are RFC3339 timestamps; `to` is
/// exclusive. When omitted, `from` defaults to a recent window and the range is
/// clamped to the configured retention.
#[derive(Debug, Deserialize, IntoParams)]
pub struct TurnUsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    /// Time-bucket granularity (`hour` / `day`). Omitted defaults to `hour`; a
    /// range wider than the day threshold forces `day` regardless.
    pub granularity: Option<String>,
}

/// One per-device hourly usage row, projected for the frontend chart. Traffic is
/// split into billable `relay_*` (ChannelData + Send/Data indications) and
/// observation-only `control_*` (STUN + TURN control).
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsageItem {
    pub device_code: String,
    pub hour_bucket: String,
    pub relay_received_bytes: i64,
    pub relay_sent_bytes: i64,
    pub relay_received_pkts: i64,
    pub relay_sent_pkts: i64,
    pub control_received_bytes: i64,
    pub control_sent_bytes: i64,
    pub control_received_pkts: i64,
    pub control_sent_pkts: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TurnUsageResult {
    pub items: Vec<TurnUsageItem>,
    /// The effective range actually queried (after clamping to retention / now)
    /// and the granularity applied. Day buckets are UTC-0.
    pub range: usage_query::UsageRangeDto,
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
    let db = crate::db::get_db();
    // Clamp the requested range to the configured TURN retention window, and resolve
    // the effective granularity (wide ranges force `day`).
    let now = chrono::Utc::now();
    let retention = crate::usage_retention::load(db).await?;
    let range = usage_query::resolve_effective_range(
        query.from.as_deref(),
        query.to.as_deref(),
        Granularity::parse(query.granularity.as_deref()),
        now,
        retention.turn_days,
    )
    .map_err(|e| {
        DeskSignalError::new_custom_error(desk_utils::error::DeskErrorCode::INVALID_PARAMS, &e)
    })?;

    let items = if range.is_empty {
        Vec::new()
    } else {
        query_turn_usage(db, range.from, range.to, range.granularity)
            .await?
            .into_iter()
            .map(|row| TurnUsageItem {
                device_code: row.device_code,
                hour_bucket: chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                    row.hour_bucket,
                    chrono::Utc,
                )
                .to_rfc3339(),
                relay_received_bytes: row.relay_received_bytes,
                relay_sent_bytes: row.relay_sent_bytes,
                relay_received_pkts: row.relay_received_pkts,
                relay_sent_pkts: row.relay_sent_pkts,
                control_received_bytes: row.control_received_bytes,
                control_sent_bytes: row.control_sent_bytes,
                control_received_pkts: row.control_received_pkts,
                control_sent_pkts: row.control_sent_pkts,
            })
            .collect()
    };

    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(TurnUsageResult {
            items,
            range: range.to_dto(),
        })),
    )
}
