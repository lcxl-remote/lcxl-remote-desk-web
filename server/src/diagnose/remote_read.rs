//! Edge-side execution of a manager remote read tool call (§8.3).
//!
//! When the agentic loop runs centrally on the manager, the manager ships a
//! server-stamped [`RemoteToolRequest`](desk_agent_protocol::remote_tool::RemoteToolRequest)
//! (one capability call) to this host over the signaling link. The edge keeps
//! **final say** over what may leave the machine: before running anything it
//! re-checks the operation against the envelope's granted scope and the device's
//! local collection policy (the in-process agent itself does not enforce the
//! scope), runs the read, redacts the result fail-closed, and returns the
//! already-redacted [`AgentOutcome`]. The router chunks that into a
//! `RemoteToolResponse`. A gate denial or a redaction failure surfaces as an
//! error, never as leaked raw output.

use std::sync::Arc;

use desk_agent_protocol::{
    AgentEnvelope, AgentError, AgentErrorKind, AgentOutcome, Capability, DeviceAgent,
};

use super::redaction::{Redactor, redact_snapshot};
use crate::model::settings::SharedSettings;
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::eval::EvidenceSnapshot;

/// Current time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A model-safe permission denial (the edge's final say; the loop turns it into an
/// error tool-result).
fn denied(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
    }
}

/// Runs a single server-stamped read envelope against the in-process device agent
/// and returns the already-redacted outcome (fail-closed). Present only where an
/// in-process worker can read locally (Default / DeskServer), mirroring the
/// diagnose collector's availability.
///
/// The manager stamps the envelope, but the edge keeps **final say**: it re-checks
/// the operation is within the envelope's granted scope and that the device's local
/// collection policy permits it (e.g. logs may be disabled here) before invoking,
/// and redacts fail-closed afterward.
pub struct EdgeReadInvoker {
    agent: Arc<LocalDeviceAgent>,
    redactor: Arc<dyn Redactor>,
    settings: Arc<SharedSettings>,
}

impl EdgeReadInvoker {
    pub fn new(
        agent: Arc<LocalDeviceAgent>,
        redactor: Arc<dyn Redactor>,
        settings: Arc<SharedSettings>,
    ) -> Self {
        Self {
            agent,
            redactor,
            settings,
        }
    }

    /// Invoke `envelope` and return its redacted [`AgentOutcome`]. Re-checks the
    /// edge's gates first (scope consistency + local policy), then invokes; a gate
    /// / exec error or a redaction failure returns `Err` (the router turns it into a
    /// wholesale `RemoteToolResponse::Error`, never leaking raw output). The result
    /// is redacted via the same one-entry snapshot path the Direct read seam uses,
    /// so the exact send-time redaction + screenshot refit run.
    pub async fn invoke_redacted(
        &self,
        envelope: AgentEnvelope,
    ) -> Result<AgentOutcome, AgentError> {
        let cap = envelope
            .operation
            .input
            .capability()
            .ok_or_else(|| AgentError {
                kind: AgentErrorKind::UnsupportedCapability,
                message: "remote tool request carries no read capability".to_string(),
                retryable: false,
                safe_for_model: true,
            })?;

        // Edge re-check 1: the operation must be within the envelope's granted
        // scope. The manager always stamps `granted = [cap]`, so a mismatch is a
        // malformed / forged request — deny rather than run an ungranted operation.
        if !envelope.scope.granted.contains(&cap) {
            return Err(denied("operation is outside the granted scope"));
        }

        // Edge re-check 2: local collection policy has the final say. Logs (and
        // container inspect / logs, which carry free text) are gated by `allow_logs`
        // here even when the manager scope permitted them.
        let allow_logs = self.settings.read().await.collection_policy.allow_logs;
        let is_log_read = matches!(
            cap,
            Capability::LogRecent | Capability::ContainerLogs | Capability::ContainerInspect
        );
        if is_log_read && !allow_logs {
            return Err(denied("this read is disabled by the device's local policy"));
        }

        let output = self.agent.invoke(envelope).await?;

        let mut snapshot = EvidenceSnapshot::record(
            "live",
            String::new(),
            now_rfc3339(),
            vec![(cap, AgentOutcome::Ok(output))],
        );
        redact_snapshot(self.redactor.as_ref(), &mut snapshot).map_err(|e| AgentError {
            kind: AgentErrorKind::RedactionFailed,
            message: format!("evidence redaction failed: {}", e.reason),
            retryable: false,
            safe_for_model: true,
        })?;
        super::model::screenshot::refit_snapshot_screenshots(&mut snapshot);

        let entry = snapshot
            .contexts
            .into_iter()
            .next()
            .expect("the one entry we recorded is present");
        Ok(entry.outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnose::redaction::RegexRedactor;
    use crate::model::settings::Settings;
    use desk_agent_protocol::{
        ActorRef, ActorType, AgentOperation, AgentScope, AuditMeta, CallerRef, CallerType,
        ContextKind, ExecutionMode, LogRecentParams, OperationInput, ProtocolVersion,
        ReadContextInput, RequestId, SystemInfoParams, TargetRef,
    };

    fn settings(allow_logs: bool) -> Arc<SharedSettings> {
        let mut s = Settings::default();
        s.collection_policy.allow_logs = allow_logs;
        Arc::new(SharedSettings::from(s))
    }

    fn read_envelope(cap_input: ContextKind, granted: Capability) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("rt-1".into()),
            parent_task_id: None,
            target: TargetRef::default(),
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: "manager".into(),
                tenant_id: None,
            },
            caller: CallerRef {
                caller_type: CallerType::AiModel,
                model_provider: Some("example".into()),
                model_name: Some("m".into()),
                adapter: Some("manager".into()),
            },
            scope: AgentScope {
                granted: vec![granted],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input: OperationInput::ReadContext(ReadContextInput { kind: cap_input }),
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("remote read".into()),
            },
        }
    }

    /// A granted system-info read runs against the real agent and returns a
    /// redacted `Ok` outcome (succeeds on every CI host).
    #[tokio::test]
    async fn invokes_and_returns_redacted_outcome() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true),
        );
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::SystemInfo,
        );
        let outcome = invoker.invoke_redacted(envelope).await.expect("read ok");
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("SystemInfo") || json.contains("hostname"));
    }

    /// An envelope whose granted scope does not cover the operation is denied by
    /// the edge's re-check — final say, an `Err`, never raw output.
    #[tokio::test]
    async fn denies_operation_outside_granted_scope() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(true),
        );
        // Operation reads system info, but the scope grants only process.list.
        let envelope = read_envelope(
            ContextKind::SystemInfo(SystemInfoParams::default()),
            Capability::ProcessList,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("scope mismatch must be denied");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
        assert!(err.safe_for_model);
    }

    /// The device's local policy has final say: a log read is denied when the host
    /// has `allow_logs = false`, even though the manager scope granted it.
    #[tokio::test]
    async fn local_policy_denies_logs_when_disabled() {
        let invoker = EdgeReadInvoker::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            settings(false),
        );
        let envelope = read_envelope(
            ContextKind::LogRecent(LogRecentParams::default()),
            Capability::LogRecent,
        );
        let err = invoker
            .invoke_redacted(envelope)
            .await
            .expect_err("local policy must deny logs");
        assert_eq!(err.kind, AgentErrorKind::PermissionDenied);
    }
}
