use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ModelProvider::Table)
                    .add_column(
                        ColumnDef::new(ModelProvider::MaxSameToolCallsPerTurn)
                            .integer()
                            .not_null()
                            .default(20),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ModelProvider::Table)
                    .drop_column(ModelProvider::MaxSameToolCallsPerTurn)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum ModelProvider {
    Table,
    MaxSameToolCallsPerTurn,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, Statement};
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    #[derive(FromQueryResult)]
    struct AddedColumn {
        max_same_tool_calls_per_turn: i32,
    }

    #[tokio::test]
    async fn legacy_provider_rows_receive_the_default_limit() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "CREATE TABLE model_provider ( \
                 id INTEGER PRIMARY KEY, \
                 provider VARCHAR NULL, \
                 model VARCHAR NULL, \
                 base_url VARCHAR NULL, \
                 api_key VARCHAR NULL, \
                 max_context_bytes BIGINT NULL, \
                 response_format VARCHAR NOT NULL, \
                 execution_mode VARCHAR NOT NULL, \
                 updated_at TIMESTAMP NOT NULL )",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO model_provider \
             (id, response_format, execution_mode, updated_at) \
             VALUES (1, 'json_object', 'suggest_only', '2026-01-01 00:00:00')",
        )
        .await
        .unwrap();

        super::Migration.up(&SchemaManager::new(&db)).await.unwrap();

        let row = AddedColumn::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT max_same_tool_calls_per_turn FROM model_provider WHERE id = 1".to_string(),
        ))
        .one(&db)
        .await
        .unwrap()
        .expect("legacy provider row survives the migration");
        assert_eq!(row.max_same_tool_calls_per_turn, 20);
    }
}
