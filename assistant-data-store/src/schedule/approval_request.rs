//! Live task ceiling checks for proposals; no decision or grant is manufactured.
use super::{
    ScheduleStoreError,
    approval_admission::lock_fresh_approval_on,
    continuation_wait::lock_approval_on,
    resume_admission::{fresh_action_authority_on, lock_action_session_row},
    session::ScheduleHostStores,
};
use desk_diagnose_core::{
    dynamic_run::PermissionRequest,
    session::{PersistedAgentSession, TriggerOrigin},
};
use sea_orm::ConnectionTrait;

/// The caller has authorized the subject and locks task before session.
pub async fn validate_task_permission_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    request: &PermissionRequest,
) -> Result<(), ScheduleStoreError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Ok(());
    }
    let row = lock_action_session_row(host, txn, &session.conversation_id)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if row.version != session.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || row.actor_id != session.actor_id
        || row.device_id != session.device_id
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let current = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    fresh_action_authority_on(txn, &current).await?;
    desk_diagnose_core::schedule::run_approval::validate_request(session, request)
        .map_err(|_| ScheduleStoreError::Invalid)
}

/// Directory confirmation is a prerequisite only, never a file capability grant.
/// Called in the same transaction as the file-scope receipt and session update.
pub async fn validate_task_directory_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    session: &PersistedAgentSession,
    mutation: &desk_diagnose_core::file_scope::transaction::FileScopeMutation,
) -> Result<(), ScheduleStoreError> {
    use desk_diagnose_core::file_scope::transaction::FileScopeMutation;
    if session.trigger_origin == TriggerOrigin::ScheduledContinuation {
        if let FileScopeMutation::Decide {
            directory_request_id,
            approve: true,
        } = mutation
        {
            lock_approval_on(host, txn, session, directory_request_id).await?;
        }
        return Ok(());
    }
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Ok(());
    }
    match mutation {
        FileScopeMutation::Revoke { .. } | FileScopeMutation::Decide { approve: false, .. } => {
            Ok(())
        }
        FileScopeMutation::Propose { proposal } => {
            if proposal.source
                != desk_diagnose_core::file_scope::DirectoryConsentSource::ModelProposal
            {
                return Err(ScheduleStoreError::Invalid);
            }
            let row = lock_action_session_row(host, txn, &session.conversation_id)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            let current = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            if current != *session {
                return Err(ScheduleStoreError::Conflict);
            }
            fresh_action_authority_on(txn, &current).await?;
            Ok(())
        }
        FileScopeMutation::Decide {
            directory_request_id,
            approve: true,
        } => {
            lock_fresh_approval_on(host, txn, &session.conversation_id, directory_request_id)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            Ok(())
        }
        // A scheduled run selects no directory by itself; it proposes one.
        FileScopeMutation::Select { .. } => Err(ScheduleStoreError::Invalid),
    }
}
