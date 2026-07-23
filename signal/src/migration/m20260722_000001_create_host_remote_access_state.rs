use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(HostRemoteAccessState::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(HostRemoteAccessState::ClientId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(HostRemoteAccessState::Locked)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(HostRemoteAccessState::StateVersion)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(HostRemoteAccessState::LockId)
                            .string()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(HostRemoteAccessState::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(HostRemoteAccessState::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum HostRemoteAccessState {
    Table,
    ClientId,
    Locked,
    StateVersion,
    LockId,
    UpdatedAt,
}
