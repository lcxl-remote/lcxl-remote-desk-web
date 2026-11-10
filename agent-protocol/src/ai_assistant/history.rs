//! Bounded cross-conversation retrieval contracts for personal assistants.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use super::source::{AssistantSourceDevice, AssistantSourceKind, AssistantSourceLocator};

pub const MAX_ASSISTANT_QUERY_CURSOR_BYTES: usize = 128;

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
pub enum AssistantDeviceScope {
    #[default]
    CurrentDevice,
    AllMyDevices,
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
pub enum HistorySort {
    #[default]
    Relevance,
    Time,
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
pub enum HistoryOrder {
    Asc,
    #[default]
    Desc,
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
pub enum HistoryTimeField {
    #[default]
    RecordedAt,
    NativeActionAt,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct HistoryTimeRange {
    pub from_unix_ms: u64,
    pub until_unix_ms: u64,
}

impl HistoryTimeRange {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.until_unix_ms <= self.from_unix_ms {
            return Err(
                "Invalid input at $.time_range.until_unix_ms: must be greater than $.time_range.from_unix_ms",
            );
        }
        if self.until_unix_ms > i64::MAX as u64 {
            return Err(
                "Invalid input at $.time_range.until_unix_ms: expected a signed 64-bit Unix millisecond timestamp",
            );
        }
        Ok(())
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantRetrievalLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.until_unix_ms - self.from_unix_ms
            > u64::from(limits.search_window_days) * 86_400_000
        {
            return Err(
                "Invalid input at $.time_range: duration exceeds the configured search window",
            );
        }
        Ok(())
    }
}

fn default_search_limit() -> u32 {
    5
}
fn default_read_limit() -> u32 {
    10
}
fn default_max_bytes() -> u32 {
    super::limits::RetrievalEpisodeLimits::default().response_bytes
}
fn default_true() -> bool {
    true
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ConversationSearchRequest {
    #[serde(default)]
    pub queries: Vec<String>,
    #[serde(default)]
    pub scope: AssistantDeviceScope,
    #[serde(default)]
    pub conversation_ids: Vec<String>,
    pub time_range: Option<HistoryTimeRange>,
    #[serde(default)]
    pub time_field: HistoryTimeField,
    #[serde(default)]
    pub source_kinds: Vec<AssistantSourceKind>,
    #[serde(default = "default_true")]
    pub exclude_current_conversation: bool,
    #[serde(default)]
    pub sort: HistorySort,
    #[serde(default)]
    pub order: HistoryOrder,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    pub cursor: Option<String>,
}

impl ConversationSearchRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_queries(&self.queries, self.sort == HistorySort::Time)?;
        if self.limit == 0
            || self
                .conversation_ids
                .iter()
                .any(|v| super::source::validate_source_id(v).is_err())
            || self.source_kinds.len() > 4
            || self
                .source_kinds
                .contains(&AssistantSourceKind::PreviousMemory)
            || self
                .cursor
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > MAX_ASSISTANT_QUERY_CURSOR_BYTES)
        {
            return Err("invalid assistant history search");
        }
        if self.sort == HistorySort::Time
            && self.time_range.is_none()
            && self.conversation_ids.is_empty()
        {
            return Err("time enumeration requires an explicit time or conversation boundary");
        }
        if let Some(range) = &self.time_range {
            range.validate()?;
        }
        Ok(())
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantRetrievalLimits,
    ) -> Result<(), String> {
        self.validate()?;
        validate_queries_with_limits(&self.queries, self.sort == HistorySort::Time, limits)?;
        let maximum = if self.sort == HistorySort::Time {
            limits.time_hits
        } else {
            limits.relevance_hits
        };
        if self.limit > maximum
            || self.conversation_ids.len() > limits.search_conversations as usize
        {
            return Err("invalid assistant history search".into());
        }
        if let Some(range) = &self.time_range {
            range.validate_with_limits(limits).map_err(|reason| {
                let maximum = u64::from(limits.search_window_days) * 86_400_000;
                let actual = range.until_unix_ms - range.from_unix_ms;
                format!(
                    "{reason}; maximum {} days ({maximum} ms); observed {actual} ms",
                    limits.search_window_days
                )
            })?;
        }
        Ok(())
    }
}

pub fn validate_queries(queries: &[String], allow_empty: bool) -> Result<(), &'static str> {
    if (!allow_empty && queries.is_empty())
        || queries
            .iter()
            .any(|q| q.trim().is_empty() || q.chars().any(char::is_control))
    {
        return Err("invalid assistant query groups");
    }
    Ok(())
}

pub fn validate_queries_with_limits(
    queries: &[String],
    allow_empty: bool,
    limits: &super::limits::AssistantRetrievalLimits,
) -> Result<(), &'static str> {
    validate_queries(queries, allow_empty)?;
    if queries.len() > limits.query_groups as usize
        || queries
            .iter()
            .any(|query| query.chars().count() > limits.query_chars as usize)
    {
        return Err("invalid assistant query groups");
    }
    Ok(())
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
pub enum ConversationReadView {
    #[default]
    Context,
    Latest,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ConversationReadRequest {
    pub source_id: String,
    #[serde(default)]
    pub view: ConversationReadView,
    #[serde(default = "default_read_limit")]
    pub limit: u32,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u32,
    pub cursor: Option<String>,
}

impl ConversationReadRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::source::validate_source_id(&self.source_id)?;
        if self.limit == 0
            || self.max_bytes < 4
            || self
                .cursor
                .as_ref()
                .is_some_and(|c| c.is_empty() || c.len() > MAX_ASSISTANT_QUERY_CURSOR_BYTES)
        {
            return Err("invalid assistant conversation window");
        }
        Ok(())
    }
    pub fn validate_with_limits(
        &self,
        limits: &super::limits::AssistantRetrievalLimits,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.limit > limits.read_messages || self.max_bytes > limits.episode.response_bytes {
            return Err("invalid assistant conversation window");
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAdmission {
    Indexed,
    Redacted,
    OmittedCapacity,
    OmittedPolicy,
    OmittedError,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AssistantHistoryCoverage {
    pub retained_from_unix_ms: Option<u64>,
    pub retained_until_unix_ms: Option<u64>,
    pub indexed_through_unix_ms: Option<u64>,
    pub pending: u32,
    pub failed: u32,
    pub has_gaps: bool,
    pub partial: bool,
    pub reasons: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct NativeStatusProjection {
    pub native_id: String,
    pub kind: String,
    pub state: String,
    pub revision: Option<u64>,
    pub observed_at_unix_ms: u64,
    pub historical: bool,
    pub source_id: String,
    pub source_device: Option<AssistantSourceDevice>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct NativeStatusSummary {
    pub source_id: String,
    pub source_revision: u64,
    pub additional_sources: Vec<super::source::AssistantSourceRef>,
    pub last_recorded_at_unix_ms: u64,
    pub items: Vec<NativeStatusProjection>,
    pub additional_count: u32,
    pub pending_count: u32,
    pub unknown_count: u32,
    pub no_native_task: bool,
    pub has_more: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ConversationSearchHit {
    pub source_id: String,
    pub revision: u64,
    pub kind: AssistantSourceKind,
    pub locator: AssistantSourceLocator,
    /// Absent for account-level conversations that belong to no device.
    pub source_device: Option<AssistantSourceDevice>,
    pub conversation_title: Option<String>,
    pub snippet: String,
    pub recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    pub native_action_at_unix_ms: Option<u64>,
    pub native_action_interval: Option<HistoryTimeRange>,
    pub native_time_basis: Option<String>,
    pub native_status_summary: Option<NativeStatusSummary>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ConversationSearchResult {
    pub hits: Vec<ConversationSearchHit>,
    pub scope: AssistantDeviceScope,
    pub effective_queries: Vec<Vec<String>>,
    pub retrieval_mode: String,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub scope_changed: bool,
    pub coverage: AssistantHistoryCoverage,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct HistoryMessage {
    pub source_id: String,
    pub message_id: String,
    pub revision: u64,
    pub role: String,
    pub content: String,
    pub recorded_at_unix_ms: u64,
    pub statement_timezone: String,
    /// Absent for account-level conversations that belong to no device.
    pub source_device: Option<AssistantSourceDevice>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ConversationReadResult {
    pub conversation_id: String,
    pub messages: Vec<HistoryMessage>,
    pub related_results: Vec<NativeStatusProjection>,
    pub previous_cursor: Option<String>,
    pub next_cursor: Option<String>,
    pub source_observed_through_unix_ms: u64,
    pub actual_bytes: u32,
    pub has_more: bool,
    pub partial: bool,
    pub reasons: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct HistoryAccessScope {
    pub actor_id: String,
    pub root_conversation_id: String,
    pub task_id: Option<String>,
    pub authorized_input_seq: u64,
    pub accepted_at_unix_ms: u64,
    pub device_rule: AssistantDeviceScope,
    pub current_device_ref: String,
    pub allowed_kinds: Vec<AssistantSourceKind>,
    pub time_range: Option<HistoryTimeRange>,
    pub timezone: String,
    pub policy_revision: u64,
    /// An absent task deadline does not create a new artificial lifetime.
    pub expires_at_unix_ms: Option<u64>,
    pub retrieval_episode_id: String,
}

impl HistoryAccessScope {
    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [
            &self.actor_id,
            &self.root_conversation_id,
            &self.retrieval_episode_id,
            &self.timezone,
        ] {
            super::source::validate_source_id(id)?;
        }
        // An empty device names the account-level conversations.
        super::source::validate_optional_device_ref(&self.current_device_ref)?;
        if let Some(id) = &self.task_id {
            super::source::validate_source_id(id)?;
        }
        if self.authorized_input_seq == 0
            || self.accepted_at_unix_ms == 0
            || self.accepted_at_unix_ms > i64::MAX as u64
            || self.policy_revision == 0
            || self.timezone.len() > 128
            || self.allowed_kinds.is_empty()
            || self.allowed_kinds.len() > 4
            || self
                .allowed_kinds
                .contains(&AssistantSourceKind::PreviousMemory)
            || self
                .expires_at_unix_ms
                .is_some_and(|until| until <= self.accepted_at_unix_ms || until > i64::MAX as u64)
            || self
                .allowed_kinds
                .iter()
                .enumerate()
                .any(|(index, kind)| self.allowed_kinds[..index].contains(kind))
        {
            return Err("invalid original assistant history scope");
        }
        if self.time_range.as_ref().is_some_and(|range| {
            range.until_unix_ms <= range.from_unix_ms || range.until_unix_ms > i64::MAX as u64
        }) {
            return Err("invalid original assistant history time boundary");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
