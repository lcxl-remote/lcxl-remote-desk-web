//! Bounded, auditable metadata for screen pixels shown to a model and owner.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::data_lineage::ContentRef;

pub const VISUAL_EVIDENCE_SCHEMA_VERSION: u16 = 1;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum VisualEvidencePhase {
    Before,
    Observation,
    After,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum VisualEvidenceStatus {
    Available,
    Expired,
    NotRetained,
    Failed,
    Blocked,
}

/// The optional preview is emitted only on the live owner stream. Durable
/// session JSON and snapshot DTOs carry metadata plus `content`, never pixels.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct VisualEvidenceFrame {
    pub schema_version: u16,
    pub evidence_id: String,
    pub conversation_id: String,
    pub focus_input_revision: u64,
    pub turn_id: String,
    pub tool_call_id: String,
    pub frame_id: String,
    pub phase: VisualEvidencePhase,
    pub status: VisualEvidenceStatus,
    pub captured_at_unix_ms: u64,
    pub expires_at_unix_ms: Option<u64>,
    pub device_id: String,
    pub display_summary: Option<String>,
    pub application_summary: Option<String>,
    pub content: Option<ContentRef>,
    pub digest_sha256: Option<String>,
    pub size_bytes: u64,
    pub media_type: Option<String>,
    /// Bounded data URL already authorized for this active owner stream. It is
    /// never accepted from a client and is never persisted by the session store.
    pub preview_data_url: Option<String>,
}
