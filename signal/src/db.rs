use sea_orm::sea_query::Index;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, EntityTrait,
    FromQueryResult, Schema, Statement, TransactionTrait,
};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::entity::{
    agent_exec_task, agent_session, ai_usage, device_code, host_remote_access_state, turn_usage,
    usage_retention,
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

const SIGNAL_SCHEMA_VERSION: i32 = 3;
const MIGRATION_LOCK_TABLE: &str = "signal_schema_migration_lock";
const LEGACY_TABLES: [&str; 8] = [
    "agent_exec_task",
    "agent_session",
    "ai_usage_hourly",
    "device_code",
    "host_remote_access_state",
    "model_provider",
    "turn_usage_hourly",
    "usage_retention",
];

#[derive(Debug, FromQueryResult)]
struct NameRow {
    name: String,
}

#[derive(Debug, FromQueryResult)]
struct UserVersionRow {
    user_version: i32,
}

#[derive(Debug, FromQueryResult)]
struct CountRow {
    count: i64,
}

/// Initialize or migrate the signal database under a SQLite write lock.
pub(crate) async fn initialize_schema(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Creating and touching this one-row table is the SeaORM equivalent of
    // BEGIN IMMEDIATE: the transaction obtains SQLite's write reservation before
    // reading schema state, so two startup processes cannot both classify v0.
    db.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS {MIGRATION_LOCK_TABLE} (id INTEGER PRIMARY KEY CHECK (id = 1)); \
         INSERT OR IGNORE INTO {MIGRATION_LOCK_TABLE}(id) VALUES (1);"
    ))
    .await?;
    let txn = db.begin().await?;
    txn.execute_unprepared(&format!(
        "UPDATE {MIGRATION_LOCK_TABLE} SET id = id WHERE id = 1"
    ))
    .await?;

    let version = query_user_version(&txn).await?;
    let tables = application_tables(&txn).await?;
    if version == 0 && tables.is_empty() {
        create_latest_schema(&txn).await?;
    } else {
        if !(0..=SIGNAL_SCHEMA_VERSION).contains(&version) {
            return Err(DbErr::Custom(format!(
                "unsupported signal database schema version {version}"
            )));
        }
        let mut migration_version = version;
        while migration_version < SIGNAL_SCHEMA_VERSION {
            let tables = application_tables(&txn).await?;
            match migration_version {
                0 => migrate_legacy_v0_to_v1(&txn, &tables).await?,
                1 => migrate_v1_to_v2(&txn, &tables).await?,
                2 => migrate_v2_to_v3(&txn, &tables).await?,
                other => {
                    return Err(DbErr::Custom(format!(
                        "no signal database migration registered from version {other}"
                    )));
                }
            }
            migration_version += 1;
        }
        validate_latest_schema(&txn, &application_tables(&txn).await?).await?;
    }
    txn.execute_unprepared(&format!("PRAGMA user_version = {SIGNAL_SCHEMA_VERSION}"))
        .await?;
    txn.commit().await?;
    Ok(())
}

async fn create_latest_schema<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, device_code::Entity).await?;
    create_entity(db, &schema, turn_usage::Entity).await?;
    create_entity(db, &schema, ai_usage::Entity).await?;
    create_latest_model_provider(db).await?;
    create_latest_probe_observation(db).await?;
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

async fn create_latest_model_provider<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    db.execute_unprepared(
        "CREATE TABLE model_provider (\
           id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),\
           wire_protocol TEXT NULL, model TEXT NULL,\
           supports_image_input INTEGER NOT NULL DEFAULT 0,\
           base_url TEXT NULL, api_key TEXT NULL,\
           profile_schema_version INTEGER NOT NULL CHECK (profile_schema_version >= 1),\
           request_options TEXT NOT NULL CHECK (json_valid(request_options) AND json_type(request_options) = 'object'),\
           output_limit_field TEXT NOT NULL,\
           probe_max_output_tokens INTEGER NOT NULL CHECK (probe_max_output_tokens > 0),\
           runtime_max_output_tokens INTEGER NOT NULL CHECK (runtime_max_output_tokens > 0),\
           max_context_bytes INTEGER NOT NULL CHECK (max_context_bytes BETWEEN 4096 AND 16777216),\
           connection_revision INTEGER NOT NULL CHECK (connection_revision >= 1),\
           profile_revision INTEGER NOT NULL CHECK (profile_revision >= 1),\
           response_format TEXT NOT NULL, execution_mode TEXT NOT NULL,\
           max_same_tool_calls_per_turn INTEGER NOT NULL,\
           max_steps_per_turn INTEGER NOT NULL,\
           exec_approval_timeout_secs INTEGER NOT NULL DEFAULT 120 \
             CHECK (exec_approval_timeout_secs BETWEEN 30 AND 1800),\
           updated_at TEXT NOT NULL\
         )",
    )
    .await?;
    Ok(())
}

/// Historical v1 model-provider shape used only by the legacy v0 migration.
/// Later migrations add their columns in version order inside the same startup
/// transaction.
async fn create_v1_model_provider<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    db.execute_unprepared(
        "CREATE TABLE model_provider (\
           id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),\
           wire_protocol TEXT NULL, model TEXT NULL,\
           supports_image_input INTEGER NOT NULL DEFAULT 0,\
           base_url TEXT NULL, api_key TEXT NULL,\
           profile_schema_version INTEGER NOT NULL CHECK (profile_schema_version >= 1),\
           request_options TEXT NOT NULL CHECK (json_valid(request_options) AND json_type(request_options) = 'object'),\
           output_limit_field TEXT NOT NULL,\
           probe_max_output_tokens INTEGER NOT NULL CHECK (probe_max_output_tokens > 0),\
           runtime_max_output_tokens INTEGER NOT NULL CHECK (runtime_max_output_tokens > 0),\
           max_context_bytes INTEGER NOT NULL CHECK (max_context_bytes BETWEEN 4096 AND 16777216),\
           connection_revision INTEGER NOT NULL CHECK (connection_revision >= 1),\
           profile_revision INTEGER NOT NULL CHECK (profile_revision >= 1),\
           response_format TEXT NOT NULL, execution_mode TEXT NOT NULL,\
           max_same_tool_calls_per_turn INTEGER NOT NULL,\
           max_steps_per_turn INTEGER NOT NULL, updated_at TEXT NOT NULL\
         )",
    )
    .await?;
    Ok(())
}

async fn create_latest_probe_observation<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    db.execute_unprepared(
        "CREATE TABLE model_probe_observation (\
           model_provider_id INTEGER PRIMARY KEY NOT NULL CHECK (model_provider_id = 1),\
           connection_revision INTEGER NOT NULL CHECK (connection_revision >= 1),\
           profile_revision INTEGER NOT NULL CHECK (profile_revision >= 1),\
           tested_at TEXT NOT NULL, reasoning_observed INTEGER NULL,\
           reasoning_tokens INTEGER NULL CHECK (reasoning_tokens IS NULL OR reasoning_tokens >= 0),\
           stop_reason TEXT NULL,\
           validated_capabilities TEXT NOT NULL CHECK (json_valid(validated_capabilities) AND json_type(validated_capabilities) = 'object'),\
           FOREIGN KEY (model_provider_id) REFERENCES model_provider(id) ON DELETE CASCADE\
         )",
    )
    .await?;
    Ok(())
}

async fn migrate_legacy_v0_to_v1<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    let expected: HashSet<String> = LEGACY_TABLES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    if tables != &expected {
        return Err(DbErr::Custom(format!(
            "legacy signal database has an unknown or partial table set: {tables:?}"
        )));
    }
    let columns = table_columns(db, "model_provider").await?;
    let expected_columns: HashSet<String> = [
        "id",
        "provider",
        "model",
        "supports_image_input",
        "base_url",
        "api_key",
        "max_context_bytes",
        "response_format",
        "execution_mode",
        "max_same_tool_calls_per_turn",
        "max_steps_per_turn",
        "updated_at",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if columns != expected_columns {
        return Err(DbErr::Custom(format!(
            "legacy model_provider schema fingerprint mismatch: {columns:?}"
        )));
    }

    let oversized = CountRow::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        "SELECT COUNT(*) AS count FROM model_provider WHERE max_context_bytes > 16777216"
            .to_string(),
    ))
    .one(db)
    .await?
    .map_or(0, |row| row.count);
    if oversized > 0 {
        log::warn!(
            "legacy model-provider max_context_bytes exceeded the application safety limit; clamped {oversized} row(s) to 16777216"
        );
    }

    db.execute_unprepared("ALTER TABLE model_provider RENAME TO model_provider_v0")
        .await?;
    create_v1_model_provider(db).await?;
    db.execute_unprepared(
        "INSERT INTO model_provider (\
           id, wire_protocol, model, supports_image_input, base_url, api_key,\
           profile_schema_version, request_options, output_limit_field,\
           probe_max_output_tokens, runtime_max_output_tokens, max_context_bytes,\
           connection_revision, profile_revision, response_format, execution_mode,\
           max_same_tool_calls_per_turn, max_steps_per_turn, updated_at\
         ) SELECT id,\
           CASE provider WHEN 'anthropic' THEN 'anthropic_messages' \
             WHEN 'openai-compatible' THEN 'open_ai_chat_completions' \
             ELSE provider END,\
           model, supports_image_input, base_url, api_key, 1, '{}', 'max_tokens',\
           512, 4096,\
           CASE WHEN max_context_bytes IS NULL OR max_context_bytes = 0 THEN 131072 \
             WHEN max_context_bytes < 4096 THEN 4096 \
             WHEN max_context_bytes > 16777216 THEN 16777216 \
             ELSE max_context_bytes END,\
           1, 1, response_format, execution_mode, max_same_tool_calls_per_turn,\
           max_steps_per_turn, updated_at FROM model_provider_v0",
    )
    .await?;
    db.execute_unprepared("DROP TABLE model_provider_v0")
        .await?;
    Ok(())
}

async fn migrate_v1_to_v2<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v1_schema(db, tables).await?;
    create_latest_probe_observation(db).await
}

async fn migrate_v2_to_v3<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v2_schema(db, tables).await?;
    db.execute_unprepared(
        "ALTER TABLE model_provider ADD COLUMN exec_approval_timeout_secs INTEGER \
         NOT NULL DEFAULT 120 CHECK (exec_approval_timeout_secs BETWEEN 30 AND 1800)",
    )
    .await?;
    Ok(())
}

async fn validate_v1_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    let expected: HashSet<String> = LEGACY_TABLES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    if tables != &expected {
        return Err(DbErr::Custom(format!(
            "signal database schema v{SIGNAL_SCHEMA_VERSION} has an unknown or partial table set: {tables:?}"
        )));
    }
    validate_profile_columns(db, 1).await
}

async fn validate_latest_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v2_schema(db, tables).await?;
    let columns = table_columns(db, "model_provider").await?;
    if !columns.contains("exec_approval_timeout_secs") {
        return Err(DbErr::Custom(format!(
            "signal schema v{SIGNAL_SCHEMA_VERSION} is missing \
             model_provider.exec_approval_timeout_secs"
        )));
    }
    Ok(())
}

async fn validate_v2_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    let mut expected: HashSet<String> = LEGACY_TABLES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    expected.insert("model_probe_observation".to_string());
    if tables != &expected {
        return Err(DbErr::Custom(format!(
            "signal database schema v2 has an unknown or partial table set: {tables:?}"
        )));
    }
    validate_profile_columns(db, 2).await?;
    let observation_columns = table_columns(db, "model_probe_observation").await?;
    for required in [
        "model_provider_id",
        "connection_revision",
        "profile_revision",
        "tested_at",
        "reasoning_observed",
        "reasoning_tokens",
        "stop_reason",
        "validated_capabilities",
    ] {
        if !observation_columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v2 is missing model_probe_observation.{required}"
            )));
        }
    }
    Ok(())
}

async fn validate_profile_columns<C: ConnectionTrait>(
    db: &C,
    schema_version: i32,
) -> Result<(), DbErr> {
    let columns = table_columns(db, "model_provider").await?;
    for required in [
        "wire_protocol",
        "profile_schema_version",
        "request_options",
        "output_limit_field",
        "probe_max_output_tokens",
        "runtime_max_output_tokens",
        "max_context_bytes",
        "connection_revision",
        "profile_revision",
    ] {
        if !columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{schema_version} is missing model_provider.{required}"
            )));
        }
    }
    Ok(())
}

async fn query_user_version<C: ConnectionTrait>(db: &C) -> Result<i32, DbErr> {
    let row = UserVersionRow::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        "PRAGMA user_version".to_string(),
    ))
    .one(db)
    .await?
    .ok_or_else(|| DbErr::Custom("PRAGMA user_version returned no row".to_string()))?;
    Ok(row.user_version)
}

async fn application_tables<C: ConnectionTrait>(db: &C) -> Result<HashSet<String>, DbErr> {
    Ok(NameRow::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        format!(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name <> '{MIGRATION_LOCK_TABLE}'"
        ),
    ))
    .all(db)
    .await?
    .into_iter()
    .map(|row| row.name)
    .collect())
}

async fn table_columns<C: ConnectionTrait>(db: &C, table: &str) -> Result<HashSet<String>, DbErr> {
    Ok(NameRow::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        format!("SELECT name FROM pragma_table_info('{table}')"),
    ))
    .all(db)
    .await?
    .into_iter()
    .map(|row| row.name)
    .collect())
}

async fn create_entity<C, E>(db: &C, schema: &Schema, entity: E) -> Result<(), DbErr>
where
    C: ConnectionTrait,
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

    async fn install_legacy_v0(
        db: &DatabaseConnection,
        id: i64,
        provider: &str,
        max_context_bytes: Option<i64>,
    ) {
        let schema = Schema::new(db.get_database_backend());
        for entity in [
            schema.create_table_from_entity(device_code::Entity),
            schema.create_table_from_entity(turn_usage::Entity),
            schema.create_table_from_entity(ai_usage::Entity),
            schema.create_table_from_entity(usage_retention::Entity),
            schema.create_table_from_entity(host_remote_access_state::Entity),
            schema.create_table_from_entity(agent_session::Entity),
            schema.create_table_from_entity(agent_exec_task::Entity),
        ] {
            db.execute(&entity).await.unwrap();
        }
        let max_context_bytes =
            max_context_bytes.map_or_else(|| "NULL".to_string(), |value| value.to_string());
        db.execute_unprepared(&format!(
            "CREATE TABLE model_provider (\
               id INTEGER PRIMARY KEY NOT NULL, provider TEXT NULL, model TEXT NULL,\
               supports_image_input INTEGER NOT NULL DEFAULT 0, base_url TEXT NULL,\
               api_key TEXT NULL, max_context_bytes INTEGER NULL, response_format TEXT NOT NULL,\
               execution_mode TEXT NOT NULL, max_same_tool_calls_per_turn INTEGER NOT NULL,\
               max_steps_per_turn INTEGER NOT NULL, updated_at TEXT NOT NULL\
             );\
             INSERT INTO model_provider VALUES ({id}, '{provider}', 'claude-test', 0,\
               'https://example.test', 'preserved-secret', {max_context_bytes}, 'json_object',\
               'confirm_each_action', 20, 40, '2026-08-20T00:00:00Z')"
        ))
        .await
        .unwrap();
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
            "model_probe_observation",
            MIGRATION_LOCK_TABLE,
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
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn v1_to_latest_runner_adds_probe_and_timeout_then_reopens_cleanly() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared(
            "DROP TABLE model_probe_observation; \
             ALTER TABLE model_provider DROP COLUMN exec_approval_timeout_secs; \
             PRAGMA user_version = 1",
        )
        .await
        .unwrap();

        initialize_schema(&db).await.unwrap();
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
        assert!(
            application_tables(&db)
                .await
                .unwrap()
                .contains("model_probe_observation")
        );
        assert!(
            table_columns(&db, "model_provider")
                .await
                .unwrap()
                .contains("exec_approval_timeout_secs")
        );
        initialize_schema(&db).await.unwrap();
    }

    #[tokio::test]
    async fn v2_to_v3_defaults_existing_provider_timeout() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared(
            "ALTER TABLE model_provider DROP COLUMN exec_approval_timeout_secs; \
             INSERT INTO model_provider (id, wire_protocol, model, supports_image_input, \
               base_url, api_key, profile_schema_version, request_options, output_limit_field, \
               probe_max_output_tokens, runtime_max_output_tokens, max_context_bytes, \
               connection_revision, profile_revision, response_format, execution_mode, \
               max_same_tool_calls_per_turn, max_steps_per_turn, updated_at) \
             VALUES (1, 'open_ai_chat_completions', 'test', 0, 'https://example.test', \
               'secret', 1, '{}', 'max_tokens', 512, 4096, 131072, 1, 1, 'json_object', \
               'confirm_each_action', 20, 40, '2026-08-21T00:00:00Z'); \
             PRAGMA user_version = 2",
        )
        .await
        .unwrap();

        initialize_schema(&db).await.unwrap();
        let row = crate::entity::model_provider::Entity::find_by_id(1)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.exec_approval_timeout_secs, 120);
        assert_eq!(query_user_version(&db).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn unknown_future_schema_version_fails_closed() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared("PRAGMA user_version = 99")
            .await
            .unwrap();
        let error = initialize_schema(&db).await.unwrap_err().to_string();
        assert!(error.contains("unsupported signal database schema version 99"));
    }

    #[tokio::test]
    async fn legacy_v0_migrates_profile_and_preserves_secret() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        install_legacy_v0(&db, 1, "anthropic", Some(0)).await;

        initialize_schema(&db).await.unwrap();
        let row = crate::entity::model_provider::Entity::find_by_id(1)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.wire_protocol.as_deref(), Some("anthropic_messages"));
        assert_eq!(row.api_key.as_deref(), Some("preserved-secret"));
        assert_eq!(row.max_context_bytes, 131_072);
        assert_eq!(row.request_options, "{}");
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn legacy_context_budget_materialization_covers_every_boundary_class() {
        for (input, expected) in [
            (None, 131_072),
            (Some(0), 131_072),
            (Some(1), 4096),
            (Some(4095), 4096),
            (Some(4096), 4096),
            (Some(16_777_216), 16_777_216),
            (Some(16_777_217), 16_777_216),
        ] {
            let db = Database::connect("sqlite::memory:").await.unwrap();
            install_legacy_v0(&db, 1, "openai-compatible", input).await;
            initialize_schema(&db).await.unwrap();
            let row = crate::entity::model_provider::Entity::find_by_id(1)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.max_context_bytes, expected, "legacy input {input:?}");
        }
    }

    #[tokio::test]
    async fn failed_legacy_migration_rolls_back_and_can_be_retried() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        // The v0 fingerprint permits arbitrary ids, but latest enforces the
        // published singleton id=1 invariant. This fails after rename/create,
        // proving that the whole migration—not only classification—rolls back.
        install_legacy_v0(&db, 2, "openai-compatible", Some(131_072)).await;
        initialize_schema(&db)
            .await
            .expect_err("id=2 must fail latest singleton check");

        assert_eq!(query_user_version(&db).await.unwrap(), 0);
        let columns = table_columns(&db, "model_provider").await.unwrap();
        assert!(columns.contains("provider"));
        assert!(!columns.contains("wire_protocol"));
        let secret = db
            .query_one_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT api_key FROM model_provider WHERE id = 2".to_string(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<String>("", "api_key")
            .unwrap();
        assert_eq!(secret, "preserved-secret");

        db.execute_unprepared("UPDATE model_provider SET id = 1 WHERE id = 2")
            .await
            .unwrap();
        initialize_schema(&db).await.unwrap();
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn concurrent_startup_serializes_the_legacy_migration() {
        let path =
            std::env::temp_dir().join(format!("lrdm-signal-migration-{}.db", uuid::Uuid::new_v4()));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let seed = Database::connect(&url).await.unwrap();
        install_legacy_v0(&seed, 1, "openai-compatible", Some(131_072)).await;
        drop(seed);

        let first = Database::connect(&url).await.unwrap();
        let second = Database::connect(&url).await.unwrap();
        let (first_result, second_result) =
            tokio::join!(initialize_schema(&first), initialize_schema(&second));
        first_result.unwrap();
        second_result.unwrap();
        assert_eq!(
            query_user_version(&first).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
        assert_eq!(
            crate::entity::model_provider::Entity::find()
                .all(&first)
                .await
                .unwrap()
                .len(),
            1
        );

        drop(first);
        drop(second);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn partial_legacy_schema_fails_closed() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared("CREATE TABLE model_provider (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
        let error = initialize_schema(&db).await.unwrap_err().to_string();
        assert!(error.contains("unknown or partial table set"), "{error}");
    }
}
