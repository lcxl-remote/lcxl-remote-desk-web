//! AI audit event skeleton.
//!
//! Every AI capability call produces a small lifecycle of audit events
//! (`ai.task.created` → `ai.context.collected` → `ai.task.completed` /
//! `ai.task.failed`). This module defines the runtime event shape, the field
//! mapping from an [`AgentEnvelope`] + outcome, and the [`AuditSink`] contract
//! that consumes them.
//!
//! The event fields mirror the `ai_audit_event` Sea-ORM entity **minus** the
//! database primary key, so a persistence sink can map an [`AuditEvent`] onto a
//! row one-to-one. The current form is single-machine (the server logs events);
//! the database-backed sink lands in M2. Because this crate is whitelisted as a
//! cross-boundary dependency, both the device-side emitter (worker) and the
//! future parent-repo persistence sink share this one definition.
//!
//! Two invariants the builders enforce:
//! - **Summaries only.** `input_summary` / `output_summary` carry counts and
//!   sizes, never raw stdout / log lines / screenshot bytes (security model
//!   §6.3). The full artifact never enters an audit event.
//! - **Server-authoritative subject.** Every subject / correlation field is
//!   read from the server-stamped [`AgentEnvelope`], never from a control end.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{AgentEnvelope, AgentError, CallerRef, Capability, ExecutionMode, OperationOutput};

/// The fixed set of audit event types currently emitted. Stored on the wire /
/// in the audit row as the dotted string form (free-text column), so adding a
/// type later (e.g. `ai.capability.denied` with the M4 policy engine) is
/// additive.
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
pub enum AuditEventType {
    /// A capability call was accepted and started.
    TaskCreated,
    /// A read context was collected successfully.
    ContextCollected,
    /// The call finished successfully.
    TaskCompleted,
    /// The call failed (collector error, unsupported capability, ...).
    TaskFailed,
    /// A model request was issued by the diagnose orchestrator.
    ModelRequested,
    /// A model response was received.
    ModelResponded,
    /// The redactor failed and the orchestrator refused to send to the model
    /// (fail-closed).
    RedactionFailed,
    /// A diagnose task was cancelled (e.g. the operator handed off to manual
    /// remote control).
    TaskCancelled,

    // ---- confirmed-execution lifecycle ----
    /// A command execution was requested and classified (preview produced).
    CapabilityRequested,
    /// The required exec capability was granted against the active scope.
    CapabilityAllowed,
    /// The required exec capability was denied (blocked / off-template / scope).
    CapabilityDenied,
    /// A pending approval was created for a previewed execution.
    ApprovalRequested,
    /// The user approved a previewed execution (an `approval_id` was minted).
    ApprovalGranted,
    /// The user rejected a previewed execution.
    ApprovalDenied,
    /// An approved command was dispatched to the worker for execution.
    CommandExecuted,
    /// An executed command finished (result returned).
    CommandCompleted,
}

impl AuditEventType {
    /// Dotted event name as written to the audit `event_type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            AuditEventType::TaskCreated => "ai.task.created",
            AuditEventType::ContextCollected => "ai.context.collected",
            AuditEventType::TaskCompleted => "ai.task.completed",
            AuditEventType::TaskFailed => "ai.task.failed",
            AuditEventType::ModelRequested => "ai.model.requested",
            AuditEventType::ModelResponded => "ai.model.responded",
            AuditEventType::RedactionFailed => "ai.redaction.failed",
            AuditEventType::TaskCancelled => "ai.task.cancelled",
            AuditEventType::CapabilityRequested => "ai.capability.requested",
            AuditEventType::CapabilityAllowed => "ai.capability.allowed",
            AuditEventType::CapabilityDenied => "ai.capability.denied",
            AuditEventType::ApprovalRequested => "ai.approval.requested",
            AuditEventType::ApprovalGranted => "ai.approval.granted",
            AuditEventType::ApprovalDenied => "ai.approval.denied",
            AuditEventType::CommandExecuted => "ai.command.executed",
            AuditEventType::CommandCompleted => "ai.command.completed",
        }
    }
}

/// One audit event. Field-for-field compatible with the `ai_audit_event`
/// entity minus its database `id`. `created_at` is an RFC3339 string (the
/// emitter stamps it) so this crate stays free of a `chrono` dependency; the
/// persistence sink parses it into a timestamp column.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AuditEvent {
    /// Stable event identifier (emitter-generated, e.g. a UUID).
    pub event_id: String,
    /// RFC3339 timestamp; emitter-stamped.
    pub created_at: String,

    // ---- correlation ----
    pub request_id: String,
    pub task_id: Option<String>,
    pub policy_id: Option<String>,
    pub approval_id: Option<String>,

    // ---- subject (all server-authoritative, read from the envelope) ----
    pub tenant_id: Option<String>,
    pub actor_id: String,
    pub device_id: String,
    pub session_id: Option<String>,

    // ---- what ----
    pub event_type: String,
    /// Dotted capability name (e.g. `process.list`). `None` for task-level
    /// events (created / completed / failed).
    pub capability: Option<String>,
    pub risk: Option<String>,
    pub mode: Option<String>,
    /// `created` | `ok` | `error`.
    pub result: String,

    // ---- model accounting (no model yet; reserved) ----
    pub model_provider: Option<String>,
    pub model_name: Option<String>,
    pub adapter: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,

    // ---- summaries only (never full stdout / screenshot / prompt) ----
    pub input_summary: Option<String>,
    pub output_summary: Option<String>,
    pub redaction_count: Option<i32>,
    /// Opaque reference into a separately-designed evidence store, if any.
    pub evidence_ref: Option<String>,

    pub duration_ms: Option<i64>,
}

impl AuditEvent {
    /// Fill the subject / correlation fields shared by every event from the
    /// server-stamped envelope. Event-specific fields (`result`, `capability`,
    /// summaries, duration) are set by the named constructors.
    fn from_envelope(
        event_id: String,
        created_at: String,
        event_type: AuditEventType,
        envelope: &AgentEnvelope,
    ) -> Self {
        AuditEvent {
            event_id,
            created_at,
            request_id: envelope.request_id.0.clone(),
            task_id: envelope.parent_task_id.as_ref().map(|t| t.0.clone()),
            policy_id: envelope.scope.policy_id.clone(),
            approval_id: envelope.audit.approval_id.clone(),
            tenant_id: envelope.actor.tenant_id.clone(),
            actor_id: envelope.actor.actor_id.clone(),
            device_id: envelope.target.device_id.clone(),
            session_id: envelope.target.session_id.clone(),
            event_type: event_type.as_str().to_string(),
            capability: None,
            // Risk is the server-classified final value; there is no classifier
            // yet (it lands with exec / policy in M2/M4), so it stays unset.
            risk: None,
            mode: Some(envelope.scope.mode.as_str().to_string()),
            result: String::new(),
            model_provider: envelope.caller.model_provider.clone(),
            model_name: envelope.caller.model_name.clone(),
            adapter: envelope.caller.adapter.clone(),
            input_tokens: None,
            output_tokens: None,
            input_summary: Some(summarize_input(envelope)),
            output_summary: None,
            redaction_count: None,
            evidence_ref: None,
            duration_ms: None,
        }
    }

    /// `ai.task.created` — the call was accepted and is about to run.
    pub fn task_created(event_id: String, created_at: String, envelope: &AgentEnvelope) -> Self {
        let mut event =
            Self::from_envelope(event_id, created_at, AuditEventType::TaskCreated, envelope);
        event.result = "created".to_string();
        event
    }

    /// `ai.context.collected` — a read context was produced. Carries the
    /// derived capability, an output summary (counts / sizes only), the
    /// redaction count, and how long the collection took.
    pub fn context_collected(
        event_id: String,
        created_at: String,
        envelope: &AgentEnvelope,
        output: &OperationOutput,
        duration_ms: i64,
    ) -> Self {
        let mut event = Self::from_envelope(
            event_id,
            created_at,
            AuditEventType::ContextCollected,
            envelope,
        );
        event.capability = envelope
            .operation
            .input
            .capability()
            .map(|c| c.as_str().to_string());
        event.result = "ok".to_string();
        event.output_summary = Some(summarize_output(output));
        event.redaction_count = Some(count_redactions(output));
        event.duration_ms = Some(duration_ms);
        event
    }

    /// `ai.task.completed` — the call finished successfully.
    pub fn task_completed(
        event_id: String,
        created_at: String,
        envelope: &AgentEnvelope,
        duration_ms: i64,
    ) -> Self {
        let mut event = Self::from_envelope(
            event_id,
            created_at,
            AuditEventType::TaskCompleted,
            envelope,
        );
        event.result = "ok".to_string();
        event.duration_ms = Some(duration_ms);
        event
    }

    /// `ai.task.failed` — the call failed. The error *kind* is recorded as the
    /// output summary; the human-readable message is not stored here (it can
    /// carry policy detail — see `AgentError::safe_for_model`).
    pub fn task_failed(
        event_id: String,
        created_at: String,
        envelope: &AgentEnvelope,
        error: &AgentError,
        duration_ms: i64,
    ) -> Self {
        let mut event =
            Self::from_envelope(event_id, created_at, AuditEventType::TaskFailed, envelope);
        event.capability = envelope
            .operation
            .input
            .capability()
            .map(|c| c.as_str().to_string());
        event.result = "error".to_string();
        event.output_summary = Some(format!("{:?}", error.kind));
        event.duration_ms = Some(duration_ms);
        event
    }

    /// `ai.task.failed` correlated only by `request_id` — the central
    /// orchestrator path, which fails a diagnosis without a per-capability
    /// [`AgentEnvelope`] (e.g. the remote evidence collection or the model call
    /// failed). Like [`task_failed`](Self::task_failed) the error *kind* is the
    /// content-free output summary; the human message may carry policy detail and
    /// is not stored.
    pub fn task_failed_for_request(
        event_id: String,
        created_at: String,
        request_id: &str,
        error: &AgentError,
    ) -> Self {
        let mut event =
            Self::task_scoped(event_id, created_at, AuditEventType::TaskFailed, request_id);
        event.result = "error".to_string();
        event.output_summary = Some(format!("{:?}", error.kind));
        event
    }

    /// Base for orchestrator **task-level** events (model / redaction / cancel).
    /// These are correlated by `request_id` rather than a per-capability
    /// [`AgentEnvelope`], so the subject fields the envelope would supply
    /// (actor / device / tenant) default empty here; M4 enriches them when the
    /// policy engine carries identity through the orchestrator.
    fn task_scoped(
        event_id: String,
        created_at: String,
        event_type: AuditEventType,
        request_id: &str,
    ) -> Self {
        AuditEvent {
            event_id,
            created_at,
            request_id: request_id.to_string(),
            event_type: event_type.as_str().to_string(),
            ..Default::default()
        }
    }

    /// `ai.model.requested` — the orchestrator is about to call the model.
    /// `input_summary` is content-free (e.g. evidence item count / size); the
    /// prompt itself is never stored.
    pub fn model_requested(
        event_id: String,
        created_at: String,
        request_id: &str,
        caller: &CallerRef,
        input_summary: String,
        input_tokens: Option<i64>,
    ) -> Self {
        let mut event = Self::task_scoped(
            event_id,
            created_at,
            AuditEventType::ModelRequested,
            request_id,
        );
        event.model_provider = caller.model_provider.clone();
        event.model_name = caller.model_name.clone();
        event.adapter = caller.adapter.clone();
        event.result = "requested".to_string();
        event.input_summary = Some(input_summary);
        event.input_tokens = input_tokens;
        event
    }

    /// `ai.model.responded` — the model returned. Carries token accounting and a
    /// content-free output summary (e.g. finding / command counts).
    #[allow(clippy::too_many_arguments)]
    pub fn model_responded(
        event_id: String,
        created_at: String,
        request_id: &str,
        caller: &CallerRef,
        output_summary: String,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        duration_ms: i64,
    ) -> Self {
        let mut event = Self::task_scoped(
            event_id,
            created_at,
            AuditEventType::ModelResponded,
            request_id,
        );
        event.model_provider = caller.model_provider.clone();
        event.model_name = caller.model_name.clone();
        event.adapter = caller.adapter.clone();
        event.result = "ok".to_string();
        event.output_summary = Some(output_summary);
        event.input_tokens = input_tokens;
        event.output_tokens = output_tokens;
        event.duration_ms = Some(duration_ms);
        event
    }

    /// `ai.redaction.failed` — the redactor failed; the orchestrator refused to
    /// send to the model (fail-closed). `reason` is a short, content-free
    /// description, never the unredacted data.
    pub fn redaction_failed(
        event_id: String,
        created_at: String,
        request_id: &str,
        reason: &str,
    ) -> Self {
        let mut event = Self::task_scoped(
            event_id,
            created_at,
            AuditEventType::RedactionFailed,
            request_id,
        );
        event.result = "error".to_string();
        event.output_summary = Some(reason.to_string());
        event
    }

    /// `ai.task.cancelled` — the diagnose task was cancelled (e.g. operator
    /// handoff to manual remote control).
    pub fn task_cancelled(event_id: String, created_at: String, request_id: &str) -> Self {
        let mut event = Self::task_scoped(
            event_id,
            created_at,
            AuditEventType::TaskCancelled,
            request_id,
        );
        event.result = "cancelled".to_string();
        event
    }

    /// Base for confirmed-execution events. These are correlated by the stable
    /// server-minted `exec_request_id` (which threads the whole confirm →
    /// approve → execute → complete lifecycle), stored in the `request_id`
    /// column so a replay filters one execution by a single key. Subject fields
    /// (actor / device / tenant) default empty on the single-machine path, like
    /// the orchestrator task-scoped events.
    fn exec_scoped(
        event_id: String,
        created_at: String,
        event_type: AuditEventType,
        exec_request_id: &str,
    ) -> Self {
        AuditEvent {
            event_id,
            created_at,
            request_id: exec_request_id.to_string(),
            event_type: event_type.as_str().to_string(),
            ..Default::default()
        }
    }

    /// `ai.capability.requested` — an exec command was classified and a preview
    /// produced. `summary` is content-free (the impact description / template).
    pub fn capability_requested(
        event_id: String,
        created_at: String,
        exec_request_id: &str,
        capability: Option<&str>,
        risk: &str,
        summary: String,
    ) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::CapabilityRequested,
            exec_request_id,
        );
        event.capability = capability.map(str::to_string);
        event.risk = Some(risk.to_string());
        event.result = "requested".to_string();
        event.output_summary = Some(summary);
        event
    }

    /// `ai.capability.denied` — an exec command was blocked, off-template, or
    /// not permitted by the active mode. `reason` is content-free.
    pub fn capability_denied(
        event_id: String,
        created_at: String,
        exec_request_id: &str,
        risk: &str,
        reason: String,
    ) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::CapabilityDenied,
            exec_request_id,
        );
        event.risk = Some(risk.to_string());
        event.result = "denied".to_string();
        event.output_summary = Some(reason);
        event
    }

    /// `ai.capability.allowed` — the required exec capability was granted
    /// against the active scope (the user approved).
    pub fn capability_allowed(
        event_id: String,
        created_at: String,
        exec_request_id: &str,
        capability: Option<&str>,
        risk: &str,
    ) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::CapabilityAllowed,
            exec_request_id,
        );
        event.capability = capability.map(str::to_string);
        event.risk = Some(risk.to_string());
        event.result = "allowed".to_string();
        event
    }

    /// `ai.approval.granted` — the user approved a previewed execution; the
    /// server minted `approval_id`.
    pub fn approval_granted(
        event_id: String,
        created_at: String,
        exec_request_id: &str,
        approval_id: &str,
    ) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::ApprovalGranted,
            exec_request_id,
        );
        event.approval_id = Some(approval_id.to_string());
        event.result = "granted".to_string();
        event
    }

    /// `ai.approval.denied` — the user rejected a previewed execution.
    pub fn approval_denied(event_id: String, created_at: String, exec_request_id: &str) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::ApprovalDenied,
            exec_request_id,
        );
        event.result = "denied".to_string();
        event
    }

    /// `ai.command.executed` — an approved command was dispatched to the worker.
    pub fn command_executed(
        event_id: String,
        created_at: String,
        exec_request_id: &str,
        approval_id: &str,
        capability: Option<&str>,
        risk: &str,
    ) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::CommandExecuted,
            exec_request_id,
        );
        event.approval_id = Some(approval_id.to_string());
        event.capability = capability.map(str::to_string);
        event.risk = Some(risk.to_string());
        event.result = "executed".to_string();
        event
    }

    /// `ai.command.completed` — an executed command finished. `summary` carries
    /// only counts / exit code (never stdout); `redaction_count` and
    /// `duration_ms` mirror the read-collection events.
    pub fn command_completed(
        event_id: String,
        created_at: String,
        exec_request_id: &str,
        success: bool,
        summary: String,
        redaction_count: i32,
        duration_ms: i64,
    ) -> Self {
        let mut event = Self::exec_scoped(
            event_id,
            created_at,
            AuditEventType::CommandCompleted,
            exec_request_id,
        );
        event.result = if success { "ok" } else { "error" }.to_string();
        event.output_summary = Some(summary);
        event.redaction_count = Some(redaction_count);
        event.duration_ms = Some(duration_ms);
        event
    }

    /// Attach the audit correlation key — the source frame `request_id` that the
    /// PDP wrapped and recorded in its authorization ledger — so the real
    /// operator (and its decision policy) can be looked up at persist time.
    ///
    /// Exec lifecycle events are correlated by a server-minted `exec_request_id`
    /// that the manager never sees, so they carry the originating ConfirmExec
    /// frame's `request_id` here in `task_id` to bridge back to the ledger.
    /// Orchestrator / model events are already keyed by the frame `request_id`
    /// and leave this `None`.
    pub fn with_task_id(mut self, task_id: Option<&str>) -> Self {
        self.task_id = task_id.map(str::to_string);
        self
    }
}

/// Manager-bound audit report payload.
///
/// web/server has no database; it reports each [`AuditEvent`] to the parent
/// manager over the existing manager WebSocket, where it is persisted to the
/// `ai_audit_event` table. This thin wrapper is the payload of that report
/// (the signaling variant carrying it is added in a later step). It exists as a
/// distinct type so the manager-facing wire can evolve (e.g. a batch or
/// schema-version field) without touching the [`AuditEvent`] shape that the
/// device-side emitter and the persistence mapping both share.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AiAuditEventPayload {
    pub event: AuditEvent,
}

/// Short, content-free description of what was requested (the capability /
/// operation kind), for the `input_summary` column.
fn summarize_input(envelope: &AgentEnvelope) -> String {
    match envelope.operation.input.capability() {
        Some(cap) => cap.as_str().to_string(),
        None => "exec".to_string(),
    }
}

/// Content-free summary of an output: counts and sizes only, never the data
/// itself. Safe to store in the audit trail and to hand to operators.
pub fn summarize_output(output: &OperationOutput) -> String {
    use crate::ReadContextOutput as R;
    match output {
        OperationOutput::ReadContext(rc) => match rc {
            R::SystemInfo(o) => {
                format!(
                    "system.info: {} cores, {} disks",
                    o.cpu.logical_cores,
                    o.disks.len()
                )
            }
            R::ProcessList(o) => format!(
                "process.list: {} processes{}",
                o.processes.len(),
                if o.truncated { " (truncated)" } else { "" }
            ),
            R::NetworkPorts(o) => format!("network.ports: {} ports", o.ports.len()),
            R::ServiceStatus(o) => format!("service.status: {} services", o.services.len()),
            R::LogRecent(o) => format!("log.recent: {} events", o.events.len()),
            R::ContainerList(o) => format!("container.list: {} containers", o.containers.len()),
            R::ContainerInspect(o) => {
                format!("container.inspect: {} bytes", o.details_json.len())
            }
            R::ContainerLogs(o) => format!("container.logs: {} lines", o.lines.len()),
            R::ScreenCaptureCurrent(o) => {
                format!(
                    "screen.capture.current: {}x{}, {} bytes",
                    o.width,
                    o.height,
                    o.image.len()
                )
            }
        },
        OperationOutput::Exec(o) => format!("exec: exit {}", o.exit_code),
    }
}

/// Count the redaction markers carried by an output. Collectors emit no
/// redactions yet (scrubbing lands in M1b), so this is `0` in practice, but the
/// count is wired so the audit trail reflects scrubbing the moment it lands.
pub fn count_redactions(output: &OperationOutput) -> i32 {
    use crate::ReadContextOutput as R;
    let n: usize = match output {
        OperationOutput::ReadContext(rc) => match rc {
            R::ProcessList(o) => o
                .processes
                .iter()
                .filter(|p| p.command_line_redacted)
                .count(),
            R::LogRecent(o) => o.events.iter().map(|e| e.redactions.len()).sum(),
            R::ContainerInspect(o) => o.redactions.len(),
            R::ContainerLogs(o) => o.redactions.len(),
            _ => 0,
        },
        OperationOutput::Exec(o) => o.redactions.len(),
    };
    n.try_into().unwrap_or(i32::MAX)
}

impl Capability {
    /// Dotted capability name — the same string serde writes on the wire and
    /// the audit `capability` column stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::SystemInfo => "system.info",
            Capability::ProcessList => "process.list",
            Capability::NetworkPorts => "network.ports",
            Capability::ServiceStatus => "service.status",
            Capability::LogRecent => "log.recent",
            Capability::ContainerList => "container.list",
            Capability::ContainerInspect => "container.inspect",
            Capability::ContainerLogs => "container.logs",
            Capability::ScreenCaptureCurrent => "screen.capture.current",
            Capability::ShellExecReadonly => "shell.exec.readonly",
            Capability::ShellExecConfirmed => "shell.exec.confirmed",
        }
    }
}

impl ExecutionMode {
    /// snake_case name — the same string serde writes and the audit `mode`
    /// column stores.
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionMode::SuggestOnly => "suggest_only",
            ExecutionMode::ReadOnly => "read_only",
            ExecutionMode::ConfirmEachAction => "confirm_each_action",
            ExecutionMode::SessionApproved => "session_approved",
            ExecutionMode::Automated => "automated",
        }
    }
}

/// Consumer of audit events. A logging sink is wired today (single-machine
/// form); M2 adds a database-backed sink that maps each [`AuditEvent`] to an
/// `ai_audit_event` row. Object-safe so the emitter can hold
/// `Arc<dyn AuditSink>` and tests can substitute a recording mock — mirroring
/// the [`crate::DeviceAgent`] rationale.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent);
}

/// Discards every event. The default when no audit sink is configured (tests,
/// hosts without a session) so emission is always safe to call.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuditSink;

#[async_trait::async_trait]
impl AuditSink for NoopAuditSink {
    async fn record(&self, _event: AuditEvent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ActorRef, ActorType, AgentOperation, AgentScope, AuditMeta, CallerRef, CallerType,
        ContainerLogsOutput, ContextKind, LogEvent, LogRecentOutput, LogSeverity, OperationInput,
        ProcessEntry, ProcessListOutput, ProcessListParams, ProtocolVersion, ReadContextInput,
        ReadContextOutput, RequestId, RiskLevel, SystemInfoParams, TargetRef, TaskId,
    };

    fn envelope(input: OperationInput) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("req_42".into()),
            parent_task_id: Some(TaskId("task_7".into())),
            target: TargetRef {
                device_id: "dev_9".into(),
                session_id: Some("sess_3".into()),
                worker_id: None,
            },
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: "local-operator".into(),
                tenant_id: Some("tenant_1".into()),
            },
            caller: CallerRef {
                caller_type: CallerType::Human,
                model_provider: None,
                model_name: None,
                adapter: None,
            },
            scope: AgentScope {
                granted: vec![Capability::ProcessList],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_id: Some("policy_x".into()),
            },
            operation: AgentOperation {
                risk_hint: Some(RiskLevel::Low),
                input,
            },
            audit: AuditMeta {
                approval_id: Some("appr_1".into()),
                reason: Some("why".into()),
            },
        }
    }

    fn process_list_input() -> OperationInput {
        OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ProcessList(ProcessListParams::default()),
        })
    }

    #[test]
    fn task_created_maps_trusted_subject_fields() {
        let env = envelope(process_list_input());
        let e = AuditEvent::task_created("evt_1".into(), "2026-06-12T10:00:00Z".into(), &env);

        assert_eq!(e.event_id, "evt_1");
        assert_eq!(e.created_at, "2026-06-12T10:00:00Z");
        assert_eq!(e.event_type, "ai.task.created");
        assert_eq!(e.request_id, "req_42");
        assert_eq!(e.task_id.as_deref(), Some("task_7"));
        assert_eq!(e.policy_id.as_deref(), Some("policy_x"));
        assert_eq!(e.approval_id.as_deref(), Some("appr_1"));
        assert_eq!(e.tenant_id.as_deref(), Some("tenant_1"));
        assert_eq!(e.actor_id, "local-operator");
        assert_eq!(e.device_id, "dev_9");
        assert_eq!(e.session_id.as_deref(), Some("sess_3"));
        assert_eq!(e.mode.as_deref(), Some("read_only"));
        assert_eq!(e.result, "created");
        // Task-level event: no capability, no duration yet.
        assert_eq!(e.capability, None);
        assert_eq!(e.duration_ms, None);
        // Input summary names the capability but carries no data.
        assert_eq!(e.input_summary.as_deref(), Some("process.list"));
    }

    fn model_caller() -> CallerRef {
        CallerRef {
            caller_type: CallerType::AiModel,
            model_provider: Some("openai-compatible".into()),
            model_name: Some("example-model".into()),
            adapter: Some("lcxl-openai".into()),
        }
    }

    #[test]
    fn model_requested_records_caller_and_input_tokens() {
        let e = AuditEvent::model_requested(
            "evt_m1".into(),
            "2026-06-13T10:00:00Z".into(),
            "req_42",
            &model_caller(),
            "evidence: 5 items, 75000 bytes".into(),
            Some(1234),
        );
        assert_eq!(e.event_type, "ai.model.requested");
        assert_eq!(e.request_id, "req_42");
        assert_eq!(e.result, "requested");
        assert_eq!(e.model_provider.as_deref(), Some("openai-compatible"));
        assert_eq!(e.model_name.as_deref(), Some("example-model"));
        assert_eq!(e.adapter.as_deref(), Some("lcxl-openai"));
        assert_eq!(e.input_tokens, Some(1234));
        assert_eq!(
            e.input_summary.as_deref(),
            Some("evidence: 5 items, 75000 bytes")
        );
    }

    #[test]
    fn model_responded_records_tokens_and_duration() {
        let e = AuditEvent::model_responded(
            "evt_m2".into(),
            "2026-06-13T10:00:05Z".into(),
            "req_42",
            &model_caller(),
            "diagnosis: 2 findings, 1 command".into(),
            Some(1234),
            Some(567),
            4200,
        );
        assert_eq!(e.event_type, "ai.model.responded");
        assert_eq!(e.result, "ok");
        assert_eq!(e.input_tokens, Some(1234));
        assert_eq!(e.output_tokens, Some(567));
        assert_eq!(e.duration_ms, Some(4200));
        assert_eq!(
            e.output_summary.as_deref(),
            Some("diagnosis: 2 findings, 1 command")
        );
    }

    #[test]
    fn redaction_failed_is_an_error_with_reason() {
        let e = AuditEvent::redaction_failed(
            "evt_r".into(),
            "2026-06-13T10:00:00Z".into(),
            "req_42",
            "redactor panicked",
        );
        assert_eq!(e.event_type, "ai.redaction.failed");
        assert_eq!(e.result, "error");
        assert_eq!(e.output_summary.as_deref(), Some("redactor panicked"));
    }

    #[test]
    fn task_cancelled_records_request_id() {
        let e = AuditEvent::task_cancelled("evt_c".into(), "2026-06-13T10:00:00Z".into(), "req_42");
        assert_eq!(e.event_type, "ai.task.cancelled");
        assert_eq!(e.result, "cancelled");
        assert_eq!(e.request_id, "req_42");
    }

    #[test]
    fn context_collected_records_capability_summary_and_duration() {
        let env = envelope(process_list_input());
        let output =
            OperationOutput::ReadContext(ReadContextOutput::ProcessList(ProcessListOutput {
                processes: vec![
                    ProcessEntry {
                        pid: 1,
                        name: "a".into(),
                        cpu_percent: 0.0,
                        memory_bytes: 0,
                        user: None,
                        command_line_redacted: true,
                    },
                    ProcessEntry {
                        pid: 2,
                        name: "b".into(),
                        cpu_percent: 0.0,
                        memory_bytes: 0,
                        user: None,
                        command_line_redacted: false,
                    },
                ],
                truncated: true,
            }));
        let e = AuditEvent::context_collected(
            "evt_2".into(),
            "2026-06-12T10:00:01Z".into(),
            &env,
            &output,
            123,
        );

        assert_eq!(e.event_type, "ai.context.collected");
        assert_eq!(e.capability.as_deref(), Some("process.list"));
        assert_eq!(e.result, "ok");
        assert_eq!(e.duration_ms, Some(123));
        assert_eq!(
            e.output_summary.as_deref(),
            Some("process.list: 2 processes (truncated)")
        );
        // One process carries a redacted command line.
        assert_eq!(e.redaction_count, Some(1));
    }

    #[test]
    fn task_failed_records_error_kind_not_message() {
        let env = envelope(process_list_input());
        let error = AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "secret policy detail".into(),
            retryable: false,
            safe_for_model: true,
        };
        let e = AuditEvent::task_failed("evt_3".into(), "ts".into(), &env, &error, 5);

        assert_eq!(e.event_type, "ai.task.failed");
        assert_eq!(e.result, "error");
        assert_eq!(e.duration_ms, Some(5));
        // The kind is recorded; the (potentially sensitive) message is not.
        assert_eq!(e.output_summary.as_deref(), Some("UnsupportedCapability"));
        assert!(!e.output_summary.as_deref().unwrap().contains("secret"));
    }

    #[test]
    fn task_failed_for_request_records_kind_and_request_id_without_envelope() {
        let error = AgentError {
            kind: AgentErrorKind::RedactionFailed,
            message: "secret policy detail".into(),
            retryable: false,
            safe_for_model: true,
        };
        let e = AuditEvent::task_failed_for_request(
            "evt_tf".into(),
            "2026-06-16T00:00:00Z".into(),
            "req_99",
            &error,
        );
        assert_eq!(e.event_type, "ai.task.failed");
        assert_eq!(e.result, "error");
        assert_eq!(e.request_id, "req_99");
        // Kind is recorded; the (potentially sensitive) message is not.
        assert_eq!(e.output_summary.as_deref(), Some("RedactionFailed"));
        assert!(!e.output_summary.as_deref().unwrap().contains("secret"));
    }

    use crate::AgentErrorKind;

    #[test]
    fn redaction_count_sums_log_and_container_markers() {
        let logs = OperationOutput::ReadContext(ReadContextOutput::LogRecent(LogRecentOutput {
            events: vec![
                LogEvent {
                    timestamp: "t".into(),
                    source: "s".into(),
                    severity: LogSeverity::Error,
                    message: "m".into(),
                    redactions: vec!["path".into(), "user".into()],
                },
                LogEvent {
                    timestamp: "t".into(),
                    source: "s".into(),
                    severity: LogSeverity::Info,
                    message: "m".into(),
                    redactions: vec!["token".into()],
                },
            ],
            truncated: false,
        }));
        assert_eq!(count_redactions(&logs), 3);

        let container_logs =
            OperationOutput::ReadContext(ReadContextOutput::ContainerLogs(ContainerLogsOutput {
                lines: vec!["a".into()],
                redactions: vec!["secret".into()],
                truncated: false,
            }));
        assert_eq!(count_redactions(&container_logs), 1);
    }

    #[test]
    fn system_info_output_summary_is_content_free() {
        use crate::{CpuInfo, MemoryInfo, SystemInfoOutput};
        let env = envelope(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SystemInfo(SystemInfoParams::default()),
        }));
        let output =
            OperationOutput::ReadContext(ReadContextOutput::SystemInfo(SystemInfoOutput {
                hostname: "secret-host".into(),
                os: "windows".into(),
                os_version: "11".into(),
                arch: "x86_64".into(),
                uptime_seconds: 10,
                cpu: CpuInfo {
                    usage_percent: 0.0,
                    logical_cores: 8,
                },
                memory: MemoryInfo {
                    total_bytes: 0,
                    used_bytes: 0,
                },
                disks: vec![],
            }));
        let e = AuditEvent::context_collected("id".into(), "ts".into(), &env, &output, 0);
        let summary = e.output_summary.unwrap();
        assert_eq!(summary, "system.info: 8 cores, 0 disks");
        // The summary must not leak the hostname.
        assert!(!summary.contains("secret-host"));
    }

    #[test]
    fn event_type_round_trips() {
        let json = serde_json::to_string(&AuditEventType::ContextCollected).unwrap();
        assert_eq!(json, "\"context_collected\"");
        let back: AuditEventType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AuditEventType::ContextCollected);
    }

    #[test]
    fn audit_event_wincode_round_trips() {
        use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};
        let config: Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> = Configuration::new();
        let env = envelope(process_list_input());
        let event = AuditEvent::task_created("evt".into(), "ts".into(), &env);
        let bytes = wincode::config::serialize(&event, config).expect("encode");
        let back: AuditEvent = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(event, back);
    }

    #[test]
    fn exec_lifecycle_event_types_have_dotted_names() {
        let cases = [
            (
                AuditEventType::CapabilityRequested,
                "ai.capability.requested",
            ),
            (AuditEventType::CapabilityAllowed, "ai.capability.allowed"),
            (AuditEventType::CapabilityDenied, "ai.capability.denied"),
            (AuditEventType::ApprovalRequested, "ai.approval.requested"),
            (AuditEventType::ApprovalGranted, "ai.approval.granted"),
            (AuditEventType::ApprovalDenied, "ai.approval.denied"),
            (AuditEventType::CommandExecuted, "ai.command.executed"),
            (AuditEventType::CommandCompleted, "ai.command.completed"),
        ];
        for (ty, dotted) in cases {
            assert_eq!(ty.as_str(), dotted);
            // snake_case serde form round-trips.
            let json = serde_json::to_string(&ty).unwrap();
            let back: AuditEventType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ty);
        }
    }

    #[test]
    fn exec_lifecycle_builders_set_type_result_and_correlation() {
        let xr = "exec_42";
        let requested = AuditEvent::capability_requested(
            "e1".into(),
            "ts".into(),
            xr,
            Some("shell.exec.readonly"),
            "low",
            "Read the status of a Windows service".into(),
        );
        assert_eq!(requested.event_type, "ai.capability.requested");
        assert_eq!(requested.request_id, xr);
        assert_eq!(requested.result, "requested");
        assert_eq!(requested.capability.as_deref(), Some("shell.exec.readonly"));
        assert_eq!(requested.risk.as_deref(), Some("low"));

        let denied = AuditEvent::capability_denied(
            "e2".into(),
            "ts".into(),
            xr,
            "blocked",
            "blocklist".into(),
        );
        assert_eq!(denied.event_type, "ai.capability.denied");
        assert_eq!(denied.result, "denied");

        let granted = AuditEvent::approval_granted("e3".into(), "ts".into(), xr, "appr_1");
        assert_eq!(granted.event_type, "ai.approval.granted");
        assert_eq!(granted.approval_id.as_deref(), Some("appr_1"));

        let rejected = AuditEvent::approval_denied("e4".into(), "ts".into(), xr);
        assert_eq!(rejected.event_type, "ai.approval.denied");
        assert_eq!(rejected.result, "denied");

        let allowed = AuditEvent::capability_allowed(
            "e5".into(),
            "ts".into(),
            xr,
            Some("shell.exec.confirmed"),
            "high",
        );
        assert_eq!(allowed.event_type, "ai.capability.allowed");
        assert_eq!(allowed.result, "allowed");

        let executed = AuditEvent::command_executed(
            "e6".into(),
            "ts".into(),
            xr,
            "appr_1",
            Some("shell.exec.confirmed"),
            "high",
        );
        assert_eq!(executed.event_type, "ai.command.executed");
        assert_eq!(executed.approval_id.as_deref(), Some("appr_1"));

        let completed = AuditEvent::command_completed(
            "e7".into(),
            "ts".into(),
            xr,
            true,
            "exit 0".into(),
            0,
            12,
        );
        assert_eq!(completed.event_type, "ai.command.completed");
        assert_eq!(completed.result, "ok");
        assert_eq!(completed.duration_ms, Some(12));
        // The whole lifecycle shares one correlation key.
        for e in [
            requested, denied, granted, rejected, allowed, executed, completed,
        ] {
            assert_eq!(e.request_id, xr);
        }
    }

    #[test]
    fn with_task_id_attaches_source_request_id_for_every_exec_event() {
        let xr = "exec_42";
        let src = "frame_req_7";
        // Each exec lifecycle builder threads the source frame request_id into
        // task_id so the manager can bridge the minted exec id back to its
        // authorization ledger.
        let events = [
            AuditEvent::capability_requested(
                "e1".into(),
                "ts".into(),
                xr,
                Some("shell.exec.confirmed"),
                "high",
                "preview".into(),
            )
            .with_task_id(Some(src)),
            AuditEvent::capability_allowed(
                "e2".into(),
                "ts".into(),
                xr,
                Some("shell.exec.confirmed"),
                "high",
            )
            .with_task_id(Some(src)),
            AuditEvent::approval_granted("e3".into(), "ts".into(), xr, "appr_1")
                .with_task_id(Some(src)),
            AuditEvent::approval_denied("e4".into(), "ts".into(), xr).with_task_id(Some(src)),
            AuditEvent::command_executed(
                "e5".into(),
                "ts".into(),
                xr,
                "appr_1",
                Some("shell.exec.confirmed"),
                "high",
            )
            .with_task_id(Some(src)),
            AuditEvent::command_completed(
                "e6".into(),
                "ts".into(),
                xr,
                true,
                "exit 0".into(),
                0,
                1,
            )
            .with_task_id(Some(src)),
        ];
        for e in events {
            assert_eq!(e.request_id, xr, "exec id stays the correlation request_id");
            assert_eq!(
                e.task_id.as_deref(),
                Some(src),
                "source frame id rides in task_id"
            );
        }

        // None is a no-op: the field stays unset (single-machine / non-manager).
        let plain = AuditEvent::approval_denied("e".into(), "ts".into(), xr).with_task_id(None);
        assert_eq!(plain.task_id, None);
    }

    #[test]
    fn ai_audit_event_payload_round_trips() {
        use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};
        let config: Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> = Configuration::new();
        let env = envelope(process_list_input());
        let payload = AiAuditEventPayload {
            event: AuditEvent::task_created("evt".into(), "ts".into(), &env),
        };

        let json = serde_json::to_string(&payload).expect("json encode");
        let back: AiAuditEventPayload = serde_json::from_str(&json).expect("json decode");
        assert_eq!(payload, back);

        let bytes = wincode::config::serialize(&payload, config).expect("wincode encode");
        let back2: AiAuditEventPayload =
            wincode::config::deserialize(&bytes, config).expect("wincode decode");
        assert_eq!(payload, back2);
    }

    #[test]
    fn audit_sink_is_object_safe() {
        // The emitter holds `Arc<dyn AuditSink>`; confirm the trait is
        // object-safe and `NoopAuditSink` coerces. The async `record`
        // behaviour is exercised in the worker tests (which have a runtime).
        let _sink: std::sync::Arc<dyn AuditSink> = std::sync::Arc::new(NoopAuditSink);
    }
}
