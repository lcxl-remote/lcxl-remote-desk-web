//! Provider-neutral Computer Use protocol types.
//!
//! Observation stays on the read-only [`crate::OperationInput::ReadContext`]
//! lane. A model may produce a [`ComputerActionDraft`], but only the
//! orchestrator can turn an approved draft into a [`SealedComputerActionPlan`].
//! The two types are intentionally unrelated on the wire: no `approved` boolean
//! can upgrade model output into something a worker will execute.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{Capability, RiskLevel};

pub const COMPUTER_USE_SCHEMA_VERSION: u16 = 1;
pub const MAX_COMPUTER_ACTIONS: usize = 32;
pub const MAX_COMPUTER_ACTION_PLAN_BYTES: usize = 256 * 1024;
pub const MAX_COMPUTER_ACTION_TIMEOUT_MS: u32 = 120_000;
pub const MAX_COMPUTER_USE_READINESS_ENTRIES: usize = 64;
pub const MAX_COMPUTER_USE_READINESS_BYTES: usize = 64 * 1024;
pub const MAX_COMPUTER_USE_INSPECT_BYTES: u32 = 1024 * 1024;
pub const MAX_COMPUTER_USE_INSPECT_NODES: u32 = 4096;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    DesktopSession,
    Application,
    Window,
    UiElement,
    OfficeDocument,
    Worksheet,
    Range,
    Presentation,
    Slide,
    Shape,
    File,
    Directory,
    TerminalOutput,
}

/// Short-lived, device-issued reference to an observed object.
///
/// `token` is opaque to the model and must bind the native locator,
/// interactive-session incarnation, adapter version, snapshot generation,
/// fingerprint and expiry in the device-side reference store. Native handles,
/// process ids, paths and coordinates are never authoritative wire inputs.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ObjectRef {
    pub token: String,
    pub snapshot_id: String,
    pub object_kind: ObjectKind,
    /// RFC3339 timestamp. Kept as a string to avoid imposing a clock library on
    /// this pure protocol crate.
    pub expires_at: String,
}

impl std::fmt::Debug for ObjectRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectRef")
            .field("token", &"[redacted]")
            .field("snapshot_id", &self.snapshot_id)
            .field("object_kind", &self.object_kind)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUseAdapterKind {
    WindowsUia,
    MacosAccessibility,
    OfficeExcel,
    OfficePowerPoint,
    FileSystem,
    Terminal,
    ScreenCapture,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerUseAdapterRef {
    pub kind: ComputerUseAdapterKind,
    pub version: String,
}

// ============================ Read-only observation ============================

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DesktopSessionInspectParams {
    pub include_active_application: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DesktopSessionInspectOutput {
    pub session: ObjectRef,
    pub os: String,
    pub interactive_session_incarnation: String,
    pub active_application: Option<ObjectRef>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct UiInspectParams {
    pub root: Option<ObjectRef>,
    pub max_depth: u16,
    pub max_nodes: u32,
    pub max_bytes: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct UiNodeProjection {
    pub object_ref: ObjectRef,
    pub parent_index: Option<u32>,
    pub role: String,
    pub name: Option<String>,
    /// Never contains a password/secure-field value. Adapters omit the value
    /// and set `is_protected` instead.
    pub value: Option<String>,
    pub is_protected: bool,
    pub enabled: bool,
    pub supported_actions: Vec<UiSemanticActionKind>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct UiInspectOutput {
    pub snapshot_id: String,
    pub adapter: ComputerUseAdapterRef,
    pub nodes: Vec<UiNodeProjection>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct OfficeInspectParams {
    pub document: Option<ObjectRef>,
    pub selection_only: bool,
    pub max_objects: u32,
    pub max_bytes: u32,
}

/// Bounded, locale-neutral value projected from one Excel cell. Numbers stay as
/// strings so the protocol preserves the exact Office.js JSON representation
/// without admitting NaN/Infinity or introducing floating-point equality into
/// the version-locked worker wire.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum OfficeCellValue {
    Blank,
    Text(String),
    Number(String),
    Boolean(bool),
    Error(String),
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExcelCellProjection {
    /// Zero-based offset inside the selected range, never a worksheet-global
    /// coordinate or native locator.
    pub row_offset: u32,
    pub column_offset: u32,
    pub formula: Option<String>,
    pub value: OfficeCellValue,
    pub number_format: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum OfficeSelectionProjection {
    Excel {
        document: ObjectRef,
        worksheet: ObjectRef,
        range: ObjectRef,
        address: String,
        row_count: u32,
        column_count: u32,
        has_formulas: bool,
        cells: Vec<ExcelCellProjection>,
    },
    PowerPoint {
        document: ObjectRef,
        slides: Vec<ObjectRef>,
        shapes: Vec<ObjectRef>,
    },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct OfficeInspectOutput {
    pub snapshot_id: String,
    pub adapter: ComputerUseAdapterRef,
    pub selection: OfficeSelectionProjection,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct FileMetadataInspectParams {
    pub roots: Vec<ObjectRef>,
    pub max_entries: u32,
    pub max_bytes: u32,
    /// List immediate children when a selected root is a directory. The edge
    /// never follows a child directory and never accepts a model-provided path.
    #[serde(default)]
    pub enumerate_directories: bool,
    /// Optional, case-insensitive file extensions (including the leading dot)
    /// used to filter immediate file children. Directories are omitted when
    /// any file filter is active. The edge validates this list again.
    #[serde(default)]
    pub file_extensions: Vec<String>,
    #[serde(default)]
    pub min_file_bytes: Option<u64>,
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
    /// Inclusive RFC3339 modification-time bounds for immediate file children.
    #[serde(default)]
    pub modified_after: Option<String>,
    #[serde(default)]
    pub modified_before: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct FileMetadataProjection {
    pub object_ref: ObjectRef,
    pub display_name: String,
    pub is_directory: bool,
    pub byte_len: Option<u64>,
    pub modified_at: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DirectoryEntryProjection {
    pub parent_snapshot_id: String,
    pub display_name: String,
    pub is_directory: bool,
    pub byte_len: Option<u64>,
    pub modified_at: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct FileMetadataInspectOutput {
    pub snapshot_id: String,
    pub entries: Vec<FileMetadataProjection>,
    /// Metadata-only, immediate children of explicitly selected directories.
    /// These rows intentionally carry no reusable object reference.
    #[serde(default)]
    pub directory_entries: Vec<DirectoryEntryProjection>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalOutputInspectParams {
    pub roots: Vec<ObjectRef>,
    pub max_bytes: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalOutputProjection {
    pub snapshot_id: String,
    pub display_summary: String,
    pub content: String,
    pub redaction_count: u32,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct TerminalOutputInspectOutput {
    pub entries: Vec<TerminalOutputProjection>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct FileContentReadParams {
    pub file: ObjectRef,
    pub max_bytes: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct FileContentReadOutput {
    pub file: ObjectRef,
    pub display_name: String,
    pub content_utf8: String,
    pub byte_len: u64,
    pub sha256: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetFileInspectParams {
    pub files: Vec<ObjectRef>,
    pub max_workbooks: u32,
    pub max_sheets: u32,
    pub max_rows: u32,
    pub max_columns: u32,
    pub max_bytes: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SpreadsheetCellKind {
    Blank,
    Text,
    Number,
    Boolean,
    Error,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetCellProjection {
    pub row: u32,
    pub column: u32,
    pub address: String,
    pub kind: SpreadsheetCellKind,
    pub value: String,
    pub formula: Option<String>,
    pub formula_injection_candidate: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetSheetProjection {
    pub name: String,
    pub observed_rows: u32,
    pub observed_columns: u32,
    pub cells: Vec<SpreadsheetCellProjection>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetWorkbookProjection {
    pub display_name: String,
    pub format: String,
    pub byte_len: u64,
    pub sha256: String,
    pub sheets: Vec<SpreadsheetSheetProjection>,
    pub unsupported_features: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetFileInspectOutput {
    pub snapshot_id: String,
    pub workbooks: Vec<SpreadsheetWorkbookProjection>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetMergeColumnRule {
    pub output_header: String,
    pub source_headers: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SpreadsheetStatisticOperation {
    Count,
    Sum,
    Average,
    Min,
    Max,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetStatisticRequest {
    pub operation: SpreadsheetStatisticOperation,
    #[serde(default)]
    pub column: Option<String>,
    #[serde(default)]
    pub group_by: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetMergePreviewParams {
    pub files: Vec<ObjectRef>,
    pub source_sheet: Option<String>,
    pub header_row: u32,
    pub columns: Vec<SpreadsheetMergeColumnRule>,
    pub dedupe_keys: Vec<String>,
    pub statistics: Vec<SpreadsheetStatisticRequest>,
    pub max_rows: u32,
    pub max_bytes: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetRowSource {
    pub workbook_sha256: String,
    pub sheet_name: String,
    pub source_row: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetNamedValue {
    pub name: String,
    pub value: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetStatisticResult {
    pub operation: SpreadsheetStatisticOperation,
    pub column: Option<String>,
    pub group: Vec<SpreadsheetNamedValue>,
    pub value: String,
    pub row_count: u32,
    pub skipped_non_numeric: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SpreadsheetMergePreviewOutput {
    pub preview_id: String,
    pub input_digests_sha256: Vec<String>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub lineage: Vec<SpreadsheetRowSource>,
    pub statistics: Vec<SpreadsheetStatisticResult>,
    pub duplicate_rows_removed: u32,
    pub warnings: Vec<String>,
    pub truncated: bool,
}

// ============================ Draft and sealed mutation ============================

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UiSemanticActionKind {
    Invoke,
    Toggle,
    Select,
    SetValue,
    Scroll,
    Focus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum UiSemanticAction {
    Invoke,
    Toggle { desired: bool },
    Select,
    SetValue { value: String },
    Scroll { horizontal: i32, vertical: i32 },
    Focus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum ExcelPatchAction {
    SetFormula { formula: String },
    FillDown,
    SetValue { value: String },
    SetNumberFormat { format: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum PowerPointPatchAction {
    ReplaceText {
        text: String,
    },
    MoveResizeShape {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
    SetTextStyle {
        font_family: Option<String>,
        font_size: Option<f64>,
        bold: Option<bool>,
        color: Option<String>,
    },
    DeleteShape,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum FilePatchAction {
    /// Create one new UTF-8 artifact relative to an exact edge-issued directory
    /// reference. The worker must use create-new semantics and read the bytes
    /// back through a no-follow handle before reporting success.
    CreateTextArtifact {
        file_name: String,
        content_utf8: String,
    },
    /// Materialize an immutable, worker-retained spreadsheet merge preview as
    /// one new formula-free XLSX artifact. The model cannot supply workbook
    /// rows or bytes; it can only name an unexpired preview and a safe leaf.
    CreateSpreadsheetArtifact {
        preview_id: String,
        file_name: String,
    },
    /// Create a new XLSX copy from a retained merge preview and insert exactly
    /// one formula cell. The caller carries the policy digest produced by the
    /// shared AST validator; the worker recomputes it before writing.
    CreateSpreadsheetFormulaArtifact {
        preview_id: String,
        file_name: String,
        target_cell: String,
        formula: String,
        locale: String,
        formula_policy_digest_sha256: String,
    },
    /// Materialize an immutable, worker-retained spreadsheet merge preview as
    /// one new deterministic, macro-free DOCX report. The model can choose only
    /// a bounded title and safe leaf; it cannot supply document XML or bytes.
    CreateWordReportArtifact {
        preview_id: String,
        file_name: String,
        title: String,
    },
    ApplyTextPatch {
        patch: String,
    },
    Copy {
        destination_parent: ObjectRef,
        new_name: String,
    },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "adapter", content = "action", rename_all = "snake_case")]
pub enum ComputerActionKind {
    Ui(UiSemanticAction),
    Excel(ExcelPatchAction),
    PowerPoint(PowerPointPatchAction),
    File(FilePatchAction),
}

impl ComputerActionKind {
    #[must_use]
    pub const fn required_capability(&self) -> Capability {
        match self {
            Self::Ui(_) => Capability::DesktopUiActionConfirmed,
            Self::Excel(_) => Capability::OfficeExcelPatchConfirmed,
            Self::PowerPoint(_) => Capability::OfficePowerPointPatchConfirmed,
            Self::File(FilePatchAction::Copy { .. }) => Capability::FileCopyConfirmed,
            Self::File(FilePatchAction::CreateTextArtifact { .. }) => {
                Capability::FileArtifactCreateConfirmed
            }
            Self::File(FilePatchAction::CreateSpreadsheetArtifact { .. }) => {
                Capability::SpreadsheetWorkbookCreateConfirmed
            }
            Self::File(FilePatchAction::CreateSpreadsheetFormulaArtifact { .. }) => {
                Capability::SpreadsheetFormulaWorkbookCreateConfirmed
            }
            Self::File(FilePatchAction::CreateWordReportArtifact { .. }) => {
                Capability::WordDocumentCreateConfirmed
            }
            Self::File(FilePatchAction::Delete) => Capability::FileDeleteConfirmed,
            Self::File(FilePatchAction::ApplyTextPatch { .. }) => Capability::FilePatchConfirmed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ComputerActionStep {
    pub target: ObjectRef,
    pub action: ComputerActionKind,
    pub before_summary: String,
    pub after_intent: String,
    pub verification: String,
}

/// Model/orchestrator-local proposal. This type is never accepted by a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ComputerActionDraft {
    pub schema_version: u16,
    pub adapter: ComputerUseAdapterRef,
    pub risk: RiskLevel,
    pub reversible: bool,
    pub data_egress: bool,
    pub actions: Vec<ComputerActionStep>,
}

/// Immutable, exact-owner-approved device execution wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct SealedComputerActionPlan {
    pub schema_version: u16,
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
    pub device_id: String,
    pub interactive_session_incarnation: String,
    pub adapter: ComputerUseAdapterRef,
    pub approval_id: String,
    pub approved_actor_id: String,
    pub draft_hash: String,
    pub expires_at: String,
    pub timeout_ms: u32,
    pub actions: Vec<ComputerActionStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputerUseValidationError {
    UnsupportedSchemaVersion { actual: u16 },
    EmptyField(&'static str),
    EmptyActionBatch,
    TooManyActions { actual: usize, max: usize },
    MixedSnapshots,
    IncompatibleActionAdapter,
    IncompatibleActionTarget,
    InvalidTimeout { actual: u32, max: u32 },
    OversizedPayload { actual: usize, max: usize },
    TooManyReadinessEntries { actual: usize, max: usize },
    InvalidContextReference(&'static str),
    InspectLimitExceeded(&'static str),
}

impl std::fmt::Display for ComputerUseValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { actual } => {
                write!(f, "unsupported Computer Use schema version {actual}")
            }
            Self::EmptyField(field) => write!(f, "Computer Use field `{field}` is empty"),
            Self::EmptyActionBatch => f.write_str("Computer Use action batch is empty"),
            Self::TooManyActions { actual, max } => {
                write!(f, "Computer Use action count {actual} exceeds {max}")
            }
            Self::MixedSnapshots => {
                f.write_str("Computer Use action batch spans multiple observation snapshots")
            }
            Self::IncompatibleActionAdapter => {
                f.write_str("Computer Use action does not match the declared adapter")
            }
            Self::IncompatibleActionTarget => {
                f.write_str("Computer Use action does not match its target object kind")
            }
            Self::InvalidTimeout { actual, max } => {
                write!(f, "Computer Use timeout {actual}ms is outside 1..={max}ms")
            }
            Self::OversizedPayload { actual, max } => {
                write!(
                    f,
                    "Computer Use payload is {actual} bytes; maximum is {max}"
                )
            }
            Self::TooManyReadinessEntries { actual, max } => {
                write!(
                    f,
                    "Computer Use readiness has {actual} entries; maximum is {max}"
                )
            }
            Self::InvalidContextReference(reason) => {
                write!(f, "Computer Use context reference is invalid: {reason}")
            }
            Self::InspectLimitExceeded(field) => {
                write!(
                    f,
                    "Computer Use inspect limit `{field}` exceeds the protocol ceiling"
                )
            }
        }
    }
}

impl std::error::Error for ComputerUseValidationError {}

impl ComputerActionDraft {
    pub fn validate(&self) -> Result<(), ComputerUseValidationError> {
        validate_schema(self.schema_version)?;
        validate_adapter(&self.adapter)?;
        validate_actions(&self.adapter, &self.actions)?;
        validate_encoded_size(self)
    }
}

impl SealedComputerActionPlan {
    pub fn validate(&self) -> Result<(), ComputerUseValidationError> {
        validate_schema(self.schema_version)?;
        for (field, value) in [
            ("work_id", self.work_id.as_str()),
            ("action_request_id", self.action_request_id.as_str()),
            ("execution_generation", self.execution_generation.as_str()),
            ("device_id", self.device_id.as_str()),
            (
                "interactive_session_incarnation",
                self.interactive_session_incarnation.as_str(),
            ),
            ("approval_id", self.approval_id.as_str()),
            ("approved_actor_id", self.approved_actor_id.as_str()),
            ("draft_hash", self.draft_hash.as_str()),
            ("expires_at", self.expires_at.as_str()),
        ] {
            require_non_empty(field, value)?;
        }
        validate_adapter(&self.adapter)?;
        validate_actions(&self.adapter, &self.actions)?;
        if self.timeout_ms == 0 || self.timeout_ms > MAX_COMPUTER_ACTION_TIMEOUT_MS {
            return Err(ComputerUseValidationError::InvalidTimeout {
                actual: self.timeout_ms,
                max: MAX_COMPUTER_ACTION_TIMEOUT_MS,
            });
        }
        validate_encoded_size(self)
    }
}

fn validate_schema(schema_version: u16) -> Result<(), ComputerUseValidationError> {
    if schema_version == COMPUTER_USE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ComputerUseValidationError::UnsupportedSchemaVersion {
            actual: schema_version,
        })
    }
}

fn validate_adapter(adapter: &ComputerUseAdapterRef) -> Result<(), ComputerUseValidationError> {
    require_non_empty("adapter.version", &adapter.version)
}

fn validate_actions(
    adapter: &ComputerUseAdapterRef,
    actions: &[ComputerActionStep],
) -> Result<(), ComputerUseValidationError> {
    if actions.is_empty() {
        return Err(ComputerUseValidationError::EmptyActionBatch);
    }
    if actions.len() > MAX_COMPUTER_ACTIONS {
        return Err(ComputerUseValidationError::TooManyActions {
            actual: actions.len(),
            max: MAX_COMPUTER_ACTIONS,
        });
    }

    let snapshot_id = actions[0].target.snapshot_id.as_str();
    require_non_empty("object_ref.snapshot_id", snapshot_id)?;
    for step in actions {
        let adapter_matches = matches!(
            (&adapter.kind, &step.action),
            (
                ComputerUseAdapterKind::WindowsUia | ComputerUseAdapterKind::MacosAccessibility,
                ComputerActionKind::Ui(_)
            ) | (
                ComputerUseAdapterKind::OfficeExcel,
                ComputerActionKind::Excel(_)
            ) | (
                ComputerUseAdapterKind::OfficePowerPoint,
                ComputerActionKind::PowerPoint(_)
            ) | (
                ComputerUseAdapterKind::FileSystem,
                ComputerActionKind::File(_)
            )
        );
        if !adapter_matches {
            return Err(ComputerUseValidationError::IncompatibleActionAdapter);
        }
        let target_matches = matches!(
            (&step.action, step.target.object_kind),
            (ComputerActionKind::Ui(_), ObjectKind::UiElement)
                | (ComputerActionKind::Excel(_), ObjectKind::Range)
                | (ComputerActionKind::PowerPoint(_), ObjectKind::Shape)
                | (
                    ComputerActionKind::File(
                        FilePatchAction::CreateTextArtifact { .. }
                            | FilePatchAction::CreateSpreadsheetArtifact { .. }
                            | FilePatchAction::CreateSpreadsheetFormulaArtifact { .. }
                            | FilePatchAction::CreateWordReportArtifact { .. }
                    ),
                    ObjectKind::Directory
                )
                | (
                    ComputerActionKind::File(
                        FilePatchAction::ApplyTextPatch { .. }
                            | FilePatchAction::Copy { .. }
                            | FilePatchAction::Delete
                    ),
                    ObjectKind::File
                )
        );
        if !target_matches {
            return Err(ComputerUseValidationError::IncompatibleActionTarget);
        }
        for (field, value) in [
            ("object_ref.token", step.target.token.as_str()),
            ("object_ref.expires_at", step.target.expires_at.as_str()),
            ("before_summary", step.before_summary.as_str()),
            ("after_intent", step.after_intent.as_str()),
            ("verification", step.verification.as_str()),
        ] {
            require_non_empty(field, value)?;
        }
        if step.target.snapshot_id != snapshot_id {
            return Err(ComputerUseValidationError::MixedSnapshots);
        }
    }
    Ok(())
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), ComputerUseValidationError> {
    if value.trim().is_empty() {
        Err(ComputerUseValidationError::EmptyField(field))
    } else {
        Ok(())
    }
}

fn validate_encoded_size<T: Serialize>(value: &T) -> Result<(), ComputerUseValidationError> {
    validate_encoded_size_with_limit(value, MAX_COMPUTER_ACTION_PLAN_BYTES)
}

fn validate_encoded_size_with_limit<T: Serialize>(
    value: &T,
    max: usize,
) -> Result<(), ComputerUseValidationError> {
    let actual = serde_json::to_vec(value)
        .map_err(|_| ComputerUseValidationError::OversizedPayload {
            actual: usize::MAX,
            max,
        })?
        .len();
    if actual > max {
        Err(ComputerUseValidationError::OversizedPayload { actual, max })
    } else {
        Ok(())
    }
}

pub fn validate_ui_inspect_params(
    params: &UiInspectParams,
) -> Result<(), ComputerUseValidationError> {
    if params.max_nodes == 0 || params.max_nodes > MAX_COMPUTER_USE_INSPECT_NODES {
        return Err(ComputerUseValidationError::InspectLimitExceeded(
            "max_nodes",
        ));
    }
    if params.max_bytes == 0 || params.max_bytes > MAX_COMPUTER_USE_INSPECT_BYTES {
        return Err(ComputerUseValidationError::InspectLimitExceeded(
            "max_bytes",
        ));
    }
    Ok(())
}

// ============================ Lifecycle ============================

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComputerActionStartDisposition {
    DefinitelyNotStarted,
    MayHaveStarted,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerActionStarted {
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
    pub disposition: ComputerActionStartDisposition,
    pub reason: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComputerActionResultClass {
    Verified,
    DefinitelyNotStarted,
    OutcomeUnknown,
    StaleObservation,
    PausedByUser,
    ChangedButUnverified,
    PartiallyApplied,
    RollbackUnsafe,
    NotReady,
    Failed,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerActionStepFact {
    pub index: u32,
    pub changed: bool,
    pub verified: bool,
    pub summary: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerActionCompleted {
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
    pub result: ComputerActionResultClass,
    pub facts: Vec<ComputerActionStepFact>,
    pub message: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerActionCancel {
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
    pub reason: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerActionStateQuery {
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComputerActionPhase {
    Approved,
    Dispatching,
    MayHaveStarted,
    CancelRequested,
    Completed,
    OutcomeUnknown,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerActionStateReport {
    pub work_id: String,
    pub action_request_id: String,
    pub execution_generation: String,
    pub phase: ComputerActionPhase,
    pub result: Option<ComputerActionResultClass>,
}

// ============================ Dynamic readiness ============================

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUseReadinessReason {
    DisabledByLocalCeiling,
    UnsupportedPlatform,
    UnsupportedServerVersion,
    NoInteractiveSession,
    NoDisplaySelected,
    AdapterUnavailable,
    PermissionMissing,
    OfficeBridgeNotPaired,
    NoActiveDocument,
    HumanWriterActive,
    AiWriterActive,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerUseCapabilityReadiness {
    pub capability: Capability,
    pub adapter: ComputerUseAdapterRef,
    pub supported: bool,
    pub ready: bool,
    pub reason: Option<ComputerUseReadinessReason>,
}

/// Edge-issued object references that freeze an otherwise dynamic capability
/// target for one bounded Assistant turn. They remain internal to the
/// signal/manager control plane and are never projected into the browser
/// capability directory.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerUseContextReference {
    pub capability: Capability,
    pub object_ref: ObjectRef,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ComputerUseReadiness {
    pub schema_version: u16,
    pub revision: u64,
    pub observed_at: String,
    pub expires_at: String,
    pub server_api_version: i32,
    pub os: String,
    pub interactive_session_incarnation: String,
    pub local_ceiling_revision: u64,
    pub capabilities: Vec<ComputerUseCapabilityReadiness>,
    #[serde(default)]
    pub context_references: Vec<ComputerUseContextReference>,
}

impl ComputerUseReadiness {
    pub fn validate(&self) -> Result<(), ComputerUseValidationError> {
        validate_schema(self.schema_version)?;
        for (field, value) in [
            ("observed_at", self.observed_at.as_str()),
            ("expires_at", self.expires_at.as_str()),
            ("os", self.os.as_str()),
            (
                "interactive_session_incarnation",
                self.interactive_session_incarnation.as_str(),
            ),
        ] {
            require_non_empty(field, value)?;
        }
        if self.capabilities.len() > MAX_COMPUTER_USE_READINESS_ENTRIES {
            return Err(ComputerUseValidationError::TooManyReadinessEntries {
                actual: self.capabilities.len(),
                max: MAX_COMPUTER_USE_READINESS_ENTRIES,
            });
        }
        if self.context_references.len() > MAX_COMPUTER_USE_READINESS_ENTRIES {
            return Err(ComputerUseValidationError::TooManyReadinessEntries {
                actual: self.context_references.len(),
                max: MAX_COMPUTER_USE_READINESS_ENTRIES,
            });
        }
        let mut referenced = std::collections::HashSet::new();
        for reference in &self.context_references {
            for (field, value) in [
                ("context_ref.token", reference.object_ref.token.as_str()),
                (
                    "context_ref.snapshot_id",
                    reference.object_ref.snapshot_id.as_str(),
                ),
                (
                    "context_ref.expires_at",
                    reference.object_ref.expires_at.as_str(),
                ),
            ] {
                require_non_empty(field, value)?;
            }
            if !referenced.insert(reference.capability)
                || !self
                    .capabilities
                    .iter()
                    .any(|entry| entry.capability == reference.capability && entry.ready)
            {
                return Err(ComputerUseValidationError::InvalidContextReference(
                    "capability is duplicated or not ready",
                ));
            }
            if reference.capability == Capability::OfficeDocumentInspect
                && reference.object_ref.object_kind != ObjectKind::OfficeDocument
            {
                return Err(ComputerUseValidationError::InvalidContextReference(
                    "Office capability requires an Office document object",
                ));
            }
        }
        for entry in &self.capabilities {
            validate_adapter(&entry.adapter)?;
            if entry.ready && (!entry.supported || entry.reason.is_some()) {
                return Err(ComputerUseValidationError::EmptyField(
                    "ready capability must be supported and have no reason",
                ));
            }
            if !entry.ready && entry.reason.is_none() {
                return Err(ComputerUseValidationError::EmptyField(
                    "unready capability reason",
                ));
            }
        }
        validate_encoded_size_with_limit(self, MAX_COMPUTER_USE_READINESS_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(token: &str) -> ObjectRef {
        ObjectRef {
            token: token.to_string(),
            snapshot_id: "snapshot-1".to_string(),
            object_kind: ObjectKind::UiElement,
            expires_at: "2026-08-23T12:00:00Z".to_string(),
        }
    }

    fn step(token: &str) -> ComputerActionStep {
        ComputerActionStep {
            target: object(token),
            action: ComputerActionKind::Ui(UiSemanticAction::Invoke),
            before_summary: "button is idle".to_string(),
            after_intent: "invoke the button".to_string(),
            verification: "button state changes".to_string(),
        }
    }

    fn plan() -> SealedComputerActionPlan {
        SealedComputerActionPlan {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            work_id: "work-1".to_string(),
            action_request_id: "action-1".to_string(),
            execution_generation: "generation-1".to_string(),
            device_id: "device-1".to_string(),
            interactive_session_incarnation: "session-1".to_string(),
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::WindowsUia,
                version: "1".to_string(),
            },
            approval_id: "approval-1".to_string(),
            approved_actor_id: "owner-1".to_string(),
            draft_hash: "sha256:draft".to_string(),
            expires_at: "2026-08-23T12:00:00Z".to_string(),
            timeout_ms: 10_000,
            actions: vec![step("token-1")],
        }
    }

    #[test]
    fn sealed_plan_round_trips_and_validates() {
        let plan = plan();
        plan.validate().expect("valid plan");
        let encoded = serde_json::to_string(&plan).expect("encode");
        let decoded: SealedComputerActionPlan = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, plan);
    }

    #[test]
    fn object_ref_debug_redacts_the_opaque_token() {
        let rendered = format!("{:?}", object("secret-token"));
        assert!(!rendered.contains("secret-token"));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn validation_rejects_mixed_snapshots_but_allows_multiple_steps_per_target() {
        let mut same_target = plan();
        same_target.actions.push(step("token-1"));
        same_target
            .validate()
            .expect("one target may have multiple typed steps");

        let mut mixed = plan();
        let mut other = step("token-2");
        other.target.snapshot_id = "snapshot-2".to_string();
        mixed.actions.push(other);
        assert_eq!(
            mixed.validate(),
            Err(ComputerUseValidationError::MixedSnapshots)
        );
    }

    #[test]
    fn mutation_capability_is_derived_from_the_typed_action() {
        assert_eq!(
            ComputerActionKind::Ui(UiSemanticAction::Focus).required_capability(),
            Capability::DesktopUiActionConfirmed
        );
        assert_eq!(
            ComputerActionKind::File(FilePatchAction::Delete).required_capability(),
            Capability::FileDeleteConfirmed
        );
    }

    #[test]
    fn draft_adapter_and_target_must_match_the_typed_action() {
        let mut draft = ComputerActionDraft {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficeExcel,
                version: "office-js-bridge-read/v1".into(),
            },
            risk: RiskLevel::Medium,
            reversible: true,
            data_egress: false,
            actions: vec![step("token-1")],
        };
        assert_eq!(
            draft.validate(),
            Err(ComputerUseValidationError::IncompatibleActionAdapter)
        );
        draft.actions[0].action = ComputerActionKind::Excel(ExcelPatchAction::SetFormula {
            formula: "=1+1".into(),
        });
        assert_eq!(
            draft.validate(),
            Err(ComputerUseValidationError::IncompatibleActionTarget)
        );
        draft.actions[0].target.object_kind = ObjectKind::Range;
        draft.validate().unwrap();
    }

    #[test]
    fn readiness_requires_explicit_unavailable_reason() {
        let readiness = ComputerUseReadiness {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            revision: 1,
            observed_at: "2026-08-23T11:00:00Z".to_string(),
            expires_at: "2026-08-23T11:01:00Z".to_string(),
            server_api_version: 2,
            os: "windows".to_string(),
            interactive_session_incarnation: "session-1".to_string(),
            local_ceiling_revision: 1,
            capabilities: vec![ComputerUseCapabilityReadiness {
                capability: Capability::DesktopUiInspect,
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::WindowsUia,
                    version: "1".to_string(),
                },
                supported: true,
                ready: false,
                reason: None,
            }],
            context_references: Vec::new(),
        };
        assert!(readiness.validate().is_err());
    }

    #[test]
    fn readiness_binds_a_ready_office_capability_to_an_exact_document_ref() {
        let mut document = object("office-document-token");
        document.object_kind = ObjectKind::OfficeDocument;
        let mut readiness = ComputerUseReadiness {
            schema_version: COMPUTER_USE_SCHEMA_VERSION,
            revision: 1,
            observed_at: "2026-08-23T11:00:00Z".into(),
            expires_at: "2026-08-23T11:01:00Z".into(),
            server_api_version: 2,
            os: "windows".into(),
            interactive_session_incarnation: "session-1".into(),
            local_ceiling_revision: 1,
            capabilities: vec![ComputerUseCapabilityReadiness {
                capability: Capability::OfficeDocumentInspect,
                adapter: ComputerUseAdapterRef {
                    kind: ComputerUseAdapterKind::OfficeExcel,
                    version: "office-js-bridge-read/v1".into(),
                },
                supported: true,
                ready: true,
                reason: None,
            }],
            context_references: vec![ComputerUseContextReference {
                capability: Capability::OfficeDocumentInspect,
                object_ref: document,
            }],
        };
        readiness.validate().unwrap();
        readiness.context_references[0].object_ref.object_kind = ObjectKind::File;
        assert!(readiness.validate().is_err());
    }
}
