//! Preserve a known pre-dispatch budget denial through storage adapters.
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_utils::error::DeskErrorCode;

#[derive(Debug)]
pub enum ModelAdmissionError<E> {
    BudgetExceeded,
    AssistantDataChanged,
    Backend(E),
}

impl<E: std::fmt::Display> std::fmt::Display for ModelAdmissionError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BudgetExceeded => f.write_str("model budget exceeded"),
            Self::AssistantDataChanged => f.write_str("assistant data access changed"),
            Self::Backend(error) => std::fmt::Display::fmt(error, f),
        }
    }
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
            Self::AssistantDataChanged => assistant_data_changed_error(),
            Self::Backend(error) => fallback(error),
        }
    }
}

pub fn assistant_data_changed_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::RiskBlocked,
        message: "Assistant data access changed. Retry with the current settings.".into(),
        retryable: false,
        safe_for_model: false,
        error_code: Some(DeskErrorCode::AI_ASSISTANT_DATA_CHANGED.code()),
    }
}

/// The deadline belongs to this physical call, never to a mutable seam instance.
pub fn check_assistant_data_deadline(
    deadline: Option<u64>,
    now_unix_ms: i64,
) -> Result<(), AgentError> {
    if deadline.is_some_and(|until| {
        now_unix_ms < 0 || until == 0 || until > i64::MAX as u64 || until <= now_unix_ms as u64
    }) {
        return Err(assistant_data_changed_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_changes_are_distinct_from_budget_or_transport_failures() {
        let error = ModelAdmissionError::<()>::AssistantDataChanged
            .into_agent_error(|_| panic!("must not become a transport error"));
        assert_eq!(
            error.error_code,
            Some(DeskErrorCode::AI_ASSISTANT_DATA_CHANGED.code())
        );
        assert!(!error.retryable && !error.safe_for_model);
    }

    #[test]
    fn a_physical_send_cannot_outlive_its_own_source_task_or_session_deadline() {
        assert!(check_assistant_data_deadline(None, 100).is_ok());
        assert!(check_assistant_data_deadline(Some(101), 100).is_ok());
        for (until, now) in [(100, 100), (99, 100), (0, 100), (101, -1), (u64::MAX, 100)] {
            assert_eq!(
                check_assistant_data_deadline(Some(until), now)
                    .unwrap_err()
                    .error_code,
                Some(DeskErrorCode::AI_ASSISTANT_DATA_CHANGED.code())
            );
        }
    }
}
