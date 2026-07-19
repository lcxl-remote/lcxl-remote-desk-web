use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ExecLedgerEntry::Table)
                    .if_not_exists()
                    // The generation is the primary key, which is what makes a
                    // second spawn of the same dispatch impossible rather than
                    // merely unlikely.
                    .col(
                        ColumnDef::new(ExecLedgerEntry::ExecutionGeneration)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ExecLedgerEntry::TaskId).string().not_null())
                    .col(ColumnDef::new(ExecLedgerEntry::State).string().not_null())
                    .col(
                        ColumnDef::new(ExecLedgerEntry::PlanFingerprint)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ExecLedgerEntry::ContainmentIdentity).string())
                    .col(ColumnDef::new(ExecLedgerEntry::ResultJson).text())
                    .col(
                        ColumnDef::new(ExecLedgerEntry::CreatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ExecLedgerEntry::UpdatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        // A restarting daemon asks "what was in flight?" and the result sweep asks
        // "what is old?"; both scan by state.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_exec_ledger_entry_state")
                    .table(ExecLedgerEntry::Table)
                    .col(ExecLedgerEntry::State)
                    .to_owned(),
            )
            .await?;

        // Answering "how did this task's attempts go?" without a table scan.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_exec_ledger_entry_task_id")
                    .table(ExecLedgerEntry::Table)
                    .col(ExecLedgerEntry::TaskId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ExecLedgerEntry::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ExecLedgerEntry {
    Table,
    ExecutionGeneration,
    TaskId,
    State,
    PlanFingerprint,
    ContainmentIdentity,
    ResultJson,
    CreatedAt,
    UpdatedAt,
}
