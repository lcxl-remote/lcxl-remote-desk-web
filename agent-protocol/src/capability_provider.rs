//! Current-schema wire contracts for first-party Capability Providers.
//!
//! Capability Providers are product-compiled tool modules. They are unrelated
//! to model-provider/API-key configuration and cannot load runtime code.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub const CAPABILITY_PROVIDER_SCHEMA_VERSION: u16 = 1;
pub const MAX_PROVIDER_ID_BYTES: usize = 128;
pub const MAX_CAPABILITIES_PER_PROVIDER: usize = 128;
pub const MAX_CAPABILITY_INVENTORY_ENTRIES: usize = 512;
pub const MAX_FOREGROUND_BUDGET_MS: u32 = 30_000;
pub const MAX_CAPABILITY_TIMEOUT_MS: u32 = 7_200_000;
pub const MAX_CAPABILITY_CANCEL_REASON_BYTES: usize = 1024;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityEffect {
    ReadDevice,
    ReadFile,
    ReadExternal,
    ExportData,
    WriteArtifact,
    MutateApplication,
    WriteExternalDraft,
    SendExternal,
    CaptureScreen,
    InputFallback,
    ExecuteCommand,
}

impl CapabilityEffect {
    pub fn is_side_effecting(self) -> bool {
        matches!(
            self,
            Self::WriteArtifact
                | Self::MutateApplication
                | Self::WriteExternalDraft
                | Self::SendExternal
                | Self::InputFallback
                | Self::ExecuteCommand
        )
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLocality {
    Central,
    Edge,
    CentralAndEdge,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionPolicy {
    InlineOnly,
    DurableRequired,
    Adaptive { foreground_budget_ms: u32 },
}

impl ExecutionPolicy {
    pub fn validate(self, hard_timeout_ms: u32) -> Result<(), CapabilityContractError> {
        if hard_timeout_ms == 0 || hard_timeout_ms > MAX_CAPABILITY_TIMEOUT_MS {
            return Err(CapabilityContractError::InvalidHardTimeout);
        }
        if let Self::Adaptive {
            foreground_budget_ms,
        } = self
            && (foreground_budget_ms == 0
                || foreground_budget_ms > MAX_FOREGROUND_BUDGET_MS
                || foreground_budget_ms >= hard_timeout_ms)
        {
            return Err(CapabilityContractError::InvalidForegroundBudget);
        }
        Ok(())
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProductSurface {
    OssPersonalOwner,
    ManagerPersonalOwner,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityPlatform {
    Windows,
    Linux,
    Macos,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationPrerequisite {
    GoogleChrome,
    MicrosoftExcel,
    MicrosoftWord,
    MicrosoftPowerPoint,
    WebAccess,
    EmailAccount,
    ChatAccount,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityPrerequisites {
    /// Empty means platform-neutral. This is a compile-time constraint, not a
    /// model-selectable target claim.
    pub platforms: Vec<CapabilityPlatform>,
    pub applications: Vec<ApplicationPrerequisite>,
    pub requires_edge_connection: bool,
    pub requires_interactive_session: bool,
    pub requires_credential_connection: bool,
}

impl CapabilityPrerequisites {
    fn validate(&self) -> Result<(), CapabilityContractError> {
        if has_duplicates(&self.platforms) || has_duplicates(&self.applications) {
            return Err(CapabilityContractError::DuplicatePrerequisite);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRateClass {
    InteractiveRead,
    BackgroundRead,
    InteractiveMutation,
    ExternalWrite,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDataCategory {
    UserRequest,
    DesktopSessionMetadata,
    UiSemanticTree,
    OfficeSelection,
    ScreenPixels,
    FileMetadata,
    FileContent,
    TerminalOutput,
    SystemMetadata,
    ProcessMetadata,
    NetworkMetadata,
    ServiceMetadata,
    LogContent,
    ContainerMetadata,
    CommandOutput,
    ExternalContent,
    CommunicationContent,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityDataPolicy {
    pub reads: Vec<CapabilityDataCategory>,
    pub may_export_data: bool,
}

impl CapabilityDataPolicy {
    fn validate(&self) -> Result<(), CapabilityContractError> {
        if has_duplicates(&self.reads) {
            return Err(CapabilityContractError::DuplicateDataCategory);
        }
        Ok(())
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationResourceKind {
    TargetDevice,
    FreshObjectReference,
    WorkspaceRoot,
    /// One server-validated public HTTPS URL, bound to the canonical tool
    /// input digest. Model-provided resource labels are never authority.
    ExternalUrl,
    /// One exact owner-supplied Web Search query, bound to the canonical tool
    /// input digest and to a server-owned connector destination.
    ExternalQuery,
    /// One server-classified command plan, bound to the exact canonical tool
    /// input digest and the target device. The raw command is never authority.
    ExactCommand,
    /// One canonical browser origin on the current approved Chrome profile
    /// incarnation. Paths and model labels are not authority.
    BrowserOrigin,
    /// One stable page/document incarnation produced by the controlled-edge
    /// Browser Provider. Stale page and element references fail closed.
    BrowserPage,
    ExternalAccount,
    ExactRecipientsAndArtifacts,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityAuthorizationHint {
    pub resources: Vec<AuthorizationResourceKind>,
}

impl CapabilityAuthorizationHint {
    fn validate(&self) -> Result<(), CapabilityContractError> {
        if self.resources.is_empty() || has_duplicates(&self.resources) {
            return Err(CapabilityContractError::InvalidAuthorizationHint);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityLimits {
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
    pub max_objects: u32,
    pub hard_timeout_ms: u32,
}

impl CapabilityLimits {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        if self.max_input_bytes == 0 || self.max_output_bytes == 0 || self.max_objects == 0 {
            return Err(CapabilityContractError::InvalidLimit);
        }
        if self.hard_timeout_ms == 0 || self.hard_timeout_ms > MAX_CAPABILITY_TIMEOUT_MS {
            return Err(CapabilityContractError::InvalidHardTimeout);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityWireDescriptor {
    pub capability_id: String,
    pub tool_name: String,
    pub display_name_key: String,
    pub input_schema_version: u16,
    pub output_schema_version: u16,
    pub effect: CapabilityEffect,
    pub execution_locality: ExecutionLocality,
    pub prerequisites: CapabilityPrerequisites,
    pub execution_policy: ExecutionPolicy,
    pub rate_class: CapabilityRateClass,
    pub limits: CapabilityLimits,
    pub supports_progress: bool,
    pub supports_cancel: bool,
    pub data_policy: CapabilityDataPolicy,
    pub authorization_hint: CapabilityAuthorizationHint,
    /// Ordered high-level capability ids that policy explicitly permits as
    /// fallbacks. An empty list means no automatic fallback relation.
    pub fallback_capability_ids: Vec<String>,
    pub surfaces: Vec<ProductSurface>,
}

impl CapabilityWireDescriptor {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        validate_id("capability_id", &self.capability_id)?;
        validate_id("tool_name", &self.tool_name)?;
        validate_id("display_name_key", &self.display_name_key)?;
        if self.input_schema_version == 0 || self.output_schema_version == 0 {
            return Err(CapabilityContractError::InvalidSchemaVersion);
        }
        self.prerequisites.validate()?;
        match self.execution_locality {
            ExecutionLocality::Central if self.prerequisites.requires_edge_connection => {
                return Err(CapabilityContractError::InconsistentPrerequisite);
            }
            ExecutionLocality::Edge | ExecutionLocality::CentralAndEdge
                if !self.prerequisites.requires_edge_connection =>
            {
                return Err(CapabilityContractError::InconsistentPrerequisite);
            }
            _ => {}
        }
        self.limits.validate()?;
        self.execution_policy
            .validate(self.limits.hard_timeout_ms)?;
        if self.effect.is_side_effecting()
            && matches!(self.execution_policy, ExecutionPolicy::InlineOnly)
        {
            return Err(CapabilityContractError::SideEffectRequiresDurableLifecycle);
        }
        self.data_policy.validate()?;
        self.authorization_hint.validate()?;
        let fallbacks = self
            .fallback_capability_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if fallbacks.len() != self.fallback_capability_ids.len()
            || fallbacks.contains(self.capability_id.as_str())
            || fallbacks
                .iter()
                .any(|value| validate_id("fallback_capability_id", value).is_err())
        {
            return Err(CapabilityContractError::InvalidFallback);
        }
        if self.surfaces.is_empty() {
            return Err(CapabilityContractError::MissingSurface);
        }
        let unique: BTreeSet<_> = self.surfaces.iter().copied().collect();
        if unique.len() != self.surfaces.len() {
            return Err(CapabilityContractError::DuplicateSurface);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ProviderWireDescriptor {
    pub schema_version: u16,
    pub provider_id: String,
    pub display_name_key: String,
    pub provider_version: u16,
    pub capabilities: Vec<CapabilityWireDescriptor>,
}

impl ProviderWireDescriptor {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        if self.schema_version != CAPABILITY_PROVIDER_SCHEMA_VERSION {
            return Err(CapabilityContractError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        validate_id("provider_id", &self.provider_id)?;
        validate_id("display_name_key", &self.display_name_key)?;
        if self.provider_version == 0 {
            return Err(CapabilityContractError::InvalidSchemaVersion);
        }
        if self.capabilities.is_empty() {
            return Err(CapabilityContractError::MissingCapability);
        }
        if self.capabilities.len() > MAX_CAPABILITIES_PER_PROVIDER {
            return Err(CapabilityContractError::TooManyCapabilities);
        }
        let mut capability_ids = BTreeSet::new();
        let mut tool_names = BTreeSet::new();
        for capability in &self.capabilities {
            capability.validate()?;
            if !capability_ids.insert(&capability.capability_id) {
                return Err(CapabilityContractError::DuplicateCapabilityId);
            }
            if !tool_names.insert(&capability.tool_name) {
                return Err(CapabilityContractError::DuplicateToolName);
            }
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityBlockedReason {
    Disabled,
    UnsupportedPlatform,
    VersionMismatch,
    EdgeDisconnected,
    AdapterUnavailable,
    RemoteDebuggingDisabled,
    BrowserApprovalRequired,
    BrowserDisconnected,
    ApplicationNotInstalled,
    PermissionMissing,
    OfficeBridgeNotPaired,
    NoActiveDocument,
    NoInteractiveSession,
    NoDisplaySelected,
    LocalCeiling,
    ModelIncompatible,
    Busy,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityReadinessReport {
    pub schema_version: u16,
    pub provider_id: String,
    pub capability_id: String,
    pub adapter_id: Option<String>,
    pub adapter_version: Option<String>,
    pub revision: u64,
    pub observed_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub local_ceiling_revision: u64,
    pub compiled: bool,
    pub enabled: bool,
    pub connected: bool,
    pub ready: bool,
    pub reason: Option<CapabilityBlockedReason>,
}

impl CapabilityReadinessReport {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        if self.schema_version != CAPABILITY_PROVIDER_SCHEMA_VERSION {
            return Err(CapabilityContractError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        validate_id("provider_id", &self.provider_id)?;
        validate_id("capability_id", &self.capability_id)?;
        if let Some(value) = &self.adapter_id {
            validate_id("adapter_id", value)?;
        }
        if let Some(value) = &self.adapter_version {
            validate_id("adapter_version", value)?;
        }
        if self.revision == 0
            || self.observed_at_unix_ms == 0
            || self.expires_at_unix_ms <= self.observed_at_unix_ms
        {
            return Err(CapabilityContractError::InvalidReadinessRevision);
        }
        if self.ready {
            if !(self.compiled && self.enabled && self.connected) || self.reason.is_some() {
                return Err(CapabilityContractError::InconsistentReadiness);
            }
        } else if self.reason.is_none() {
            return Err(CapabilityContractError::InconsistentReadiness);
        }
        Ok(())
    }
}

/// Secret-free, browser-safe projection of one compiled capability and its
/// current callability. Tool input JSON schema and credentials are deliberately
/// absent; the central registry remains the only authority for model exposure.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityInventoryEntry {
    pub provider_id: String,
    pub provider_display_name_key: String,
    pub provider_version: u16,
    pub capability: CapabilityWireDescriptor,
    pub compiled: bool,
    pub enabled: bool,
    pub connected: bool,
    pub ready: bool,
    pub reason: Option<CapabilityBlockedReason>,
}

impl CapabilityInventoryEntry {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        validate_id("provider_id", &self.provider_id)?;
        validate_id("provider_display_name_key", &self.provider_display_name_key)?;
        if self.provider_version == 0 {
            return Err(CapabilityContractError::InvalidSchemaVersion);
        }
        self.capability.validate()?;
        if self.ready {
            if !(self.compiled && self.enabled && self.connected) || self.reason.is_some() {
                return Err(CapabilityContractError::InconsistentReadiness);
            }
        } else if self.reason.is_none() {
            return Err(CapabilityContractError::InconsistentReadiness);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityInventorySnapshot {
    pub schema_version: u16,
    pub surface: ProductSurface,
    pub generated_at_unix_ms: u64,
    pub entries: Vec<CapabilityInventoryEntry>,
}

impl CapabilityInventorySnapshot {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        if self.schema_version != CAPABILITY_PROVIDER_SCHEMA_VERSION {
            return Err(CapabilityContractError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if self.generated_at_unix_ms == 0 {
            return Err(CapabilityContractError::InvalidReadinessRevision);
        }
        if self.entries.len() > MAX_CAPABILITY_INVENTORY_ENTRIES {
            return Err(CapabilityContractError::TooManyCapabilities);
        }
        let mut capability_ids = BTreeSet::new();
        let mut tool_names = BTreeSet::new();
        for entry in &self.entries {
            entry.validate()?;
            if !entry.capability.surfaces.contains(&self.surface) {
                return Err(CapabilityContractError::MissingSurface);
            }
            if !capability_ids.insert(&entry.capability.capability_id) {
                return Err(CapabilityContractError::DuplicateCapabilityId);
            }
            if !tool_names.insert(&entry.capability.tool_name) {
                return Err(CapabilityContractError::DuplicateToolName);
            }
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityTaskRef {
    pub task_id: String,
    pub call_id: String,
    pub run_id: String,
    pub provider_id: String,
    pub capability_id: String,
    pub input_revision: u64,
    pub generation: u64,
}

impl CapabilityTaskRef {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        for (field, value) in [
            ("task_id", self.task_id.as_str()),
            ("call_id", self.call_id.as_str()),
            ("run_id", self.run_id.as_str()),
            ("provider_id", self.provider_id.as_str()),
            ("capability_id", self.capability_id.as_str()),
        ] {
            validate_id(field, value)?;
        }
        if self.input_revision == 0 || self.generation == 0 {
            return Err(CapabilityContractError::InvalidTaskRevision);
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityFailureReason {
    PermissionRequired,
    NotReady,
    InvalidInput,
    BudgetExceeded,
    PolicyDenied,
    Internal,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapabilityInvocationOutcome {
    Completed {
        task: Option<CapabilityTaskRef>,
        result_envelope_ids: Vec<String>,
    },
    Accepted {
        task: CapabilityTaskRef,
    },
    FailedBeforeStart {
        reason: CapabilityFailureReason,
    },
}

impl CapabilityInvocationOutcome {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        match self {
            Self::Completed {
                task,
                result_envelope_ids,
            } => {
                if let Some(task) = task {
                    task.validate()?;
                }
                validate_result_envelope_ids(result_envelope_ids)
            }
            Self::Accepted { task } => task.validate(),
            Self::FailedBeforeStart { .. } => Ok(()),
        }
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityProgressEvent {
    pub task: CapabilityTaskRef,
    pub sequence: u64,
    pub completed_units: Option<u64>,
    pub total_units: Option<u64>,
    pub message_key: Option<String>,
}

impl CapabilityProgressEvent {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        self.task.validate()?;
        if self.sequence == 0 {
            return Err(CapabilityContractError::InvalidProgress);
        }
        if let (Some(completed), Some(total)) = (self.completed_units, self.total_units)
            && (total == 0 || completed > total)
        {
            return Err(CapabilityContractError::InvalidProgress);
        }
        if let Some(value) = &self.message_key {
            validate_id("message_key", value)?;
        }
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityCompletionClass {
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityCompletionEvent {
    pub task: CapabilityTaskRef,
    pub sequence: u64,
    pub completion: CapabilityCompletionClass,
    pub result_envelope_ids: Vec<String>,
}

impl CapabilityCompletionEvent {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        self.task.validate()?;
        if self.sequence == 0 {
            return Err(CapabilityContractError::InvalidCompletion);
        }
        validate_result_envelope_ids(&self.result_envelope_ids)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CapabilityCancelRequest {
    pub task: CapabilityTaskRef,
    pub request_id: String,
    pub requested_by_actor_id: String,
    pub reason: String,
}

impl CapabilityCancelRequest {
    pub fn validate(&self) -> Result<(), CapabilityContractError> {
        self.task.validate()?;
        validate_id("request_id", &self.request_id)?;
        validate_id("requested_by_actor_id", &self.requested_by_actor_id)?;
        let reason = self.reason.trim();
        if reason.is_empty() {
            return Err(CapabilityContractError::EmptyField("reason"));
        }
        if reason.len() > MAX_CAPABILITY_CANCEL_REASON_BYTES {
            return Err(CapabilityContractError::OversizedField("reason"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityContractError {
    UnsupportedSchemaVersion(u16),
    EmptyField(&'static str),
    OversizedField(&'static str),
    InvalidSchemaVersion,
    InvalidLimit,
    InvalidHardTimeout,
    InvalidForegroundBudget,
    MissingSurface,
    DuplicateSurface,
    DuplicatePrerequisite,
    InconsistentPrerequisite,
    DuplicateDataCategory,
    InvalidAuthorizationHint,
    InvalidFallback,
    SideEffectRequiresDurableLifecycle,
    MissingCapability,
    TooManyCapabilities,
    DuplicateCapabilityId,
    DuplicateToolName,
    InvalidReadinessRevision,
    InconsistentReadiness,
    InvalidTaskRevision,
    InvalidProgress,
    InvalidCompletion,
    DuplicateResultEnvelope,
}

impl fmt::Display for CapabilityContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(value) => {
                write!(f, "unsupported capability-provider schema version: {value}")
            }
            Self::EmptyField(field) => write!(f, "{field} must not be empty"),
            Self::OversizedField(field) => write!(f, "{field} is too long"),
            Self::InvalidSchemaVersion => f.write_str("schema versions must be non-zero"),
            Self::InvalidLimit => f.write_str("capability limits must be non-zero"),
            Self::InvalidHardTimeout => f.write_str("hard timeout is outside product bounds"),
            Self::InvalidForegroundBudget => {
                f.write_str("adaptive foreground budget is outside product bounds")
            }
            Self::MissingSurface => f.write_str("capability has no product surface"),
            Self::DuplicateSurface => f.write_str("capability has duplicate product surfaces"),
            Self::DuplicatePrerequisite => f.write_str("capability has duplicate prerequisites"),
            Self::InconsistentPrerequisite => {
                f.write_str("capability locality and prerequisites disagree")
            }
            Self::DuplicateDataCategory => f.write_str("capability has duplicate data categories"),
            Self::InvalidAuthorizationHint => {
                f.write_str("capability authorization hint is invalid")
            }
            Self::InvalidFallback => f.write_str("capability fallback relation is invalid"),
            Self::SideEffectRequiresDurableLifecycle => {
                f.write_str("side-effect capability requires a durable execution policy")
            }
            Self::MissingCapability => f.write_str("provider has no capabilities"),
            Self::TooManyCapabilities => f.write_str("provider has too many capabilities"),
            Self::DuplicateCapabilityId => f.write_str("provider has a duplicate capability id"),
            Self::DuplicateToolName => f.write_str("provider has a duplicate tool name"),
            Self::InvalidReadinessRevision => f.write_str("readiness revision/time is invalid"),
            Self::InconsistentReadiness => f.write_str("readiness flags and reason disagree"),
            Self::InvalidTaskRevision => f.write_str("task revision/generation is invalid"),
            Self::InvalidProgress => f.write_str("progress event is invalid"),
            Self::InvalidCompletion => f.write_str("completion event is invalid"),
            Self::DuplicateResultEnvelope => {
                f.write_str("capability outcome has duplicate result envelopes")
            }
        }
    }
}

impl std::error::Error for CapabilityContractError {}

fn validate_id(field: &'static str, value: &str) -> Result<(), CapabilityContractError> {
    let value = value.trim();
    if value.is_empty() {
        Err(CapabilityContractError::EmptyField(field))
    } else if value.len() > MAX_PROVIDER_ID_BYTES {
        Err(CapabilityContractError::OversizedField(field))
    } else {
        Ok(())
    }
}

fn validate_result_envelope_ids(values: &[String]) -> Result<(), CapabilityContractError> {
    let mut ids = BTreeSet::new();
    for value in values {
        validate_id("result_envelope_id", value)?;
        if !ids.insert(value.as_str()) {
            return Err(CapabilityContractError::DuplicateResultEnvelope);
        }
    }
    Ok(())
}

fn has_duplicates<T: Ord + Copy>(values: &[T]) -> bool {
    values.iter().copied().collect::<BTreeSet<_>>().len() != values.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(policy: ExecutionPolicy) -> CapabilityWireDescriptor {
        CapabilityWireDescriptor {
            capability_id: "desktop.session.inspect".into(),
            tool_name: "inspect_desktop_session".into(),
            display_name_key: "assistant.capability.desktopSession".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            execution_locality: ExecutionLocality::Edge,
            prerequisites: CapabilityPrerequisites {
                platforms: vec![CapabilityPlatform::Windows],
                applications: Vec::new(),
                requires_edge_connection: true,
                requires_interactive_session: true,
                requires_credential_connection: false,
            },
            execution_policy: policy,
            rate_class: CapabilityRateClass::InteractiveRead,
            limits: CapabilityLimits {
                max_input_bytes: 4_096,
                max_output_bytes: 262_144,
                max_objects: 256,
                hard_timeout_ms: 30_000,
            },
            supports_progress: false,
            supports_cancel: false,
            data_policy: CapabilityDataPolicy {
                reads: vec![CapabilityDataCategory::DesktopSessionMetadata],
                may_export_data: false,
            },
            authorization_hint: CapabilityAuthorizationHint {
                resources: vec![AuthorizationResourceKind::TargetDevice],
            },
            fallback_capability_ids: Vec::new(),
            surfaces: vec![ProductSurface::OssPersonalOwner],
        }
    }

    #[test]
    fn effect_set_contains_all_mutating_authorities_as_side_effects() {
        assert!(CapabilityEffect::WriteExternalDraft.is_side_effecting());
        assert!(CapabilityEffect::ExecuteCommand.is_side_effecting());
        assert!(!CapabilityEffect::ReadDevice.is_side_effecting());
        assert!(serde_json::from_str::<CapabilityEffect>("\"future_effect\"").is_err());
    }

    #[test]
    fn communication_draft_contract_requires_durable_lifecycle() {
        let mut draft = capability(ExecutionPolicy::InlineOnly);
        draft.effect = CapabilityEffect::WriteExternalDraft;
        draft.rate_class = CapabilityRateClass::ExternalWrite;
        assert_eq!(
            draft.validate(),
            Err(CapabilityContractError::SideEffectRequiresDurableLifecycle)
        );
        draft.execution_policy = ExecutionPolicy::DurableRequired;
        assert_eq!(draft.validate(), Ok(()));
    }

    #[test]
    fn adaptive_budget_is_bounded_by_foreground_and_hard_timeout() {
        let mut item = capability(ExecutionPolicy::Adaptive {
            foreground_budget_ms: 5_000,
        });
        assert_eq!(item.validate(), Ok(()));
        item.execution_policy = ExecutionPolicy::Adaptive {
            foreground_budget_ms: 30_000,
        };
        assert_eq!(
            item.validate(),
            Err(CapabilityContractError::InvalidForegroundBudget)
        );
    }

    #[test]
    fn provider_rejects_duplicate_tool_names() {
        let first = capability(ExecutionPolicy::InlineOnly);
        let mut second = first.clone();
        second.capability_id = "desktop.ui.inspect".into();
        let provider = ProviderWireDescriptor {
            schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
            provider_id: "desktop".into(),
            display_name_key: "assistant.provider.desktop".into(),
            provider_version: 1,
            capabilities: vec![first, second],
        };
        assert_eq!(
            provider.validate(),
            Err(CapabilityContractError::DuplicateToolName)
        );
    }

    #[test]
    fn ready_requires_all_prerequisites_and_no_reason() {
        let readiness = CapabilityReadinessReport {
            schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
            provider_id: "desktop".into(),
            capability_id: "desktop.ui.inspect".into(),
            adapter_id: Some("windows.uia".into()),
            adapter_version: Some("1".into()),
            revision: 1,
            observed_at_unix_ms: 10,
            expires_at_unix_ms: 20,
            local_ceiling_revision: 1,
            compiled: true,
            enabled: true,
            connected: false,
            ready: true,
            reason: None,
        };
        assert_eq!(
            readiness.validate(),
            Err(CapabilityContractError::InconsistentReadiness)
        );
    }

    #[test]
    fn browser_inventory_is_secret_free_and_validates_status() {
        let entry = CapabilityInventoryEntry {
            provider_id: "desktop".into(),
            provider_display_name_key: "assistant.provider.desktop".into(),
            provider_version: 1,
            capability: capability(ExecutionPolicy::InlineOnly),
            compiled: true,
            enabled: true,
            connected: false,
            ready: false,
            reason: Some(CapabilityBlockedReason::EdgeDisconnected),
        };
        let snapshot = CapabilityInventorySnapshot {
            schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
            surface: ProductSurface::OssPersonalOwner,
            generated_at_unix_ms: 10,
            entries: vec![entry],
        };
        assert_eq!(snapshot.validate(), Ok(()));
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("parameters_schema"));
        assert!(!json.contains("api_key"));
        assert!(!json.contains("access_token"));
        assert!(!json.contains("\"secret\""));
    }

    #[test]
    fn accepted_task_keeps_server_owned_identity() {
        let task = CapabilityTaskRef {
            task_id: "task".into(),
            call_id: "call".into(),
            run_id: "run".into(),
            provider_id: "provider".into(),
            capability_id: "capability".into(),
            input_revision: 1,
            generation: 1,
        };
        assert_eq!(task.validate(), Ok(()));
        let outcome = CapabilityInvocationOutcome::Accepted { task };
        assert_eq!(outcome.validate(), Ok(()));
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("accepted"));
        assert!(json.contains("call"));
    }

    #[test]
    fn background_events_reject_invalid_sequences_and_duplicate_results() {
        let task = CapabilityTaskRef {
            task_id: "task".into(),
            call_id: "call".into(),
            run_id: "run".into(),
            provider_id: "provider".into(),
            capability_id: "capability".into(),
            input_revision: 1,
            generation: 1,
        };
        let progress = CapabilityProgressEvent {
            task: task.clone(),
            sequence: 0,
            completed_units: None,
            total_units: None,
            message_key: None,
        };
        assert_eq!(
            progress.validate(),
            Err(CapabilityContractError::InvalidProgress)
        );
        let completion = CapabilityCompletionEvent {
            task: task.clone(),
            sequence: 1,
            completion: CapabilityCompletionClass::Succeeded,
            result_envelope_ids: vec!["result-1".into(), "result-1".into()],
        };
        assert_eq!(
            completion.validate(),
            Err(CapabilityContractError::DuplicateResultEnvelope)
        );
        let cancel = CapabilityCancelRequest {
            task,
            request_id: "cancel-1".into(),
            requested_by_actor_id: "actor-1".into(),
            reason: "stop this task".into(),
        };
        assert_eq!(cancel.validate(), Ok(()));
    }
}
