//! Live task ceiling checks for proposals; no decision or grant is manufactured.
use super::{ScheduleStore, ScheduleStoreError};
use desk_diagnose_core::{
    dynamic_run::PermissionRequest,
    session::{PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{DatabaseTransaction, TransactionTrait};

impl ScheduleStore {
    pub async fn validate_task_permission_request(
        &self,
        session: &PersistedAgentSession,
        request: &PermissionRequest,
    ) -> Result<(), ScheduleStoreError> {
        if session.trigger_origin != TriggerOrigin::ScheduledTask {
            return Ok(());
        }
        let txn = self.db.begin().await?;

        validate_task_permission_on(&txn, session, request).await?;
        txn.commit().await?;
        Ok(())
    }
}

/// The caller has authorized the subject and locks task before session. Reused
/// during publication so a contract revocation cannot race a successful precheck.
pub(crate) async fn validate_task_permission_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &PermissionRequest,
) -> Result<(), ScheduleStoreError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Ok(());
    }
    let row = super::lock_action_session(txn, &session.conversation_id)
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
    let authority = super::fresh_action_authority_on(txn, &current).await?;
    let provider_device_id = session.device_id.clone();
    desk_diagnose_core::schedule::contract::exception::validate_request(
        authority.contract(),
        session,
        request,
        &provider_device_id,
    )
    .map_err(|_| ScheduleStoreError::Invalid)
}

/// Directory confirmation is a prerequisite only, never a file capability grant.
/// Called in the same transaction as the file-scope receipt and session update.
pub(crate) async fn validate_task_directory_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    mutation: &desk_diagnose_core::file_scope::transaction::FileScopeMutation,
) -> Result<(), ScheduleStoreError> {
    use desk_diagnose_core::file_scope::transaction::FileScopeMutation;
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
            let row = super::lock_action_session(txn, &session.conversation_id)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            let current = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            if current != *session {
                return Err(ScheduleStoreError::Conflict);
            }
            let authority = super::fresh_action_authority_on(txn, &current).await?;
            if authority.contract().contract().exception_mode
                != desk_agent_protocol::schedule::contract::TaskExceptionMode::RequestApproval
            {
                return Err(ScheduleStoreError::Invalid);
            }
            Ok(())
        }
        FileScopeMutation::Decide {
            directory_request_id,
            approve: true,
        } => {
            super::lock_fresh_approval_on(txn, &session.conversation_id, directory_request_id)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            Ok(())
        }
        FileScopeMutation::Select { proposal } => {
            if proposal.source
                != desk_diagnose_core::file_scope::DirectoryConsentSource::TaskContract
                || !task_directory_resolution_matches_on(txn, session, proposal).await?
            {
                return Err(ScheduleStoreError::Invalid);
            }
            Ok(())
        }
    }
}

/// Called only after owner/task/session fencing by the runtime directory writer.
/// The returned match is usable only within this same database transaction.
pub(crate) async fn task_directory_resolution_matches_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    proposal: &desk_diagnose_core::file_scope::DirectoryProposal,
) -> Result<bool, ScheduleStoreError> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask {
        return Ok(false);
    }
    let row = super::lock_action_session(txn, &session.conversation_id)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let current = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if current != *session {
        return Err(ScheduleStoreError::Conflict);
    }
    let authority = super::fresh_action_authority_on(txn, &current).await?;
    let now = u64::try_from(authority.verified_at()).map_err(|_| ScheduleStoreError::Invalid)?;
    Ok(
        desk_diagnose_core::schedule::contract::artifact::permits_directory_resolution(
            authority.contract(),
            &current,
            proposal,
            now,
        ),
    )
}
