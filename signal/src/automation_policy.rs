//! File-owned OSS bound on automatic follow-up turns per chain.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_agent_protocol::ai_assistant::automation_policy::{
    AutomationPolicy, UpdateAutomationPolicy,
};

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Invalid,
    Db(sea_orm::DbErr),
}
impl From<sea_orm::DbErr> for WriteError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Db(error)
    }
}

pub async fn read<C: ConfigConnection>(db: &C) -> AutomationPolicy {
    db.config_read().await.automation_policy.clone()
}

/// The automatic turns one chain may spend.
pub async fn max_auto_turns_per_chain<C: ConfigConnection>(db: &C) -> u32 {
    db.config_read()
        .await
        .automation_policy
        .max_auto_turns_per_chain
}

pub async fn update(
    db: &DatabaseConnection,
    request: &UpdateAutomationPolicy,
) -> Result<AutomationPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.automation_policy.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = config
                .automation_policy
                .candidate(request.max_auto_turns_per_chain)
                .map_err(|_| WriteError::Invalid)?;
            config.automation_policy = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
