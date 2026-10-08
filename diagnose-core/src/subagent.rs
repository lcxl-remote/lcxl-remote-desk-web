//! Trusted one-level delegation contracts shared by both central runtimes.
//!
//! A task's lifetime is independent of an individual session turn and its history.
//! Stores own transactions, leases, identifiers and authorization; this module owns
//! the common role, fencing, completion and control rules.

pub mod budget;
pub mod control;
pub mod creation;
pub mod facts;
pub mod group;
pub mod notification;
pub mod policy;
pub mod projection;
pub mod report;
pub mod reservation;
pub mod result;
pub mod role;
pub mod runtime;
pub mod seam;
pub mod state;
pub mod tools;
pub mod wait;

#[cfg(test)]
mod tests;

pub use desk_agent_protocol::ai_assistant::subagent::{
    SubAgentState, SubAgentWaitReason, TaskAssessment, TaskFinalReport,
};
pub use role::{AgentRole, DelegatedTaskBinding, DelegationSource};

/// Absolute structural bound, not the configured admission limit.
pub const SUBAGENT_ROOT_CAPACITY: usize = policy::MAX_UNFINISHED_PER_ROOT as usize;
pub const UNFINISHED_SUBAGENT_CAPACITY: &str = "delegation_unfinished_capacity";
/// Transport batch bounds do not limit cumulative child creation.
pub const MAX_SUBAGENT_WAIT_TASKS: usize = 128;
pub const MAX_SUBAGENT_NOTIFICATION_EVENTS: usize = 128;
pub const MAX_SUBAGENT_NAME_CHARS: usize = 128;
pub const MAX_DELEGATED_TASK_CHARS: usize = 8 * 1024;
pub const MAX_ACCEPTANCE_CRITERIA: usize = 16;
pub const MAX_ACCEPTANCE_CRITERION_CHARS: usize = 1024;
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 128 * 1024;

pub fn capacity_storage_message(limit: u32) -> String {
    format!("{UNFINISHED_SUBAGENT_CAPACITY}:{limit}")
}

/// Recognize only our bounded numeric admission errors, never arbitrary DB text.
pub fn capacity_error(message: &str) -> Option<desk_agent_protocol::AgentError> {
    let (kind, raw_limit) = message.split_once(':')?;
    let limit: u32 = raw_limit.parse().ok()?;
    match kind {
        UNFINISHED_SUBAGENT_CAPACITY if (1..=policy::MAX_UNFINISHED_PER_ROOT).contains(&limit) => {
            Some(invalid(format!(
                "This conversation has reached its configured limit of {limit} unfinished subagents, including tasks awaiting approval. Wait for one to finish or explicitly stop one before creating another."
            )))
        }
        _ => None,
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}

pub fn invalid(message: impl Into<String>) -> desk_agent_protocol::AgentError {
    desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
