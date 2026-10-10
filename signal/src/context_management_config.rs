//! File-owned OSS policy; updates retain the existing revision and validation rules.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_diagnose_core::model_context::PlatformContextPolicy;
use sea_orm::DbErr;

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Invalid,
    Db(DbErr),
}
impl From<DbErr> for WriteError {
    fn from(error: DbErr) -> Self {
        Self::Db(error)
    }
}

pub async fn read<C: ConfigConnection>(db: &C) -> Result<PlatformContextPolicy, DbErr> {
    Ok(db.config_read().await.context_management.clone())
}

pub async fn update(
    db: &DatabaseConnection,
    request: &desk_signal_facade::context_management::UpdateContextManagementRequest,
) -> Result<PlatformContextPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.context_management.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = config
                .context_management
                .candidate(request.strategy.into(), request.summary_max_output_tokens)
                .map_err(|_| WriteError::Invalid)?;
            config.context_management = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
