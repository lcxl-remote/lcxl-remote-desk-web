//! Typed requests and results for physical display mode changes.

use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SetPhysicalDisplayModePayload {
    pub request_id: String,
    pub connection_id: String,
    pub connection_epoch: String,
    pub device_name: String,
    pub capture_backend: String,
    pub operation_id: u64,
    pub action: PhysicalDisplayAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum PhysicalDisplayAction {
    Auto {
        viewport_width: u32,
        viewport_height: u32,
        viewport_sequence: u64,
        max_capture_width: u32,
        max_capture_height: u32,
    },
    Select {
        selector: String,
    },
    Restore {
        original_selector: String,
        expected_applied_selector: String,
        expected_display_identity: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct PhysicalDisplayModeData {
    pub device_name: String,
    pub display_identity: String,
    pub previous_selector: String,
    pub selector: String,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub refresh_millihz: u32,
    pub changed: bool,
    pub restored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum PhysicalDisplayModeOutcome {
    Applied(PhysicalDisplayModeData),
    /// The OS mode still reads back as applied, but capture or encoding did
    /// not recover and rollback failed. The daemon must retain the original
    /// mode snapshot even though the browser receives an error.
    AppliedWithoutVideo {
        data: PhysicalDisplayModeData,
        reason: String,
    },
    Failed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct PhysicalDisplayModeResponsePayload {
    pub request_id: String,
    pub connection_id: String,
    pub connection_epoch: String,
    pub operation_id: u64,
    pub outcome: PhysicalDisplayModeOutcome,
}
