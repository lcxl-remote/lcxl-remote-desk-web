//! Shared helpers for the signal usage-rollup read endpoints: time-bucket
//! granularity (hour / day) and the range clamp that keeps queries inside the
//! retention window. The signal server is single-node SQLite, so this is the
//! SQLite-only counterpart of the manager's `usage_query`.
//!
//! Day boundaries are fixed at UTC-0 (no session timezone). The time bucket is
//! produced with SQLite's `datetime(...)`, which yields a `YYYY-MM-DD HH:MM:SS`
//! string that decodes into `NaiveDateTime`; `date()` must NOT be used — it returns
//! a bare `YYYY-MM-DD` string that fails to decode.

use chrono::{DateTime, Duration, Utc};
use sea_orm::prelude::DateTimeUtc;
use sea_orm::prelude::Expr;
use sea_orm::sea_query::SimpleExpr;
use serde::Serialize;
use utoipa::ToSchema;

/// Above this span the backend forces `day` granularity regardless of the request.
pub const DAY_GRANULARITY_THRESHOLD_DAYS: i64 = 14;
/// Fallback span when the client supplies neither `from` nor `to`.
pub const DEFAULT_RANGE_DAYS: i64 = 7;

/// Time-bucket granularity of a usage query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Granularity {
    #[default]
    Hour,
    Day,
}

impl Granularity {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some(v) if v.eq_ignore_ascii_case("day") => Granularity::Day,
            _ => Granularity::Hour,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Granularity::Hour => "hour",
            Granularity::Day => "day",
        }
    }
}

/// The SQL expression to SELECT / GROUP BY / ORDER BY, aliased `hour_bucket` by the
/// caller. Both forms use SQLite's `datetime(...)` so the result is a tz-less
/// timestamp string that decodes into `NaiveDateTime` (never the bare `date()`).
pub fn time_bucket_expr(granularity: Granularity) -> SimpleExpr {
    match granularity {
        Granularity::Hour => Expr::cust("datetime(hour_bucket)"),
        Granularity::Day => Expr::cust("datetime(hour_bucket, 'start of day')"),
    }
}

/// The effective, clamped query range plus the granularity actually applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveRange {
    pub from: DateTimeUtc,
    pub to: DateTimeUtc,
    pub granularity: Granularity,
    pub is_empty: bool,
}

/// The effective range returned to the console alongside the usage rows.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageRangeDto {
    pub from: String,
    pub to: String,
    pub granularity: String,
}

impl EffectiveRange {
    pub fn to_dto(self) -> UsageRangeDto {
        UsageRangeDto {
            from: self.from.to_rfc3339(),
            to: self.to.to_rfc3339(),
            granularity: self.granularity.as_str().to_string(),
        }
    }
}

fn parse_rfc3339(raw: &str) -> Result<DateTimeUtc, String> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("invalid timestamp '{raw}': {e}"))
}

/// Resolve the effective range in one deterministic order: parse (defaults last
/// [`DEFAULT_RANGE_DAYS`]) → `to = min(to, now)` → `from = max(from, now -
/// retention)` → empty if `from >= to` → force `day` beyond the threshold.
pub fn resolve_effective_range(
    from: Option<&str>,
    to: Option<&str>,
    requested: Granularity,
    now: DateTimeUtc,
    retention_days: u32,
) -> Result<EffectiveRange, String> {
    let mut to_ts = match to {
        Some(raw) => parse_rfc3339(raw)?,
        None => now,
    };
    let mut from_ts = match from {
        Some(raw) => parse_rfc3339(raw)?,
        None => now - Duration::days(DEFAULT_RANGE_DAYS),
    };
    to_ts = to_ts.min(now);
    from_ts = from_ts.max(now - Duration::days(retention_days as i64));

    if from_ts >= to_ts {
        return Ok(EffectiveRange {
            from: from_ts,
            to: to_ts,
            granularity: requested,
            is_empty: true,
        });
    }
    let granularity = if (to_ts - from_ts) > Duration::days(DAY_GRANULARITY_THRESHOLD_DAYS) {
        Granularity::Day
    } else {
        requested
    };
    Ok(EffectiveRange {
        from: from_ts,
        to: to_ts,
        granularity,
        is_empty: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTimeUtc {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn parse_granularity() {
        assert_eq!(Granularity::parse(Some("day")), Granularity::Day);
        assert_eq!(Granularity::parse(None), Granularity::Hour);
        assert_eq!(Granularity::parse(Some("x")), Granularity::Hour);
    }

    #[test]
    fn clamps_and_defaults() {
        let now = at(2026, 7, 8, 12);
        let r = resolve_effective_range(None, None, Granularity::Hour, now, 30).unwrap();
        assert_eq!(r.to, now);
        assert_eq!(r.from, now - Duration::days(DEFAULT_RANGE_DAYS));
        assert!(!r.is_empty);
    }

    #[test]
    fn from_clamped_to_retention() {
        let now = at(2026, 7, 8, 12);
        let r = resolve_effective_range(
            Some(&(now - Duration::days(100)).to_rfc3339()),
            Some(&now.to_rfc3339()),
            Granularity::Day,
            now,
            30,
        )
        .unwrap();
        assert_eq!(r.from, now - Duration::days(30));
    }

    #[test]
    fn wide_range_forces_day() {
        let now = at(2026, 7, 8, 12);
        let over = resolve_effective_range(
            Some(&(now - Duration::days(14) - Duration::hours(1)).to_rfc3339()),
            Some(&now.to_rfc3339()),
            Granularity::Hour,
            now,
            180,
        )
        .unwrap();
        assert_eq!(over.granularity, Granularity::Day);
    }

    #[test]
    fn empty_when_before_retention() {
        let now = at(2026, 7, 8, 12);
        let r = resolve_effective_range(
            Some(&(now - Duration::days(90)).to_rfc3339()),
            Some(&(now - Duration::days(60)).to_rfc3339()),
            Granularity::Hour,
            now,
            30,
        )
        .unwrap();
        assert!(r.is_empty);
    }
}
