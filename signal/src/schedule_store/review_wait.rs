//! Wait one bounded tick for the owner decision without holding a DB transaction.
use super::{ScheduleStore, ScheduleStoreError};
use desk_assistant_data_store::schedule::ReviewPoll;
use desk_diagnose_core::{
    model_observability::tool::ToolObservation, session::PersistedAgentSession,
};

impl ScheduleStore {
    pub(crate) async fn poll_review_decision(
        &self,
        session: &mut PersistedAgentSession,
        schedule_id: &str,
        observation: &ToolObservation,
    ) -> Result<bool, ScheduleStoreError> {
        match desk_assistant_data_store::schedule::poll_review_decision_on(
            &super::session_host::SignalScheduleHost,
            &self.db,
            session,
            schedule_id,
            observation,
        )
        .await?
        {
            ReviewPoll::Decided => Ok(true),
            ReviewPoll::Unchanged => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                Ok(false)
            }
        }
    }
}
