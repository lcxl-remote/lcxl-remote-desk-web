use sea_orm_migration::prelude::*;

/// Adds `hour_bucket` secondary indexes to both usage-rollup tables. Both tables
/// key `hour_bucket` as the non-leading PK column (`(device_code, hour_bucket)` /
/// `(model_name, hour_bucket)`), so an hour-range scan — the shape used by usage
/// queries and retention cleanup, neither of which fixes the leading column —
/// would otherwise sequentially scan the whole table.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_turn_usage_hour")
                    .table(TurnUsageHourly::Table)
                    .col(TurnUsageHourly::HourBucket)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_ai_usage_hour")
                    .table(AiUsageHourly::Table)
                    .col(AiUsageHourly::HourBucket)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name("idx_turn_usage_hour")
                    .table(TurnUsageHourly::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name("idx_ai_usage_hour")
                    .table(AiUsageHourly::Table)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum TurnUsageHourly {
    Table,
    HourBucket,
}

#[derive(Iden)]
enum AiUsageHourly {
    Table,
    HourBucket,
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, Statement};
    use sea_orm_migration::MigratorTrait;

    #[derive(FromQueryResult)]
    struct IndexName {
        name: String,
    }

    /// Running the full migrator on a fresh in-memory database creates the two
    /// `hour_bucket` indexes; re-issuing the `CREATE INDEX IF NOT EXISTS` DDL is
    /// idempotent.
    #[tokio::test]
    async fn usage_hour_indexes_created_and_idempotent() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        // The fixed-name `IF NOT EXISTS` index DDL must be safe to re-apply.
        for ddl in [
            "CREATE INDEX IF NOT EXISTS idx_turn_usage_hour ON turn_usage_hourly (hour_bucket)",
            "CREATE INDEX IF NOT EXISTS idx_ai_usage_hour ON ai_usage_hourly (hour_bucket)",
        ] {
            db.execute_unprepared(ddl).await.unwrap();
        }

        let rows = IndexName::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name IN \
             ('idx_turn_usage_hour', 'idx_ai_usage_hour')"
                .to_string(),
        ))
        .all(&db)
        .await
        .unwrap();
        let names: std::collections::HashSet<_> = rows.into_iter().map(|r| r.name).collect();
        assert!(
            names.contains("idx_turn_usage_hour"),
            "idx_turn_usage_hour must exist; got {names:?}"
        );
        assert!(
            names.contains("idx_ai_usage_hour"),
            "idx_ai_usage_hour must exist; got {names:?}"
        );
    }
}
