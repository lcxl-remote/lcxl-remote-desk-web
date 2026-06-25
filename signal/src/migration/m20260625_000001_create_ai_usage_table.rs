use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AiUsageHourly::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AiUsageHourly::ModelName)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::HourBucket)
                            .timestamp()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::InputTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::OutputTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::CacheReadTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::CacheWriteTokens)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::RequestCount)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(AiUsageHourly::UpdatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    // Composite primary key (model_name, hour_bucket).
                    .primary_key(
                        Index::create()
                            .col(AiUsageHourly::ModelName)
                            .col(AiUsageHourly::HourBucket),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(AiUsageHourly::Table).to_owned())
            .await
    }
}

#[derive(Iden)]
pub enum AiUsageHourly {
    Table,
    ModelName,
    HourBucket,
    InputTokens,
    OutputTokens,
    CacheReadTokens,
    CacheWriteTokens,
    RequestCount,
    UpdatedAt,
}
