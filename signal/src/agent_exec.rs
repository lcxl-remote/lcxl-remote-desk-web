//! Owner-confirmed agentic execution for the single-node OSS signal brain.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use desk_agent_protocol::authz::{
    AUTHORIZATION_BLOCK_VERSION, AuthorizationBlock, AuthorizedControlPayload, AuthzActor,
    AuthzDevice, ExecAdmissionPolicy,
};
use desk_agent_protocol::edge_exec::{
    EdgeExecDisposition, EdgeExecRequestPayload, EdgeExecResultPayload,
};
use desk_agent_protocol::evidence::EvidenceSnapshot;
use desk_agent_protocol::exec::{
    ApprovalDecision, ApprovalId, ExecDecision, ExecPlan, ExecPreview, ExecRequestId,
    ResolveExecData,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, AgentOutcome, AgentScope, ExecInput, OperationInput, RiskLevel,
};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::exec_classify::classify_command_with_policy;
use desk_diagnose_core::exec_tools::build_exec_input;
use desk_diagnose_core::read_tools::build_read_operation;
use desk_diagnose_core::seam::{ExecContext, ExecIdentity, ExecOutcome, ToolRunOutput, ToolSeam};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::service::EdgeExecObserver;
use sea_orm::DatabaseConnection;
use tokio::sync::oneshot;

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const RESULT_SLACK: Duration = Duration::from_secs(30);

struct ApprovalPending {
    browser_connection_id: String,
    tx: oneshot::Sender<ApprovalDecision>,
}

struct ResultPending {
    target_connection_id: String,
    tx: oneshot::Sender<EdgeExecDisposition>,
}

#[derive(Default)]
pub struct SignalAgentExecPending {
    approvals: Mutex<HashMap<String, ApprovalPending>>,
    results: Mutex<HashMap<String, ResultPending>>,
}

impl SignalAgentExecPending {
    pub fn new() -> Self {
        Self::default()
    }

    fn register_approval(
        &self,
        request_id: String,
        browser_connection_id: String,
    ) -> Option<oneshot::Receiver<ApprovalDecision>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.approvals.lock().expect("approval pending lock");
        if pending.contains_key(&request_id) {
            return None;
        }
        pending.insert(
            request_id,
            ApprovalPending {
                browser_connection_id,
                tx,
            },
        );
        Some(rx)
    }

    pub fn resolve(&self, browser_connection_id: &str, data: &ResolveExecData) -> bool {
        let mut pending = self.approvals.lock().expect("approval pending lock");
        let Some(entry) = pending.get(&data.exec_request_id.0) else {
            return false;
        };
        if entry.browser_connection_id != browser_connection_id {
            return false;
        }
        let entry = pending
            .remove(&data.exec_request_id.0)
            .expect("entry checked above");
        let _ = entry.tx.send(data.decision);
        true
    }

    fn cancel_approval(&self, request_id: &str) {
        self.approvals
            .lock()
            .expect("approval pending lock")
            .remove(request_id);
    }

    /// Cancel every approval currently waiting on this browser. Dropping the
    /// senders wakes the parked tool calls, which settle as cancelled and never
    /// reach dispatch.
    pub fn cancel_approvals_for_browser(&self, browser_connection_id: &str) -> usize {
        let mut pending = self.approvals.lock().expect("approval pending lock");
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, entry)| entry.browser_connection_id == browser_connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            pending.remove(id);
        }
        ids.len()
    }

    fn register_result(
        &self,
        request_id: String,
        target_connection_id: String,
    ) -> Option<oneshot::Receiver<EdgeExecDisposition>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.results.lock().expect("result pending lock");
        if pending.contains_key(&request_id) {
            return None;
        }
        pending.insert(
            request_id,
            ResultPending {
                target_connection_id,
                tx,
            },
        );
        Some(rx)
    }

    fn deliver_result(&self, source_connection_id: &str, payload: EdgeExecResultPayload) -> bool {
        let mut pending = self.results.lock().expect("result pending lock");
        let Some(entry) = pending.get(&payload.request_id) else {
            return false;
        };
        if entry.target_connection_id != source_connection_id {
            return false;
        }
        let entry = pending
            .remove(&payload.request_id)
            .expect("entry checked above");
        let _ = entry.tx.send(payload.disposition);
        true
    }

    fn cancel_result(&self, request_id: &str) {
        self.results
            .lock()
            .expect("result pending lock")
            .remove(request_id);
    }

    /// Wake every waiter bound to a signaling connection that just closed.
    pub fn drain_for_connection(&self, connection_id: &str) {
        self.cancel_approvals_for_browser(connection_id);
        let mut results = self.results.lock().expect("result pending lock");
        let ids: Vec<String> = results
            .iter()
            .filter(|(_, entry)| entry.target_connection_id == connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            results.remove(&id);
        }
    }
}

pub fn global_agent_exec_pending() -> Arc<SignalAgentExecPending> {
    static STORE: OnceLock<Arc<SignalAgentExecPending>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(SignalAgentExecPending::new()))
        .clone()
}

fn safe(kind: AgentErrorKind, message: impl Into<String>) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

fn outcome_content(outcome: &AgentOutcome) -> String {
    match outcome {
        AgentOutcome::Ok(output) => {
            serde_json::to_string(output).unwrap_or_else(|_| "{}".to_string())
        }
        AgentOutcome::Err(error) if error.safe_for_model => {
            format!("execution failed: {}", error.message)
        }
        AgentOutcome::Err(_) => "execution failed".to_string(),
    }
}

async fn send_frame(target: &ConnectionState, frame: &SignalingModel) -> Result<(), AgentError> {
    let text = serde_json::to_string(frame).map_err(|e| {
        safe(
            AgentErrorKind::Internal,
            format!("encode signaling frame: {e}"),
        )
    })?;
    target.session.write().await.text(text).await.map_err(|e| {
        safe(
            AgentErrorKind::TransportError,
            format!("send signaling frame: {e}"),
        )
    })
}

/// Tools for one signal-owned agent turn. Reads replay the already-redacted
/// collection snapshot; mutations take the explicit browser approval path.
pub struct SignalAgentTools {
    db: DatabaseConnection,
    connections: Arc<SharedConnectionMap>,
    pending: Arc<SignalAgentExecPending>,
    target_connection_id: String,
    snapshot: EvidenceSnapshot,
    admission_policy: ExecAdmissionPolicy,
    max_risk: RiskLevel,
}

impl SignalAgentTools {
    pub fn new(
        db: DatabaseConnection,
        connections: Arc<SharedConnectionMap>,
        pending: Arc<SignalAgentExecPending>,
        target_connection_id: String,
        snapshot: EvidenceSnapshot,
        admission_policy: ExecAdmissionPolicy,
        max_risk: RiskLevel,
    ) -> Self {
        Self {
            db,
            connections,
            pending,
            target_connection_id,
            snapshot,
            admission_policy,
            max_risk,
        }
    }

    async fn connection(&self, id: &str) -> Result<ConnectionState, AgentError> {
        self.connections
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| safe(AgentErrorKind::TargetOffline, "signaling peer disconnected"))
    }

    async fn push_preview(
        &self,
        browser_connection_id: &str,
        preview: &ExecPreview,
    ) -> Result<(), AgentError> {
        let browser = self.connection(browser_connection_id).await?;
        let frame = SignalingModel::new(
            preview
                .exec_request_id
                .as_ref()
                .map(|id| id.0.as_str())
                .unwrap_or_default(),
            SignalingType::ExecPreview,
            None,
            Some(browser_connection_id.to_string()),
            serde_json::to_value(preview).ok(),
            None,
        );
        send_frame(&browser, &frame).await
    }

    async fn dispatch(
        &self,
        actor_user_id: i32,
        scope: AgentScope,
        plan: ExecPlan,
        validation_input: ExecInput,
    ) -> Result<EdgeExecDisposition, AgentError> {
        let target = self.connection(&self.target_connection_id).await?;
        let audience = target
            .model
            .version_info
            .client_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                safe(
                    AgentErrorKind::PermissionDenied,
                    "target device has no trusted client id",
                )
            })?;
        let request_id = plan.execution_generation.clone();
        let Some(rx) = self
            .pending
            .register_result(request_id.clone(), self.target_connection_id.clone())
        else {
            return Err(safe(
                AgentErrorKind::Internal,
                "an execution with this id is already pending",
            ));
        };
        let authz = AuthorizationBlock {
            version: AUTHORIZATION_BLOCK_VERSION,
            exec_admission_policy: self.admission_policy,
            scope,
            orchestrator_grants: vec!["shell.plan".to_string()],
            max_risk: self.max_risk,
            actor: AuthzActor {
                user_id: Some(actor_user_id),
            },
            device: AuthzDevice { device_id: None },
            request_id: request_id.clone(),
            session_id: None,
            expires_at: Some(
                (chrono::Utc::now() + chrono::Duration::minutes(5))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            issuer: "signal".to_string(),
            audience,
            signature: None,
        };
        let inner = serde_json::to_value(EdgeExecRequestPayload::Agentic {
            plan: plan.clone(),
            validation_input,
        })
        .map_err(|e| {
            safe(
                AgentErrorKind::Internal,
                format!("encode exec request: {e}"),
            )
        })?;
        let wrapper = AuthorizedControlPayload { inner, authz };
        let frame = SignalingModel::new(
            &request_id,
            SignalingType::EdgeExecRequest,
            None,
            Some(self.target_connection_id.clone()),
            serde_json::to_value(wrapper).ok(),
            None,
        );
        if let Err(error) = send_frame(&target, &frame).await {
            self.pending.cancel_result(&request_id);
            return Err(error);
        }
        let timeout = Duration::from_millis(plan.timeout_ms as u64) + RESULT_SLACK;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(disposition)) => Ok(disposition),
            _ => {
                self.pending.cancel_result(&request_id);
                Ok(EdgeExecDisposition::ExecutionStateUnknown {
                    reason: "the host did not return an execution result".into(),
                })
            }
        }
    }
}

#[async_trait(?Send)]
impl ToolSeam for SignalAgentTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        let (capability, _) = build_read_operation(call)?;
        let Some(entry) = self
            .snapshot
            .contexts
            .iter()
            .find(|entry| entry.capability == capability.as_str())
        else {
            return Err(safe(
                AgentErrorKind::UnsupportedCapability,
                format!("{} was not collected for this turn", capability.as_str()),
            ));
        };
        match &entry.outcome {
            AgentOutcome::Ok(output) => Ok(ToolRunOutput {
                content: serde_json::to_string(output).unwrap_or_else(|_| "{}".to_string()),
                image_data_url: entry.image_data_url.clone(),
            }),
            AgentOutcome::Err(error) => Err(error.clone()),
        }
    }

    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        let (operation, _) = build_exec_input(call)?;
        let validation_input = match operation {
            OperationInput::Exec(input) => input,
            _ => {
                return Err(safe(
                    AgentErrorKind::Internal,
                    "exec tool mapped to non-exec input",
                ));
            }
        };
        let classified = classify_command_with_policy(
            &validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            self.admission_policy,
        );
        let Some(draft) = classified.draft else {
            return Ok(ExecOutcome::Rejected {
                reason: Some(classified.classification.impact),
            });
        };
        if classified.classification.decision != ExecDecision::ConfirmRequired
            || draft.risk > self.max_risk
        {
            return Ok(ExecOutcome::Rejected {
                reason: Some("the command is not admitted by the current policy".into()),
            });
        }
        let browser_connection_id = ctx.connection_id.as_deref().ok_or_else(|| {
            safe(
                AgentErrorKind::PermissionDenied,
                "no live operator connection is available for approval",
            )
        })?;
        let exec_request_id = ExecRequestId(uuid::Uuid::new_v4().to_string());
        let preview = ExecPreview {
            exec_request_id: Some(exec_request_id.clone()),
            shell: match &validation_input.target {
                desk_agent_protocol::ExecTarget::Shell { shell } => shell.clone(),
                _ => String::new(),
            },
            command: validation_input.command.clone(),
            cwd: validation_input.cwd.clone(),
            timeout_ms: draft.timeout_ms,
            risk: draft.risk,
            execution_basis: draft.execution_basis,
            impact: classified.classification.impact.clone(),
            policy_note: if draft.execution_basis
                == desk_agent_protocol::exec::ExecExecutionBasis::OwnerBlocklistOnly
            {
                Some("owner-confirmed free-form command; only the blocklist was checked".into())
            } else {
                classified.classification.matched_template.clone()
            },
            requires_confirmation: true,
            executable: true,
            blocked_reason: None,
        };
        let Some(approval_rx) = self
            .pending
            .register_approval(exec_request_id.0.clone(), browser_connection_id.to_string())
        else {
            return Err(safe(
                AgentErrorKind::Internal,
                "an approval with this id is already pending",
            ));
        };
        if let Err(error) = self.push_preview(browser_connection_id, &preview).await {
            self.pending.cancel_approval(&exec_request_id.0);
            return Err(error);
        }
        let approved = match tokio::time::timeout(APPROVAL_TIMEOUT, approval_rx).await {
            Ok(Ok(ApprovalDecision::Approve)) => true,
            Ok(Ok(ApprovalDecision::Reject)) => false,
            Ok(Err(_)) => {
                return Ok(ExecOutcome::Cancelled {
                    reason: Some("the approval session was cancelled".into()),
                });
            }
            Err(_) => {
                self.pending.cancel_approval(&exec_request_id.0);
                return Ok(ExecOutcome::ApprovalTimeout);
            }
        };
        if !approved {
            return Ok(ExecOutcome::Rejected { reason: None });
        }

        // The provider execution mode is the OSS signal's central emergency
        // switch. Re-read it after approval so a preview cannot outlive a
        // tightening from Confirm/Session to ReadOnly/SuggestOnly.
        let current_mode = crate::model_provider::load(&self.db)
            .await
            .map_err(|e| {
                safe(
                    AgentErrorKind::PermissionDenied,
                    format!("execution policy could not be refreshed: {e}"),
                )
            })?
            .execution_mode;
        let effective_mode = ctx.scope.mode.restrict_to(current_mode);
        if !matches!(
            effective_mode,
            desk_agent_protocol::ExecutionMode::ConfirmEachAction
                | desk_agent_protocol::ExecutionMode::SessionApproved
        ) {
            return Ok(ExecOutcome::Rejected {
                reason: Some("execution policy changed; preview the command again".into()),
            });
        }
        let refreshed = classify_command_with_policy(
            &validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            self.admission_policy,
        );
        if refreshed.draft.as_ref() != Some(&draft) {
            return Ok(ExecOutcome::Rejected {
                reason: Some("execution policy changed; preview the command again".into()),
            });
        }

        let execution_id = uuid::Uuid::new_v4().to_string();
        let plan = ExecPlan::from_draft(
            exec_request_id.clone(),
            execution_id.clone(),
            ApprovalId(uuid::Uuid::new_v4().to_string()),
            draft,
        );
        let actor_user_id = ctx.actor_id.parse::<i32>().map_err(|_| {
            safe(
                AgentErrorKind::PermissionDenied,
                "invalid operator identity",
            )
        })?;
        let mut refreshed_scope = ctx.scope.clone();
        refreshed_scope.mode = effective_mode;
        match self
            .dispatch(actor_user_id, refreshed_scope, plan, validation_input)
            .await?
        {
            EdgeExecDisposition::Executed { outcome } => Ok(ExecOutcome::Executed {
                output: ToolRunOutput {
                    content: outcome_content(&outcome),
                    image_data_url: None,
                },
                event_id: None,
            }),
            EdgeExecDisposition::RejectedBeforeDispatch { reason }
            | EdgeExecDisposition::DispatchFailedBeforeWorker { reason }
            | EdgeExecDisposition::HostAtCapacity { reason } => Ok(ExecOutcome::Rejected {
                reason: Some(reason),
            }),
            EdgeExecDisposition::ExecutionStateUnknown { .. } => {
                Ok(ExecOutcome::Unknown(ExecIdentity {
                    work_id: 0,
                    execution_id,
                    exec_request_id: exec_request_id.0,
                }))
            }
        }
    }
}

pub struct SignalEdgeExecObserver {
    pending: Arc<SignalAgentExecPending>,
}

impl SignalEdgeExecObserver {
    pub fn new(pending: Arc<SignalAgentExecPending>) -> Self {
        Self { pending }
    }
}

impl EdgeExecObserver for SignalEdgeExecObserver {
    fn on_fleet_exec_result<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let payload = match model.get_data::<EdgeExecResultPayload>() {
                Ok(payload) => payload,
                Err(error) => {
                    log::warn!("[agent-exec] malformed EdgeExecResult: {error}");
                    return;
                }
            };
            if !self
                .pending
                .deliver_result(&source.model.connection_id, payload)
            {
                log::warn!("[agent-exec] uncorrelated EdgeExecResult was dropped");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(id: &str, decision: ApprovalDecision) -> ResolveExecData {
        ResolveExecData {
            exec_request_id: ExecRequestId(id.into()),
            decision,
        }
    }

    #[tokio::test]
    async fn approval_is_bound_to_the_originating_browser_and_one_shot() {
        let pending = SignalAgentExecPending::new();
        let rx = pending
            .register_approval("e1".into(), "browser-a".into())
            .unwrap();
        assert!(!pending.resolve("browser-b", &resolve("e1", ApprovalDecision::Approve)));
        assert!(pending.resolve("browser-a", &resolve("e1", ApprovalDecision::Approve)));
        assert_eq!(rx.await.unwrap(), ApprovalDecision::Approve);
        assert!(!pending.resolve("browser-a", &resolve("e1", ApprovalDecision::Approve)));
    }

    #[tokio::test]
    async fn browser_disconnect_cancels_only_its_pending_approvals() {
        let pending = SignalAgentExecPending::new();
        let a = pending
            .register_approval("a".into(), "browser-a".into())
            .unwrap();
        let b = pending
            .register_approval("b".into(), "browser-b".into())
            .unwrap();

        assert_eq!(pending.cancel_approvals_for_browser("browser-a"), 1);
        assert!(a.await.is_err());
        assert!(pending.resolve("browser-b", &resolve("b", ApprovalDecision::Approve),));
        assert_eq!(b.await.unwrap(), ApprovalDecision::Approve);
    }

    #[tokio::test]
    async fn target_disconnect_wakes_a_bound_result_waiter() {
        let pending = SignalAgentExecPending::new();
        let rx = pending
            .register_result("r".into(), "edge-a".into())
            .unwrap();
        pending.drain_for_connection("edge-a");
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn edge_result_is_bound_to_the_target_connection_and_one_shot() {
        let pending = SignalAgentExecPending::new();
        let rx = pending
            .register_result("g1".into(), "edge-a".into())
            .unwrap();
        let payload = EdgeExecResultPayload {
            request_id: "g1".into(),
            disposition: EdgeExecDisposition::RejectedBeforeDispatch {
                reason: "test".into(),
            },
        };
        assert!(!pending.deliver_result("edge-b", payload.clone()));
        assert!(pending.deliver_result("edge-a", payload));
        assert!(matches!(
            rx.await.unwrap(),
            EdgeExecDisposition::RejectedBeforeDispatch { .. }
        ));
    }
}
