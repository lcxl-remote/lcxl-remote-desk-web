//! Verify the original directory proposal without granting or resolving a path.
use super::ScheduleStoreError;
use crate::entity::agent_run_event;
use desk_diagnose_core::{
    file_scope::transaction::{self, FileScopeMutation, FileScopeReceipt},
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter};

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

/// Recover only already-committed directory control results, never an operation.
pub(super) async fn restore_results(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
) -> Result<(), ScheduleStoreError> {
    let recovered = desk_diagnose_core::schedule::directory_recovery::missing_results(session)
        .map_err(|_| ScheduleStoreError::Conflict)?;
    for (request_id, _) in &recovered {
        verify(txn, session, request_id).await?;
    }
    session
        .conversation
        .extend(recovered.into_iter().map(|(_, message)| message));
    Ok(())
}

/// Directory confirmation is control evidence, never observed file execution.
pub(super) async fn verified_control_calls(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<std::collections::BTreeSet<String>, ScheduleStoreError> {
    let requests =
        desk_diagnose_core::schedule::rehearsal::sources::directory_control_requests(session)
            .map_err(|_| ScheduleStoreError::Conflict)?;
    let mut calls = std::collections::BTreeSet::new();
    for (call, request) in requests {
        verify(txn, session, &request).await?;
        if !calls.insert(call) {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    let permissions =
        desk_diagnose_core::schedule::rehearsal::permission_controls::collect(session)
            .map_err(|_| ScheduleStoreError::Conflict)?;
    for (call, request) in permissions.calls {
        crate::agent_session_store::permission_resume::verify_control_request_on(
            txn, session, &request,
        )
        .await
        .map_err(|_| ScheduleStoreError::Conflict)?;
        if !calls.insert(call) {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    Ok(calls)
}
