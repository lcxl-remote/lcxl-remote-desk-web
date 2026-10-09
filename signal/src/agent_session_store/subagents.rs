//! The durable child runtime shares normal business sessions and their lease.
use super::*;
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall},
    subagent::{
        seam::{ChildAdmission, SubAgentSeam, ToolReceipt},
        state::{CompletionDisposition, PlanningFence},
        tools::Operation,
    },
};

fn delegation_storage(error: sea_orm::DbErr) -> AgentError {
    if let sea_orm::DbErr::Custom(message) = &error {
        if let Some(capacity) = desk_diagnose_core::subagent::capacity_error(message) {
            return capacity;
        }
    }
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "delegated task control or storage is unavailable".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_refusal_is_actionable_without_exposing_storage_errors() {
        let capacity = delegation_storage(sea_orm::DbErr::Custom(
            desk_diagnose_core::subagent::capacity_storage_message(8),
        ));
        assert_eq!(capacity.kind, AgentErrorKind::InvalidInput);
        assert!(capacity.safe_for_model);
        assert!(!capacity.retryable);
        assert!(capacity.message.contains("8 unfinished subagents"));
        assert!(capacity.message.contains("awaiting approval"));
        let storage = delegation_storage(sea_orm::DbErr::Custom("private storage detail".into()));
        assert_eq!(storage.kind, AgentErrorKind::Internal);
        assert!(!storage.safe_for_model);
        assert!(!storage.message.contains("private storage detail"));
    }
}

#[async_trait(?Send)]
impl SubAgentSeam for SignalAgentSessionStore {
    async fn planning_projection(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Option<ChatMessage>, AgentError> {
        let store = crate::agent_subagent_store::SubAgentStore::new(self.db.clone());
        if session.agent_role.is_main() {
            store.main_projection_for_turn(session).await
        } else {
            store.child_projection_for_turn(session).await
        }
        .map(Some)
        .map_err(delegation_storage)
    }
    async fn child_creation_context(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<desk_diagnose_core::subagent::creation::TaskCreationEnvelope, AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .child_creation_context(session)
            .await
            .map_err(delegation_storage)?
            .ok_or_else(|| desk_diagnose_core::subagent::invalid("child source is unavailable"))
    }
    async fn execute(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        operation: Operation,
        result_message_id: &str,
        observation: &desk_diagnose_core::model_observability::tool::ToolObservation,
    ) -> Result<ToolReceipt, AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .execute_main_tool_observed(session, call, operation, result_message_id, observation)
            .await
            .map_err(delegation_storage)
    }
    async fn validate_child_admission(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<ChildAdmission, AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .child_admission(session)
            .await
            .map_err(delegation_storage)
    }
    async fn required_children_complete(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<bool, AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .required_children_complete_for_turn(session)
            .await
            .map_err(delegation_storage)
    }
    async fn evaluate_child_answer(
        &self,
        session: &PersistedAgentSession,
        expected: PlanningFence,
        answer: &str,
    ) -> Result<CompletionDisposition, AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .evaluate_answer_for_turn(session, expected, answer)
            .await
            .map_err(delegation_storage)?
            .map_err(desk_diagnose_core::subagent::invalid)
    }
    async fn settle_child_answer(
        &self,
        session: &mut PersistedAgentSession,
        expected: PlanningFence,
        answer: String,
    ) -> Result<CompletionDisposition, AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .settle_answer_for_turn(session, expected, answer)
            .await
            .map_err(delegation_storage)?
            .map_err(desk_diagnose_core::subagent::invalid)
    }
    async fn settle_child_turn(
        &self,
        session: &mut PersistedAgentSession,
        failure_reason: Option<&str>,
        allow_continue: bool,
    ) -> Result<(), AgentError> {
        crate::agent_subagent_store::SubAgentStore::new(self.db.clone())
            .settle_turn_for_task(session, failure_reason, allow_continue)
            .await
            .map_err(delegation_storage)
    }
}
