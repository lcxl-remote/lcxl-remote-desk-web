use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(UsageRetention::Table)
                    .add_column(
                        ColumnDef::new(UsageRetention::AgentSessionDays)
                            .integer()
                            .not_null()
                            .default(30),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(UsageRetention::Table)
                    .drop_column(UsageRetention::AgentSessionDays)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum UsageRetention {
    Table,
    AgentSessionDays,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, Statement};
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    #[derive(FromQueryResult)]
    struct AddedColumn {
        agent_session_days: i32,
    }

    #[tokio::test]
    async fn legacy_retention_rows_receive_the_thirty_day_default() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "CREATE TABLE usage_retention ( \
                 id INTEGER PRIMARY KEY, \
                 turn_days INTEGER NOT NULL, \
                 ai_days INTEGER NOT NULL, \
                 updated_at TIMESTAMP NOT NULL )",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO usage_retention (id, turn_days, ai_days, updated_at) \
             VALUES (1, 90, 45, '2026-07-25 00:00:00')",
        )
        .await
        .unwrap();

        super::Migration.up(&SchemaManager::new(&db)).await.unwrap();

        let row = AddedColumn::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT agent_session_days FROM usage_retention WHERE id = 1".to_string(),
        ))
        .one(&db)
        .await
        .unwrap()
        .expect("legacy retention row survives the migration");
        assert_eq!(row.agent_session_days, 30);
    }
}
