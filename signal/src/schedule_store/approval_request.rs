//! Live task ceiling checks for proposals; no decision or grant is manufactured.
use super::{ScheduleStore, ScheduleStoreError};
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
    txn: &crate::config::connection::DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &PermissionRequest,
) -> Result<(), ScheduleStoreError> {
    desk_assistant_data_store::schedule::validate_task_permission_on(
        &super::SignalScheduleHost,
        txn,
        session,
        request,
    )
    .await
}

/// Directory confirmation is a prerequisite only, never a file capability grant.
pub(crate) async fn validate_task_directory_on(
    txn: &crate::config::connection::DatabaseTransaction,
    session: &PersistedAgentSession,
    mutation: &desk_diagnose_core::file_scope::transaction::FileScopeMutation,
) -> Result<(), ScheduleStoreError> {
    desk_assistant_data_store::schedule::validate_task_directory_on(
        &super::SignalScheduleHost,
        txn,
        session,
        mutation,
    )
    .await
}
