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

            Ok(db)
        })
        .await
}

const SIGNAL_SCHEMA_VERSION: i32 = 7;
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
                3 => migrate_v3_to_v4(&txn, &tables).await?,
                4 => migrate_v4_to_v5(&txn, &tables).await?,
                5 => migrate_v5_to_v6(&txn, &tables).await?,
                6 => migrate_v6_to_v7(&txn, &tables).await?,
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
    create_entity(db, &schema, agent_action_item::Entity).await?;
    create_entity(db, &schema, agent_capability_grant::Entity).await?;
    create_entity(db, &schema, agent_grant_reservation::Entity).await?;
    create_entity(db, &schema, agent_capability_dispatch_outbox::Entity).await?;
    create_entity(db, &schema, model_egress_receipt::Entity).await?;
    create_entity(db, &schema, agent_run_event::Entity).await?;

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

async fn migrate_v3_to_v4<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v3_schema(db, tables).await?;
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, agent_action_item::Entity).await
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

async fn migrate_v4_to_v5<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v4_schema(db, tables).await?;
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, model_egress_receipt::Entity).await?;
    db.execute(
        &Index::create()
            .if_not_exists()
            .unique()
            .name("idx-model-egress-export-call")
            .table(model_egress_receipt::Entity)
            .col(model_egress_receipt::Column::ExportAuthorizationId)
            .col(model_egress_receipt::Column::ModelCallOrdinal)
            .to_owned(),
    )
    .await?;
    Ok(())
}

async fn migrate_v5_to_v6<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v5_schema(db, tables).await?;
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, agent_run_event::Entity).await?;
    db.execute(
        &Index::create()
            .if_not_exists()
            .unique()
            .name("idx-agent-run-event-sequence")
            .table(agent_run_event::Entity)
            .col(agent_run_event::Column::RunId)
            .col(agent_run_event::Column::EventSeq)
            .to_owned(),
    )
    .await?;
    db.execute(
        &Index::create()
            .if_not_exists()
            .name("idx-agent-run-event-input")
            .table(agent_run_event::Entity)
            .col(agent_run_event::Column::RunId)
            .col(agent_run_event::Column::Kind)
            .col(agent_run_event::Column::InputSeq)
            .to_owned(),
    )
    .await?;
    Ok(())
}

async fn migrate_v6_to_v7<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    validate_v6_schema(db, tables).await?;
    let schema = Schema::new(db.get_database_backend());
    create_entity(db, &schema, agent_capability_grant::Entity).await?;
    create_entity(db, &schema, agent_grant_reservation::Entity).await?;
    create_entity(db, &schema, agent_capability_dispatch_outbox::Entity).await?;
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
    let mut v6_tables = tables.clone();
    for table in [
        "agent_capability_grant",
        "agent_grant_reservation",
        "agent_capability_dispatch_outbox",
    ] {
        if !v6_tables.remove(table) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing {table}"
            )));
        }
    }
    validate_v6_schema(db, &v6_tables).await?;
    let grant_columns = table_columns(db, "agent_capability_grant").await?;
    for required in [
        "grant_id",
        "actor_id",
        "run_id",
        "remaining_uses",
        "payload_json",
        "version",
    ] {
        if !grant_columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_capability_grant.{required}"
            )));
        }
    }
    let reservation_columns = table_columns(db, "agent_grant_reservation").await?;
    for required in [
        "reservation_id",
        "grant_id",
        "run_id",
        "call_id",
        "work_id",
        "canonical_input_digest_sha256",
        "state",
        "generation",
    ] {
        if !reservation_columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_grant_reservation.{required}"
            )));
        }
    }
    let outbox_columns = table_columns(db, "agent_capability_dispatch_outbox").await?;
    for required in [
        "dispatch_id",
        "call_id",
        "work_id",
        "reservation_id",
        "generation",
        "state",
        "payload_json",
    ] {
        if !outbox_columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_capability_dispatch_outbox.{required}"
            )));
        }
    }
    Ok(())
}

async fn validate_v6_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    let mut v5_tables = tables.clone();
    if !v5_tables.remove("agent_run_event") {
        return Err(DbErr::Custom(format!(
            "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_run_event"
        )));
    }
    validate_v5_schema(db, &v5_tables).await?;
    let columns = table_columns(db, "agent_run_event").await?;
    for required in [
        "event_id",
        "run_id",
        "event_seq",
        "input_revision",
        "kind",
        "input_seq",
        "source_envelope_ids_json",
        "result_envelope_ids_json",
        "payload_json",
        "payload_schema_version",
    ] {
        if !columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_run_event.{required}"
            )));
        }
    }
    Ok(())
}

async fn validate_v5_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    let mut v4_tables = tables.clone();
    if !v4_tables.remove("model_egress_receipt") {
        return Err(DbErr::Custom(format!(
            "signal schema v{SIGNAL_SCHEMA_VERSION} is missing model_egress_receipt"
        )));
    }
    validate_v4_schema(db, &v4_tables).await?;
    let columns = table_columns(db, "model_egress_receipt").await?;
    for required in [
        "receipt_id",
        "export_authorization_id",
        "model_call_ordinal",
        "destination_json",
        "envelope_ids_json",
        "digests_sha256_json",
        "projection_digest_sha256",
        "total_bytes",
        "state",
        "model_output_envelope_id",
        "authorized_at",
        "completed_at",
    ] {
        if !columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing model_egress_receipt.{required}"
            )));
        }
    }
    Ok(())
}

async fn validate_v4_schema<C: ConnectionTrait>(
    db: &C,
    tables: &HashSet<String>,
) -> Result<(), DbErr> {
    let mut v3_tables = tables.clone();
    if !v3_tables.remove("agent_action_item") {
        return Err(DbErr::Custom(format!(
            "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_action_item"
        )));
    }
    validate_v3_schema(db, &v3_tables).await?;
    let columns = table_columns(db, "agent_action_item").await?;
    for required in [
        "kind",
        "action_request_id",
        "execution_id",
        "payload_schema_version",
        "result_schema_version",
        "cancel_generation",
    ] {
        if !columns.contains(required) {
            return Err(DbErr::Custom(format!(
                "signal schema v{SIGNAL_SCHEMA_VERSION} is missing agent_action_item.{required}"
            )));
        }
    }
    Ok(())
}

async fn validate_v3_schema<C: ConnectionTrait>(
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
            "DROP TABLE agent_capability_dispatch_outbox; \
             DROP TABLE agent_grant_reservation; \
             DROP TABLE agent_capability_grant; \
             DROP TABLE agent_run_event; \
             DROP TABLE model_egress_receipt; \
             DROP TABLE agent_action_item; \
             DROP TABLE model_probe_observation; \
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
    async fn v2_to_latest_defaults_existing_provider_timeout() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared(
            "DROP TABLE agent_capability_dispatch_outbox; \
             DROP TABLE agent_grant_reservation; \
             DROP TABLE agent_capability_grant; \
             DROP TABLE agent_run_event; \
             DROP TABLE model_egress_receipt; \
             DROP TABLE agent_action_item; \
             ALTER TABLE model_provider DROP COLUMN exec_approval_timeout_secs; \
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
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
    }

    #[tokio::test]
    async fn v4_to_latest_adds_model_egress_receipts_then_reopens_cleanly() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared(
            "DROP TABLE agent_capability_dispatch_outbox; \
             DROP TABLE agent_grant_reservation; \
             DROP TABLE agent_capability_grant; \
             DROP TABLE agent_run_event; \
             DROP TABLE model_egress_receipt; \
             PRAGMA user_version = 4",
        )
        .await
        .unwrap();

        initialize_schema(&db).await.unwrap();
        assert!(
            application_tables(&db)
                .await
                .unwrap()
                .contains("model_egress_receipt")
        );
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
        initialize_schema(&db).await.unwrap();
    }

    #[tokio::test]
    async fn v5_to_latest_adds_ordered_agent_run_events_then_reopens_cleanly() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared(
            "DROP TABLE agent_capability_dispatch_outbox; \
             DROP TABLE agent_grant_reservation; \
             DROP TABLE agent_capability_grant; \
             DROP TABLE agent_run_event; \
             PRAGMA user_version = 5",
        )
        .await
        .unwrap();

        initialize_schema(&db).await.unwrap();
        assert!(
            application_tables(&db)
                .await
                .unwrap()
                .contains("agent_run_event")
        );
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
        initialize_schema(&db).await.unwrap();
    }

    #[tokio::test]
    async fn v6_to_latest_adds_capability_grants_reservations_and_outbox_then_reopens_cleanly() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        create_latest_schema(&db).await.unwrap();
        db.execute_unprepared(
            "DROP TABLE agent_capability_dispatch_outbox; \
             DROP TABLE agent_grant_reservation; \
             DROP TABLE agent_capability_grant; \
             PRAGMA user_version = 6",
        )
        .await
        .unwrap();

        initialize_schema(&db).await.unwrap();
        let tables = application_tables(&db).await.unwrap();
        assert!(tables.contains("agent_capability_grant"));
        assert!(tables.contains("agent_grant_reservation"));
        assert!(tables.contains("agent_capability_dispatch_outbox"));
        assert_eq!(
            query_user_version(&db).await.unwrap(),
            SIGNAL_SCHEMA_VERSION
        );
        initialize_schema(&db).await.unwrap();
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
        seed.close().await.unwrap();

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

        first.close().await.unwrap();
        second.close().await.unwrap();
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
