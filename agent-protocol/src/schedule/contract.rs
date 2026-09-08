//! Immutable owner-reviewed task scope. This document is not an execution grant.
use crate::{
    capability_grant::{CapabilityGrantLimits, CapabilityRiskTier},
    capability_provider::CapabilityEffect,
    communication::{
        CommunicationChannel, CommunicationSurfaceKind, CommunicationSurfaceScope,
        RecipientIdentity,
    },
    data_lineage::DestinationIdentity,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const TASK_CONTRACT_SCHEMA_VERSION: u16 = 1;
pub const MAX_TASK_CONTRACT_BYTES: usize = 256 * 1024;
pub const MAX_TASK_PERMISSION_RULES: usize = 64;
pub const MAX_TASK_FIXED_STEPS: usize = 32;

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskContract {
    pub schema_version: u16,
    pub schedule_id: String,
    pub task_revision: u64,
    pub contract_revision: u64,
    pub target_device_id: String,
    pub prompt_sha256: String,
    pub permissions: Vec<TaskPermissionRule>,
    /// Steps are stored in execution order; dependencies can only reference earlier steps.
    pub steps: Vec<TaskFixedStep>,
    pub exception_mode: TaskExceptionMode,
    pub budget: TaskBudget,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskExceptionMode {
    Deny,
    RequestApproval,
}

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

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskPermissionScope {
    pub resources: Vec<String>,
    pub operations: Vec<String>,
    pub export_destinations: Vec<DestinationIdentity>,
    pub limits: CapabilityGrantLimits,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskPermissionRule {
    pub rule_id: String,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub tool_schema_version: u16,
    pub effect: CapabilityEffect,
    pub risk_tier: CapabilityRiskTier,
    pub input: TaskInputConstraint,
    pub automatic: TaskPermissionScope,
    pub approval_ceiling: TaskPermissionScope,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskInputConstraint {
    /// Complete canonical object, not a command prefix or a regular expression.
    Exact { canonical_json: String },
    /// Only a provider's validated read input and resolved stable scopes qualify.
    ScopedRead,
    /// Create a new UTF-8 artifact within a separately verified directory scope.
    GeneratedTextArtifact {
        file_name: String,
        max_content_bytes: u32,
    },
    /// Model output has only subject/body; destination comes from the fixed step.
    GeneratedMessage {
        max_subject_bytes: u32,
        max_body_bytes: u32,
        /// None forbids attachments; populated limits never bypass source or destination checks.
        attachment_policy: Option<TaskAttachmentPolicy>,
    },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskFixedStep {
    pub step_id: String,
    pub rule_id: String,
    pub depends_on: Vec<String>,
    pub binding: TaskStepBinding,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskStepBinding {
    Exact,
    ProduceTextArtifact {
        canonical_directory: String,
        allowed_source_scopes: Vec<String>,
    },
    SendMessage {
        destination: TaskMessageDestination,
        allowed_source_scopes: Vec<String>,
    },
}

/// Stable destination identity. Per-run session/readiness and UI references are resolved afresh.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskMessageDestination {
    pub channel: CommunicationChannel,
    pub surface_kind: CommunicationSurfaceKind,
    pub scope: CommunicationSurfaceScope,
    pub adapter_id: String,
    pub adapter_version: String,
    pub profile_id: String,
    pub account_id: String,
    pub recipients: Vec<RecipientIdentity>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskGeneratedMessage {
    pub subject: String,
    pub body: String,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepStatus {
    Pending,
    Prepared,
    Running,
    AwaitingApproval,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

/// Attachment quotas are additional to resource, lineage and destination authority.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskAttachmentLimits {
    pub max_count: u32,
    pub max_bytes_per_attachment: u64,
    pub max_total_bytes: u64,
    /// Exact normalized media types; wildcard matching is not supported.
    pub media_types: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskAttachmentPolicy {
    pub automatic: TaskAttachmentLimits,
    pub approval_ceiling: TaskAttachmentLimits,
}

/// Only generated content; per-run directory selections are resolved by the server.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TaskGeneratedTextArtifact {
    pub file_name: String,
    pub content_utf8: String,
}
