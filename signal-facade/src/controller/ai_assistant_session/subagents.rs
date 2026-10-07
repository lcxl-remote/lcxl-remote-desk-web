//! The same one-level delegation API shape for OSS Signal and Manager.

pub use desk_agent_protocol::ai_assistant::subagent::{
    AiAssistantStopControl, AiAssistantStopResult, AiAssistantSubAgentControl,
    AiAssistantSubAgentEvent, AiAssistantSubAgentPage, AiAssistantSubAgentResult,
    AiAssistantSubAgentSummary,
};
use serde::Deserialize;
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantSubAgentsQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantSubAgentQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub task_id: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantSubAgentControlBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub control: AiAssistantSubAgentControl,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantStopBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub control: AiAssistantStopControl,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AiAssistantDirectoryControlAction {
    SelectDirectory {
        path: String,
        purpose: String,
        expected_revision: u64,
    },
    DecideDirectory {
        directory_request_id: String,
        expected_revision: u64,
        approve: bool,
    },
    RevokeDirectory {
        directory_request_id: String,
        expected_revision: u64,
    },
}

impl From<AiAssistantDirectoryControlAction>
    for desk_agent_protocol::ai_assistant::AiAssistantObjectContextOperation
{
    fn from(value: AiAssistantDirectoryControlAction) -> Self {
        match value {
            AiAssistantDirectoryControlAction::SelectDirectory {
                path,
                purpose,
                expected_revision,
            } => Self::SelectDirectory {
                path,
                purpose,
                expected_revision,
            },
            AiAssistantDirectoryControlAction::DecideDirectory {
                directory_request_id,
                expected_revision,
                approve,
            } => Self::DecideDirectory {
                directory_request_id,
                expected_revision,
                approve,
            },
            AiAssistantDirectoryControlAction::RevokeDirectory {
                directory_request_id,
                expected_revision,
            } => Self::RevokeDirectory {
                directory_request_id,
                expected_revision,
            },
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantDirectoryControlBody {
    pub connection: String,
    pub session: String,
    pub client_request_id: String,
    pub operation: AiAssistantDirectoryControlAction,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantSubAgentReadBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub task_id: String,
    /// Marks only UI notifications at or before the revision displayed by the owner.
    pub through_state_revision: u64,
}

/// Idempotency is the immutable native request/generation pair, not the latest turn.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AiAssistantCommandCancelBody {
    pub connection: String,
    pub session: String,
    pub exec_request_id: String,
    pub execution_generation: String,
}
