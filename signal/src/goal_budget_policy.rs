//! File-owned OSS policy; updates retain the existing revision and validation rules.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_agent_protocol::ai_assistant::goal_budget::{GoalBudgetPolicy, UpdateGoalBudgetPolicy};
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

pub async fn read<C: ConfigConnection>(db: &C) -> Result<GoalBudgetPolicy, DbErr> {
    Ok(db.config_read().await.goal_budget_policy.clone())
}

pub async fn update(
    db: &DatabaseConnection,
    request: &UpdateGoalBudgetPolicy,
) -> Result<GoalBudgetPolicy, WriteError> {
    db.config_context()
        .update(|config| {
            if config.goal_budget_policy.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = desk_diagnose_core::goal_budget::candidate(
                &config.goal_budget_policy,
                request.limits,
                request.device_unavailable_max_ms,
            )
            .map_err(|_| WriteError::Invalid)?;
            config.goal_budget_policy = next.clone();
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}
