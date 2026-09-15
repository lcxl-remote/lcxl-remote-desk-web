//! Preserve a known pre-dispatch budget denial through storage adapters.
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_utils::error::DeskErrorCode;

#[derive(Debug)]
pub enum ModelAdmissionError<E> {
    BudgetExceeded,
    Backend(E),
}

impl<E> From<E> for ModelAdmissionError<E> {
    fn from(error: E) -> Self {
        Self::Backend(error)
    }
}

impl<E> ModelAdmissionError<E> {
    pub fn into_agent_error(self, fallback: impl FnOnce(E) -> AgentError) -> AgentError {
        match self {
            Self::BudgetExceeded => AgentError {
                kind: AgentErrorKind::RiskBlocked,
                message: "The scheduled task's remaining model-token budget is insufficient. Review the task budget before running it again.".into(),
                retryable: false,
                safe_for_model: true,
                error_code: Some(DeskErrorCode::SCHEDULE_MODEL_BUDGET_EXCEEDED.code()),
            },
            Self::Backend(error) => fallback(error),
        }
    }
}
