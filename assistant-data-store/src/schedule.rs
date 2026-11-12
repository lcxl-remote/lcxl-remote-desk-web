//! Scheduled-task storage rules shared by Manager and standalone Signal.
//! Callers own transactions, locks taken before a rule, and policy sources.
use crate::entity::{agent_schedule, agent_task_budget_reservation as budget};
use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
use sea_orm::{ConnectionTrait, DbErr, sea_query::Index};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub enum ScheduleStoreError {
    Invalid,
    ToolInput(&'static str),
    NotFound,
    Conflict,
    BudgetExceeded,
    /// A conversation timer would run after its source session expires.
    ExceedsSessionRetention {
        max_delay_seconds: u64,
    },
    Backend(DbErr),
}
impl From<DbErr> for ScheduleStoreError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}

/// A bounded, trimmed identifier without control characters.
pub fn key(value: &str) -> Result<(), ScheduleStoreError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok(())
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn json<T: serde::Serialize>(value: &T) -> Result<String, ScheduleStoreError> {
    serde_json::to_string(value).map_err(|_| ScheduleStoreError::Invalid)
}

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// The task's frozen retrieval consent, validated against its definition.
pub fn retrieval_consent(
    task: &agent_schedule::Model,
) -> Result<Option<TaskRetrievalConsent>, ScheduleStoreError> {
    let Some(raw) = task.retrieval_consent_json.as_deref() else {
        return Ok(None);
    };
    let consent = TaskRetrievalConsent::decode(raw).map_err(|_| ScheduleStoreError::Invalid)?;
    consent
        .validate_definition(
            &task.schedule_id,
            task.task_revision
                .try_into()
                .map_err(|_| ScheduleStoreError::Invalid)?,
            &task.owner_user_id.to_string(),
            &task.target_device_id,
            consent.source_conversation_id.as_deref(),
        )
        .map_err(|_| ScheduleStoreError::Conflict)?;
    if task.kind == "conversation_resume"
        && task.source_conversation_id.as_deref() != consent.source_conversation_id.as_deref()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(Some(consent))
}

/// The source session device a continuation was proposed from.
pub fn continuation_subject(task: &agent_schedule::Model) -> Result<String, ScheduleStoreError> {
    retrieval_consent(task)?
        .map(|consent| consent.source_session_device_id)
        .ok_or(ScheduleStoreError::Invalid)
}

/// Due scans and budget windows use bounded index ranges.
pub async fn ensure_indexes<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    let due = Index::create()
        .if_not_exists()
        .name("idx_agent_schedule_due")
        .table(agent_schedule::Entity)
        .col(agent_schedule::Column::Status)
        .col(agent_schedule::Column::NextRunAt)
        .col(agent_schedule::Column::Id)
        .to_owned();
    db.execute(&due).await?;
    let budget_window = Index::create()
        .if_not_exists()
        .name("idx_task_budget_schedule_day")
        .table(budget::Entity)
        .col(budget::Column::ScheduleId)
        .col(budget::Column::Kind)
        .col(budget::Column::UtcDay)
        .to_owned();
    db.execute(&budget_window).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_bounded_trimmed_and_printable() {
        assert!(key("task-1").is_ok());
        for value in ["", " task", "task ", "a\nb"] {
            assert!(matches!(key(value), Err(ScheduleStoreError::Invalid)));
        }
        assert!(key(&"k".repeat(256)).is_ok());
        assert!(key(&"k".repeat(257)).is_err());
    }

    #[test]
    fn digests_are_lowercase_sha256_hex() {
        let value = digest("schedule");
        assert!(valid_digest(&value));
        assert!(!valid_digest(&value.to_uppercase()));
        assert!(!valid_digest(&value[1..]));
    }

    #[tokio::test]
    async fn indexes_are_idempotent_on_existing_tables() {
        use sea_orm::{ConnectionTrait, Database, Schema};
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(agent_schedule::Entity))
            .await
            .unwrap();
        db.execute(&schema.create_table_from_entity(budget::Entity))
            .await
            .unwrap();
        ensure_indexes(&db).await.unwrap();
        ensure_indexes(&db).await.unwrap();
    }
}
