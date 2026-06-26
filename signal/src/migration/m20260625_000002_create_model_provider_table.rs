use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ModelProvider::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ModelProvider::Id)
                            .integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ModelProvider::Provider).string().null())
                    .col(ColumnDef::new(ModelProvider::Model).string().null())
                    .col(ColumnDef::new(ModelProvider::BaseUrl).string().null())
                    .col(ColumnDef::new(ModelProvider::ApiKey).string().null())
                    .col(
                        ColumnDef::new(ModelProvider::MaxContextBytes)
                            .big_integer()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(ModelProvider::ResponseFormat)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ModelProvider::ExecutionMode)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ModelProvider::UpdatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ModelProvider::Table).to_owned())
            .await
    }
}

#[derive(Iden)]
pub enum ModelProvider {
    Table,
    Id,
    Provider,
    Model,
    BaseUrl,
    ApiKey,
    MaxContextBytes,
    ResponseFormat,
    ExecutionMode,
    UpdatedAt,
}
