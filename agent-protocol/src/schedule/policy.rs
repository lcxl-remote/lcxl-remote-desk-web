//! Server-owned scheduling limits, separate from owner-approved task contracts.
use super::contract::TaskBudget;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
