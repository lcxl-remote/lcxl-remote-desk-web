use sea_orm::sea_query::Index;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, EntityTrait,
    FromQueryResult, Schema, Statement, TransactionTrait,
};
use std::collections::HashSet;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::entity::{
    agent_action_item, agent_capability_dispatch_outbox, agent_capability_grant, agent_exec_task,
    agent_grant_reservation, agent_run_event, agent_session, ai_usage, device_code,
    host_remote_access_state, model_egress_receipt, turn_usage, usage_retention,
};
use crate::error::DeskSignalError;
use sea_orm::sqlx::sqlite::{SqliteJournalMode, SqliteSynchronous};

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

/// Stage 3 grant reservation/dispatch facts require a local filesystem whose
/// lock and flush semantics are under this host's control. SQLite WAL on UNC,
/// mapped network drives or removable media is not an OSS durability boundary.
fn validate_signal_db_location(path: &Path) -> Result<(), DbErr> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| DbErr::Custom(format!("resolve signal database directory: {error}")))?
            .join(path)
    };
    validate_signal_db_location_platform(&absolute)
}

#[cfg(windows)]
fn validate_signal_db_location_platform(path: &Path) -> Result<(), DbErr> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumeInformationW};
    use windows::core::PCWSTR;

    const DRIVE_FIXED: u32 = 3;
    let prefix = path
        .components()
        .next()
        .ok_or_else(|| DbErr::Custom("signal database path has no Windows volume prefix".into()))?;
    let root = match prefix {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                PathBuf::from(format!("{}:\\", char::from(letter)))
            }
            Prefix::UNC(..)
            | Prefix::VerbatimUNC(..)
            | Prefix::DeviceNS(..)
            | Prefix::Verbatim(..) => {
                return Err(DbErr::Custom(
                    "signal database must not use a UNC, network-share or device path".into(),
                ));
            }
        },
        _ => {
            return Err(DbErr::Custom(
                "signal database path is not rooted on a local Windows volume".into(),
            ));
        }
    };
    let mut wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    // SAFETY: `wide` is a live, NUL-terminated UTF-16 root path for the duration
    // of the call. GetDriveTypeW neither retains nor mutates the buffer.
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
    if drive_type != DRIVE_FIXED {
        return Err(DbErr::Custom(format!(
            "signal database requires a fixed local Windows volume; drive type {drive_type} is unsupported"
        )));
    }
    let mut filesystem_name = [0_u16; 64];
    // SAFETY: both buffers remain live for the call, and the root is the same
    // NUL-terminated local volume root already validated by GetDriveTypeW.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut filesystem_name),
        )
        .map_err(|error| DbErr::Custom(format!("query signal database volume: {error}")))?;
    }
    let filesystem_name = String::from_utf16_lossy(
        &filesystem_name[..filesystem_name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(filesystem_name.len())],
    );
    if !supported_windows_signal_db_filesystem(&filesystem_name) {
        return Err(DbErr::Custom(format!(
            "signal database requires an explicitly supported local filesystem (NTFS or ReFS); {filesystem_name:?} is unsupported"
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn supported_windows_signal_db_filesystem(name: &str) -> bool {
    name.eq_ignore_ascii_case("NTFS") || name.eq_ignore_ascii_case("ReFS")
}

#[cfg(not(windows))]
fn validate_signal_db_location_platform(_path: &Path) -> Result<(), DbErr> {
    Ok(())
}

/// Initialize database connection and return it.
pub async fn init_db(config_dir: &str) -> Result<&'static DatabaseConnection, DeskSignalError> {
    DB_CONN
        .get_or_try_init(|| async {
            let db_path = Path::new(config_dir).join("desk_signal.db");
            validate_signal_db_location(&db_path)?;

            let db_url = path_to_sqlite_url(&db_path);
            log::info!("Connecting to SQLite database at {}", db_url);

            let mut opt = ConnectOptions::new(db_url);
            opt.max_connections(100)
                .min_connections(5)
                .connect_timeout(Duration::from_secs(8))
                .idle_timeout(Duration::from_secs(8))
                .max_lifetime(Duration::from_secs(8))
                .sqlx_logging(false); // Optional: enable/disable query logging
            opt.map_sqlx_sqlite_opts(|options| {
                options
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(5))
            });

            let db = Database::connect(opt).await?;
            verify_sqlite_durability(&db).await?;

            initialize_schema(&db).await?;
            let recovery_now = u64::try_from(chrono::Utc::now().timestamp_millis())
                .map_err(|_| DbErr::Custom("system clock is before Unix epoch".into()))?;
            let uncertain = crate::capability_grant_store::SignalCapabilityGrantStore::new(
                db.clone(),
            )
            .recover_unfinished_dispatches_after_restart(recovery_now)
            .await?;
            if uncertain > 0 {
                log::warn!(
                    "recovered {uncertain} capability dispatch intent(s) as outcome unknown; automatic retry is forbidden"
                );
            }
            crate::agent_exec_store::start_completion_publisher(db.clone());
            crate::agent_background_task_store::start_completion_publisher(db.clone());
            actix_web::rt::spawn(
                crate::schedule_store::ScheduleStore::new(db.clone())
                    .run_calendar_materializer(),
            );

            Ok(db)
        })
        .await
}

const SIGNAL_SCHEMA_VERSION: i32 = 14;
const SCHEMA_LOCK_TABLE: &str = "signal_schema_init_lock";

#[derive(Debug, FromQueryResult)]
struct NameRow {
    name: String,
}

#[derive(Debug, FromQueryResult)]
struct UserVersionRow {
    user_version: i32,
}

/// Initialize or validate the signal database under a SQLite write lock.
pub(crate) async fn initialize_schema(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Creating and touching this one-row table is the SeaORM equivalent of
    // BEGIN IMMEDIATE: the transaction obtains SQLite's write reservation before
    // reading schema state, so two startup processes cannot both classify v0.
    db.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS {SCHEMA_LOCK_TABLE} (id INTEGER PRIMARY KEY CHECK (id = 1)); \
         INSERT OR IGNORE INTO {SCHEMA_LOCK_TABLE}(id) VALUES (1);"
    ))
    .await?;
    let txn = db.begin().await?;
    txn.execute_unprepared(&format!(
        "UPDATE {SCHEMA_LOCK_TABLE} SET id = id WHERE id = 1"
    ))
    .await?;

    let version = query_user_version(&txn).await?;
    let tables = application_tables(&txn).await?;
    if version == 0 && tables.is_empty() {
        create_latest_schema(&txn).await?;
    } else {
        if version != SIGNAL_SCHEMA_VERSION {
            return Err(DbErr::Custom(format!(
                "unsupported signal database schema version {version}; expected {SIGNAL_SCHEMA_VERSION}; automatic migration is not supported"
            )));
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
    create_schedule_schema(db).await?;
    create_entity(db, &schema, device_code::Entity).await?;
    create_entity(db, &schema, turn_usage::Entity).await?;
    create_entity(db, &schema, ai_usage::Entity).await?;
    create_latest_model_provider(db).await?;
    create_latest_probe_observation(db).await?;
    create_entity(db, &schema, usage_retention::Entity).await?;
    create_entity(db, &schema, host_remote_access_state::Entity).await?;
    create_entity(db, &schema, agent_session::Entity).await?;
    create_entity(db, &schema, agent_exec_task::Entity).await?;
    create_entity(db, &schema, agent_action_item::Entity).await?;
    create_entity(db, &schema, agent_capability_grant::Entity).await?;
    create_entity(db, &schema, agent_grant_reservation::Entity).await?;
    create_entity(db, &schema, agent_capability_dispatch_outbox::Entity).await?;
    create_entity(db, &schema, model_egress_receipt::Entity).await?;
    create_entity(db, &schema, agent_run_event::Entity).await?;
    create_entity(db, &schema, crate::entity::agent_permission_resume::Entity).await?;
    create_entity(db, &schema, crate::entity::web_search_config::Entity).await?;
    create_entity(db, &schema, crate::entity::schedule_budget_policy::Entity).await?;
    create_entity(
        db,
        &schema,
        crate::entity::context_management_config::Entity,
    )
    .await?;

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
        Index::create()
            .if_not_exists()
            .unique()
            .name("idx-model-egress-export-call")
            .table(model_egress_receipt::Entity)
            .col(model_egress_receipt::Column::ExportAuthorizationId)
            .col(model_egress_receipt::Column::ModelCallOrdinal)
            .to_owned(),
        Index::create()
            .if_not_exists()
            .unique()
            .name("idx-agent-run-event-sequence")
            .table(agent_run_event::Entity)
            .col(agent_run_event::Column::RunId)
            .col(agent_run_event::Column::EventSeq)
            .to_owned(),
        Index::create()
            .if_not_exists()
            .name("idx-agent-run-event-input")
            .table(agent_run_event::Entity)
            .col(agent_run_event::Column::RunId)
            .col(agent_run_event::Column::Kind)
            .col(agent_run_event::Column::InputSeq)
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

async fn verify_sqlite_durability(db: &DatabaseConnection) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    let pragma = |name: &str| Statement::from_string(backend, format!("PRAGMA {name}"));
    let journal: String = db
        .query_one_raw(pragma("journal_mode"))
        .await?
        .ok_or_else(|| DbErr::Custom("SQLite did not report journal_mode".into()))?
        .try_get("", "journal_mode")?;
    let synchronous: i64 = db
        .query_one_raw(pragma("synchronous"))
        .await?
        .ok_or_else(|| DbErr::Custom("SQLite did not report synchronous".into()))?
        .try_get("", "synchronous")?;
    let foreign_keys: i64 = db
        .query_one_raw(pragma("foreign_keys"))
        .await?
        .ok_or_else(|| DbErr::Custom("SQLite did not report foreign_keys".into()))?
        .try_get("", "foreign_keys")?;
    let busy_timeout: i64 = db
        .query_one_raw(pragma("busy_timeout"))
        .await?
        .ok_or_else(|| DbErr::Custom("SQLite did not report busy_timeout".into()))?
        .try_get("", "timeout")?;
    let quick_check = db.query_all_raw(pragma("quick_check")).await?;
    let quick_check = quick_check
        .iter()
        .map(|row| row.try_get::<String>("", "quick_check"))
        .collect::<Result<Vec<_>, _>>()?;
    if !journal.eq_ignore_ascii_case("wal")
        || synchronous != 2
        || foreign_keys != 1
        || busy_timeout < 5_000
    {
        return Err(DbErr::Custom(format!(
            "unsafe SQLite durability settings: journal_mode={}, synchronous={}, foreign_keys={}, busy_timeout={}",
            journal, synchronous, foreign_keys, busy_timeout
        )));
    }
    if quick_check.as_slice() != ["ok"] {
        return Err(DbErr::Custom(format!(
            "SQLite quick_check failed: {}",
            quick_check.join("; ")
        )));
    }
    Ok(())
}

async fn validate_latest_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    use sea_orm::{EntityName, Iterable, sea_query::Iden};
    let mut remaining = tables.clone();
    macro_rules! check_entity {
        ($module:ident) => {{
            let table = crate::entity::$module::Entity.table_name();
            if !remaining.remove(table) {
                return Err(DbErr::Custom(format!("signal schema is missing {table}")));
            }
            let columns = table_columns(db, table).await?;
            for column in crate::entity::$module::Column::iter() {
                let name = column.to_string();
                if !columns.contains(&name) {
                    return Err(DbErr::Custom(format!(
                        "signal schema is missing {table}.{name}"
                    )));
                }
            }
        }};
    }
    check_entity!(device_code);
    check_entity!(turn_usage);
    check_entity!(ai_usage);
    check_entity!(model_provider);
    check_entity!(model_probe_observation);
    check_entity!(usage_retention);
    check_entity!(host_remote_access_state);
    check_entity!(agent_session);
    check_entity!(agent_exec_task);
    check_entity!(agent_action_item);
    check_entity!(agent_capability_grant);
    check_entity!(agent_grant_reservation);
    check_entity!(agent_capability_dispatch_outbox);
    check_entity!(model_egress_receipt);
    check_entity!(agent_run_event);
    check_entity!(agent_permission_resume);
    check_entity!(web_search_config);
    check_entity!(context_management_config);
    check_entity!(schedule_budget_policy);
    check_entity!(agent_schedule);
    check_entity!(agent_schedule_run);
    check_entity!(agent_task_contract);
    check_entity!(agent_task_rehearsal);
    check_entity!(agent_task_authorization);
    check_entity!(agent_task_budget_reservation);
    if !remaining.is_empty() {
        return Err(DbErr::Custom(format!(
            "signal schema has unexpected tables: {remaining:?}"
        )));
    }
    Ok(())
}

async fn create_schedule_schema<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    use crate::entity::{
        agent_schedule, agent_schedule_run, agent_task_authorization, agent_task_contract,
    };
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, agent_schedule::Entity).await?;
    create_entity(db, &schema, agent_schedule_run::Entity).await?;
    create_entity(db, &schema, agent_task_contract::Entity).await?;
    create_entity(db, &schema, crate::entity::agent_task_rehearsal::Entity).await?;
    create_entity(db, &schema, agent_task_authorization::Entity).await?;
    create_entity(
        db,
        &schema,
        crate::entity::agent_task_budget_reservation::Entity,
    )
    .await?;
    let index = Index::create()
        .name("idx_agent_schedule_due")
        .table(agent_schedule::Entity)
        .col(agent_schedule::Column::Status)
        .col(agent_schedule::Column::NextRunAt)
        .col(agent_schedule::Column::Id)
        .to_owned();
    db.execute(&index).await?;
    use crate::entity::agent_task_budget_reservation as budget;
    let budget_index = Index::create()
        .name("idx_task_budget_schedule_day")
        .table(budget::Entity)
        .col(budget::Column::ScheduleId)
        .col(budget::Column::Kind)
        .col(budget::Column::UtcDay)
        .to_owned();
    db.execute(&budget_index).await?;
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
             AND name NOT LIKE 'sqlite_%' AND name <> '{SCHEMA_LOCK_TABLE}'"
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

    #[cfg(windows)]
    #[test]
    fn signal_db_rejects_unc_network_share() {
        let error =
            validate_signal_db_location(Path::new(r"\\server\share\assistant\desk_signal.db"))
                .unwrap_err();
        assert!(error.to_string().contains("must not use a UNC"));
    }

    #[cfg(windows)]
    #[test]
    fn signal_db_accepts_the_current_fixed_volume() {
        let path = std::env::current_dir().unwrap().join("desk_signal.db");
        validate_signal_db_location(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn signal_db_filesystem_support_is_an_explicit_allowlist() {
        assert!(supported_windows_signal_db_filesystem("NTFS"));
        assert!(supported_windows_signal_db_filesystem("refs"));
        assert!(!supported_windows_signal_db_filesystem("exFAT"));
        assert!(!supported_windows_signal_db_filesystem("FAT32"));
        assert!(!supported_windows_signal_db_filesystem("unknown"));
    }

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
            "model_probe_observation",
            SCHEMA_LOCK_TABLE,
            "usage_retention",
            "host_remote_access_state",
            "agent_session",
            "agent_exec_task",
            "agent_action_item",
            "agent_capability_grant",
            "agent_grant_reservation",
            "agent_capability_dispatch_outbox",
            "model_egress_receipt",
            "agent_run_event",
            "idx_turn_usage_hour",
            "idx_ai_usage_hour",
            "idx-agent-exec-task-delivery",
            "idx-model-egress-export-call",
            "idx-agent-run-event-sequence",
            "idx-agent-run-event-input",
            "idx_task_budget_schedule_day",
            "agent_task_budget_reservation",
            "agent_task_rehearsal",
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
    async fn latest_missing_acceptance_column_fails_without_silent_repair() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        initialize_schema(&db).await.unwrap();
        db.execute_unprepared(
            "ALTER TABLE agent_capability_dispatch_outbox DROP COLUMN computer_acceptance_json",
        )
        .await
        .unwrap();
        let error = initialize_schema(&db).await.unwrap_err().to_string();
        assert!(error.contains("computer_acceptance_json"), "{error}");
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
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
    async fn schedules_initialize_and_survive_reinitialization() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        initialize_schema(&db).await.unwrap();
        initialize_schema(&db).await.unwrap();
        let tables = application_tables(&db).await.unwrap();
        for table in [
            "agent_schedule",
            "agent_schedule_run",
            "agent_task_contract",
            "agent_task_authorization",
        ] {
            assert!(tables.contains(table));
        }
        validate_latest_schema(&db, &tables).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_empty_database_startup_creates_one_complete_schema() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let (first, second) = tokio::join!(initialize_schema(&db), initialize_schema(&db));
        first.unwrap();
        second.unwrap();
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
        validate_latest_schema(&db, &application_tables(&db).await.unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn obsolete_schema_is_rejected_without_migration_or_data_deletion() {
        for version in 0..SIGNAL_SCHEMA_VERSION {
            let db = Database::connect("sqlite::memory:").await.unwrap();
            initialize_schema(&db).await.unwrap();
            db.execute_unprepared("CREATE TABLE development_marker(value TEXT); INSERT INTO development_marker VALUES ('preserve')").await.unwrap();
            db.execute_unprepared(&format!("PRAGMA user_version = {version}"))
                .await
                .unwrap();
            let tables = application_tables(&db).await.unwrap();
            let error = initialize_schema(&db).await.unwrap_err().to_string();
            assert!(
                error.contains("automatic migration is not supported"),
                "{error}"
            );
            assert_eq!(query_user_version(&db).await.unwrap(), version);
            assert_eq!(application_tables(&db).await.unwrap(), tables);
            let value = db
                .query_one_raw(Statement::from_string(
                    db.get_database_backend(),
                    "SELECT value FROM development_marker",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(value.try_get::<String>("", "value").unwrap(), "preserve");
        }
    }
}
