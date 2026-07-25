use sea_orm_migration::prelude::*;

const OLD_SAME_TOOL_DEFAULT: i32 = 10;
const OLD_STEP_DEFAULT: i32 = 20;
const NEW_SAME_TOOL_DEFAULT: i32 = 20;
const NEW_STEP_DEFAULT: i32 = 40;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        update_exact_pair(
            manager,
            OLD_SAME_TOOL_DEFAULT,
            OLD_STEP_DEFAULT,
            NEW_SAME_TOOL_DEFAULT,
            NEW_STEP_DEFAULT,
        )
        .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        update_exact_pair(
            manager,
            NEW_SAME_TOOL_DEFAULT,
            NEW_STEP_DEFAULT,
            OLD_SAME_TOOL_DEFAULT,
            OLD_STEP_DEFAULT,
        )
        .await
    }
}

async fn update_exact_pair(
    manager: &SchemaManager<'_>,
    from_same_tool: i32,
    from_steps: i32,
    to_same_tool: i32,
    to_steps: i32,
) -> Result<(), DbErr> {
    manager
        .exec_stmt(
            Query::update()
                .table(ModelProvider::Table)
                .values([
                    (ModelProvider::MaxSameToolCallsPerTurn, to_same_tool.into()),
                    (ModelProvider::MaxStepsPerTurn, to_steps.into()),
                ])
                .and_where(Expr::col(ModelProvider::MaxSameToolCallsPerTurn).eq(from_same_tool))
                .and_where(Expr::col(ModelProvider::MaxStepsPerTurn).eq(from_steps))
                .to_owned(),
        )
        .await
}

#[derive(Iden)]
enum ModelProvider {
    Table,
    MaxSameToolCallsPerTurn,
    MaxStepsPerTurn,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, Statement};
    use sea_orm_migration::{MigrationTrait, SchemaManager};

    #[derive(Debug, FromQueryResult, PartialEq, Eq)]
    struct Limits {
        id: i32,
        max_same_tool_calls_per_turn: i32,
        max_steps_per_turn: i32,
    }

    async fn rows(db: &sea_orm::DatabaseConnection) -> Vec<Limits> {
        Limits::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT id, max_same_tool_calls_per_turn, max_steps_per_turn \
             FROM model_provider ORDER BY id"
                .to_string(),
        ))
        .all(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn raises_only_the_previous_default_pair() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "CREATE TABLE model_provider ( \
                 id INTEGER PRIMARY KEY, \
                 max_same_tool_calls_per_turn INTEGER NOT NULL, \
                 max_steps_per_turn INTEGER NOT NULL )",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO model_provider VALUES \
                 (1, 10, 20), \
                 (2, 10, 30), \
                 (3, 15, 20)",
        )
        .await
        .unwrap();

        super::Migration.up(&SchemaManager::new(&db)).await.unwrap();

        assert_eq!(
            rows(&db).await,
            vec![
                Limits {
                    id: 1,
                    max_same_tool_calls_per_turn: 20,
                    max_steps_per_turn: 40,
                },
                Limits {
                    id: 2,
                    max_same_tool_calls_per_turn: 10,
                    max_steps_per_turn: 30,
                },
                Limits {
                    id: 3,
                    max_same_tool_calls_per_turn: 15,
                    max_steps_per_turn: 20,
                },
            ]
        );
    }
}
