//! Persistent assistant provenance. Source identifiers never confer read authority.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::data_lineage::DestinationIdentity;

pub const MAX_ASSISTANT_SOURCE_ID_BYTES: usize = 256;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AssistantSourceKind {
    Input,
    AssistantMessage,
    Receipt,
    NativeStatus,
    PreviousMemory,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceRefRole {
    Background,
    Evidence,
    PreviousMemory,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantSourceRef {
    pub kind: AssistantSourceKind,
    pub id: String,
    pub revision: Option<u64>,
    pub role: SourceRefRole,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantSourceDevice {
    pub device_ref: String,
    pub recorded_name: Option<String>,
    pub current_name: Option<String>,
    pub historical: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantSourceLocator {
    pub conversation_id: String,
    pub root_conversation_id: String,
    pub message_id: Option<String>,
    pub input_event_id: Option<String>,
    pub action_id: Option<String>,
    pub result_id: Option<String>,
    pub content_sha256: String,
    pub fragment_index: u32,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceReadPurpose {
    OwnerHistory,
    ModelHistory,
    MemoryEvidence,
    ModelMemory,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceAvailability {
    Available,
    EvidenceOnly,
    Partial,
    Redacted,
    Unavailable,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantSourceReadRequest {
    pub source_id: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantSourceResolution {
    pub source_id: String,
    pub revision: u64,
    pub availability: SourceAvailability,
    pub locator: Option<AssistantSourceLocator>,
    pub source_device: Option<AssistantSourceDevice>,
    pub content: Option<String>,
    pub recorded_at_unix_ms: Option<u64>,
    pub statement_timezone: Option<String>,
    pub notice: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantCitation {
    pub answer_message_id: String,
    pub answer_revision: u64,
    pub source_id: String,
    pub source_revision: u64,
    pub source_device: Option<AssistantSourceDevice>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourcePolicyClass {
    UserText,
    AssistantText,
    StructuredObservation,
    CommunicationReceipt,
    VisibleProviderText,
    Media,
    ProviderPrivate,
    Unknown,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SourceUsePolicy {
    pub policy_revision: u64,
    pub policy_sha256: String,
    pub class: SourcePolicyClass,
    pub history_retention_allowed: bool,
    pub independent_fact_allowed: bool,
    pub derived_visible_text_allowed: bool,
    pub no_copy: bool,
    pub no_export: bool,
    pub content_hard_expires_at_unix_ms: Option<u64>,
    pub reference_fresh_until_unix_ms: Option<u64>,
    pub fixed_destinations: Vec<DestinationIdentity>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct HistoryDependency {
    pub source_id: String,
    pub source_revision: u64,
    pub content_sha256: String,
    pub origin_id: String,
    pub origin_deletion_generation: u64,
    pub policy_revision: u64,
    pub expires_at_unix_ms: Option<u64>,
}

pub fn validate_source_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty()
        || value.len() > MAX_ASSISTANT_SOURCE_ID_BYTES
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("invalid assistant source identifier");
    }
    Ok(())
}

/// An empty device reference denotes account-owned data with no device binding.
pub fn validate_optional_device_ref(value: &str) -> Result<(), &'static str> {
    if !value.is_empty() {
        validate_source_id(value)?;
    }
    Ok(())
}
