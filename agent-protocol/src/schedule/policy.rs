//! Server-owned scheduling limits. Every run uses the limits pinned when it is claimed.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

/// Per-run and per-day limits for scheduled tasks.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskBudget {
    pub max_runs_per_utc_day: u32,
    pub max_calls_per_run: u32,
    pub max_model_tokens_per_run: u64,
    pub max_runtime_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleBudgetPolicy {
    pub schema_version: u16,
    pub revision: u64,
    pub maximum: TaskBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateScheduleBudgetPolicy {
    pub expected_revision: u64,
    pub maximum: TaskBudget,
}
