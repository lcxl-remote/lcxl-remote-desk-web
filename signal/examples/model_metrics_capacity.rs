//! Disposable SQLite capacity runner using the OSS Store and current schema.

use desk_signal::model_metrics::{runtime, store::Store};
use sea_orm::{ConnectOptions, Database};
use std::time::Duration;

mod benchmark {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_capacity.rs"
    ));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count = benchmark::sample_count()?;
    let directory = std::env::temp_dir().join(format!(
        "model-metrics-capacity-{}",
        uuid::Uuid::new_v4().simple()
    ));
    tokio::fs::create_dir(&directory).await?;
    let path = directory.join("metrics.sqlite");
    let mut options = ConnectOptions::new(format!(
        "sqlite://{}?mode=rwc",
        path.to_string_lossy().replace('\\', "/")
    ));
    options
        .max_connections(1)
        .min_connections(0)
        .connect_timeout(Duration::from_secs(3))
        .acquire_timeout(Duration::from_secs(3))
        .sqlx_logging(false);
    options.map_sqlx_sqlite_opts(|options| {
        options
            .journal_mode(sea_orm::sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sea_orm::sqlx::sqlite::SqliteSynchronous::Normal)
    });
    let result = async {
        let db = Database::connect(options).await?;
        let result = async {
            runtime::create_schema(&db).await?;
            let store = Store::new(db.clone(), false, "capacity-runner".into());
            let mut report = benchmark::run(&store, count).await?;
            let main = tokio::fs::metadata(&path).await?.len();
            let wal = match tokio::fs::metadata(directory.join("metrics.sqlite-wal")).await {
                Ok(value) => value.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error.into()),
            };
            report["physical_allocated_bytes"] = (main + wal).to_string().into();
            Ok::<_, Box<dyn std::error::Error>>(report)
        }
        .await;
        db.close().await?;
        result
    }
    .await;
    tokio::fs::remove_dir_all(&directory).await?;
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}
