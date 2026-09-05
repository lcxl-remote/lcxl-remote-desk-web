//! Shared central context-management API contract.
use desk_diagnose_core::model_context::ContextManagementStrategy;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextManagementDto {
    pub revision: u64,
    pub strategy: ContextManagementStrategyDto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextManagementStrategyDto {
    Window,
    CheckpointSummary,
}

impl From<ContextManagementStrategy> for ContextManagementStrategyDto {
    fn from(value: ContextManagementStrategy) -> Self {
        match value {
            ContextManagementStrategy::Window => Self::Window,
            ContextManagementStrategy::CheckpointSummary => Self::CheckpointSummary,
        }
    }
}

impl From<ContextManagementStrategyDto> for ContextManagementStrategy {
    fn from(value: ContextManagementStrategyDto) -> Self {
        match value {
            ContextManagementStrategyDto::Window => Self::Window,
            ContextManagementStrategyDto::CheckpointSummary => Self::CheckpointSummary,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateContextManagementRequest {
    pub expected_revision: u64,
    pub strategy: ContextManagementStrategyDto,
}
