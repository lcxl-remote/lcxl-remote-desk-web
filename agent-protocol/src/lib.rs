//! Device Capability Protocol — the LCXL-internal contract for what an AI
//! caller may do to a remote device.
//!
//! This crate is **pure protocol**: wire types + the [`DeviceAgent`] trait,
//! with no platform implementation. It is consumed by the signaling facade,
//! the IPC layer (daemon ↔ worker), and the server-side orchestrator.
//!
//! Design invariants (frozen — a change here is breaking and must be
//! re-reviewed):
//!
//! - The protocol is **device-facing and client-agnostic**: it describes what
//!   can be done to a device, not which control end (browser / android / iOS /
//!   MCP client) issued the call.
//! - The **server is the source of truth** for every trusted field
//!   (`request_id`, `target`, `actor`, `scope`, `caller`, final `risk`,
//!   `approval_id`). A control end can never self-report them — the
//!   browser-facing request type ([`AgentRequestData`]) does not even expose
//!   those fields.
//! - The permission point of a read is **derived** from its input
//!   ([`OperationInput::capability`]) so the capability, collector dispatch,
//!   and audit can never disagree.
//!
//! All wire types derive `serde` (JSON, for the control-end signaling wire),
//! `wincode` (for the daemon ↔ worker IPC wire), and `utoipa::ToSchema` (for
//! OpenAPI).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

pub mod agent_event;
pub mod audit;
pub mod authz;
pub mod browser_control;
pub mod capability_grant;
pub mod capability_provider;
pub mod command_blocklist;
pub mod command_template;
pub mod communication;
pub mod computer_use;
pub mod content_safety;
pub mod data_lineage;
pub mod device_assistant;
pub mod diagnose;
pub mod edge_exec;
pub mod evidence;
pub mod exec;
pub mod exec_lifecycle;
pub mod exec_policy;
pub mod exec_pty;
pub mod exec_pty_wire;
pub mod model_proxy;
pub mod provenance;
pub mod remote_tool;
pub mod terminal_complete;
pub mod terminal_copilot;
pub mod visual_evidence;

use crate::exec::{CommandClassification, ExecDecision, ExecEffect, ExecIoMode};

// ============================ Envelope ============================

/// Single envelope for every AI capability call. Structurally identical
/// across the single-machine (signaling) and fleet (manager fan-out) paths —
/// only the outer transport wrapper differs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct AgentEnvelope {
    /// Protocol version for consumer-side negotiation. Server-stamped.
    pub protocol_version: ProtocolVersion,
    /// Server-generated / server-validated. A control-end value is ignored.
    pub request_id: RequestId,
    /// Correlates all child calls of one AI task (fleet fan-out, multi-step
    /// diagnose). Server-owned.
    pub parent_task_id: Option<TaskId>,
    /// Routing target. **Server-resolved** (single machine: from the current
    /// connection/session; fleet: from the manager device selector). Never
    /// self-reported by the control end.
    pub target: TargetRef,
    /// **Server-injected** from session/manager auth.
    pub actor: ActorRef,
    /// Caller (model) metadata — audit only, carries no authority, and is
    /// **server-injected** (the server-side orchestrator knows the real
    /// provider/model/adapter; never trusted from the control end).
    pub caller: CallerRef,
    /// **Server-computed** grant. `granted` / `mode` are authoritative from
    /// the policy engine.
    pub scope: AgentScope,
    pub operation: AgentOperation,
    pub audit: AuditMeta,
}

/// Structurally read-only form used by the remote-tool and daemon→worker
/// capability lanes. Unlike [`AgentEnvelope`], it cannot represent
/// [`OperationInput::Exec`] or any sealed Computer Use mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ReadonlyAgentEnvelope {
    pub protocol_version: ProtocolVersion,
    pub request_id: RequestId,
    pub parent_task_id: Option<TaskId>,
    pub target: TargetRef,
    pub actor: ActorRef,
    pub caller: CallerRef,
    pub scope: AgentScope,
    pub operation: ReadonlyAgentOperation,
    pub audit: AuditMeta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ReadonlyAgentOperation {
    pub risk_hint: Option<RiskLevel>,
    pub input: ReadContextInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationInReadonlyLane;

impl std::fmt::Display for MutationInReadonlyLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mutation cannot enter the read-only agent capability lane")
    }
}

impl std::error::Error for MutationInReadonlyLane {}

impl TryFrom<AgentEnvelope> for ReadonlyAgentEnvelope {
    type Error = MutationInReadonlyLane;

    fn try_from(envelope: AgentEnvelope) -> Result<Self, Self::Error> {
        let OperationInput::ReadContext(input) = envelope.operation.input else {
            return Err(MutationInReadonlyLane);
        };
        Ok(Self {
            protocol_version: envelope.protocol_version,
            request_id: envelope.request_id,
            parent_task_id: envelope.parent_task_id,
            target: envelope.target,
            actor: envelope.actor,
            caller: envelope.caller,
            scope: envelope.scope,
            operation: ReadonlyAgentOperation {
                risk_hint: envelope.operation.risk_hint,
                input,
            },
            audit: envelope.audit,
        })
    }
}

impl From<ReadonlyAgentEnvelope> for AgentEnvelope {
    fn from(envelope: ReadonlyAgentEnvelope) -> Self {
        Self {
            protocol_version: envelope.protocol_version,
            request_id: envelope.request_id,
            parent_task_id: envelope.parent_task_id,
            target: envelope.target,
            actor: envelope.actor,
            caller: envelope.caller,
            scope: envelope.scope,
            operation: AgentOperation {
                risk_hint: envelope.operation.risk_hint,
                input: OperationInput::ReadContext(envelope.operation.input),
            },
            audit: envelope.audit,
        }
    }
}

/// `"0.2"`-style protocol tag. Newtype so negotiation logic has a type to
/// hang on; serializes transparently as the inner string.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(transparent)]
pub struct ProtocolVersion(pub String);

/// Current protocol version this build speaks.
pub const PROTOCOL_VERSION: &str = "0.2";

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self(PROTOCOL_VERSION.to_string())
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(transparent)]
pub struct RequestId(pub String);

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(transparent)]
pub struct TaskId(pub String);

/// Which device/session/worker this call lands on. `session_id` / `worker_id`
/// are `None` on the fleet path where resolution happens later.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
pub struct TargetRef {
    pub device_id: String,
    pub session_id: Option<String>,
    pub worker_id: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ActorRef {
    pub actor_type: ActorType,
    pub actor_id: String,
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
pub enum ActorType {
    User,
    Service,
    System,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CallerRef {
    pub caller_type: CallerType,
    pub model_provider: Option<String>,
    pub model_name: Option<String>,
    pub adapter: Option<String>,
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
pub enum CallerType {
    AiModel,
    Human,
    McpClient,
}

/// Audit metadata that travels with the call. `policy_name` is intentionally
/// **not** here — its single source of truth is [`AgentScope::policy_name`].
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AuditMeta {
    /// Must originate from the LCXL confirmation flow; model-supplied values
    /// are rejected.
    pub approval_id: Option<String>,
    /// Free-text "why" from the control end; flows into the audit event.
    pub reason: Option<String>,
}

// ============================ Scope ============================

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AgentScope {
    pub granted: Vec<Capability>,
    pub mode: ExecutionMode,
    /// RFC3339 timestamp; kept as `String` so this crate stays free of a
    /// `chrono` dependency.
    pub expires_at: Option<String>,
    pub policy_name: Option<String>,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// No execution at all — the AI may only suggest commands. The safe default.
    #[default]
    SuggestOnly,
    ReadOnly,
    ConfirmEachAction,
    SessionApproved,
    Automated,
}

impl ExecutionMode {
    /// Executable breadth as an explicit rank: how much the mode lets the model
    /// actually run, from none (`SuggestOnly`) to unattended (`Automated`). Kept
    /// as a private helper rather than a derived `Ord` so reordering the variants
    /// cannot silently change the ceiling semantics below.
    fn breadth_rank(self) -> u8 {
        match self {
            ExecutionMode::SuggestOnly => 0,
            ExecutionMode::ReadOnly => 1,
            ExecutionMode::ConfirmEachAction => 2,
            ExecutionMode::SessionApproved => 3,
            ExecutionMode::Automated => 4,
        }
    }

    /// Clamp this mode so it permits no more execution than `ceiling` does,
    /// returning the stricter (narrower breadth) of the two. Used to make a
    /// locally configured `execution_mode` an upper bound on a manager-issued
    /// authorization mode: the result is always `⊆ self` and `⊆ ceiling`, so a
    /// remote policy can narrow but never widen what runs on the device.
    #[must_use]
    pub fn restrict_to(self, ceiling: ExecutionMode) -> ExecutionMode {
        if ceiling.breadth_rank() < self.breadth_rank() {
            ceiling
        } else {
            self
        }
    }
}

/// Risk ordering is monotone in declaration order (`Low < … < Blocked`), so
/// the policy engine can express "require confirmation at or above threshold"
/// with a plain comparison.
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
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
    Blocked,
}

// ============================ Capability ============================

/// Dotted capability name. A closed enum so scope/audit matching is
/// exhaustive and typo-proof; serde uses the dotted string form on the wire.
///
/// This type only ever crosses the version-locked daemon ↔ worker IPC (inside
/// [`AgentEnvelope`]) and the server-side audit string column — it is not part
/// of the control-end request wire (that carries [`OperationInput`], from
/// which the capability is derived). So the closed-set representation is safe
/// across version skew. `shell.plan` is intentionally absent (it is an
/// orchestrator-layer concern, recorded in audit as a free-form string).
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
pub enum Capability {
    #[serde(rename = "system.info")]
    SystemInfo,
    #[serde(rename = "process.list")]
    ProcessList,
    #[serde(rename = "network.ports")]
    NetworkPorts,
    #[serde(rename = "service.status")]
    ServiceStatus,
    #[serde(rename = "log.recent")]
    LogRecent,
    #[serde(rename = "container.list")]
    ContainerList,
    #[serde(rename = "container.inspect")]
    ContainerInspect,
    #[serde(rename = "container.logs")]
    ContainerLogs,
    #[serde(rename = "screen.capture.current")]
    ScreenCaptureCurrent,
    #[serde(rename = "desktop.session.inspect")]
    DesktopSessionInspect,
    #[serde(rename = "desktop.ui.inspect")]
    DesktopUiInspect,
    #[serde(rename = "office.document.inspect")]
    OfficeDocumentInspect,
    #[serde(rename = "spreadsheet.live.inspect")]
    SpreadsheetLiveInspect,
    #[serde(rename = "document.live.inspect")]
    DocumentLiveInspect,
    #[serde(rename = "presentation.live.inspect")]
    PresentationLiveInspect,
    #[serde(rename = "file.metadata.read")]
    FileMetadataRead,
    #[serde(rename = "file.content.read")]
    FileContentRead,
    #[serde(rename = "spreadsheet.file.inspect")]
    SpreadsheetFileInspect,
    #[serde(rename = "spreadsheet.merge.preview")]
    SpreadsheetMergePreview,
    #[serde(rename = "spreadsheet.workbook.create.confirmed")]
    SpreadsheetWorkbookCreateConfirmed,
    /// Create a new XLSX copy from a retained merge preview with one formula
    /// cell accepted by the frozen formula AST policy. This is batch file
    /// generation, not Excel Live mutation.
    #[serde(rename = "spreadsheet.formula_workbook.create.confirmed")]
    SpreadsheetFormulaWorkbookCreateConfirmed,
    #[serde(rename = "word.document.create.confirmed")]
    WordDocumentCreateConfirmed,
    /// Fetch one exact public HTTPS URL supplied verbatim by the owner. The
    /// central provider applies strict connect-time SSRF checks and bounded
    /// content extraction; this capability never reaches the device edge.
    #[serde(rename = "web.research.fetch")]
    WebResearchFetch,
    /// Search public Web metadata through one server-owned, bounded connector.
    /// The query is an external data egress and therefore needs an explicit
    /// exact-input ExportData grant before the central provider can run.
    #[serde(rename = "web.research.search")]
    WebResearchSearch,
    #[serde(rename = "terminal.output.read")]
    TerminalOutputRead,
    /// Central-only validation/display of an inert typed action proposal. This
    /// capability never authorizes a device read or mutation transport.
    #[serde(rename = "assistant.action.preview")]
    AssistantActionPreview,
    // Execute capabilities are reserved; their wire shape is frozen.
    #[serde(rename = "shell.exec.readonly")]
    ShellExecReadonly,
    #[serde(rename = "shell.exec.confirmed")]
    ShellExecConfirmed,
    #[serde(rename = "desktop.ui.action.confirmed")]
    DesktopUiActionConfirmed,
    #[serde(rename = "desktop.input.fallback.confirmed")]
    DesktopInputFallbackConfirmed,
    #[serde(rename = "office.excel.patch.confirmed")]
    OfficeExcelPatchConfirmed,
    #[serde(rename = "office.powerpoint.patch.confirmed")]
    OfficePowerPointPatchConfirmed,
    #[serde(rename = "spreadsheet.live.patch.confirmed")]
    SpreadsheetLivePatchConfirmed,
    #[serde(rename = "document.live.patch.confirmed")]
    DocumentLivePatchConfirmed,
    #[serde(rename = "presentation.live.patch.confirmed")]
    PresentationLivePatchConfirmed,
    #[serde(rename = "file.patch.confirmed")]
    FilePatchConfirmed,
    #[serde(rename = "file.copy.confirmed")]
    FileCopyConfirmed,
    #[serde(rename = "file.artifact.create.confirmed")]
    FileArtifactCreateConfirmed,
    #[serde(rename = "communication.local_draft.create.confirmed")]
    CommunicationLocalDraftCreateConfirmed,
    /// Open the Windows Outlook (new) compose surface with bounded, plain-text
    /// fields and stop for manual review. This may create a cloud-synchronised
    /// draft, but never carries send authority.
    #[serde(rename = "communication.outlook_new.handoff.confirmed")]
    CommunicationOutlookNewHandoffConfirmed,
    /// Read a bounded semantic projection from one browser page already bound
    /// to the current device, OS session, profile and connection revision.
    #[serde(rename = "browser.page.observe")]
    BrowserPageObserve,
    /// Open or navigate one browser page to an exact canonical target.
    #[serde(rename = "browser.page.navigate.confirmed")]
    BrowserPageNavigateConfirmed,
    /// Generic browser form input or activation whose business effect cannot
    /// be proven by a reviewed site adapter.
    #[serde(rename = "browser.input.fallback.confirmed")]
    BrowserInputFallbackConfirmed,
    /// Create or modify a cloud-synchronised draft through a reviewed browser
    /// site adapter. This never includes final delivery.
    #[serde(rename = "browser.external_draft.write.confirmed")]
    BrowserExternalDraftWriteConfirmed,
    /// Deliver one exact, read-back-verified browser payload.
    #[serde(rename = "browser.external.send.confirmed")]
    BrowserExternalSendConfirmed,
    #[serde(rename = "file.delete.confirmed")]
    FileDeleteConfirmed,
}

// ============================ Operation ============================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct AgentOperation {
    /// Client-suggested risk; **non-authoritative**. The server re-classifies.
    pub risk_hint: Option<RiskLevel>,
    pub input: OperationInput,
}

/// Tagged union of per-capability inputs. Adding a variant is additive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum OperationInput {
    ReadContext(ReadContextInput),
    /// Rejected by raw agent-request routing; confirmed execution uses its
    /// dedicated preview and approval flow.
    Exec(ExecInput),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub enum OperationOutput {
    ReadContext(ReadContextOutput),
    Exec(ExecOutput),
}

impl OperationInput {
    /// Single source of truth for the permission point of **reads**. The
    /// server matches the returned capability against `scope.granted` and
    /// stamps the audit `capability` — there is no separate client-supplied
    /// field that could drift from the input.
    ///
    /// Returns `Option` because exec's required capability is **not derivable
    /// from the input alone**: the `shell.exec.readonly` vs
    /// `shell.exec.confirmed` split is the output of server-side risk
    /// classification, not a wire choice. Hard-coding a value here would break
    /// authz (always returning `ShellExecConfirmed` would reject a user who
    /// only holds `shell.exec.readonly` before the server could classify the
    /// command). Therefore `Exec` returns `None`; callers must use
    /// `required_capability(classification)` after classification.
    pub fn capability(&self) -> Option<Capability> {
        match self {
            OperationInput::ReadContext(rc) => Some(match &rc.kind {
                ContextKind::SystemInfo(_) => Capability::SystemInfo,
                ContextKind::ProcessList(_) => Capability::ProcessList,
                ContextKind::NetworkPorts(_) => Capability::NetworkPorts,
                ContextKind::ServiceStatus(_) => Capability::ServiceStatus,
                ContextKind::LogRecent(_) => Capability::LogRecent,
                ContextKind::ContainerList(_) => Capability::ContainerList,
                ContextKind::ContainerInspect(_) => Capability::ContainerInspect,
                ContextKind::ContainerLogs(_) => Capability::ContainerLogs,
                ContextKind::ScreenCaptureCurrent(_) => Capability::ScreenCaptureCurrent,
                ContextKind::DesktopSessionInspect(_) => Capability::DesktopSessionInspect,
                ContextKind::DesktopUiInspect(_) => Capability::DesktopUiInspect,
                ContextKind::OfficeDocumentInspect(_) => Capability::OfficeDocumentInspect,
                ContextKind::SpreadsheetLiveInspect(_) => Capability::SpreadsheetLiveInspect,
                ContextKind::DocumentLiveInspect(_) => Capability::DocumentLiveInspect,
                ContextKind::PresentationLiveInspect(_) => Capability::PresentationLiveInspect,
                ContextKind::FileMetadataInspect(_) => Capability::FileMetadataRead,
                ContextKind::FileContentRead(_) => Capability::FileContentRead,
                ContextKind::SpreadsheetFileInspect(_) => Capability::SpreadsheetFileInspect,
                ContextKind::SpreadsheetMergePreview(_) => Capability::SpreadsheetMergePreview,
                ContextKind::TerminalOutputInspect(_) => Capability::TerminalOutputRead,
            }),
            OperationInput::Exec(_) => None,
        }
    }

    /// Exec capability mapping. The `shell.exec.readonly` vs
    /// `shell.exec.confirmed` split is **not** derivable from the wire input —
    /// it is the output of server-side risk classification — so it is resolved
    /// here from a [`CommandClassification`] instead.
    ///
    /// This is an **associated function**, not a `self` method: the classified
    /// effect, not the raw `ExecInput`, decides the capability. It is called
    /// **only inside the daemon confirm flow** (never fed back into the raw
    /// read-only [`OperationInput::capability`] gate). Returns `None` when the
    /// command is not executable through the AI path (blocked / off-template),
    /// which carries no executable capability.
    pub fn required_capability(classification: &CommandClassification) -> Option<Capability> {
        match (classification.decision, classification.effect) {
            (ExecDecision::ConfirmRequired, Some(ExecEffect::ReadOnly)) => {
                Some(Capability::ShellExecReadonly)
            }
            (ExecDecision::ConfirmRequired, Some(ExecEffect::Mutating)) => {
                Some(Capability::ShellExecConfirmed)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ReadContextInput {
    pub kind: ContextKind,
}

/// One read kind + its per-kind params. The [`Capability`] enum names the
/// permission point; this enum carries the query shape. Per-kind param/output
/// fields are additive (new optional fields do not break the skeleton).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum ContextKind {
    SystemInfo(SystemInfoParams),
    ProcessList(ProcessListParams),
    NetworkPorts(NetworkPortsParams),
    ServiceStatus(ServiceStatusParams),
    LogRecent(LogRecentParams),
    ContainerList(ContainerListParams),
    ContainerInspect(ContainerInspectParams),
    ContainerLogs(ContainerLogsParams),
    ScreenCaptureCurrent(ScreenCaptureParams),
    DesktopSessionInspect(computer_use::DesktopSessionInspectParams),
    DesktopUiInspect(computer_use::UiInspectParams),
    OfficeDocumentInspect(computer_use::OfficeInspectParams),
    SpreadsheetLiveInspect(computer_use::LiveDocumentInspectParams),
    DocumentLiveInspect(computer_use::LiveDocumentInspectParams),
    PresentationLiveInspect(computer_use::LiveDocumentInspectParams),
    FileMetadataInspect(computer_use::FileMetadataInspectParams),
    FileContentRead(computer_use::FileContentReadParams),
    SpreadsheetFileInspect(computer_use::SpreadsheetFileInspectParams),
    SpreadsheetMergePreview(computer_use::SpreadsheetMergePreviewParams),
    TerminalOutputInspect(computer_use::TerminalOutputInspectParams),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub enum ReadContextOutput {
    SystemInfo(SystemInfoOutput),
    ProcessList(ProcessListOutput),
    NetworkPorts(NetworkPortsOutput),
    ServiceStatus(ServiceStatusOutput),
    LogRecent(LogRecentOutput),
    ContainerList(ContainerListOutput),
    ContainerInspect(ContainerInspectOutput),
    ContainerLogs(ContainerLogsOutput),
    ScreenCaptureCurrent(ScreenCaptureOutput),
    DesktopSessionInspect(computer_use::DesktopSessionInspectOutput),
    DesktopUiInspect(computer_use::UiInspectOutput),
    OfficeDocumentInspect(computer_use::OfficeInspectOutput),
    SpreadsheetLiveInspect(computer_use::LiveDocumentInspectOutput),
    DocumentLiveInspect(computer_use::LiveDocumentInspectOutput),
    PresentationLiveInspect(computer_use::LiveDocumentInspectOutput),
    FileMetadataInspect(computer_use::FileMetadataInspectOutput),
    FileContentRead(computer_use::FileContentReadOutput),
    SpreadsheetFileInspect(computer_use::SpreadsheetFileInspectOutput),
    SpreadsheetMergePreview(computer_use::SpreadsheetMergePreviewOutput),
    TerminalOutputInspect(computer_use::TerminalOutputInspectOutput),
}

// -------- read params (fields are additive) --------

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct SystemInfoParams {
    #[serde(default)]
    pub include_hardware: bool,
    #[serde(default)]
    pub include_network_summary: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ProcessListParams {
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub sort: ProcessSort,
    #[serde(default)]
    pub include_command_line: bool,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSort {
    #[default]
    CpuDesc,
    MemoryDesc,
    Pid,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct NetworkPortsParams {
    /// Filter to a single transport; `None` returns both.
    pub protocol: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ServiceStatusParams {
    /// Specific service to query; `None` enumerates.
    pub name: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct LogRecentParams {
    pub source: Option<String>,
    pub since_minutes: Option<u32>,
    pub limit: Option<u32>,
    #[serde(default)]
    pub severity: Vec<LogSeverity>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LogSeverity {
    Error,
    Warning,
    Info,
    Debug,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerListParams {}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerInspectParams {
    pub container_id: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerLogsParams {
    pub container_id: String,
    pub since_minutes: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ScreenCaptureParams {
    /// Display name to capture; `None` captures the primary / current target.
    pub display: Option<String>,
    /// Exact owner-attached, edge-issued window reference. Model-authored
    /// window handles, titles, process ids and coordinates are never accepted.
    #[serde(default)]
    pub window: Option<computer_use::ObjectRef>,
}

// -------- read outputs (carry the frozen `truncated` / `redactions`) --------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct SystemInfoOutput {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub uptime_seconds: u64,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub disks: Vec<DiskInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct CpuInfo {
    pub usage_percent: f32,
    pub logical_cores: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct DiskInfo {
    pub mount: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ProcessListOutput {
    pub processes: Vec<ProcessEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    pub user: Option<String>,
    #[serde(default)]
    pub command_line_redacted: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct NetworkPortsOutput {
    pub ports: Vec<PortEntry>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct PortEntry {
    pub protocol: String,
    pub local_address: String,
    pub local_port: u16,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ServiceStatusOutput {
    pub services: Vec<ServiceEntry>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ServiceEntry {
    pub name: String,
    pub display_name: Option<String>,
    pub state: String,
    pub start_type: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct LogRecentOutput {
    pub events: Vec<LogEvent>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct LogEvent {
    pub timestamp: String,
    pub source: String,
    pub severity: LogSeverity,
    pub message: String,
    /// Names of fields scrubbed from `message` (e.g. `["path", "username"]`).
    #[serde(default)]
    pub redactions: Vec<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerListOutput {
    pub containers: Vec<ContainerSummary>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerInspectOutput {
    pub container_id: String,
    /// Raw inspect document as a JSON string. Kept as `String` (not
    /// `serde_json::Value`) so the IPC `wincode` wire stays a plain byte
    /// buffer. May contain secrets, hence `redactions`.
    pub details_json: String,
    #[serde(default)]
    pub redactions: Vec<String>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ContainerLogsOutput {
    pub lines: Vec<String>,
    #[serde(default)]
    pub redactions: Vec<String>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ScreenCaptureOutput {
    /// Exact owner-selected display identifier used by the edge capture
    /// backend. Raw-input fallback may bind to this value but cannot replace
    /// it with a model-selected target.
    pub display: String,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
    /// Physical display DPI observed with this frame. A raw-input action must
    /// present the same values and the edge rechecks them before injection.
    pub dpi_x: u32,
    pub dpi_y: u32,
    /// Present only when the edge captured the exact owner-attached window.
    #[serde(default)]
    pub window: Option<computer_use::ObjectRef>,
    /// Encoded image bytes (per `format`). Truncated outputs set `truncated`.
    pub image: Vec<u8>,
    pub truncated: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

// -------- exec request shape --------

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecInput {
    pub target: ExecTarget,
    pub command: String,
    pub cwd: Option<String>,
    pub io_mode: ExecIoMode,
    pub timeout_ms: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
}

/// Open-ended exec target. `Shell` carries shell commands; `Domain` reserves
/// domain-specific tools without requiring a wire-shape break.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecTarget {
    Shell { shell: String },
    Domain { tool: String, args: Vec<String> },
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub streams: ExecOutputStreams,
    pub duration_ms: u32,
    #[serde(default)]
    pub redactions: Vec<String>,
}

/// Output shape follows the sealed I/O mode. A PTY has one combined terminal
/// byte stream; projecting it into stdout with an empty stderr would create a
/// false distinction and is intentionally not supported.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecOutputStreams {
    Split {
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    PtyCombined {
        terminal: String,
        truncated: bool,
    },
}

// ============================ Error ============================

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AgentError {
    pub kind: AgentErrorKind,
    /// Human-readable. When `safe_for_model = false` the orchestrator must
    /// hand the model a generic message instead of this one, so policy detail
    /// does not leak into the prompt. The control-end UI always receives the
    /// full message (the `safe_for_model` gate is on the server → model edge,
    /// not the server → control-end edge).
    pub message: String,
    pub retryable: bool,
    pub safe_for_model: bool,
    /// Optional machine-readable business code (a `DeskErrorCode` value) so the
    /// control end can localize the error instead of showing the raw English
    /// `message`. `None` for errors without a dedicated code (the UI falls back
    /// to `message`). Optional on the wire so both roles — the manager and the
    /// open-source single-instance signal — interoperate unchanged; an older or
    /// code-less producer simply omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<i32>,
}

impl AgentError {
    /// Attach a machine-readable business code (a `DeskErrorCode` value) so the
    /// control end can localize the error. Chainable on any constructed error.
    pub fn with_error_code(mut self, code: i32) -> Self {
        self.error_code = Some(code);
        self
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
pub enum AgentErrorKind {
    PermissionDenied,
    ApprovalRequired,
    RiskBlocked,
    UnsupportedCapability,
    UnsupportedPlatform,
    TargetOffline,
    SessionUnavailable,
    /// The host is already running as many commands as it permits. Nothing ran,
    /// and the condition is transient — distinct from a refusal on policy grounds,
    /// which will not become permitted by waiting.
    HostAtCapacity,
    Timeout,
    /// The command was stopped on request and its process tree reclaimed. It did
    /// start, so this says nothing about how much of its effect landed before it
    /// was stopped — a mutating command cancelled mid-flight needs review, not a
    /// retry.
    Cancelled,
    OutputLimitExceeded,
    InvalidInput,
    RedactionFailed,
    TransportError,
    /// A closed content-policy decision rejected the request or model turn.
    /// Waiting cannot make the same content permissible.
    ContentBlocked,
    /// The required content-safety service could not produce a trustworthy
    /// verdict. Protected manager surfaces fail closed and may retry.
    ContentSafetyUnavailable,
    Internal,
}

// ============================ Wire (control end ↔ server) ============================

/// Browser-facing AI request body (control end → server). The control end can
/// only express the non-authoritative parts; the server assembles the full
/// [`AgentEnvelope`] by injecting target/actor/scope/caller/request_id. This
/// type structurally cannot carry a trusted field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AgentRequestData {
    pub operation: AgentOperation,
    /// Free-text "why"; flows into `AgentEnvelope.audit.reason`.
    pub reason: Option<String>,
    /// Manager-only org context hint: the id of the organization the operator is
    /// acting within. NON-authoritative — the manager validates the operator's
    /// membership in this org AND the org's device-access grant to the target
    /// device before trusting it, then adjudicates the request against that single
    /// org's policy. The open-source single-instance desk-server has no org concept
    /// and **ignores** this field; `None` (the default, sent by every non-manager
    /// client) is the personal view.
    #[serde(default)]
    pub org_id: Option<i32>,
}

/// Result of one capability call. Reused verbatim by the IPC layer so the
/// daemon ↔ worker reply and the control-end reply share one shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
#[serde(tag = "status", content = "data")]
pub enum AgentOutcome {
    Ok(OperationOutput),
    Err(AgentError),
}

// ============================ Trait ============================

/// Device-side capability surface. One async dispatch over [`AgentEnvelope`].
/// Implemented by DeskServer / SessionWorker for read context. Confirmed
/// execution uses its dedicated preview and approval flow.
///
/// `#[async_trait]` (boxed future) is used deliberately instead of RPITIT: the
/// server-side orchestrator/router needs `Arc<dyn DeviceAgent>` (a registry of
/// per-target agents, a policy/audit decorator wrapping the inner agent) and
/// tests need a mock impl — all of which require object-safety. RPITIT
/// (`-> impl Future`) is not object-safe. This is a low-frequency
/// control-plane call, so the boxed-future allocation is acceptable.
#[async_trait::async_trait]
pub trait DeviceAgent: Send + Sync {
    async fn invoke(&self, envelope: AgentEnvelope) -> Result<OperationOutput, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    #[test]
    fn restrict_to_clamps_to_the_stricter_mode() {
        use ExecutionMode::*;
        let modes = [
            SuggestOnly,
            ReadOnly,
            ConfirmEachAction,
            SessionApproved,
            Automated,
        ];
        // The full breadth lattice: restrict_to returns the narrower of the two,
        // and is symmetric. Indices match `breadth_rank`.
        for (i, &a) in modes.iter().enumerate() {
            for (j, &b) in modes.iter().enumerate() {
                let expected = modes[i.min(j)];
                assert_eq!(a.restrict_to(b), expected, "{a:?}.restrict_to({b:?})");
                assert_eq!(b.restrict_to(a), expected, "{b:?}.restrict_to({a:?})");
            }
        }
        // Spot-check the security-critical rows: a SuggestOnly / ReadOnly local
        // ceiling caps a broad manager authorization.
        assert_eq!(ConfirmEachAction.restrict_to(SuggestOnly), SuggestOnly);
        assert_eq!(Automated.restrict_to(ReadOnly), ReadOnly);
        assert_eq!(
            SessionApproved.restrict_to(ConfirmEachAction),
            ConfirmEachAction
        );
        // A looser ceiling is a no-op.
        assert_eq!(ReadOnly.restrict_to(Automated), ReadOnly);
    }

    /// Locally constructed wincode config matching the IPC transport's
    /// (unbounded, preallocation limit disabled). Deliberately built here
    /// rather than importing `desk_ipc_protocol::transport::IPC_CONFIG` —
    /// `ipc-protocol` depends on this crate, so the reverse dependency would
    /// be a cycle.
    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    fn sample_envelope(input: OperationInput) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("req_1".into()),
            parent_task_id: Some(TaskId("task_1".into())),
            target: TargetRef {
                device_id: "dev_1".into(),
                session_id: Some("sess_1".into()),
                worker_id: None,
            },
            actor: ActorRef {
                actor_type: ActorType::User,
                actor_id: "user_1".into(),
            },
            caller: CallerRef {
                caller_type: CallerType::AiModel,
                model_provider: Some("example".into()),
                model_name: Some("example-model".into()),
                adapter: Some("lcxl-openai-tools".into()),
            },
            scope: AgentScope {
                granted: vec![Capability::ProcessList, Capability::SystemInfo],
                mode: ExecutionMode::ReadOnly,
                expires_at: Some("2026-06-12T18:00:00Z".into()),
                policy_name: Some("policy_default".into()),
            },
            operation: AgentOperation {
                risk_hint: Some(RiskLevel::Low),
                input,
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("AI Diagnose requested by user".into()),
            },
        }
    }

    #[test]
    fn envelope_json_round_trips() {
        let env = sample_envelope(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ProcessList(ProcessListParams {
                limit: 50,
                sort: ProcessSort::CpuDesc,
                include_command_line: false,
            }),
        }));
        let json = serde_json::to_string(&env).expect("encode");
        let back: AgentEnvelope = serde_json::from_str(&json).expect("decode");
        assert_eq!(env, back);
    }

    #[test]
    fn envelope_wincode_round_trips() {
        let config = unbounded_config();
        let env = sample_envelope(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SystemInfo(SystemInfoParams {
                include_hardware: true,
                include_network_summary: true,
            }),
        }));
        let bytes = wincode::config::serialize(&env, config).expect("encode");
        let back: AgentEnvelope = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(env, back);
    }

    #[test]
    fn request_data_omits_trusted_fields_and_round_trips() {
        // The control-end request body has no target/actor/scope/caller/
        // request_id field to set — the JSON only carries operation + reason.
        let req = AgentRequestData {
            operation: AgentOperation {
                risk_hint: None,
                input: OperationInput::ReadContext(ReadContextInput {
                    kind: ContextKind::NetworkPorts(NetworkPortsParams { protocol: None }),
                }),
            },
            reason: Some("why".into()),
            org_id: None,
        };
        let json = serde_json::to_string(&req).expect("encode");
        assert!(!json.contains("\"actor\""));
        assert!(!json.contains("\"scope\""));
        assert!(!json.contains("\"target\""));
        assert!(!json.contains("\"caller\""));
        let back: AgentRequestData = serde_json::from_str(&json).expect("decode");
        assert_eq!(req, back);
    }

    #[test]
    fn outcome_round_trips_both_arms() {
        let config = unbounded_config();
        for outcome in [
            AgentOutcome::Ok(OperationOutput::ReadContext(
                ReadContextOutput::ContainerList(ContainerListOutput {
                    containers: vec![ContainerSummary {
                        id: "abc".into(),
                        name: "web".into(),
                        image: "nginx".into(),
                        state: "running".into(),
                    }],
                    truncated: false,
                }),
            )),
            AgentOutcome::Err(AgentError {
                kind: AgentErrorKind::UnsupportedCapability,
                message: "not supported".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        ] {
            let json = serde_json::to_string(&outcome).expect("json encode");
            let back: AgentOutcome = serde_json::from_str(&json).expect("json decode");
            assert_eq!(outcome, back);

            let bytes = wincode::config::serialize(&outcome, config).expect("wincode encode");
            let back2: AgentOutcome =
                wincode::config::deserialize(&bytes, config).expect("wincode decode");
            assert_eq!(outcome, back2);
        }
    }

    /// `error_code` round-trips through JSON and wincode when set, is omitted from
    /// JSON when `None` (`skip_serializing_if`), and decodes to `None` when absent
    /// (`serde(default)`) — the compatibility contract that lets a code-less or
    /// older producer interoperate with a code-aware consumer.
    #[test]
    fn agent_error_code_round_trips_and_json_defaults() {
        let config = unbounded_config();
        let coded = AgentOutcome::Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "not configured".into(),
            retryable: false,
            safe_for_model: true,
            error_code: Some(51),
        });

        // JSON + wincode both preserve the code.
        let json = serde_json::to_string(&coded).expect("json encode");
        assert!(json.contains("\"error_code\":51"));
        let back: AgentOutcome = serde_json::from_str(&json).expect("json decode");
        assert_eq!(coded, back);
        let bytes = wincode::config::serialize(&coded, config).expect("wincode encode");
        let back2: AgentOutcome =
            wincode::config::deserialize(&bytes, config).expect("wincode decode");
        assert_eq!(coded, back2);

        // `None` is omitted from JSON, and an absent field decodes back to `None`.
        let codeless = AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "x".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        };
        let j = serde_json::to_string(&codeless).expect("encode");
        assert!(!j.contains("error_code"), "None must be omitted: {j}");
        let decoded: AgentError = serde_json::from_str(
            r#"{"kind":"internal","message":"m","retryable":false,"safe_for_model":true}"#,
        )
        .expect("decode without error_code");
        assert_eq!(decoded.error_code, None);
    }

    #[test]
    fn capability_serializes_to_dotted_names() {
        // Pin the dotted wire form so a future rename is caught.
        assert_eq!(
            serde_json::to_string(&Capability::ProcessList).unwrap(),
            "\"process.list\""
        );
        assert_eq!(
            serde_json::to_string(&Capability::ScreenCaptureCurrent).unwrap(),
            "\"screen.capture.current\""
        );
        assert_eq!(
            serde_json::to_string(&Capability::ShellExecConfirmed).unwrap(),
            "\"shell.exec.confirmed\""
        );
    }

    #[test]
    fn capability_derivation_covers_every_read_kind() {
        let cases = [
            (
                ContextKind::SystemInfo(SystemInfoParams::default()),
                Capability::SystemInfo,
            ),
            (
                ContextKind::ProcessList(ProcessListParams::default()),
                Capability::ProcessList,
            ),
            (
                ContextKind::NetworkPorts(NetworkPortsParams::default()),
                Capability::NetworkPorts,
            ),
            (
                ContextKind::ServiceStatus(ServiceStatusParams::default()),
                Capability::ServiceStatus,
            ),
            (
                ContextKind::LogRecent(LogRecentParams::default()),
                Capability::LogRecent,
            ),
            (
                ContextKind::ContainerList(ContainerListParams::default()),
                Capability::ContainerList,
            ),
            (
                ContextKind::ContainerInspect(ContainerInspectParams::default()),
                Capability::ContainerInspect,
            ),
            (
                ContextKind::ContainerLogs(ContainerLogsParams::default()),
                Capability::ContainerLogs,
            ),
            (
                ContextKind::ScreenCaptureCurrent(ScreenCaptureParams::default()),
                Capability::ScreenCaptureCurrent,
            ),
            (
                ContextKind::DesktopSessionInspect(computer_use::DesktopSessionInspectParams {
                    include_active_application: true,
                }),
                Capability::DesktopSessionInspect,
            ),
            (
                ContextKind::DesktopUiInspect(computer_use::UiInspectParams {
                    root: None,
                    max_depth: 8,
                    max_nodes: 256,
                    max_bytes: 65_536,
                }),
                Capability::DesktopUiInspect,
            ),
            (
                ContextKind::OfficeDocumentInspect(computer_use::OfficeInspectParams {
                    document: None,
                    selection_only: true,
                    max_objects: 256,
                    max_bytes: 65_536,
                }),
                Capability::OfficeDocumentInspect,
            ),
            (
                ContextKind::FileMetadataInspect(computer_use::FileMetadataInspectParams {
                    roots: vec![],
                    max_entries: 128,
                    max_bytes: 65_536,
                    enumerate_directories: false,
                    file_extensions: vec![],
                    min_file_bytes: None,
                    max_file_bytes: None,
                    modified_after: None,
                    modified_before: None,
                }),
                Capability::FileMetadataRead,
            ),
            (
                ContextKind::FileContentRead(computer_use::FileContentReadParams {
                    file: computer_use::ObjectRef {
                        token: "opaque".to_string(),
                        snapshot_id: "snapshot".to_string(),
                        object_kind: computer_use::ObjectKind::File,
                        expires_at: "2026-08-23T12:00:00Z".to_string(),
                    },
                    max_bytes: 65_536,
                }),
                Capability::FileContentRead,
            ),
            (
                ContextKind::TerminalOutputInspect(computer_use::TerminalOutputInspectParams {
                    roots: vec![],
                    max_bytes: 32_768,
                }),
                Capability::TerminalOutputRead,
            ),
        ];
        for (kind, expected) in cases {
            let input = OperationInput::ReadContext(ReadContextInput { kind });
            assert_eq!(input.capability(), Some(expected));
        }
    }

    #[test]
    fn exec_capability_is_not_derived() {
        let input = OperationInput::Exec(ExecInput {
            target: ExecTarget::Shell {
                shell: "powershell".into(),
            },
            command: "Get-Service".into(),
            cwd: None,
            io_mode: crate::exec::ExecIoMode::NonInteractive,
            timeout_ms: 10_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        });
        assert_eq!(input.capability(), None);
    }

    #[test]
    fn risk_level_is_ordered() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::High < RiskLevel::Critical);
        assert!(RiskLevel::Critical < RiskLevel::Blocked);
    }

    #[test]
    fn utoipa_schema_is_generated() {
        // `ToSchema` derive compiles and produces a named schema.
        use utoipa::PartialSchema;
        let _ = AgentEnvelope::schema();
        let _ = AgentOutcome::schema();
        let _ = AgentRequestData::schema();
    }
}
