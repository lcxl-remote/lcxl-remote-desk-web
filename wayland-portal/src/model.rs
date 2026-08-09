use desk_utils::error::DeskErrorCode;
use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

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
    SchemaRead,
    SchemaWrite,
)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationTarget {
    ScreenOnly,
    ScreenAndInput,
}

impl AuthorizationTarget {
    pub const fn needs_input(self) -> bool {
        matches!(self, Self::ScreenAndInput)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
#[serde(rename_all = "snake_case")]
pub enum PortalPhase {
    Unsupported,
    NotConfigured,
    Restoring,
    Preparing,
    Ready,
    NeedsAuthorization,
    Failed,
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite,
)]
pub struct PortalCapabilities {
    pub screen_ready: bool,
    pub input_ready: bool,
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite,
)]
pub struct PortalAvailability {
    pub remote_desktop_version: u32,
    pub available_source_types: u32,
    pub available_device_types: u32,
    pub monitor_available: bool,
    pub keyboard_available: bool,
    pub pointer_available: bool,
    pub stable_app_id: bool,
    pub persistent_restore: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
pub struct PortalSnapshot {
    pub phase: PortalPhase,
    pub capabilities: PortalCapabilities,
    pub availability: PortalAvailability,
    pub target: Option<AuthorizationTarget>,
    pub operation_id: Option<String>,
    pub generation: u64,
    pub restore_token_persisted: bool,
    pub requires_local_action: bool,
    /// Stable machine-readable reason for UI localization. The free-form
    /// `reason` is diagnostic detail and must not be rendered directly.
    pub reason_code: Option<DeskErrorCode>,
    pub reason: Option<String>,
}

impl PortalSnapshot {
    pub fn not_configured(availability: PortalAvailability) -> Self {
        Self {
            phase: PortalPhase::NotConfigured,
            capabilities: PortalCapabilities::default(),
            availability,
            target: None,
            operation_id: None,
            generation: 0,
            restore_token_persisted: false,
            requires_local_action: true,
            reason_code: None,
            reason: None,
        }
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            phase: PortalPhase::Unsupported,
            capabilities: PortalCapabilities::default(),
            availability: PortalAvailability::default(),
            target: None,
            operation_id: None,
            generation: 0,
            restore_token_persisted: false,
            requires_local_action: false,
            reason_code: Some(DeskErrorCode::FEATURE_UNAVAILABLE),
            reason: Some(reason.into()),
        }
    }

    pub fn admits(&self, needs_input: bool) -> bool {
        self.phase == PortalPhase::Ready
            && self.capabilities.screen_ready
            && (!needs_input || self.capabilities.input_ready)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalStream {
    pub node_id: u32,
    pub id: Option<String>,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub mapping_id: Option<String>,
}

pub const SOURCE_TYPE_MONITOR: u32 = 1;
pub const DEVICE_TYPE_KEYBOARD: u32 = 1;
pub const DEVICE_TYPE_POINTER: u32 = 2;
pub const REQUIRED_INPUT_DEVICE_TYPES: u32 = DEVICE_TYPE_KEYBOARD | DEVICE_TYPE_POINTER;
