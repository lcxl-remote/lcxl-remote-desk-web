//! Single-node AI gateway token-usage collection helpers (collect-only, no
//! billing).
//!
//! The portable/signal server folds each completed model call's token usage into
//! a per-model hourly rollup stored in the local sqlite database. This mirrors
//! the manager's bill-ready rollup token classes (non-cached input, output,
//! cache read, cache write) but without the subject/tier/node dimensions a
//! single-process portable server does not have.

use chrono::Timelike;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::prelude::Expr;
use sea_orm::sea_query::{ExprTrait, OnConflict};
use sea_orm::{ActiveValue::Set, DatabaseConnection, DbErr, EntityTrait};

use crate::entity::ai_usage;

/// A signed increment to apply to one `(model_name, hour_bucket)` rollup row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiUsageDelta {
    pub model_name: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub request_count: i64,
}

/// Truncate a timestamp to the start of its UTC hour.
pub fn truncate_to_hour(ts: DateTimeUtc) -> DateTimeUtc {
    ts.with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(ts)
}

/// Atomically add `delta` to the `(model_name, hour_bucket)` rollup, inserting
/// the row when absent. The `ON CONFLICT DO UPDATE` adds the delta to the
/// existing counters, so repeated calls accumulate.
pub async fn upsert_ai_usage(
    db: &DatabaseConnection,
    hour_bucket: DateTimeUtc,
    delta: &AiUsageDelta,
) -> Result<(), DbErr> {
    let now = chrono::Utc::now();
    let model = ai_usage::ActiveModel {
        model_name: Set(delta.model_name.clone()),
        hour_bucket: Set(hour_bucket),
        input_tokens: Set(delta.input_tokens),
        output_tokens: Set(delta.output_tokens),
        cache_read_tokens: Set(delta.cache_read_tokens),
        cache_write_tokens: Set(delta.cache_write_tokens),
        request_count: Set(delta.request_count),
        updated_at: Set(now),
    };

    ai_usage::Entity::insert(model)
        .on_conflict(
            OnConflict::columns([ai_usage::Column::ModelName, ai_usage::Column::HourBucket])
                // Unqualified columns in DO UPDATE reference the existing row, so
                // these expressions accumulate the delta into the stored counters.
                .value(
                    ai_usage::Column::InputTokens,
                    Expr::col(ai_usage::Column::InputTokens).add(delta.input_tokens),
                )
                .value(
                    ai_usage::Column::OutputTokens,
                    Expr::col(ai_usage::Column::OutputTokens).add(delta.output_tokens),
                )
                .value(
                    ai_usage::Column::CacheReadTokens,
                    Expr::col(ai_usage::Column::CacheReadTokens).add(delta.cache_read_tokens),
                )
                .value(
                    ai_usage::Column::CacheWriteTokens,
                    Expr::col(ai_usage::Column::CacheWriteTokens).add(delta.cache_write_tokens),
                )
                .value(
                    ai_usage::Column::RequestCount,
                    Expr::col(ai_usage::Column::RequestCount).add(delta.request_count),
                )
                .value(ai_usage::Column::UpdatedAt, now)
                .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}

/// Query per-model hourly rollups whose `hour_bucket` falls in `[from, to)`,
/// ordered by hour then model.
pub async fn query_ai_usage(
    db: &DatabaseConnection,
    from: DateTimeUtc,
    to: DateTimeUtc,
) -> Result<Vec<ai_usage::Model>, DbErr> {
    use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};

    ai_usage::Entity::find()
        .filter(ai_usage::Column::HourBucket.gte(from))
        .filter(ai_usage::Column::HourBucket.lt(to))
        .order_by_asc(ai_usage::Column::HourBucket)
        .order_by_asc(ai_usage::Column::ModelName)
        .all(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(ai_usage::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    fn hour(h: u32) -> DateTimeUtc {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 6, 24, h, 0, 0).unwrap()
    }

    fn delta(model: &str, input: i64) -> AiUsageDelta {
        AiUsageDelta {
            model_name: model.to_string(),
            input_tokens: input,
            output_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            request_count: 1,
        }
    }

    #[test]
    fn truncate_drops_sub_hour_components() {
        use chrono::TimeZone;
        let ts = chrono::Utc.with_ymd_and_hms(2026, 6, 24, 9, 37, 42).unwrap();
        assert_eq!(truncate_to_hour(ts), hour(9));
    }

    #[tokio::test]
    async fn upsert_accumulates_on_conflict() {
        let db = memory_db().await;
        upsert_ai_usage(&db, hour(9), &delta("gpt-x", 100))
            .await
            .unwrap();
        upsert_ai_usage(&db, hour(9), &delta("gpt-x", 100))
            .await
            .unwrap();

        let rows = query_ai_usage(&db, hour(0), hour(23)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, 200);
        assert_eq!(rows[0].output_tokens, 10);
        assert_eq!(rows[0].cache_read_tokens, 6);
        assert_eq!(rows[0].cache_write_tokens, 4);
        assert_eq!(rows[0].request_count, 2);
    }

    #[tokio::test]
    async fn distinct_models_and_range_filter() {
        let db = memory_db().await;
        upsert_ai_usage(&db, hour(9), &delta("gpt-x", 10))
            .await
            .unwrap();
        upsert_ai_usage(&db, hour(9), &delta("claude-x", 10))
            .await
            .unwrap();
        upsert_ai_usage(&db, hour(10), &delta("gpt-x", 10))
            .await
            .unwrap();

        // Range [9,10) keeps only the two hour-9 rows.
        let rows = query_ai_usage(&db, hour(9), hour(10)).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.hour_bucket == hour(9)));
    }
}
