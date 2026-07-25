use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AgentSession::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AgentSession::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AgentSession::ConversationId)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(AgentSession::ActorId).string().not_null())
                    .col(ColumnDef::new(AgentSession::DeviceId).string().not_null())
                    .col(ColumnDef::new(AgentSession::StateJson).text().not_null())
                    .col(
                        ColumnDef::new(AgentSession::Version)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AgentSession::LeaseToken)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(AgentSession::LeaseDeadline).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(AgentSession::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AgentSession::UpdatedAt)
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
                    .table(AgentSession::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum AgentSession {
    Table,
    Id,
    ConversationId,
    ActorId,
    DeviceId,
    StateJson,
    Version,
    LeaseToken,
    LeaseDeadline,
    CreatedAt,
    UpdatedAt,
}
