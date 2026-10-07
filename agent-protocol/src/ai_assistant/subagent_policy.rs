//! Central settings for one-level task creation; not a permission grant.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubAgentLimits {
    /// Includes approval and native-work waits, across all groups in a root chat.
    pub max_unfinished_per_root: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubAgentPolicy {
    pub schema_version: u16,
    pub revision: u64,
    pub limits: SubAgentLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateSubAgentPolicy {
    pub expected_revision: u64,
    pub limits: SubAgentLimits,
}
