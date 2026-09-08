//! Owner decisions are resolved through the original task occurrence, never a client session key.
use super::*;
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_diagnose_core::{
    file_scope::{
        FileScopeSubject,
        transaction::{FileScopeMutation, FileScopeUpdate},
    },
    session::{PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

#[allow(clippy::too_many_arguments)]
pub(super) async fn decide(
    db: &DatabaseConnection,
    owner: i32,
    schedule_id: &str,
    run_id: &str,
    directory_request_id: &str,
    expected_scope_revision: u64,
    approve: Option<bool>,
    client_request_key: &str,
) -> Result<(), ScheduleStoreError> {
    let task = ScheduleStore::new(db.clone())
        .read(owner, schedule_id)
        .await?;
    if task.kind != "fresh_task" {
        return Err(ScheduleStoreError::Invalid);
    }
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .filter(run::Column::ScheduleId.eq(schedule_id))
        .filter(run::Column::OwnerUserId.eq(owner))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&work.run_id))
        .one(db)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.actor_id != owner.to_string()
        || session.device_id != task.target_device_id
        || session.conversation_id != work.run_id
        || session.current_request_id.as_deref() != Some(work.run_id.as_str())
    {
        return Err(ScheduleStoreError::NotFound);
    }
    let update = FileScopeUpdate {
        subject: FileScopeSubject {
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            conversation_id: session.conversation_id.clone(),
        },
        client_conversation_id: session
            .client_conversation_id
            .clone()
            .ok_or(ScheduleStoreError::Conflict)?,
        client_request_id: client_request_key.into(),
        expected_revision: expected_scope_revision,
        mutation: match approve {
            Some(approve) => FileScopeMutation::Decide {
                directory_request_id: directory_request_id.into(),
                approve,
            },
            None => FileScopeMutation::Revoke {
                directory_request_id: directory_request_id.into(),
            },
        },
    };
    // The receipt transaction repeats subject and live task checks before mutation.
    // Its immutable request key permits history replay without issuing authority.
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(session.client_conversation_id.clone(), session.surface)
        .update_file_scope(&update, chrono::Utc::now())
        .await
        .map_err(|_| ScheduleStoreError::Conflict)?;
    Ok(())
}
