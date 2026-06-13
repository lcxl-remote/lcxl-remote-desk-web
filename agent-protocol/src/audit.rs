//! AI audit event skeleton.
//!
//! Every AI capability call produces a small lifecycle of audit events
//! (`ai.task.created` → `ai.context.collected` → `ai.task.completed` /
//! `ai.task.failed`). This module defines the runtime event shape, the field
//! mapping from an [`AgentEnvelope`] + outcome, and the [`AuditSink`] contract
//! that consumes them.
//!
//! The event fields mirror the `ai_audit_event` Sea-ORM entity (the M0 schema
//! spike) **minus** the database primary key, so a persistence sink can map an
//! [`AuditEvent`] onto a row one-to-one. M1a keeps a single-machine form (the
//! server logs events); the database-backed sink lands in M2. Because this
//! crate is whitelisted as a cross-boundary dependency, both the device-side
//! emitter (worker) and the future parent-repo persistence sink share this one
//! definition.
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

use crate::{AgentEnvelope, AgentError, Capability, ExecutionMode, OperationOutput};

/// The fixed set of audit event types the M1a skeleton emits. Stored on the
/// wire / in the audit row as the dotted string form (free-text column), so
/// adding a type later (e.g. `ai.capability.denied` with the M4 policy engine)
/// is additive.
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
}

impl AuditEventType {
    /// Dotted event name as written to the audit `event_type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            AuditEventType::TaskCreated => "ai.task.created",
            AuditEventType::ContextCollected => "ai.context.collected",
            AuditEventType::TaskCompleted => "ai.task.completed",
            AuditEventType::TaskFailed => "ai.task.failed",
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

    // ---- model accounting (no model in M1a; reserved) ----
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
            // Risk is the server-classified final value; M1a has no classifier
            // yet (lands with exec / policy in M2/M4), so it stays unset.
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

/// Count the redaction markers carried by an output. M1a collectors emit no
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

/// Consumer of audit events. M1a wires a logging sink (single-machine form);
/// M2 adds a database-backed sink that maps each [`AuditEvent`] to an
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
    fn audit_sink_is_object_safe() {
        // The emitter holds `Arc<dyn AuditSink>`; confirm the trait is
        // object-safe and `NoopAuditSink` coerces. The async `record`
        // behaviour is exercised in the worker tests (which have a runtime).
        let _sink: std::sync::Arc<dyn AuditSink> = std::sync::Arc::new(NoopAuditSink);
    }
}
