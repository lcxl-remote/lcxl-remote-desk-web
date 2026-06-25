use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(TurnUsageHourly::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TurnUsageHourly::DeviceCode)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TurnUsageHourly::HourBucket)
                            .timestamp()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TurnUsageHourly::ReceivedBytes)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TurnUsageHourly::SentBytes)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TurnUsageHourly::ReceivedPkts)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TurnUsageHourly::SentPkts)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TurnUsageHourly::UpdatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    // Composite primary key (device_code, hour_bucket).
                    .primary_key(
                        Index::create()
                            .col(TurnUsageHourly::DeviceCode)
                            .col(TurnUsageHourly::HourBucket),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(TurnUsageHourly::Table).to_owned())
            .await
    }
}

#[derive(Iden)]
pub enum TurnUsageHourly {
    Table,
    DeviceCode,
    HourBucket,
    ReceivedBytes,
    SentBytes,
    ReceivedPkts,
    SentPkts,
    UpdatedAt,
}
