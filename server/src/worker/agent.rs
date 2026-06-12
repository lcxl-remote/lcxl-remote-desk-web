//! Worker-side [`DeviceAgent`] implementation.
//!
//! Runs inside the user session (WinSta0) where the read collectors and
//! the authoritative capture frame live. The daemon two-phase-parses and
//! authorizes the request, then ships a typed
//! `ServiceToWorker::AgentRequest` carrying a fully server-stamped
//! [`AgentEnvelope`]; the worker dispatches it here and replies via
//! `WorkerToService::AgentResponse`.
//!
//! The dispatch path is wired end to end. Each P0 read collector
//! (`system.info`, `process.list`, ...) replaces its
//! `UnsupportedCapability` stub arm with a real implementation as it
//! lands — without touching this trait surface or the IPC plumbing.

use desk_agent_protocol::{
    AgentEnvelope, AgentError, AgentErrorKind, ContextKind, DeviceAgent, OperationInput,
    OperationOutput,
};

/// User-session capability surface. Holds no state yet; collectors will
/// add their handles (sysinfo system, docker client, capture source,
/// ...) here as they land.
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

/// Dispatch a single read kind to its collector. Every arm currently
/// returns `UnsupportedCapability`; collectors land incrementally and
/// replace their arm with a real result.
async fn dispatch_read_context(kind: ContextKind) -> Result<OperationOutput, AgentError> {
    match kind {
        ContextKind::SystemInfo(_)
        | ContextKind::ProcessList(_)
        | ContextKind::NetworkPorts(_)
        | ContextKind::ServiceStatus(_)
        | ContextKind::LogRecent(_)
        | ContextKind::ContainerList(_)
        | ContextKind::ContainerInspect(_)
        | ContextKind::ContainerLogs(_)
        | ContextKind::ScreenCaptureCurrent(_) => {
            Err(unsupported("read collector not implemented yet"))
        }
    }
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
        Capability, ExecutionMode, ProcessListParams, ProtocolVersion, ReadContextInput, RequestId,
        TargetRef,
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
                granted: vec![Capability::ProcessList],
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

    /// Until the collectors land, every read kind degrades to
    /// `UnsupportedCapability` rather than panicking — the dispatch path
    /// is exercised end to end.
    #[tokio::test]
    async fn read_context_is_unsupported_for_now() {
        let agent = LocalDeviceAgent::new();
        let env = envelope_for(OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ProcessList(ProcessListParams::default()),
        }));
        let err = agent.invoke(env).await.expect_err("stub must reject");
        assert_eq!(err.kind, AgentErrorKind::UnsupportedCapability);
    }
}
