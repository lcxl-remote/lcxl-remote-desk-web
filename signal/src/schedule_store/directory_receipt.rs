//! Verify the original directory proposal without granting or resolving a path.
use super::ScheduleStoreError;
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_run_event;
use desk_diagnose_core::{
    file_scope::transaction::{self, FileScopeMutation, FileScopeReceipt},
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

pub(super) async fn verify(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request_id: &str,
) -> Result<(), ScheduleStoreError> {
    let subject = session
        .file_scope_subject(
            &session.actor_id,
            &session.device_id,
            &session.conversation_id,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let stored = agent_run_event::Entity::find()
        .filter(
            agent_run_event::Column::EventId
                .eq(transaction::receipt_event_id(&subject, request_id)),
        )
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let receipt: FileScopeReceipt =
        serde_json::from_str(&stored.payload_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if stored.kind != transaction::FILE_SCOPE_EVENT_KIND
        || stored.payload_schema_version != 1
        || stored.run_id != session.conversation_id
        || stored.event_id != receipt.event_id
        || i64::try_from(receipt.event_seq).ok() != Some(stored.event_seq)
        || receipt.update.subject != subject
        || receipt.update.client_request_id != request_id
        || stored.actor_id.as_deref() != Some(session.actor_id.as_str())
        || stored.correlation_id.as_deref() != Some(request_id)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    transaction::replay(session, &receipt.update, &receipt)
        .map_err(|_| ScheduleStoreError::Conflict)?;
    let proposal = match &receipt.update.mutation {
        FileScopeMutation::Propose { proposal } | FileScopeMutation::Select { proposal } => {
            proposal
        }
        _ => return Err(ScheduleStoreError::Conflict),
    };
    if !session
        .file_scope
        .records()
        .iter()
        .any(|record| record.proposal == *proposal)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(())
}
