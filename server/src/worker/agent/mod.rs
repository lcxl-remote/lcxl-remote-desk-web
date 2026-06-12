//! Worker-side [`DeviceAgent`] implementation.
//!
//! Runs inside the user session (WinSta0) where the read collectors and
//! the authoritative capture frame live. The daemon two-phase-parses and
//! authorizes the request, then ships a typed
//! `ServiceToWorker::AgentRequest` carrying a fully server-stamped
//! [`AgentEnvelope`]; the worker dispatches it here and replies via
//! `WorkerToService::AgentResponse`.
//!
//! Each P0 read kind dispatches to a collector in [`collectors`]. A kind
//! whose collector has not landed yet returns `UnsupportedCapability` so the
//! path degrades gracefully instead of failing the transport.

pub mod collectors;

use desk_agent_protocol::{
    AgentEnvelope, AgentError, AgentErrorKind, ContextKind, DeviceAgent, OperationInput,
    OperationOutput, ReadContextOutput,
};

/// User-session capability surface. Holds no state yet; collectors construct
/// their own probes (sysinfo system, docker client, capture source, ...) per
/// call, so there is nothing to cache here.
#[derive(Default)]
pub struct LocalDeviceAgent;

impl LocalDeviceAgent {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl DeviceAgent for LocalDeviceAgent {
    async fn invoke(&self, envelope: AgentEnvelope) -> Result<OperationOutput, AgentError> {
        match envelope.operation.input {
            OperationInput::ReadContext(rc) => dispatch_read_context(rc.kind).await,
            // `exec` is reserved until M2; the daemon already rejects it,
            // but defend in depth here too.
            OperationInput::Exec(_) => Err(unsupported("exec is not available until M2")),
        }
    }
}

/// Dispatch a single read kind to its collector. Collectors land
/// incrementally; an unimplemented kind returns `UnsupportedCapability`.
async fn dispatch_read_context(kind: ContextKind) -> Result<OperationOutput, AgentError> {
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
        ContextKind::ContainerList(_)
        | ContextKind::ContainerInspect(_)
        | ContextKind::ContainerLogs(_)
        | ContextKind::ScreenCaptureCurrent(_) => {
            Err(unsupported("read collector not implemented yet"))
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
        })
}

fn unsupported(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: message.to_string(),
        retryable: false,
        safe_for_model: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{
        ActorRef, ActorType, AgentOperation, AgentScope, AuditMeta, CallerRef, CallerType,
        Capability, ContainerListParams, ExecInput, ExecTarget, ExecutionMode, ProtocolVersion,
        ReadContextInput, RequestId, SystemInfoParams, TargetRef,
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
                tenant_id: None,
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
                policy_id: None,
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

    /// Kinds without a collector yet degrade to `UnsupportedCapability`
    /// rather than panicking — the dispatch path stays exercised.
    #[tokio::test]
    async fn unimplemented_read_kind_is_unsupported() {
        let agent = LocalDeviceAgent::new();
        let env = envelope_for(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ContainerList(ContainerListParams::default()),
        }));
        let err = agent.invoke(env).await.expect_err("stub must reject");
        assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
    }

    /// `exec` is rejected in the worker too (defence in depth; the daemon
    /// already blocks it before M2).
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
}
