//! File-owned OSS search settings with the existing credential update semantics.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_signal_facade::web_search::{SearchConfig, SearchConfigUpdate};
use sea_orm::DbErr;

#[derive(Debug)]
pub enum WriteError {
    Conflict(u64),
    Invalid(&'static str),
    Db(DbErr),
}
impl From<DbErr> for WriteError {
    fn from(error: DbErr) -> Self {
        Self::Db(error)
    }
}

pub async fn read<C: ConfigConnection>(db: &C) -> Result<SearchConfig, DbErr> {
    Ok(db.config_read().await.web_search.clone())
}

pub async fn update(
    db: &DatabaseConnection,
    update: &SearchConfigUpdate,
) -> Result<SearchConfig, WriteError> {
    db.config_context()
        .update(|config| {
            if config.web_search.revision != update.expected_revision {
                return Err(WriteError::Conflict(config.web_search.revision));
            }
            let next = config
                .web_search
                .candidate(update)
                .map_err(WriteError::Invalid)?;
            config.web_search = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict(update.expected_revision))
}
