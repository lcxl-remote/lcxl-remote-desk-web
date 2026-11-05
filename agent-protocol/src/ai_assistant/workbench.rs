//! Owner-facing identities for personal Super Agent coordinators.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::MainRequestReceipt;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateAiAssistantWorkbench {
    /// Stable client-generated id for retrying this creation.
    pub creation_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantWorkbenchView {
    pub workbench_id: String,
    pub conversation_id: String,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantWorkbenchTasks {
    pub items: Vec<AiAssistantWorkbenchTask>,
    pub next_before: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantWorkbenchTask {
    pub receipt: MainRequestReceipt,
    pub execution_session_id: Option<String>,
    pub updated_at_ms: i64,
    pub issue_number: Option<i64>,
    pub worker_states: Vec<String>,
    pub deliveries: Vec<AiAssistantWorkbenchDelivery>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantWorkbenchDelivery {
    pub artifact_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub updated_at_ms: i64,
    pub comment_id: Option<i64>,
    pub instance: String,
    pub owner: String,
    pub repository: String,
    pub issue_number: u64,
}
