//! Transactional host seam; no borrowed parent loop is used as a child runner.

use async_trait::async_trait;
use desk_agent_protocol::AgentError;
use serde::{Deserialize, Serialize};

use super::{
    state::{CompletionDisposition, PlanningFence},
    tools::Operation,
};
use crate::{
    chat::{ChatMessage, ToolCall},
    session::PersistedAgentSession,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedResult {
    pub task_id: String,
    pub state_revision: u64,
    pub tool_call_id: String,
    pub result_message_id: String,
    pub parent_input_revision: u64,
    pub parent_control_revision: u64,
}

impl ObservedResult {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !super::valid_id(&self.task_id)
            || !super::valid_id(&self.tool_call_id)
            || !super::valid_id(&self.result_message_id)
            || self.state_revision == 0
            || self.parent_input_revision == 0
            || self.parent_control_revision == 0
        {
            return Err("invalid delegated result observation");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedResultObservation {
    pub result: ObservedResult,
    pub response_message_id: String,
}

impl AcceptedResultObservation {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.result.validate()?;
        if !super::valid_id(&self.response_message_id) {
            return Err("invalid model observation response");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ToolReceipt {
    pub payload: serde_json::Value,
    pub result_message_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildAdmission {
    Admitted,
    SourcePaused,
}

#[async_trait(?Send)]
pub trait SubAgentSeam {
    /// Read one bounded trusted status snapshot before context budgeting. The same
    /// frozen snapshot is used throughout construction of this model request.
    async fn planning_projection(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Option<ChatMessage>, AgentError>;

    /// The immutable owner evidence and current task data are recovered from
    /// durable creation records, never promoted from a compressed transcript.
    async fn child_creation_context(
        &self,
        _session: &PersistedAgentSession,
    ) -> Result<super::creation::TaskCreationEnvelope, AgentError> {
        Err(super::invalid(
            "durable child creation context is unavailable",
        ))
    }

    /// Transactions bind root/actor/device, role, source and input/control fences.
    /// Creation uses the stable tool-call ID and argument digest for idempotency.
    /// Commit the labelled tool result and updated session together with creation,
    /// controls or wait check/registration. The held session advances only after
    /// commit. Reading a result stages observation; it does not consume the inbox.
    async fn execute(
        &self,
        session: &mut PersistedAgentSession,
        call: &ToolCall,
        operation: Operation,
        result_message_id: &str,
    ) -> Result<ToolReceipt, AgentError>;

    /// Read-only preflight improves recovery from a premature goal-complete
    /// proposal. Goal settlement repeats the authoritative check atomically.
    async fn required_children_complete(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<bool, AgentError>;

    /// Reload source/task controls immediately before model planning or dispatch.
    async fn validate_child_admission(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<ChildAdmission, AgentError>;

    /// Check bounds and current runtime facts before committing the answer. No
    /// model classification or report-format repair is involved. Settlement
    /// repeats these checks in its authoritative transaction.
    async fn evaluate_child_answer(
        &self,
        session: &PersistedAgentSession,
        expected: PlanningFence,
        answer: &str,
    ) -> Result<CompletionDisposition, AgentError>;

    /// Record a normal text candidate using the existing child session lease. The store
    /// validates current dependency/action facts and writes terminal inbox together.
    async fn settle_child_answer(
        &self,
        session: &mut PersistedAgentSession,
        expected: PlanningFence,
        answer: String,
    ) -> Result<CompletionDisposition, AgentError>;

    /// Release a non-report turn without confusing idle with task completion.
    /// Only a validated exact-command interpretation may return to the original
    /// finite task without another dependency; failed planning remains terminal.
    async fn settle_child_turn(
        &self,
        session: &mut PersistedAgentSession,
        failure_reason: Option<&str>,
        allow_continue: bool,
    ) -> Result<(), AgentError>;
}
