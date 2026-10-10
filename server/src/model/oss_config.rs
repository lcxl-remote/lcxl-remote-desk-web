//! The OSS settings adapter uses the host's existing complete-document save.

use crate::model::settings::SharedSettings;
use async_trait::async_trait;
use desk_signal::config::{ConfigContext, ConfigPersistence, GlobalConfig};
use sea_orm::DbErr;
use std::sync::Arc;

struct FileSettings {
    settings: Arc<SharedSettings>,
    #[cfg(test)]
    _directory: Option<tempfile::TempDir>,
}

#[async_trait]
impl ConfigPersistence for FileSettings {
    async fn persist(&self, global: &GlobalConfig) -> Result<(), DbErr> {
        let mut live = self.settings.write().await;
        let mut candidate = live.clone();
        candidate.global_config = global.clone();
        candidate
            .save()
            .map_err(|_| DbErr::Custom("OSS settings could not be saved".into()))?;
        *live = candidate;
        Ok(())
    }
}

pub async fn context(settings: Arc<SharedSettings>) -> Result<Arc<ConfigContext>, DbErr> {
    let initial = settings.read().await.global_config.clone();
    ConfigContext::new(
        initial,
        Arc::new(FileSettings {
            settings,
            #[cfg(test)]
            _directory: None,
        }),
    )
}

#[cfg(test)]
pub(crate) struct TestDatabase;
#[cfg(test)]
impl TestDatabase {
    pub async fn connect<C: Into<sea_orm::ConnectOptions>>(
        options: C,
    ) -> Result<desk_signal::config::connection::DatabaseConnection, DbErr> {
        let directory = tempfile::tempdir().unwrap();
        let args = crate::model::settings::Args {
            config_file_path: Some(directory.path().join("config.toml")),
            ..Default::default()
        };
        let initial = crate::model::settings::Settings::new(&args).unwrap();
        let global = initial.global_config.clone();
        let settings = Arc::new(SharedSettings::from(initial));
        let configuration = ConfigContext::new(
            global,
            Arc::new(FileSettings {
                settings,
                _directory: Some(directory),
            }),
        )?;
        Ok(desk_signal::config::connection::DatabaseConnection::new(
            sea_orm::Database::connect(options).await?,
            configuration,
        ))
    }
}

#[cfg(test)]
mod tests;
