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
                        ColumnDef::new(ModelProvider::MaxStepsPerTurn)
                            .integer()
                            .not_null()
                            .default(40),
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
                    .drop_column(ModelProvider::MaxStepsPerTurn)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum ModelProvider {
    Table,
    MaxStepsPerTurn,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, Statement};
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    #[derive(FromQueryResult)]
    struct AddedColumn {
        max_steps_per_turn: i32,
    }

    #[tokio::test]
    async fn existing_provider_rows_receive_the_default_step_budget() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "CREATE TABLE model_provider ( \
                 id INTEGER PRIMARY KEY, \
                 max_same_tool_calls_per_turn INTEGER NOT NULL DEFAULT 10 )",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO model_provider (id, max_same_tool_calls_per_turn) VALUES (1, 10)",
        )
        .await
        .unwrap();

        super::Migration.up(&SchemaManager::new(&db)).await.unwrap();

        let row = AddedColumn::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT max_steps_per_turn FROM model_provider WHERE id = 1".to_string(),
        ))
        .one(&db)
        .await
        .unwrap()
        .expect("existing provider row survives the migration");
        assert_eq!(row.max_steps_per_turn, 40);
    }
}
