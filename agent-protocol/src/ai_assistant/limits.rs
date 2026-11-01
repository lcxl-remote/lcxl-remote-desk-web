//! Platform-controlled product limits. Hosts freeze episode allowances separately.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const DEFAULT_QUERY_TTL_MS: u64 = 30 * 60 * 1000;
pub const DEFAULT_UNUSED_PROJECTION_TTL_MS: u64 = 30 * 60 * 1000;

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssistantMemoryLimits {
    pub draft_bytes: u32,

    pub source_refs: u32,
    pub current_facts: u32,
    pub user_instructions_bytes: u32,
}

impl Default for AssistantMemoryLimits {
    fn default() -> Self {
        Self {
            draft_bytes: 2048,

            source_refs: 8,
            current_facts: 8,
            user_instructions_bytes: 8192,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct RetrievalEpisodeLimits {
    pub relevance_calls: u16,
    pub time_pages: u16,
    pub read_calls: u16,
    pub read_bytes: u64,
    pub response_bytes: u32,
    pub query_devices: u32,
    pub query_cursors: u32,
    pub query_state_bytes: u32,
    pub query_seen_sources: u32,
    pub query_delivered_calls: u32,
    pub query_ttl_ms: u64,
    pub unused_projection_ttl_ms: u64,
}

impl RetrievalEpisodeLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::policy::result(
            super::policy::episode_errors(self),
            "invalid assistant retrieval episode limits",
        )
    }
}

impl Default for RetrievalEpisodeLimits {
    fn default() -> Self {
        Self {
            relevance_calls: 4,
            time_pages: 10,
            read_calls: 8,
            read_bytes: 128 * 1024,
            response_bytes: 32 * 1024,
            query_devices: 10_000,
            query_cursors: 32,
            query_state_bytes: 1024 * 1024,
            query_seen_sources: 500,
            query_delivered_calls: 32,
            query_ttl_ms: DEFAULT_QUERY_TTL_MS,
            unused_projection_ttl_ms: DEFAULT_UNUSED_PROJECTION_TTL_MS,
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssistantRetrievalLimits {
    pub episode: RetrievalEpisodeLimits,
    pub snippet_bytes: u32,
    pub query_groups: u32,
    /// Unicode scalar values, matching JSON Schema maxLength.
    pub query_chars: u32,
    pub relevance_hits: u32,
    pub time_hits: u32,
    pub read_messages: u32,
    pub search_window_days: u32,
    pub search_conversations: u32,
    pub query_candidates: u32,
    pub relevance_roots: u32,
    pub fragments_per_root: u32,

    pub history_keyword_scan_ms: u64,
    pub memory_keyword_scan_ms: u64,
    pub history_time_scan_ms: u64,
    pub current_memory_selection_ms: u64,
    pub owner_scan_documents: u32,
    pub owner_scan_ms: u64,
}

impl Default for AssistantRetrievalLimits {
    fn default() -> Self {
        Self {
            episode: RetrievalEpisodeLimits::default(),
            snippet_bytes: 512,
            query_groups: 4,
            query_chars: 256,
            relevance_hits: 10,
            time_hits: 50,
            read_messages: 20,
            search_window_days: 90,
            search_conversations: 8,
            query_candidates: 40,
            relevance_roots: 20,
            fragments_per_root: 3,

            history_keyword_scan_ms: 3000,
            memory_keyword_scan_ms: 3000,
            history_time_scan_ms: 3000,
            current_memory_selection_ms: 3000,
            owner_scan_documents: 128,
            owner_scan_ms: 3000,
        }
    }
}

/// Working batches and background cadence, independent of stored-content quotas.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssistantProcessingLimits {
    pub status_coverage: u32,
    pub native_status_rows: u32,
    pub native_status_items: u32,
    pub native_status_model_items: u32,
    pub native_status_source_refs: u32,

    pub output_gc_interval_ms: u64,
    pub output_gc_actor_batch_size: u32,
    pub output_gc_batch_size: u32,
    pub native_retention_page_size: u32,
    pub history_capture_batch_size: u32,
    pub history_scan_page_size: u32,
    pub current_memory_page_size: u32,
}

impl Default for AssistantProcessingLimits {
    fn default() -> Self {
        Self {
            status_coverage: 50,
            native_status_rows: 25,
            native_status_items: 24,
            native_status_model_items: 3,
            native_status_source_refs: 4,

            output_gc_interval_ms: 5 * 60 * 1000,
            output_gc_actor_batch_size: 32,
            output_gc_batch_size: 64,
            native_retention_page_size: 64,
            history_capture_batch_size: 64,
            history_scan_page_size: 128,
            current_memory_page_size: 64,
        }
    }
}

impl AssistantProcessingLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::policy::result(
            super::policy::processing_errors(self),
            "invalid assistant processing limits",
        )
    }
}

/// Input admission and answer-reference projection, separate from stored quotas.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssistantConversationLimits {
    pub question_bytes: u32,
    /// Owner-facing page target; one indivisible entry may exceed this value.
    pub page_bytes: u32,
    pub citations: u32,
    pub citation_bytes: u32,
    pub citation_page_answers: u32,
}
impl Default for AssistantConversationLimits {
    fn default() -> Self {
        Self {
            question_bytes: 16 * 1024,
            page_bytes: 256 * 1024,
            citations: 64,
            citation_bytes: 8 * 1024,
            citation_page_answers: 100,
        }
    }
}

#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssistantDataLimits {
    pub conversation: AssistantConversationLimits,
    pub memory: AssistantMemoryLimits,
    pub retrieval: AssistantRetrievalLimits,
    pub processing: AssistantProcessingLimits,
}

impl AssistantDataLimits {
    pub fn validate(&self) -> Result<(), &'static str> {
        super::policy::result(
            super::policy::limits_errors(self),
            "invalid assistant content or retrieval limits",
        )
    }
}

#[cfg(test)]
mod tests;
