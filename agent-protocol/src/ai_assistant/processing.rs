//! Owner-visible metadata. Counts and recovery controls carry no task authority.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssistantProcessingCount {
    pub state: String,
    pub count: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssistantHistoryCoverageStatus {
    pub conversation_id: Option<String>,
    pub reason: String,
    pub source_count: u64,
    pub first_recorded_at_unix_ms: u64,
    pub last_recorded_at_unix_ms: u64,
    pub indexed_through_unix_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssistantProcessingStatus {
    pub observed_at_unix_ms: u64,
    pub history_retention_days: u32,
    pub history_generation: u64,
    pub stored_history_fragments: u64,
    pub indexed_terms: u64,
    pub coverage: Vec<AssistantHistoryCoverageStatus>,
    pub partial: bool,
}
