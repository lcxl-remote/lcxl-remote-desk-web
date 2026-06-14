//! In-process context collector backing the diagnose orchestrator.
//!
//! Wraps the worker's [`LocalDeviceAgent`] (the same read collectors a raw
//! `AgentRequest` drives) and gathers the policy-selected evidence into an
//! [`EvidenceSnapshot`]. This is the `ContextCollector` used in Default /
//! DeskServer mode, where the worker runs in the daemon process; the
//! service-daemon cross-process path is a later additive implementation of the
//! same trait.
//!
//! The collector applies the **collection-time** policy gate (which
//! capabilities to read, from [`select_capabilities`]); the orchestrator applies
//! the **send-time** redaction pass afterwards. A per-capability collector
//! failure is non-fatal: the error is recorded as an `Err` outcome in the
//! snapshot so the model still sees what could and could not be gathered.

use std::sync::Arc;

use async_trait::async_trait;
use desk_agent_protocol::diagnose::DiagnoseRequestData;
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentOperation, AgentOutcome, AgentScope, AuditMeta,
    CallerRef, CallerType, Capability, DeviceAgent, ExecutionMode, OperationInput, ProtocolVersion,
    RequestId, TargetRef,
};

use super::ContextCollector;
use crate::model::settings::SharedSettings;
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::eval::EvidenceSnapshot;

use super::selection::{CollectionPolicy, context_input_for, select_capabilities};

/// Collects evidence for a diagnosis from the in-process device agent.
pub struct AgentContextCollector {
    agent: Arc<LocalDeviceAgent>,
    settings: Arc<SharedSettings>,
}

impl AgentContextCollector {
    pub fn new(agent: Arc<LocalDeviceAgent>, settings: Arc<SharedSettings>) -> Self {
        Self { agent, settings }
    }
}

#[async_trait]
impl ContextCollector for AgentContextCollector {
    async fn collect(&self, request_id: &str, request: &DiagnoseRequestData) -> EvidenceSnapshot {
        // Read the send-to-model policy live so a settings change takes effect
        // on the next diagnosis without a restart.
        let policy = {
            let settings = self.settings.read().await;
            CollectionPolicy {
                allow_logs: settings.ai_model.allow_logs,
                allow_screen: settings.ai_model.allow_screen,
            }
        };

        let mut entries: Vec<(Capability, AgentOutcome)> = Vec::new();
        for cap in select_capabilities(request, &policy) {
            let Some(input) = context_input_for(cap) else {
                // Capability needs caller-supplied params (a container id) this
                // generic path cannot provide; skip rather than fail.
                continue;
            };
            let envelope =
                build_collect_envelope(request_id, cap, OperationInput::ReadContext(input));
            let outcome = match self.agent.invoke(envelope).await {
                Ok(output) => AgentOutcome::Ok(output),
                Err(error) => AgentOutcome::Err(error),
            };
            entries.push((cap, outcome));
        }

        EvidenceSnapshot::record("live", request.question.clone(), now_rfc3339(), entries)
    }
}

/// Assemble a read-only, server-stamped envelope for one collection call. The
/// orchestrator is the actor; the scope grants exactly the capability being
/// collected. Mirrors the daemon's `build_agent_envelope` trusted-field stamp
/// for the in-process path (no control end supplies any of these).
fn build_collect_envelope(
    request_id: &str,
    cap: Capability,
    input: OperationInput,
) -> AgentEnvelope {
    AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        request_id: RequestId(request_id.to_string()),
        parent_task_id: None,
        target: TargetRef::default(),
        actor: ActorRef {
            actor_type: ActorType::System,
            actor_id: "diagnose-orchestrator".into(),
            tenant_id: None,
        },
        // A human operator drove the diagnosis; no model is the caller of the
        // raw collection step (matches the daemon's direct-read precedent).
        caller: CallerRef {
            caller_type: CallerType::Human,
            model_provider: None,
            model_name: None,
            adapter: None,
        },
        scope: AgentScope {
            granted: vec![cap],
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
            reason: Some("ai diagnose context collection".into()),
        },
    }
}

/// Current time as an RFC3339 string (the snapshot capture timestamp).
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use desk_agent_protocol::ReadContextOutput;

    fn settings_with_logs(allow_logs: bool) -> Arc<SharedSettings> {
        let mut settings = Settings::default();
        settings.ai_model.allow_logs = allow_logs;
        settings.ai_model.allow_screen = false;
        Arc::new(SharedSettings::from(settings))
    }

    fn request(kinds: &[&str]) -> DiagnoseRequestData {
        DiagnoseRequestData {
            question: "why is the host slow?".into(),
            include_screen: false,
            context_kinds: kinds.iter().map(|s| s.to_string()).collect(),
            locale: None,
        }
    }

    /// The default set runs against the real in-process agent; `system.info`
    /// succeeds on every CI host, and the snapshot lists what was collected.
    #[tokio::test]
    async fn collects_default_set_via_agent() {
        let collector =
            AgentContextCollector::new(Arc::new(LocalDeviceAgent::new()), settings_with_logs(true));
        let snapshot = collector.collect("req_1", &request(&[])).await;

        let caps: Vec<&str> = snapshot
            .contexts
            .iter()
            .map(|c| c.capability.as_str())
            .collect();
        assert!(caps.contains(&"system.info"));
        // system.info must have produced a real Ok outcome.
        let system = snapshot
            .contexts
            .iter()
            .find(|c| c.capability == "system.info")
            .expect("system.info collected");
        assert!(matches!(
            system.outcome,
            AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                ReadContextOutput::SystemInfo(_)
            ))
        ));
    }

    /// `allow_logs = false` keeps `log.recent` out of the collected snapshot
    /// even when explicitly requested — the secret-bearing read never runs.
    #[tokio::test]
    async fn logs_disallowed_are_not_collected() {
        let collector = AgentContextCollector::new(
            Arc::new(LocalDeviceAgent::new()),
            settings_with_logs(false),
        );
        let snapshot = collector
            .collect("req_2", &request(&["system.info", "log.recent"]))
            .await;
        let caps: Vec<&str> = snapshot
            .contexts
            .iter()
            .map(|c| c.capability.as_str())
            .collect();
        assert!(caps.contains(&"system.info"));
        assert!(!caps.contains(&"log.recent"));
    }
}
