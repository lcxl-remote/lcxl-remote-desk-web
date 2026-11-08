//! Owner-managed personal instructions and independent assistant data settings.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const DEFAULT_HISTORY_RETENTION_DAYS: u32 = 90;
pub const MAX_RETRIEVAL_DOCUMENT_VISITS: u32 = 100_000;

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct UserInstructions {
    pub revision: u64,
    pub content: String,
}

impl Default for UserInstructions {
    fn default() -> Self {
        Self {
            revision: 1,
            content: String::new(),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct UserInstructionsUpdate {
    pub expected_revision: u64,
    pub content: String,
}

impl UserInstructionsUpdate {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.expected_revision == 0
            || self
                .content
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err("invalid personal instructions");
        }
        Ok(())
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantMemoryLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.content.len() > limits.user_instructions_bytes as usize {
            return Err("invalid personal instructions");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataSettings {
    pub revision: u64,
    pub automatic_memory_enabled: bool,
    pub model_history_search_enabled: bool,
    pub explanation_version: u32,
}

impl Default for AssistantDataSettings {
    fn default() -> Self {
        Self {
            revision: 1,
            automatic_memory_enabled: true,
            model_history_search_enabled: true,
            explanation_version: 0,
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataSettingsUpdate {
    pub expected_revision: u64,
    pub automatic_memory_enabled: bool,
    pub model_history_search_enabled: bool,
    pub explanation_version: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct UserInstructionsSnapshot {
    pub revision: u64,
    pub content: String,
    /// Zero only for a native scheduled conversation frozen from its owner
    /// task definition, without an accepted chat input.
    pub accepted_input_seq: u64,
    pub frozen_at_unix_ms: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataPolicy {
    /// Presentation policy stays outside persisted execution limits.
    #[serde(default = "default_conversation_title_max_chars")]
    pub conversation_title_max_chars: u32,
    #[serde(default)]
    pub limits: Box<super::limits::AssistantDataLimits>,
    pub revision: u64,
    pub history_retention_days: u32,
    pub old_memory_revision_days: u32,
    pub max_memory_revisions_per_fact: u32,
    pub max_history_records: u64,
    pub max_history_body_bytes: u64,
    pub max_memory_records: u64,
    pub max_memory_body_bytes: u64,
    pub max_term_records: u64,
    pub max_metadata_bytes: u64,

    pub max_retrieval_document_visits: u32,
}

const fn default_conversation_title_max_chars() -> u32 {
    40
}

impl Default for AssistantDataPolicy {
    fn default() -> Self {
        Self {
            conversation_title_max_chars: default_conversation_title_max_chars(),
            limits: Box::default(),
            revision: 1,
            history_retention_days: DEFAULT_HISTORY_RETENTION_DAYS,
            old_memory_revision_days: 30,
            max_memory_revisions_per_fact: 10,
            max_history_records: 10_000,
            max_history_body_bytes: 50 * 1024 * 1024,
            max_memory_records: 1000,
            max_memory_body_bytes: 2 * 1024 * 1024,
            max_term_records: 2_000_000,
            max_metadata_bytes: 128 * 1024 * 1024,

            max_retrieval_document_visits: 10_000,
        }
    }
}

impl AssistantDataPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::policy::result(
            super::policy::policy_errors(self),
            "invalid assistant data policy",
        )
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataPolicyUpdate {
    pub expected_revision: u64,
    pub policy: AssistantDataPolicy,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataSettingsView {
    pub settings: AssistantDataSettings,
    pub policy: AssistantDataPolicy,
}
