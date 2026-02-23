use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(DeviceCode::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(DeviceCode::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(DeviceCode::ClientId)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(
                        ColumnDef::new(DeviceCode::DeviceCode)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(DeviceCode::CreatedAt).timestamp().not_null())
                    .col(ColumnDef::new(DeviceCode::UpdatedAt).timestamp().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(DeviceCode::Table).to_owned())
            .await
    }
}

#[derive(Iden)]
pub enum DeviceCode {
    Table,
    Id,
    ClientId,
    DeviceCode,
    CreatedAt,
    UpdatedAt,
}
