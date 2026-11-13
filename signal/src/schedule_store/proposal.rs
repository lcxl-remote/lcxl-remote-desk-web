//! Commit an AI draft and its original tool-result receipt under one turn fence.
use super::{ScheduleStore, ScheduleStoreError};
use desk_diagnose_core::{
    chat::ToolCall,
    model_observability::{PermissionOutcome, tool::ToolObservation},
    session::PersistedAgentSession,
};

impl ScheduleStore {
    pub(crate) async fn manage_from_session(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        observation: &ToolObservation,
    ) -> Result<String, ScheduleStoreError> {
        let owner: i32 = session
            .actor_id
            .parse()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let action =
            desk_diagnose_core::schedule::management_tools::parse(session, call, observation)
                .map_err(|message| match message {
                    "schedule management is unavailable" | "schedule proposal is unavailable" => {
                        ScheduleStoreError::Invalid
                    }
                    message => ScheduleStoreError::ToolInput(message),
                })?;
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        if owner != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID {
            return Err(ScheduleStoreError::NotFound);
        }
        let (receipt, next) = desk_assistant_data_store::schedule::manage_on(
            &super::SignalScheduleHost,
            &txn,
            owner,
            session,
            call,
            action,
            observation,
        )
        .await?;
        txn.commit().await?;
        *session = next;
        if session.pending_schedule_review.is_some() {
            observation.permission(PermissionOutcome::Waiting);
        }
        Ok(receipt)
    }
}
