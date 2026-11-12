//! Live owner/device readiness before a task is enabled again.
use super::{ScheduleStoreError, entity};
use crate::config::connection::DatabaseTransaction;
use crate::{ai_assistant_gate::AiAssistantGate, control_authorizer::SINGLE_ACCOUNT_USER_ID};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};

#[async_trait::async_trait]
pub trait TaskSubjectVerifier: Send + Sync {
    /// Check the current owner/device before any task or session write fence.
    async fn lock_subject(
        &self,
        txn: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError>;
}

/// The assistant is enabled and the task's device has exactly one live connection.
pub struct SignalTaskSubjectVerifier<'a> {
    pub connections: &'a SharedConnectionMap,
    pub gate: &'a AiAssistantGate,
}

#[async_trait::async_trait]
impl TaskSubjectVerifier for SignalTaskSubjectVerifier<'_> {
    async fn lock_subject(
        &self,
        _: &DatabaseTransaction,
        task: &entity::Model,
    ) -> Result<(), ScheduleStoreError> {
        if task.owner_user_id != SINGLE_ACCOUNT_USER_ID || !self.gate.is_enabled() {
            return Err(ScheduleStoreError::NotFound);
        }
        let map = self.connections.read().await;
        let mut matches = map.values().filter(|peer| {
            peer.auth_context.auth_kind == AuthKind::TokenAuth
                && peer.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
                && peer.client_id.as_deref() == Some(task.target_device_id.as_str())
        });
        matches.next().ok_or(ScheduleStoreError::NotFound)?;
        if matches.next().is_some() {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(())
    }
}
