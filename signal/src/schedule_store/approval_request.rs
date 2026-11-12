//! Live task ceiling checks for proposals; no decision or grant is manufactured.
use super::{ScheduleStore, ScheduleStoreError};
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::{
    dynamic_run::PermissionRequest,
    session::{PersistedAgentSession, TriggerOrigin},
};

impl ScheduleStore {
    pub async fn validate_task_permission_request(
        &self,
        session: &PersistedAgentSession,
        request: &PermissionRequest,
    ) -> Result<(), ScheduleStoreError> {
        if session.trigger_origin != TriggerOrigin::ScheduledTask {
            return Ok(());
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;

        validate_task_permission_on(&txn, session, request).await?;
        txn.commit().await?;
        Ok(())
    }
}

/// The caller has authorized the subject and locks task before session.
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
    super::fresh_action_authority_on(txn, &current).await?;
    desk_diagnose_core::schedule::run_approval::validate_request(session, request)
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
    if session.trigger_origin == TriggerOrigin::ScheduledContinuation {
        if let FileScopeMutation::Decide {
            directory_request_id,
            approve: true,
        } = mutation
        {
            super::continuation_wait::lock_approval_on(txn, session, directory_request_id).await?;
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
            let row = super::lock_action_session(txn, &session.conversation_id)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            let current = PersistedAgentSession::decode_json(&row.state_json)
                .map_err(|_| ScheduleStoreError::Invalid)?;
            if current != *session {
                return Err(ScheduleStoreError::Conflict);
            }
            super::fresh_action_authority_on(txn, &current).await?;
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
        // A scheduled run selects no directory by itself; it proposes one.
        FileScopeMutation::Select { .. } => Err(ScheduleStoreError::Invalid),
    }
}
