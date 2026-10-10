//! File-owned OSS policy; updates retain the existing revision and validation rules.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_diagnose_core::terminal_completion_policy::TerminalCompletionPolicy;
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

pub async fn read<C: ConfigConnection>(db: &C) -> Result<TerminalCompletionPolicy, DbErr> {
    Ok(db.config_read().await.terminal_completion.clone())
}

pub async fn update(
    db: &DatabaseConnection,
    request: &desk_signal_facade::terminal_completion::UpdateTerminalCompletionRequest,
) -> Result<TerminalCompletionPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.terminal_completion.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = config
                .terminal_completion
                .candidate(request.max_output_tokens)
                .map_err(|_| WriteError::Invalid)?;
            config.terminal_completion = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
