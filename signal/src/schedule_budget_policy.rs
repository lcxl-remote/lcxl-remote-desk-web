//! File-owned OSS policy; updates retain the existing revision and validation rules.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_agent_protocol::schedule::policy::{ScheduleBudgetPolicy, UpdateScheduleBudgetPolicy};
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

pub async fn read<C: ConfigConnection>(db: &C) -> Result<ScheduleBudgetPolicy, DbErr> {
    Ok(db.config_read().await.schedule_budget_policy.clone())
}

pub async fn update(
    db: &DatabaseConnection,
    request: &UpdateScheduleBudgetPolicy,
) -> Result<ScheduleBudgetPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.schedule_budget_policy.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = desk_diagnose_core::schedule::policy::candidate(
                &config.schedule_budget_policy,
                request.maximum.clone(),
            )
            .map_err(|_| WriteError::Invalid)?;
            config.schedule_budget_policy = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
