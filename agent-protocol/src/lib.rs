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

pub mod audit;
pub mod authz;
pub mod command_template;
pub mod diagnose;
pub mod evidence;
pub mod exec;
pub mod exec_policy;
pub mod model_proxy;

use crate::exec::{CommandClassification, ExecDecision, ExecEffect};

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
    pub tenant_id: Option<String>,
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
    // P1 execute — reserved; shape frozen, impl in M2.
    #[serde(rename = "shell.exec.readonly")]
    ShellExecReadonly,
    #[serde(rename = "shell.exec.confirmed")]
    ShellExecConfirmed,
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
    /// Reserved; rejected with `UnsupportedCapability` until M2.
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
    /// command). So `Exec` returns `None`; M2 introduces
    /// `required_capability(classification)` and freezes the exec mapping then.
    /// Only reads exist today, so `None` is never observed.
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
            }),
            OperationInput::Exec(_) => None,
        }
    }

    /// Frozen exec capability mapping (the M2 gap [`OperationInput::capability`]
    /// deliberately left open). The `shell.exec.readonly` vs
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
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
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

// -------- exec (reserved; shape frozen, impl in M2) --------

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecInput {
    pub target: ExecTarget,
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_ms: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
}

/// Open-ended exec target. `Shell` is the M2 form; `Domain` reserves the
/// adb / domain-tool wedge without a future skeleton break.
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
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u32,
    #[serde(default)]
    pub redactions: Vec<String>,
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
    Timeout,
    OutputLimitExceeded,
    InvalidInput,
    RedactionFailed,
    TransportError,
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
/// Implemented by DeskServer / SessionWorker for read_context; exec lands in
/// M2.
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
                tenant_id: Some("tenant_1".into()),
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
