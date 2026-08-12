//! Worker-side [`DeviceAgent`] implementation.
//!
//! Runs inside the user session (WinSta0) where the read collectors and
//! the authoritative capture frame live. The daemon two-phase-parses and
//! authorizes the request, then ships a typed
//! `ServiceToWorker::InvokeAgentCapability` carrying a fully server-stamped
//! [`AgentEnvelope`]; the worker dispatches it here and replies via
//! `WorkerToService::AgentCapabilityCompleted`.
//!
//! Each supported read kind dispatches to a collector in [`collectors`]. A collector
//! that cannot run on the host (no Docker, no session context, unsupported
//! platform) returns a structured `AgentError` so the path degrades gracefully
//! instead of failing the transport.

pub mod audit_sink;
pub mod collectors;
pub mod eval;

use std::sync::Arc;
use std::time::Instant;

use desk_agent_protocol::audit::{AuditEvent, AuditSink, NoopAuditSink};
use desk_agent_protocol::{
    AgentEnvelope, AgentError, AgentErrorKind, ContextKind, DeviceAgent, OperationInput,
    OperationOutput, ReadContextOutput,
};

use crate::model::settings::SharedSettings;

/// User-session capability surface. Most collectors construct their own probes
/// per call, so the state is an optional handle to the live session settings —
/// required by `screen.capture.current` to resolve the capture backend and
/// target display — plus the audit sink that every call's lifecycle is emitted
/// to. Built without settings elsewhere (tests, hosts with no session), where
/// screen capture degrades to `UnsupportedCapability`; built without an audit
/// sink (defaults to no-op) where auditing is not wired.
pub struct LocalDeviceAgent {
    settings: Option<Arc<SharedSettings>>,
    audit: Arc<dyn AuditSink>,
}

impl Default for LocalDeviceAgent {
    fn default() -> Self {
        Self {
            settings: None,
            audit: Arc::new(NoopAuditSink),
        }
    }
}

impl LocalDeviceAgent {
    /// Build an agent without a session context. Screen capture is unavailable.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build an agent bound to the worker session settings, enabling screen
    /// capture.
    pub fn with_settings(settings: Arc<SharedSettings>) -> Self {
        Self {
            settings: Some(settings),
            audit: Arc::new(NoopAuditSink),
        }
    }

    /// Attach the audit sink that the call lifecycle is recorded to.
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }
}

#[async_trait::async_trait]
impl DeviceAgent for LocalDeviceAgent {
    async fn invoke(&self, envelope: AgentEnvelope) -> Result<OperationOutput, AgentError> {
        // Emit the full audit lifecycle around the call. The worker is the one
        // point that holds both the server-stamped envelope and the outcome, so
        // task.created / context.collected / task.completed|failed are all
        // recorded here with no cross-process correlation.
        let started = Instant::now();
        self.audit
            .record(AuditEvent::task_created(
                new_event_id(),
                now_rfc3339(),
                &envelope,
            ))
            .await;

        // Raw `exec` requests use a separate confirmed-execution path; the
        // daemon already rejects them here, but defend in depth in the worker
        // too. The input is cloned so the envelope stays
        // available for the post-dispatch audit events.
        let result = match envelope.operation.input.clone() {
            OperationInput::ReadContext(rc) => {
                dispatch_read_context(rc.kind, self.settings.as_ref()).await
            }
            OperationInput::Exec(_) => Err(unsupported("exec is not available until M2")),
        };

        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        match &result {
            Ok(output) => {
                // `context.collected` is a read concept; only read outputs emit
                // it (exec, when it lands, will not).
                if matches!(output, OperationOutput::ReadContext(_)) {
                    self.audit
                        .record(AuditEvent::context_collected(
                            new_event_id(),
                            now_rfc3339(),
                            &envelope,
                            output,
                            duration_ms,
                        ))
                        .await;
                }
                self.audit
                    .record(AuditEvent::task_completed(
                        new_event_id(),
                        now_rfc3339(),
                        &envelope,
                        duration_ms,
                    ))
                    .await;
            }
            Err(error) => {
                self.audit
                    .record(AuditEvent::task_failed(
                        new_event_id(),
                        now_rfc3339(),
                        &envelope,
                        error,
                        duration_ms,
                    ))
                    .await;
            }
        }
        result
    }
}

/// Fresh audit event identifier.
fn new_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Current time as an RFC3339 string (the audit event timestamp format).
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Dispatch a single read kind to its collector. `settings` is only consumed
/// by screen capture; the other collectors are self-contained.
async fn dispatch_read_context(
    kind: ContextKind,
    settings: Option<&Arc<SharedSettings>>,
) -> Result<OperationOutput, AgentError> {
    match kind {
        ContextKind::SystemInfo(params) => {
            let output = run_blocking(move || collectors::system_info::collect(&params)).await?;
            Ok(OperationOutput::ReadContext(ReadContextOutput::SystemInfo(
                output,
            )))
        }
        ContextKind::ProcessList(params) => {
            let output = run_blocking(move || collectors::process_list::collect(&params)).await?;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::ProcessList(output),
            ))
        }
        ContextKind::NetworkPorts(params) => {
            let output =
                run_blocking(move || collectors::network_ports::collect(&params)).await??;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::NetworkPorts(output),
            ))
        }
        ContextKind::ServiceStatus(params) => {
            let output =
                run_blocking(move || collectors::service_status::collect(&params)).await??;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::ServiceStatus(output),
            ))
        }
        ContextKind::LogRecent(params) => {
            let output = run_blocking(move || collectors::log_recent::collect(&params)).await??;
            Ok(OperationOutput::ReadContext(ReadContextOutput::LogRecent(
                output,
            )))
        }
        ContextKind::ContainerList(params) => {
            let output = collectors::container::list(&params).await?;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::ContainerList(output),
            ))
        }
        ContextKind::ContainerInspect(params) => {
            let output = collectors::container::inspect(&params).await?;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::ContainerInspect(output),
            ))
        }
        ContextKind::ContainerLogs(params) => {
            let output = collectors::container::logs(&params).await?;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::ContainerLogs(output),
            ))
        }
        ContextKind::ScreenCaptureCurrent(params) => {
            let Some(settings) = settings else {
                return Err(unsupported("screen capture requires a session context"));
            };
            let desk_settings = settings.read().await.desk.clone();
            let output =
                run_blocking(move || collectors::screen_capture::collect(&params, &desk_settings))
                    .await??;
            Ok(OperationOutput::ReadContext(
                ReadContextOutput::ScreenCaptureCurrent(output),
            ))
        }
    }
}

/// Run a synchronous, syscall-heavy collector on the blocking pool so the
/// worker's async reactor is never stalled by a probe (CPU sampling, disk
/// enumeration, ...). A panic in the collector surfaces as `Internal`.
async fn run_blocking<T, F>(f: F) -> Result<T, AgentError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AgentError {
            kind: AgentErrorKind::Internal,
            message: format!("collector task failed to join: {e}"),
            retryable: true,
            safe_for_model: true,
            error_code: None,
        })
}

fn unsupported(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: message.to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{
        ActorRef, ActorType, AgentOperation, AgentScope, AuditMeta, CallerRef, CallerType,
        Capability, ExecInput, ExecTarget, ExecutionMode, ProtocolVersion, ReadContextInput,
        RequestId, ScreenCaptureParams, SystemInfoParams, TargetRef,
    };

    fn envelope_for(input: OperationInput) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("req-1".into()),
            parent_task_id: None,
            target: TargetRef::default(),
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: "local-operator".into(),
            },
            caller: CallerRef {
                caller_type: CallerType::Human,
                model_provider: None,
                model_name: None,
                adapter: None,
            },
            scope: AgentScope {
                granted: vec![Capability::SystemInfo, Capability::ProcessList],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input,
            },
            audit: AuditMeta {
                approval_id: None,
                reason: None,
            },
        }
    }

    /// `system.info` returns a real structured snapshot through the full
    /// `invoke` → dispatch → `spawn_blocking` collector path.
    #[tokio::test]
    async fn system_info_returns_structured_snapshot() {
        let agent = LocalDeviceAgent::new();
        let env = envelope_for(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SystemInfo(SystemInfoParams::default()),
        }));
        let out = agent.invoke(env).await.expect("system.info must succeed");
        let OperationOutput::ReadContext(ReadContextOutput::SystemInfo(info)) = out else {
            panic!("expected a system.info output");
        };
        // CPU core count is the most reliably non-zero field across CI hosts.
        assert!(info.cpu.logical_cores >= 1);
        assert!(info.memory.total_bytes > 0);
    }

    /// Without a session context, screen capture degrades to
    /// `UnsupportedCapability` rather than panicking (the capture backend needs
    /// the live desk settings, absent in a settings-less agent).
    #[tokio::test]
    async fn screen_capture_without_settings_is_unsupported() {
        let agent = LocalDeviceAgent::new();
        let env = envelope_for(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ScreenCaptureCurrent(ScreenCaptureParams::default()),
        }));
        let err = agent
            .invoke(env)
            .await
            .expect_err("must reject without settings");
        assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
    }

    /// Raw `exec` is rejected in the worker too; the daemon already blocks it
    /// on this path.
    #[tokio::test]
    async fn exec_is_unsupported() {
        let agent = LocalDeviceAgent::new();
        let env = envelope_for(OperationInput::Exec(ExecInput {
            target: ExecTarget::Shell {
                shell: "powershell".into(),
            },
            command: "Get-Service".into(),
            cwd: None,
            timeout_ms: 1_000,
            max_stdout_bytes: 1_024,
            max_stderr_bytes: 1_024,
        }));
        let err = agent.invoke(env).await.expect_err("exec must reject");
        assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
    }

    /// Records every event for assertions in audit-lifecycle tests.
    #[derive(Clone, Default)]
    struct RecordingAuditSink {
        events: Arc<std::sync::Mutex<Vec<AuditEvent>>>,
    }

    #[async_trait::async_trait]
    impl AuditSink for RecordingAuditSink {
        async fn record(&self, event: AuditEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl RecordingAuditSink {
        fn event_types(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.event_type.clone())
                .collect()
        }
    }

    /// A successful read emits the full lifecycle: task.created →
    /// context.collected (with the derived capability) → task.completed.
    #[tokio::test]
    async fn audit_lifecycle_on_success() {
        let sink = RecordingAuditSink::default();
        let agent = LocalDeviceAgent::new().with_audit(Arc::new(sink.clone()));
        let env = envelope_for(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SystemInfo(SystemInfoParams::default()),
        }));
        agent.invoke(env).await.expect("system.info must succeed");

        assert_eq!(
            sink.event_types(),
            vec![
                "ai.task.created".to_string(),
                "ai.context.collected".to_string(),
                "ai.task.completed".to_string(),
            ],
        );
        let events = sink.events.lock().unwrap();
        let collected = &events[1];
        assert_eq!(collected.capability.as_deref(), Some("system.info"));
        assert_eq!(collected.result, "ok");
        assert!(collected.duration_ms.is_some());
        assert_eq!(collected.request_id, "req-1");
    }

    /// A failed read (screen capture without a session context) emits
    /// task.created → task.failed and no context.collected.
    #[tokio::test]
    async fn audit_lifecycle_on_failure() {
        let sink = RecordingAuditSink::default();
        let agent = LocalDeviceAgent::new().with_audit(Arc::new(sink.clone()));
        let env = envelope_for(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ScreenCaptureCurrent(ScreenCaptureParams::default()),
        }));
        agent
            .invoke(env)
            .await
            .expect_err("must fail without settings");

        assert_eq!(
            sink.event_types(),
            vec!["ai.task.created".to_string(), "ai.task.failed".to_string()],
        );
        let events = sink.events.lock().unwrap();
        assert_eq!(events[1].result, "error");
        assert_eq!(
            events[1].output_summary.as_deref(),
            Some("UnsupportedCapability"),
        );
    }
}
