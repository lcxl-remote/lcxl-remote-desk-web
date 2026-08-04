use sea_orm::sea_query::Index;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, EntityTrait, Schema,
};
use std::path::Path;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::entity::{
    agent_exec_task, agent_session, ai_usage, device_code, host_remote_access_state,
    model_provider, turn_usage, usage_retention,
};
use crate::error::DeskSignalError;

static DB_CONN: OnceCell<DatabaseConnection> = OnceCell::const_new();

fn path_to_sqlite_url(path: &Path) -> String {
    // let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    let path_str = path.to_string_lossy();
    let stripped_path = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);

    // Convert Windows backslash to URL slash
    let normalized_path = stripped_path.replace("\\", "/");

    // URL encode the path
    format!("sqlite://{}?mode=rwc", normalized_path)
}

/// Initialize database connection and return it.
pub async fn init_db(config_dir: &str) -> Result<&'static DatabaseConnection, DeskSignalError> {
    DB_CONN
        .get_or_try_init(|| async {
            let db_path = Path::new(config_dir).join("desk_signal.db");

            let db_url = path_to_sqlite_url(&db_path);
            log::info!("Connecting to SQLite database at {}", db_url);

            let mut opt = ConnectOptions::new(db_url);
            opt.max_connections(100)
                .min_connections(5)
                .connect_timeout(Duration::from_secs(8))
                .idle_timeout(Duration::from_secs(8))
                .max_lifetime(Duration::from_secs(8))
                .sqlx_logging(false); // Optional: enable/disable query logging

            let db = Database::connect(opt).await?;

            initialize_schema(&db).await?;
            crate::agent_exec_store::start_completion_publisher(db.clone());

            Ok(db)
        })
        .await
}

/// Create a fresh signal database at the current schema.
///
/// No signal schema has been released with an upgrade-compatibility promise, so
/// there is no historical migration chain to replay. Add migrations only after a
/// released schema actually needs an in-place upgrade.
pub(crate) async fn initialize_schema(db: &DatabaseConnection) -> Result<(), DbErr> {
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, device_code::Entity).await?;
    create_entity(db, &schema, turn_usage::Entity).await?;
    create_entity(db, &schema, ai_usage::Entity).await?;
    create_entity(db, &schema, model_provider::Entity).await?;
    create_entity(db, &schema, usage_retention::Entity).await?;
    create_entity(db, &schema, host_remote_access_state::Entity).await?;
    create_entity(db, &schema, agent_session::Entity).await?;
    create_entity(db, &schema, agent_exec_task::Entity).await?;

    for index in [
        Index::create()
            .if_not_exists()
            .name("idx_turn_usage_hour")
            .table(turn_usage::Entity)
            .col(turn_usage::Column::HourBucket)
            .to_owned(),
        Index::create()
            .if_not_exists()
            .name("idx_ai_usage_hour")
            .table(ai_usage::Entity)
            .col(ai_usage::Column::HourBucket)
            .to_owned(),
        Index::create()
            .if_not_exists()
            .name("idx-agent-exec-task-delivery")
            .table(agent_exec_task::Entity)
            .col(agent_exec_task::Column::DeliveryState)
            .col(agent_exec_task::Column::Status)
            .to_owned(),
    ] {
        db.execute(&index).await?;
    }
    Ok(())
}

async fn create_entity<E>(db: &DatabaseConnection, schema: &Schema, entity: E) -> Result<(), DbErr>
where
    E: EntityTrait + Copy,
{
    let mut table = schema.create_table_from_entity(entity);
    table.if_not_exists();
    db.execute(&table).await?;

    for mut index in schema.create_index_from_entity(entity) {
        index.if_not_exists();
        db.execute(&index).await?;
    }
    Ok(())
}

/// Get database connection, panic if not initialized
pub fn get_db() -> &'static DatabaseConnection {
    DB_CONN.get().expect("Database connection not initialized")
}

/// Get the database connection if it has been initialized, else `None`.
///
/// Used by collect-only telemetry that runs in any server mode: the signal DB
/// exists in `default` / `signaling` modes but not in a pure `desk-server`
/// process, where the telemetry simply no-ops rather than panicking.
pub fn try_get_db() -> Option<&'static DatabaseConnection> {
    DB_CONN.get()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use sea_orm::{Database, FromQueryResult, Statement};

    use super::*;

    #[derive(FromQueryResult)]
    struct SchemaObject {
        name: String,
    }

    #[tokio::test]
    async fn current_schema_is_idempotent_and_has_no_migration_history() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        initialize_schema(&db).await.unwrap();
        initialize_schema(&db).await.unwrap();

        let objects = SchemaObject::find_by_statement(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM sqlite_master \
             WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%'"
                .to_string(),
        ))
        .all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|object| object.name)
        .collect::<HashSet<_>>();

        for required in [
            "device_code",
            "turn_usage_hourly",
            "ai_usage_hourly",
            "model_provider",
            "usage_retention",
            "host_remote_access_state",
            "agent_session",
            "agent_exec_task",
            "idx_turn_usage_hour",
            "idx_ai_usage_hour",
            "idx-agent-exec-task-delivery",
        ] {
            assert!(
                objects.contains(required),
                "missing schema object {required}"
            );
        }
        assert!(!objects.contains("seaql_migrations"));
    }
}
