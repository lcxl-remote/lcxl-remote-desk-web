//! Shared central terminal-completion settings API.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCompletionDto {
    pub revision: u64,
    pub max_output_tokens: u32,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateTerminalCompletionRequest {
    pub expected_revision: u64,
    pub max_output_tokens: u32,
}
