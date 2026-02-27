use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::path::Path;
use std::time::Duration;
use tokio::sync::OnceCell;

use crate::error::DeskSignalError;
use crate::migration::Migrator;

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

            // Auto migrate
            Migrator::up(&db, None).await?;

            Ok(db)
        })
        .await
}

/// Get database connection, panic if not initialized
pub fn get_db() -> &'static DatabaseConnection {
    DB_CONN.get().expect("Database connection not initialized")
}
