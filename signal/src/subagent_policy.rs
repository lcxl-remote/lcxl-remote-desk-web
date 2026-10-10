//! File-owned OSS policy; updates retain the existing revision and validation rules.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_agent_protocol::ai_assistant::subagent_policy::{SubAgentPolicy, UpdateSubAgentPolicy};
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

pub async fn read<C: ConfigConnection>(db: &C) -> Result<SubAgentPolicy, DbErr> {
    Ok(db.config_read().await.subagent_policy.clone())
}

pub async fn update(
    db: &DatabaseConnection,
    request: &UpdateSubAgentPolicy,
) -> Result<SubAgentPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.subagent_policy.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = desk_diagnose_core::subagent::policy::candidate(
                &config.subagent_policy,
                request.limits,
            )
            .map_err(|_| WriteError::Invalid)?;
            config.subagent_policy = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
