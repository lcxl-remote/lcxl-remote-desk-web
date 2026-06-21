//! Production seams for the Direct (single-machine) agentic exec path.
//!
//! Wires the three [`super::agent`] exec seams to the daemon's real machinery,
//! reusing the single-machine confirm-exec building blocks:
//!
//! - [`TemplateExecClassifier`] classifies via the same `classify_command_with`
//!   the browser `ConfirmExec` path uses (built-in baseline ∪ operator templates);
//! - [`ControlExecApprover`] pushes an `ExecPreview` to the control connection and
//!   awaits the operator's `ResolveExec` through the [`AgenticExecCoordinator`];
//! - [`WorkerExecRunner`] dispatches the sealed plan to the worker over IPC and
//!   awaits the worker's `ExecResult`, again bridged by the coordinator.
//!
//! Approval / result waits are bounded: a timeout (or a dropped channel) maps to
//! `TimedOut` / `OutcomeUnknown`, so the loop never blocks forever.

use std::sync::Arc;
use std::time::Duration;

use desk_agent_protocol::exec::{ExecPlan, ExecPreview, ExecRequestId};
use desk_agent_protocol::{AgentError, AgentErrorKind, OperationInput};
use desk_ipc_protocol::message::{ExecPlanPayload, ServiceToWorker};
use tokio::sync::broadcast;

use super::agent::{
    DirectApproval, DirectClassified, DirectExecApprover, DirectExecClassifier, DirectExecParts,
    DirectExecRunner, DirectRun, ExecApprovalRequest,
};
use crate::daemon::agentic_exec::AgenticExecCoordinator;
use crate::daemon::command_templates::CommandTemplateCache;
use crate::daemon::signaling_router::send_exec_preview;
use crate::daemon::worker_manager::WorkerManager;

/// How long the operator has to decide before the approval is treated as timed out.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for the worker's exec result before treating the outcome as
/// unknown (the worker may have run it; the loop then closes the conversation §6).
const RESULT_TIMEOUT: Duration = Duration::from_secs(180);

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
    }
}

/// Classifier backed by the daemon's command templates (baseline ∪ operator sync).
pub struct TemplateExecClassifier {
    command_templates: Arc<CommandTemplateCache>,
}

impl TemplateExecClassifier {
    pub fn new(command_templates: Arc<CommandTemplateCache>) -> Self {
        Self { command_templates }
    }
}

#[async_trait::async_trait(?Send)]
impl DirectExecClassifier for TemplateExecClassifier {
    async fn classify(
        &self,
        input: &OperationInput,
        _reason: Option<&str>,
    ) -> Result<DirectClassified, AgentError> {
        let OperationInput::Exec(exec_input) = input else {
            return Err(internal("classifier received a non-exec operation"));
        };
        let templates = self.command_templates.snapshot();
        let outcome = crate::exec::classify_command_with(exec_input, templates.as_slice());
        Ok(DirectClassified {
            classification: outcome.classification,
            draft: outcome.draft,
        })
    }
}

/// Approver that pushes an `ExecPreview` to the control connection and awaits the
/// `ResolveExec` decision via the coordinator.
pub struct ControlExecApprover {
    coordinator: Arc<AgenticExecCoordinator>,
    outbound_tx: broadcast::Sender<String>,
    timeout: Duration,
}

impl ControlExecApprover {
    pub fn new(
        coordinator: Arc<AgenticExecCoordinator>,
        outbound_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            coordinator,
            outbound_tx,
            timeout: APPROVAL_TIMEOUT,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl DirectExecApprover for ControlExecApprover {
    async fn request_approval(
        &self,
        request: &ExecApprovalRequest,
    ) -> Result<DirectApproval, AgentError> {
        // No control connection to ask → approval is unobtainable; nothing runs.
        let Some(connection_id) = request.connection_id.clone() else {
            return Ok(DirectApproval::Cancelled);
        };
        // Register the awaiting decision before pushing the preview so a fast
        // ResolveExec can never race ahead of the registration.
        let rx = self
            .coordinator
            .register_approval(request.exec_request_id.clone());
        let preview = ExecPreview {
            exec_request_id: Some(ExecRequestId(request.exec_request_id.clone())),
            shell: request.shell.clone(),
            command: request.command.clone(),
            cwd: request.cwd.clone(),
            timeout_ms: request.draft.timeout_ms,
            risk: request.classification.risk,
            impact: request.classification.impact.clone(),
            policy_note: request.classification.matched_template.clone(),
            requires_confirmation: true,
            executable: true,
            blocked_reason: None,
        };
        send_exec_preview(
            &self.outbound_tx,
            &request.exec_request_id,
            Some(connection_id),
            preview,
        );
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => Ok(DirectApproval::Approved),
            Ok(Ok(false)) => Ok(DirectApproval::Rejected),
            // The sender was dropped (connection closed / superseded) — nothing ran.
            Ok(Err(_)) => Ok(DirectApproval::Cancelled),
            Err(_) => {
                self.coordinator.cancel_approval(&request.exec_request_id);
                Ok(DirectApproval::TimedOut)
            }
        }
    }
}

/// Runner that dispatches a sealed plan to the worker over IPC and awaits its
/// `ExecResult` (delivered through the coordinator by the signaling proxy).
pub struct WorkerExecRunner {
    coordinator: Arc<AgenticExecCoordinator>,
    worker_mgr: WorkerManager,
    timeout: Duration,
}

impl WorkerExecRunner {
    pub fn new(coordinator: Arc<AgenticExecCoordinator>, worker_mgr: WorkerManager) -> Self {
        Self {
            coordinator,
            worker_mgr,
            timeout: RESULT_TIMEOUT,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl DirectExecRunner for WorkerExecRunner {
    async fn run(&self, plan: ExecPlan) -> Result<DirectRun, AgentError> {
        let exec_request_id = plan.exec_request_id.0.clone();
        // Register the result channel before dispatch so the worker's result (which
        // can return quickly) is always delivered to this waiter.
        let rx = self.coordinator.register_result(exec_request_id.clone());
        let payload = ExecPlanPayload {
            request_id: exec_request_id.clone(),
            // The result is consumed by the coordinator, not routed to a browser, so
            // the proxy suppresses the outbound frame and connection_id is unused.
            connection_id: None,
            plan,
            audit_source_request_id: None,
        };
        if let Err(e) = self
            .worker_mgr
            .send_to_worker(ServiceToWorker::ExecPlan(payload))
            .await
        {
            self.coordinator.cancel_result(&exec_request_id);
            // The worker never received the plan, so nothing ran — a definite,
            // model-safe failure (not an unknown outcome).
            return Err(AgentError {
                kind: AgentErrorKind::TargetOffline,
                message: format!("worker unavailable: {e}"),
                retryable: true,
                safe_for_model: true,
            });
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(outcome)) => Ok(DirectRun::Sent(outcome)),
            // A dropped sender or a timeout after dispatch: the command may have run,
            // so the outcome is unknown (the loop closes the conversation via §6).
            Ok(Err(_)) | Err(_) => {
                self.coordinator.cancel_result(&exec_request_id);
                Ok(DirectRun::OutcomeUnknown)
            }
        }
    }
}

/// The daemon dependencies needed to enable the Direct agentic exec path.
pub struct DirectExecSupport {
    pub coordinator: Arc<AgenticExecCoordinator>,
    pub outbound_tx: broadcast::Sender<String>,
    pub worker_mgr: WorkerManager,
    pub command_templates: Arc<CommandTemplateCache>,
}

impl DirectExecSupport {
    /// Build the [`DirectExecParts`] (classifier + approver + runner) injected into
    /// the [`DirectToolSeam`](super::agent::DirectToolSeam).
    pub fn into_parts(self) -> DirectExecParts {
        DirectExecParts {
            classifier: Arc::new(TemplateExecClassifier::new(self.command_templates)),
            approver: Arc::new(ControlExecApprover::new(
                self.coordinator.clone(),
                self.outbound_tx,
            )),
            runner: Arc::new(WorkerExecRunner::new(self.coordinator, self.worker_mgr)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::exec::{
        CommandClassification, ExecDecision, ExecPlanDraft, ExecShellKind,
    };
    use desk_agent_protocol::{ExecInput, ExecTarget, OperationInput, RiskLevel};

    fn exec_input(command: &str) -> OperationInput {
        OperationInput::Exec(ExecInput {
            target: ExecTarget::Shell {
                shell: "powershell".into(),
            },
            command: command.into(),
            cwd: None,
            timeout_ms: 10_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        })
    }

    /// The classifier maps a `classify_command_with` outcome into a
    /// `DirectClassified`. With no operator templates an off-template command is
    /// `NotExecutable` (no sealed draft) — exercising the mapping without depending
    /// on a specific baseline template.
    #[tokio::test]
    async fn classifier_maps_outcome() {
        let classifier = TemplateExecClassifier::new(Arc::new(CommandTemplateCache::new()));
        let classified = classifier
            .classify(&exec_input("some-unmatched-command --flag"), None)
            .await
            .expect("classify ok");
        // Off-template (no operator templates synced) → not executable, no draft.
        assert_eq!(
            classified.classification.decision,
            ExecDecision::NotExecutable
        );
        assert!(classified.draft.is_none());
    }

    /// A non-exec operation is an internal error (the loop only routes exec calls
    /// here, so this is a guard against a contract break).
    #[tokio::test]
    async fn classifier_rejects_non_exec() {
        let classifier = TemplateExecClassifier::new(Arc::new(CommandTemplateCache::new()));
        let input = OperationInput::ReadContext(desk_agent_protocol::ReadContextInput {
            kind: desk_agent_protocol::ContextKind::SystemInfo(
                desk_agent_protocol::SystemInfoParams::default(),
            ),
        });
        assert!(classifier.classify(&input, None).await.is_err());
    }

    fn approval_request(exec_request_id: &str, connection_id: Option<&str>) -> ExecApprovalRequest {
        ExecApprovalRequest {
            exec_request_id: exec_request_id.into(),
            classification: CommandClassification {
                risk: RiskLevel::High,
                matched_template: Some("restart".into()),
                impact: "restarts a service".into(),
                decision: ExecDecision::ConfirmRequired,
                effect: Some(desk_agent_protocol::exec::ExecEffect::Mutating),
            },
            draft: ExecPlanDraft {
                program: "powershell".into(),
                argv: vec!["-Command".into(), "Restart-Service X".into()],
                cwd: None,
                shell: ExecShellKind::Powershell,
                risk: RiskLevel::High,
                template_id: "restart".into(),
                fingerprint: "fp".into(),
                timeout_ms: 10_000,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
            },
            reason: None,
            command: "Restart-Service X".into(),
            shell: "powershell".into(),
            cwd: None,
            connection_id: connection_id.map(|s| s.to_string()),
        }
    }

    /// With no control connection the approver cannot ask anyone → Cancelled
    /// (nothing runs).
    #[tokio::test]
    async fn approver_without_connection_is_cancelled() {
        let (tx, _rx) = broadcast::channel(8);
        let approver = ControlExecApprover::new(Arc::new(AgenticExecCoordinator::new()), tx);
        let out = approver
            .request_approval(&approval_request("e1", None))
            .await
            .unwrap();
        assert_eq!(out, DirectApproval::Cancelled);
    }

    /// The approver pushes an `ExecPreview` to the connection and resolves to the
    /// operator's decision delivered through the coordinator.
    #[tokio::test]
    async fn approver_pushes_preview_and_awaits_decision() {
        let coord = Arc::new(AgenticExecCoordinator::new());
        let (tx, mut rx) = broadcast::channel::<String>(8);
        let approver = ControlExecApprover::new(coord.clone(), tx);
        let req = approval_request("e1", Some("browser-1"));

        let (decision, frame) = tokio::join!(approver.request_approval(&req), async {
            // The preview frame is pushed synchronously inside request_approval; read
            // it, then deliver the approval.
            let frame = rx.recv().await.expect("preview frame");
            assert!(coord.resolve_approval("e1", true), "id was agentic");
            frame
        });

        assert_eq!(decision.unwrap(), DirectApproval::Approved);
        // The pushed frame is an ExecPreview carrying the exec_request_id + command.
        assert!(frame.contains("\"exec_request_id\":\"e1\""));
        assert!(frame.contains("Restart-Service X"));
        assert_eq!(coord.pending_counts().0, 0, "approval entry consumed");
    }

    /// A rejected decision maps to `Rejected`.
    #[tokio::test]
    async fn approver_maps_rejection() {
        let coord = Arc::new(AgenticExecCoordinator::new());
        let (tx, mut rx) = broadcast::channel::<String>(8);
        let approver = ControlExecApprover::new(coord.clone(), tx);
        let req = approval_request("e2", Some("browser-1"));
        let (decision, _) = tokio::join!(approver.request_approval(&req), async {
            let _ = rx.recv().await;
            coord.resolve_approval("e2", false);
        });
        assert_eq!(decision.unwrap(), DirectApproval::Rejected);
    }
}
