//! Test connections use an explicit file document, including across restarts.

use super::{ConfigContext, ConfigPersistence, GlobalConfig, connection::DatabaseConnection};
use async_trait::async_trait;
use sea_orm::{ConnectOptions, DbErr};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};

struct FileConfig {
    path: PathBuf,
    _directory: Option<tempfile::TempDir>,
}

#[async_trait]
impl ConfigPersistence for FileConfig {
    async fn persist(&self, config: &GlobalConfig) -> Result<(), DbErr> {
        let contents = toml::to_string(config)
            .map_err(|_| DbErr::Custom("test configuration encoding failed".into()))?;
        desk_utils::durable_file::durable_atomic_write(
            &self.path,
            contents.as_bytes(),
            desk_utils::durable_file::FileMode::Preserve,
        )
        .map_err(|_| DbErr::Custom("test configuration write failed".into()))
    }
}

async fn open(persistence: FileConfig) -> Arc<ConfigContext> {
    let mut initial = if persistence.path.exists() {
        toml::from_str(&std::fs::read_to_string(&persistence.path).unwrap()).unwrap()
    } else {
        GlobalConfig::default()
    };
    initial
        .initialize_metadata()
        .expect("valid test configuration");
    let persistence = Arc::new(persistence);
    persistence
        .persist(&initial)
        .await
        .expect("persist test settings");
    ConfigContext::new(initial, persistence).expect("test configuration context")
}

pub async fn context() -> Arc<ConfigContext> {
    let directory = tempfile::tempdir().expect("test settings directory");
    open(FileConfig {
        path: directory.path().join("config.toml"),
        _directory: Some(directory),
    })
    .await
}

pub struct Database;
impl Database {
    pub async fn connect<C: Into<ConnectOptions>>(options: C) -> Result<DatabaseConnection, DbErr> {
        let options = options.into();
        let configuration = if let Some(path) = options.get_url().strip_prefix("sqlite://") {
            let path = PathBuf::from(path.split('?').next().unwrap()).with_extension("config.toml");
            static CONTEXTS: OnceLock<tokio::sync::Mutex<HashMap<PathBuf, Weak<ConfigContext>>>> =
                OnceLock::new();
            let mut contexts = CONTEXTS.get_or_init(Default::default).lock().await;
            if let Some(current) = contexts.get(&path).and_then(Weak::upgrade) {
                current
            } else {
                let current = open(FileConfig {
                    path: path.clone(),
                    _directory: None,
                })
                .await;
                contexts.insert(path, Arc::downgrade(&current));
                current
            }
        } else {
            context().await
        };
        Ok(DatabaseConnection::new(
            sea_orm::Database::connect(options).await?,
            configuration,
        ))
    }
}
