//! Owner settings for the approval mode of an Assistant root conversation.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistantApprovalMode {
    #[default]
    AiReview,
    Manual,
    NoReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateAssistantApprovalMode {
    pub session: String,
    pub expected_revision: u64,
    pub mode: AssistantApprovalMode,
    pub no_review_confirmation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AssistantApprovalModeView {
    pub mode: AssistantApprovalMode,
    pub revision: u64,
    pub ai_review_available: bool,
}
