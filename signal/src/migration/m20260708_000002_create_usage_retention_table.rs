use sea_orm_migration::prelude::*;

/// Single-row usage-retention configuration table for the OSS signal server.
/// Mirrors the `model_provider` singleton pattern: exactly one row (id = 1) holds
/// the retention windows applied by the cleanup loop.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(UsageRetention::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(UsageRetention::Id)
                            .integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(UsageRetention::TurnDays)
                            .integer()
                            .not_null()
                            .default(30),
                    )
                    .col(
                        ColumnDef::new(UsageRetention::AiDays)
                            .integer()
                            .not_null()
                            .default(30),
                    )
                    .col(
                        ColumnDef::new(UsageRetention::UpdatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(UsageRetention::Table).to_owned())
            .await
    }
}

#[derive(Iden)]
pub enum UsageRetention {
    Table,
    Id,
    TurnDays,
    AiDays,
    UpdatedAt,
}
