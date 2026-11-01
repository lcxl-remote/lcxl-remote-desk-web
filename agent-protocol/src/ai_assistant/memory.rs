//! Model-authored stable facts and versioned owner data controls.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use super::{
    history::{AssistantDeviceScope, AssistantHistoryCoverage},
    source::{AssistantSourceDevice, AssistantSourceRef},
};

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    #[default]
    Personal,
    Device,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteScope {
    #[default]
    Personal,
    CurrentDevice,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    #[default]
    Fact,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryDraft {
    #[serde(default)]
    pub kind: MemoryKind,
    pub content: String,
    pub context: Option<String>,
    #[serde(default)]
    pub source_refs: Vec<AssistantSourceRef>,
    #[serde(default)]
    pub scope: MemoryWriteScope,
}

fn validate_draft_fields(
    content: &str,
    source_refs: &[AssistantSourceRef],
) -> Result<(), &'static str> {
    if content.trim().is_empty() {
        return Err("Invalid input at $.content: expected non-whitespace memory text");
    }
    if source_refs.iter().any(|source| {
        super::source::validate_source_id(&source.id).is_err() || source.revision == Some(0)
    }) {
        return Err(
            "Invalid input at $.source_refs: each source needs a valid observed id and a positive revision when supplied",
        );
    }
    Ok(())
}

fn validate_draft_limits(
    content_bytes: usize,
    context_bytes: usize,
    source_count: usize,
    limits: &super::limits::AssistantMemoryLimits,
) -> Result<(), String> {
    let total = content_bytes.saturating_add(context_bytes);
    if total > limits.draft_bytes as usize {
        return Err(format!(
            "Invalid input at $.content and $.context: maximum {} combined UTF-8 bytes; observed {total} (content={content_bytes}, context={context_bytes})",
            limits.draft_bytes
        ));
    }
    if source_count > limits.source_refs as usize {
        return Err(format!(
            "Invalid input at $.source_refs: maximum {} source references; observed {source_count}",
            limits.source_refs
        ));
    }
    Ok(())
}

impl MemoryDraft {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_draft_fields(&self.content, &self.source_refs)
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantMemoryLimits,
    ) -> Result<(), String> {
        self.validate()?;
        validate_draft_limits(
            self.content.len(),
            self.context.as_ref().map_or(0, String::len),
            self.source_refs.len(),
            limits,
        )
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryWriteRequest {
    Remember {
        #[serde(default)]
        kind: MemoryKind,
        content: String,
        context: Option<String>,
        #[serde(default)]
        source_refs: Vec<AssistantSourceRef>,
        #[serde(default)]
        scope: MemoryWriteScope,
    },
    Update {
        memory_id: String,
        expected_revision: u64,
        #[serde(default)]
        kind: MemoryKind,
        content: String,
        context: Option<String>,
        #[serde(default)]
        source_refs: Vec<AssistantSourceRef>,
        #[serde(default)]
        scope: MemoryWriteScope,
    },
    Forget {
        memory_id: String,
        expected_revision: u64,
    },
}

impl MemoryWriteRequest {
    pub fn draft(&self) -> Option<MemoryDraft> {
        match self {
            Self::Remember {
                kind,
                content,
                context,
                source_refs,
                scope,
            }
            | Self::Update {
                kind,
                content,
                context,
                source_refs,
                scope,
                ..
            } => Some(MemoryDraft {
                kind: *kind,
                content: content.clone(),
                context: context.clone(),
                source_refs: source_refs.clone(),
                scope: *scope,
            }),
            Self::Forget { .. } => None,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Remember {
                content,
                source_refs,
                ..
            } => validate_draft_fields(content, source_refs),
            Self::Update {
                memory_id,
                expected_revision,
                content,
                source_refs,
                ..
            } => {
                validate_target(memory_id, *expected_revision)?;
                validate_draft_fields(content, source_refs)
            }
            Self::Forget {
                memory_id,
                expected_revision,
            } => validate_target(memory_id, *expected_revision),
        }
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantMemoryLimits,
    ) -> Result<(), String> {
        self.validate()?;
        match self {
            Self::Remember {
                content,
                context,
                source_refs,
                ..
            }
            | Self::Update {
                content,
                context,
                source_refs,
                ..
            } => validate_draft_limits(
                content.len(),
                context.as_ref().map_or(0, String::len),
                source_refs.len(),
                limits,
            ),
            Self::Forget { .. } => Ok(()),
        }
    }
}

fn validate_target(id: &str, revision: u64) -> Result<(), &'static str> {
    super::source::validate_source_id(id)
        .map_err(|_| "Invalid input at $.memory_id: use a valid observed memory id")?;
    if revision == 0 {
        return Err("Invalid input at $.expected_revision: minimum 1; observed 0");
    }
    Ok(())
}

/// Owner management is independent of the model retrieval budget and switches.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryListRequest {
    #[serde(default = "owner_page_limit")]
    pub limit: u32,
    pub scope: Option<MemoryScope>,
    #[serde(default)]
    pub include_superseded: bool,
    /// A native memory identity, never a database row number or an authorization.
    pub cursor: Option<String>,
    pub memory_state_generation: Option<u64>,
    pub control_generation: Option<u64>,
}

fn owner_page_limit() -> u32 {
    20
}

impl Default for MemoryListRequest {
    fn default() -> Self {
        Self {
            limit: owner_page_limit(),
            scope: None,
            include_superseded: false,
            cursor: None,
            memory_state_generation: None,
            control_generation: None,
        }
    }
}

impl MemoryListRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.limit == 0
            || self.limit > 50
            || self.memory_state_generation == Some(0)
            || self.control_generation == Some(0)
        {
            return Err("invalid owner memory page");
        }
        if let Some(cursor) = &self.cursor {
            super::source::validate_source_id(cursor)?;
            if self.memory_state_generation.is_none() || self.control_generation.is_none() {
                return Err("memory continuation requires its original generations");
            }
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryListResult {
    pub memories: Vec<MemoryRecord>,
    pub next_cursor: Option<String>,
    pub memory_state_generation: u64,
    pub control_generation: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchRequest {
    #[serde(default)]
    pub queries: Vec<String>,

    #[serde(default)]
    pub scope: AssistantDeviceScope,
    #[serde(default = "default_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}

fn default_limit() -> u32 {
    5
}

impl MemorySearchRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::history::validate_queries(&self.queries, false)?;
        if self.limit == 0
            || self.cursor.as_ref().is_some_and(|v| {
                v.is_empty() || v.len() > super::history::MAX_ASSISTANT_QUERY_CURSOR_BYTES
            })
        {
            return Err("invalid stable memory query");
        }
        Ok(())
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantRetrievalLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        super::history::validate_queries_with_limits(&self.queries, false, limits)?;
        if self.limit > limits.relevance_hits {
            return Err("invalid memory search");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    PendingConflict,
    Superseded,
    Forgotten,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryUsePurpose {
    CurrentFact,
    HistoricalReference,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecord {
    pub memory_id: String,
    pub revision: u64,
    pub scope: MemoryScope,
    pub source_device: Option<AssistantSourceDevice>,

    pub content: String,
    pub context: Option<String>,
    pub status: MemoryStatus,
    pub source_ids: Vec<String>,
    pub observed_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub valid_from_unix_ms: u64,
    pub valid_until_unix_ms: Option<u64>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemorySearchResult {
    pub memories: Vec<MemoryRecord>,
    pub effective_queries: Vec<Vec<String>>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub memory_state_generation: u64,
    pub coverage: AssistantHistoryCoverage,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryHistoryRequest {
    pub memory_id: String,
    pub limit: u32,
    pub cursor: Option<String>,
}

impl MemoryHistoryRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::source::validate_source_id(&self.memory_id)?;
        if self.limit == 0
            || self.cursor.as_ref().is_some_and(|cursor| {
                cursor.is_empty() || cursor.len() > super::history::MAX_ASSISTANT_QUERY_CURSOR_BYTES
            })
        {
            return Err("invalid memory history request");
        }
        Ok(())
    }

    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantRetrievalLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.limit > limits.read_messages {
            return Err("memory history page exceeds the configured read limit");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryHistoryEntry {
    pub revision: u64,
    pub content: String,
    pub context: Option<String>,
    pub status: MemoryStatus,
    /// Platform recording time, not the real-world effective date of the fact.
    pub recorded_at_unix_ms: u64,
    pub source_ids: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryHistoryResult {
    pub memory_id: String,
    /// Head revision observed when the query was first established.
    pub current_revision: u64,
    pub versions: Vec<MemoryHistoryEntry>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub partial: bool,
    pub reasons: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryForgetTarget {
    pub memory_id: String,
    pub expected_revision: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryDeleteRequest {
    pub memory_id: String,
    pub expected_revision: u64,
    pub operation_id: String,
}

impl MemoryDeleteRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_target(&self.memory_id, self.expected_revision)?;
        super::source::validate_source_id(&self.operation_id)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryClearRequest {
    pub operation_id: String,
    pub expected_memory_state_generation: u64,
}

impl MemoryClearRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::source::validate_source_id(&self.operation_id)?;
        if self.expected_memory_state_generation == 0 {
            return Err("memory clear requires its observed generation");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DataControlOutcome {
    Applied,
    AlreadyApplied,
    StaleTarget,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DataControlReason {
    MemoryForget,
    MemoryClear,
    HistoryRestricted,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantDataControlReceipt {
    pub operation_id: String,
    pub outcome: DataControlOutcome,
    pub reason: DataControlReason,
    pub affected_count: u32,
    pub control_generation: u64,
    pub memory_state_generation: u64,
    pub cleanup_pending: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryWriteOutcome {
    Applied,
    Skipped,
    Conflict,
    Rejected,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryWritten {
    pub memory_id: String,
    pub revision: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteResult {
    pub outcome: MemoryWriteOutcome,
    pub memory: Option<MemoryWritten>,
    pub reason: Option<String>,
    pub control_receipt: Option<AssistantDataControlReceipt>,
}

#[cfg(test)]
mod tests;
