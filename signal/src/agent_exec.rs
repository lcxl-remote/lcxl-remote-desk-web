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
    ApprovalDecision, ApprovalId, ExecDecision, ExecExecutionBasis, ExecPlan, ExecPreview,
    ExecRequestId, ResolveExecData,
};
use desk_agent_protocol::exec_lifecycle::{
    ExecControlAction, ExecControlPayload, ExecState, ExecStateReplyPayload,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, AgentOutcome, AgentScope, ExecInput, OperationInput, RiskLevel,
};
use desk_diagnose_core::chat::ToolCall;
use desk_diagnose_core::exec_classify::classify_command_with_policy;
use desk_diagnose_core::exec_tools::{
    build_exec_input, canonical_exec_shell, exec_shell_is_available,
    sanitize_available_exec_shells, unsupported_exec_shell_error,
};
use desk_diagnose_core::read_tools::build_read_operation;
use desk_diagnose_core::seam::{ExecContext, ExecOutcome, ToolRunOutput, ToolSeam, WaitOutcome};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::service::{EdgeExecObserver, ExecStateReplyObserver};
use sea_orm::DatabaseConnection;
use tokio::sync::oneshot;

const RESULT_SLACK: Duration = Duration::from_secs(30);
const FOREGROUND_THRESHOLD: Duration = Duration::from_secs(8);
const WAIT_FOR_TASK_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_FOR_TASK_POLL: Duration = Duration::from_millis(250);

struct ApprovalPending {
    browser_connection_id: String,
    target_connection_id: String,
    diagnose_request_id: String,
    requires_live_carrier: bool,
    tx: oneshot::Sender<ResolveExecData>,
}

struct ResultPending {
    target_connection_id: String,
    tx: oneshot::Sender<EdgeExecDisposition>,
}

struct StateQueryPending {
    target_connection_id: String,
    tx: oneshot::Sender<ExecStateReplyPayload>,
}

#[derive(Default)]
pub struct SignalAgentExecPending {
    approvals: Mutex<HashMap<String, ApprovalPending>>,
    results: Mutex<HashMap<String, ResultPending>>,
    state_queries: Mutex<HashMap<String, StateQueryPending>>,
}

impl SignalAgentExecPending {
    pub fn new() -> Self {
        Self::default()
    }

    fn register_approval(
        &self,
        request_id: String,
        browser_connection_id: String,
        target_connection_id: String,
        diagnose_request_id: String,
        requires_live_carrier: bool,
    ) -> Option<oneshot::Receiver<ResolveExecData>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.approvals.lock().expect("approval pending lock");
        if pending.contains_key(&request_id) {
            return None;
        }
        pending.insert(
            request_id,
            ApprovalPending {
                browser_connection_id,
                target_connection_id,
                diagnose_request_id,
                requires_live_carrier,
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
        if data.decision == ApprovalDecision::Approve {
            if entry.requires_live_carrier {
                let Some(carrier_id) = data.carrier_id.as_deref() else {
                    return false;
                };
                if !crate::exec_pty_carrier::global_exec_pty_carriers().consume_for_approval(
                    carrier_id,
                    browser_connection_id,
                    &entry.target_connection_id,
                    &data.exec_request_id.0,
                ) {
                    return false;
                }
            } else if data.carrier_id.is_some() {
                return false;
            }
        }
        let entry = pending
            .remove(&data.exec_request_id.0)
            .expect("entry checked above");
        if entry.tx.send(data.clone()).is_err() {
            if let Some(carrier_id) = data.carrier_id.as_deref() {
                crate::exec_pty_carrier::global_exec_pty_carriers()
                    .release_failed_approval(carrier_id, &data.exec_request_id.0);
            }
            return false;
        }
        true
    }

    pub(crate) fn can_prepare_carrier(
        &self,
        browser_connection_id: &str,
        target_connection_id: &str,
        exec_request_id: &str,
    ) -> bool {
        self.approvals
            .lock()
            .expect("approval pending lock")
            .get(exec_request_id)
            .is_some_and(|entry| {
                entry.requires_live_carrier
                    && entry.browser_connection_id == browser_connection_id
                    && entry.target_connection_id == target_connection_id
            })
    }

    pub(crate) fn cancel_approval(&self, request_id: &str) {
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

    /// Cancel only approvals owned by one diagnose request on one browser.
    ///
    /// A stale cancellation from an older diagnosis must not cancel a newer
    /// turn's approval on the same signaling connection.
    pub fn cancel_approvals_for_diagnosis(
        &self,
        browser_connection_id: &str,
        diagnose_request_id: &str,
    ) -> usize {
        let mut pending = self.approvals.lock().expect("approval pending lock");
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, entry)| {
                entry.browser_connection_id == browser_connection_id
                    && entry.diagnose_request_id == diagnose_request_id
            })
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

    fn register_state_query(
        &self,
        execution_generation: String,
        target_connection_id: String,
    ) -> Option<oneshot::Receiver<ExecStateReplyPayload>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.state_queries.lock().expect("state query pending lock");
        if pending.contains_key(&execution_generation) {
            return None;
        }
        pending.insert(
            execution_generation,
            StateQueryPending {
                target_connection_id,
                tx,
            },
        );
        Some(rx)
    }

    fn deliver_state_reply(
        &self,
        source_connection_id: &str,
        payload: ExecStateReplyPayload,
    ) -> bool {
        let mut pending = self.state_queries.lock().expect("state query pending lock");
        let Some(entry) = pending.get(&payload.execution_generation) else {
            return false;
        };
        if entry.target_connection_id != source_connection_id {
            return false;
        }
        let entry = pending
            .remove(&payload.execution_generation)
            .expect("entry checked above");
        let _ = entry.tx.send(payload);
        true
    }

    fn cancel_state_query(&self, execution_generation: &str) {
        self.state_queries
            .lock()
            .expect("state query pending lock")
            .remove(execution_generation);
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
        let mut queries = self.state_queries.lock().expect("state query pending lock");
        let ids: Vec<String> = queries
            .iter()
            .filter(|(_, entry)| entry.target_connection_id == connection_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            queries.remove(&id);
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

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
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

fn build_exec_frame(
    target: &ConnectionState,
    target_connection_id: &str,
    actor_user_id: i32,
    scope: AgentScope,
    admission_policy: ExecAdmissionPolicy,
    max_risk: RiskLevel,
    session_connection_id: Option<String>,
    carrier_id: Option<String>,
    plan: &ExecPlan,
    validation_input: &ExecInput,
) -> Result<SignalingModel, AgentError> {
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
    let authz = AuthorizationBlock {
        version: AUTHORIZATION_BLOCK_VERSION,
        exec_admission_policy: admission_policy,
        scope,
        orchestrator_grants: vec!["shell.plan".to_string()],
        max_risk,
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
        validation_input: validation_input.clone(),
        session_connection_id,
        carrier_id,
    })
    .map_err(|error| {
        safe(
            AgentErrorKind::Internal,
            format!("encode exec request: {error}"),
        )
    })?;
    let wrapper = AuthorizedControlPayload { inner, authz };
    Ok(SignalingModel::new(
        &request_id,
        SignalingType::ExecuteEdgePlan,
        None,
        Some(target_connection_id.to_string()),
        serde_json::to_value(wrapper).ok(),
        None,
    ))
}

/// Tools for one signal-owned agent turn. Reads replay the already-redacted
/// collection snapshot; mutations take the explicit browser approval path.
pub struct SignalAgentTools {
    db: DatabaseConnection,
    connections: Arc<SharedConnectionMap>,
    pending: Arc<SignalAgentExecPending>,
    target_connection_id: String,
    diagnose_request_id: String,
    snapshot: EvidenceSnapshot,
    admission_policy: ExecAdmissionPolicy,
    max_risk: RiskLevel,
    available_exec_shells: Vec<String>,
    max_command_runtime_ms: u32,
}

enum SignalDispatch {
    Settled {
        task: crate::entity::agent_exec_task::Model,
        disposition: EdgeExecDisposition,
    },
    Dispatched(crate::entity::agent_exec_task::Model),
    Unknown(crate::entity::agent_exec_task::Model),
}

impl SignalAgentTools {
    pub fn new(
        db: DatabaseConnection,
        connections: Arc<SharedConnectionMap>,
        pending: Arc<SignalAgentExecPending>,
        target_connection_id: String,
        diagnose_request_id: String,
        snapshot: EvidenceSnapshot,
        admission_policy: ExecAdmissionPolicy,
        max_risk: RiskLevel,
        available_exec_shells: Vec<String>,
        max_command_runtime_ms: u32,
    ) -> Self {
        Self {
            db,
            connections,
            pending,
            target_connection_id,
            diagnose_request_id,
            snapshot,
            admission_policy,
            max_risk,
            available_exec_shells: sanitize_available_exec_shells(&available_exec_shells),
            max_command_runtime_ms,
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
            SignalingType::ExecutionPreviewGenerated,
            None,
            Some(browser_connection_id.to_string()),
            serde_json::to_value(preview).ok(),
            None,
        );
        send_frame(&browser, &frame).await
    }

    async fn query_state(
        &self,
        execution_generation: &str,
    ) -> Result<Option<ExecStateReplyPayload>, AgentError> {
        let target = self.connection(&self.target_connection_id).await?;
        let Some(rx) = self.pending.register_state_query(
            execution_generation.to_string(),
            self.target_connection_id.clone(),
        ) else {
            return Ok(None);
        };
        let payload = ExecControlPayload {
            execution_generation: execution_generation.to_string(),
            action: ExecControlAction::QueryState,
        };
        let frame = SignalingModel::new(
            execution_generation,
            SignalingType::ControlExecution,
            None,
            Some(self.target_connection_id.clone()),
            serde_json::to_value(payload).ok(),
            None,
        );
        if let Err(error) = send_frame(&target, &frame).await {
            self.pending.cancel_state_query(execution_generation);
            return Err(error);
        }
        match tokio::time::timeout(Duration::from_secs(5), rx).await {
            Ok(Ok(reply)) => Ok(Some(reply)),
            _ => {
                self.pending.cancel_state_query(execution_generation);
                Ok(None)
            }
        }
    }

    async fn dispatch(
        &self,
        actor_user_id: i32,
        scope: AgentScope,
        plan: ExecPlan,
        validation_input: ExecInput,
        carrier_id: Option<String>,
        ctx: &ExecContext,
    ) -> Result<SignalDispatch, AgentError> {
        let target = self.connection(&self.target_connection_id).await?;
        let request_id = plan.execution_generation.clone();
        let exec_store = crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone());
        let deadline = chrono::Utc::now()
            + chrono::Duration::milliseconds(plan.timeout_ms as i64)
            + chrono::Duration::from_std(RESULT_SLACK).unwrap_or_default();
        let task = exec_store
            .create(
                &plan.exec_request_id.0,
                &request_id,
                &ctx.conversation_id,
                &ctx.tool_call_id,
                &self.target_connection_id,
                deadline,
            )
            .await?;
        let Some(rx) = self
            .pending
            .register_result(request_id.clone(), self.target_connection_id.clone())
        else {
            if let Err(error) = exec_store.mark_unsent(&request_id).await {
                log::warn!(
                    "[agent-exec] could not settle a duplicate unsent task: {}",
                    error.message
                );
            }
            return Err(safe(
                AgentErrorKind::Internal,
                "an execution with this id is already pending",
            ));
        };
        let dispatch_carrier_id = carrier_id.clone();
        let frame = build_exec_frame(
            &target,
            &self.target_connection_id,
            actor_user_id,
            scope.clone(),
            self.admission_policy,
            self.max_risk,
            ctx.connection_id.clone(),
            carrier_id,
            &plan,
            &validation_input,
        )?;
        if let Some(carrier_id) = dispatch_carrier_id.as_deref()
            && !crate::exec_pty_carrier::global_exec_pty_carriers().bind_for_dispatch(
                carrier_id,
                &self.target_connection_id,
                &plan.exec_request_id.0,
                &request_id,
            )
        {
            self.pending.cancel_result(&request_id);
            if let Err(store_error) = exec_store.mark_unsent(&request_id).await {
                log::warn!(
                    "[agent-exec] could not settle an unbound PTY task: {}",
                    store_error.message
                );
            }
            return Err(safe(
                AgentErrorKind::TransportError,
                "the interactive carrier disconnected before dispatch",
            ));
        }
        if let Err(error) = send_frame(&target, &frame).await {
            if let Some(carrier_id) = dispatch_carrier_id.as_deref() {
                crate::exec_pty_carrier::global_exec_pty_carriers()
                    .release_dispatch(carrier_id, &request_id);
            }
            self.pending.cancel_result(&request_id);
            if let Err(store_error) = exec_store.mark_unsent(&request_id).await {
                log::warn!(
                    "[agent-exec] could not settle an unsent task: {}",
                    store_error.message
                );
            }
            return Err(error);
        }
        if let Err(error) = exec_store.mark_running(&request_id).await {
            // The frame may already be executing on the host. Never turn a
            // post-send bookkeeping failure into a claim that nothing ran; keep
            // the durable identity and let a later state query reconcile it.
            log::error!(
                "[agent-exec] execution sent but running state was not persisted: {}",
                error.message
            );
            self.pending.cancel_result(&request_id);
            return Ok(SignalDispatch::Unknown(task));
        }
        match tokio::time::timeout(FOREGROUND_THRESHOLD, rx).await {
            Ok(Ok(disposition)) => Ok(SignalDispatch::Settled { task, disposition }),
            Ok(Err(_)) => {
                self.pending.cancel_result(&request_id);
                let disposition = EdgeExecDisposition::ExecutionStateUnknown {
                    reason: "the host connection closed before returning a result".into(),
                };
                let _ = exec_store
                    .finalize(&self.target_connection_id, &request_id, &disposition)
                    .await?;
                Ok(SignalDispatch::Unknown(task))
            }
            Err(_) => Ok(SignalDispatch::Dispatched(task)),
        }
    }

    /// Execute a command whose user confirmation was already represented by a
    /// one-shot exact capability grant. This path deliberately skips the legacy
    /// `ExecPreview`/`ResolveExec` dialog, but keeps every other defense: current
    /// execution-mode ceiling, verified shell availability, TemplateOnly
    /// classification, immutable plan sealing, daemon re-classification, durable
    /// task tracking and OutcomeUnknown handling.
    pub(crate) async fn execute_preapproved(
        &self,
        mut validation_input: ExecInput,
        ctx: &ExecContext,
        exec_request_id: ExecRequestId,
        execution_generation: String,
        approval_id: ApprovalId,
    ) -> Result<ExecOutcome, AgentError> {
        desk_diagnose_core::exec_tools::apply_exec_runtime_ceiling(
            &mut validation_input,
            self.max_command_runtime_ms,
        );
        let requested_shell = match &validation_input.target {
            desk_agent_protocol::ExecTarget::Shell { shell } => shell.as_str(),
            _ => "",
        };
        if !exec_shell_is_available(requested_shell, &self.available_exec_shells) {
            return Err(unsupported_exec_shell_error(
                requested_shell,
                &self.available_exec_shells,
            ));
        }
        let classified = classify_command_with_policy(
            &validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            ExecAdmissionPolicy::TemplateOnly,
        );
        let Some(draft) = classified.draft else {
            return Ok(ExecOutcome::Rejected {
                reason: Some(classified.classification.impact),
            });
        };
        if draft.requires_root_pty_containment() {
            return Ok(ExecOutcome::Rejected {
                reason: Some(
                    "interactive elevation is unavailable until the Linux ServiceDaemon containment supervisor is ready"
                        .into(),
                ),
            });
        }
        if classified.classification.decision != ExecDecision::ConfirmRequired
            || draft.execution_basis != ExecExecutionBasis::Template
            || draft.risk > self.max_risk
        {
            return Ok(ExecOutcome::Rejected {
                reason: Some("the command is not admitted by the safe-template policy".into()),
            });
        }

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
        if effective_mode != desk_agent_protocol::ExecutionMode::ConfirmEachAction {
            return Ok(ExecOutcome::Rejected {
                reason: Some("confirmed command execution is disabled by current policy".into()),
            });
        }
        let refreshed = classify_command_with_policy(
            &validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            ExecAdmissionPolicy::TemplateOnly,
        );
        if refreshed.draft.as_ref() != Some(&draft) {
            return Ok(ExecOutcome::Rejected {
                reason: Some("execution policy changed; request permission again".into()),
            });
        }

        let plan = ExecPlan::from_draft(exec_request_id, execution_generation, approval_id, draft);
        let actor_user_id = ctx.actor_id.parse::<i32>().map_err(|_| {
            safe(
                AgentErrorKind::PermissionDenied,
                "invalid operator identity",
            )
        })?;
        let mut refreshed_scope = ctx.scope.clone();
        refreshed_scope.mode = effective_mode;
        match self
            .dispatch(
                actor_user_id,
                refreshed_scope,
                plan,
                validation_input,
                None,
                ctx,
            )
            .await?
        {
            SignalDispatch::Settled {
                task,
                disposition: EdgeExecDisposition::Executed { outcome },
            } => Ok(ExecOutcome::Executed {
                data_envelope: None,
                output: ToolRunOutput {
                    content: outcome_content(&outcome),
                    image_data_url: None,
                },
                event_id: Some(task.event_id),
            }),
            SignalDispatch::Settled {
                task,
                disposition:
                    EdgeExecDisposition::RejectedBeforeDispatch { error }
                    | EdgeExecDisposition::DispatchFailedBeforeWorker { error }
                    | EdgeExecDisposition::HostAtCapacity { error },
            } => {
                crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone())
                    .consume_event(&task.event_id)
                    .await?;
                Err(error)
            }
            SignalDispatch::Settled {
                task,
                disposition: EdgeExecDisposition::ExecutionStateUnknown { .. },
            }
            | SignalDispatch::Unknown(task) => Ok(ExecOutcome::Unknown(
                desk_diagnose_core::session::ActionIdentity::agent_exec(
                    task.id,
                    task.exec_request_id,
                    task.execution_generation,
                ),
            )),
            SignalDispatch::Dispatched(task) => Ok(ExecOutcome::Dispatched(
                desk_diagnose_core::session::ActionIdentity::agent_exec(
                    task.id,
                    task.exec_request_id,
                    task.execution_generation,
                ),
            )),
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
        let mut validation_input = match operation {
            OperationInput::Exec(input) => input,
            _ => {
                return Err(safe(
                    AgentErrorKind::Internal,
                    "exec tool mapped to non-exec input",
                ));
            }
        };
        desk_diagnose_core::exec_tools::apply_exec_runtime_ceiling(
            &mut validation_input,
            self.max_command_runtime_ms,
        );
        let classified = classify_command_with_policy(
            &validation_input,
            &[],
            desk_agent_protocol::exec_policy::builtin_blocklist(),
            self.admission_policy,
        );
        let requested_shell = match &validation_input.target {
            desk_agent_protocol::ExecTarget::Shell { shell } => shell.as_str(),
            _ => "",
        };
        if classified.draft.is_none()
            && (canonical_exec_shell(requested_shell).is_none()
                || !exec_shell_is_available(requested_shell, &self.available_exec_shells))
        {
            return Err(unsupported_exec_shell_error(
                requested_shell,
                &self.available_exec_shells,
            ));
        }
        let Some(draft) = classified.draft else {
            return Ok(ExecOutcome::Rejected {
                reason: Some(classified.classification.impact),
            });
        };
        if draft.execution_basis == ExecExecutionBasis::OwnerBlocklistOnly
            && !exec_shell_is_available(requested_shell, &self.available_exec_shells)
        {
            return Err(unsupported_exec_shell_error(
                requested_shell,
                &self.available_exec_shells,
            ));
        }
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
        let approval_timeout_secs = crate::model_provider::load(&self.db)
            .await
            .map_err(|error| {
                log::error!("[agent-exec] failed to load approval timeout: {error}");
                internal("execution approval settings are unavailable")
            })?
            .exec_approval_timeout_secs;
        let approval_timeout = Duration::from_secs(u64::from(approval_timeout_secs));
        let exec_request_id = ExecRequestId(uuid::Uuid::new_v4().to_string());
        let preview = ExecPreview {
            exec_request_id: Some(exec_request_id.clone()),
            shell: match &validation_input.target {
                desk_agent_protocol::ExecTarget::Shell { shell } => shell.clone(),
                _ => String::new(),
            },
            command: validation_input.command.clone(),
            cwd: validation_input.cwd.clone(),
            approval_timeout_ms: approval_timeout.as_millis() as u64,
            timeout_ms: draft.timeout_ms,
            risk: draft.risk,
            io_mode: draft.io_mode,
            requires_live_carrier: draft.io_mode.is_pty(),
            execution_basis: draft.execution_basis,
            requires_confirmation: true,
            executable: true,
            blocked_reason: None,
        };
        let Some(approval_rx) = self.pending.register_approval(
            exec_request_id.0.clone(),
            browser_connection_id.to_string(),
            self.target_connection_id.clone(),
            self.diagnose_request_id.clone(),
            draft.io_mode.is_pty(),
        ) else {
            return Err(safe(
                AgentErrorKind::Internal,
                "an approval with this id is already pending",
            ));
        };
        if let Err(error) = self.push_preview(browser_connection_id, &preview).await {
            self.pending.cancel_approval(&exec_request_id.0);
            return Err(error);
        }
        let approval = match tokio::time::timeout(approval_timeout, approval_rx).await {
            Ok(Ok(data)) => data,
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
        if approval.decision != ApprovalDecision::Approve {
            return Ok(ExecOutcome::Rejected { reason: None });
        }
        let carrier_id = if draft.io_mode.is_pty() {
            match approval.carrier_id.filter(|value| !value.is_empty()) {
                Some(value) => Some(value),
                None => {
                    return Ok(ExecOutcome::Cancelled {
                        reason: Some("the interactive carrier was not ready at approval".into()),
                    });
                }
            }
        } else {
            if approval.carrier_id.is_some() {
                return Ok(ExecOutcome::Rejected {
                    reason: Some("a carrier was supplied for a non-interactive command".into()),
                });
            }
            None
        };

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
            .dispatch(
                actor_user_id,
                refreshed_scope,
                plan,
                validation_input,
                carrier_id,
                ctx,
            )
            .await?
        {
            SignalDispatch::Settled {
                task,
                disposition: EdgeExecDisposition::Executed { outcome },
            } => Ok(ExecOutcome::Executed {
                data_envelope: None,
                output: ToolRunOutput {
                    content: outcome_content(&outcome),
                    image_data_url: None,
                },
                event_id: Some(task.event_id),
            }),
            SignalDispatch::Settled {
                task,
                disposition:
                    EdgeExecDisposition::RejectedBeforeDispatch { error }
                    | EdgeExecDisposition::DispatchFailedBeforeWorker { error }
                    | EdgeExecDisposition::HostAtCapacity { error },
            } => {
                crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone())
                    .consume_event(&task.event_id)
                    .await?;
                Err(error)
            }
            SignalDispatch::Settled {
                task,
                disposition: EdgeExecDisposition::ExecutionStateUnknown { .. },
            }
            | SignalDispatch::Unknown(task) => Ok(ExecOutcome::Unknown(
                desk_diagnose_core::session::ActionIdentity::agent_exec(
                    task.id,
                    task.exec_request_id,
                    task.execution_generation,
                ),
            )),
            SignalDispatch::Dispatched(task) => Ok(ExecOutcome::Dispatched(
                desk_diagnose_core::session::ActionIdentity::agent_exec(
                    task.id,
                    task.exec_request_id,
                    task.execution_generation,
                ),
            )),
        }
    }

    async fn ack_delivery(&self, event_id: &str) -> Result<(), AgentError> {
        crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone())
            .consume_event(event_id)
            .await
    }

    async fn wait_for_task(
        &self,
        exec_request_id: &str,
        execution_id: &str,
    ) -> Result<WaitOutcome, AgentError> {
        let store = crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone());
        let deadline = tokio::time::Instant::now() + WAIT_FOR_TASK_TIMEOUT;
        loop {
            let Some(task) = store.find(exec_request_id, execution_id).await? else {
                return Err(safe(
                    AgentErrorKind::InvalidInput,
                    "that background task is no longer tracked",
                ));
            };
            match task.status.as_str() {
                crate::agent_exec_store::STATUS_DONE => {
                    return Ok(WaitOutcome::Completed {
                        output: ToolRunOutput {
                            content: task
                                .result_text
                                .unwrap_or_else(|| "execution completed".to_string()),
                            image_data_url: None,
                        },
                        event_id: Some(task.event_id),
                    });
                }
                crate::agent_exec_store::STATUS_UNKNOWN => return Ok(WaitOutcome::Unknown),
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let Some(reply) = self.query_state(execution_id).await? else {
                    return Ok(WaitOutcome::StillRunning);
                };
                if matches!(reply.state, ExecState::Reserved | ExecState::Running) {
                    return Ok(WaitOutcome::StillRunning);
                }
                let disposition = EdgeExecDisposition::from_reconciled_state(&reply);
                store
                    .finalize(&task.target_connection_id, execution_id, &disposition)
                    .await?;
                let settled = store
                    .find(exec_request_id, execution_id)
                    .await?
                    .ok_or_else(|| {
                        safe(
                            AgentErrorKind::InvalidInput,
                            "that background task is no longer tracked",
                        )
                    })?;
                if settled.status == crate::agent_exec_store::STATUS_UNKNOWN {
                    return Ok(WaitOutcome::Unknown);
                }
                return Ok(WaitOutcome::Completed {
                    output: ToolRunOutput {
                        content: settled
                            .result_text
                            .unwrap_or_else(|| "execution completed".to_string()),
                        image_data_url: None,
                    },
                    event_id: Some(settled.event_id),
                });
            }
            tokio::time::sleep(WAIT_FOR_TASK_POLL).await;
        }
    }
}

pub struct SignalExecStateReplyObserver {
    pending: Arc<SignalAgentExecPending>,
}

impl SignalExecStateReplyObserver {
    pub fn new(pending: Arc<SignalAgentExecPending>) -> Self {
        Self { pending }
    }
}

impl ExecStateReplyObserver for SignalExecStateReplyObserver {
    fn on_exec_state_reply<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let Ok(payload) = model.get_data::<ExecStateReplyPayload>() else {
                log::warn!("[agent-exec] malformed ExecStateReply was dropped");
                return;
            };
            if !self
                .pending
                .deliver_state_reply(&source.model.connection_id, payload)
            {
                log::debug!("[agent-exec] uncorrelated ExecStateReply was dropped");
            }
        })
    }
}

pub struct SignalEdgeExecObserver {
    pending: Arc<SignalAgentExecPending>,
    db: DatabaseConnection,
}

impl SignalEdgeExecObserver {
    pub fn new(pending: Arc<SignalAgentExecPending>, db: DatabaseConnection) -> Self {
        Self { pending, db }
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
            let store = crate::agent_exec_store::SignalAgentExecStore::new(self.db.clone());
            let correlated = match store
                .finalize(
                    &source.model.connection_id,
                    &payload.request_id,
                    &payload.disposition,
                )
                .await
            {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(error) => {
                    log::error!(
                        "[agent-exec] could not persist EdgeExecResult: {}",
                        error.message
                    );
                    false
                }
            };
            if correlated
                || self
                    .pending
                    .deliver_result(&source.model.connection_id, payload.clone())
            {
                if correlated {
                    let _ = self
                        .pending
                        .deliver_result(&source.model.connection_id, payload);
                }
            } else {
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
            carrier_id: None,
        }
    }

    #[tokio::test]
    async fn approval_is_bound_to_the_originating_browser_and_one_shot() {
        let pending = SignalAgentExecPending::new();
        let rx = pending
            .register_approval(
                "e1".into(),
                "browser-a".into(),
                "target-a".into(),
                "diagnose-a".into(),
                false,
            )
            .unwrap();
        assert!(!pending.resolve("browser-b", &resolve("e1", ApprovalDecision::Approve)));
        assert!(pending.resolve("browser-a", &resolve("e1", ApprovalDecision::Approve)));
        assert_eq!(rx.await.unwrap().decision, ApprovalDecision::Approve);
        assert!(!pending.resolve("browser-a", &resolve("e1", ApprovalDecision::Approve)));
    }

    #[tokio::test]
    async fn browser_disconnect_cancels_only_its_pending_approvals() {
        let pending = SignalAgentExecPending::new();
        let a = pending
            .register_approval(
                "a".into(),
                "browser-a".into(),
                "target-a".into(),
                "diagnose-a".into(),
                false,
            )
            .unwrap();
        let b = pending
            .register_approval(
                "b".into(),
                "browser-b".into(),
                "target-b".into(),
                "diagnose-b".into(),
                false,
            )
            .unwrap();

        assert_eq!(pending.cancel_approvals_for_browser("browser-a"), 1);
        assert!(a.await.is_err());
        assert!(pending.resolve("browser-b", &resolve("b", ApprovalDecision::Approve),));
        assert_eq!(b.await.unwrap().decision, ApprovalDecision::Approve);
    }

    #[tokio::test]
    async fn diagnose_cancel_is_request_scoped_on_the_same_browser() {
        let pending = SignalAgentExecPending::new();
        let old = pending
            .register_approval(
                "old".into(),
                "browser-a".into(),
                "target-a".into(),
                "diagnose-old".into(),
                false,
            )
            .unwrap();
        let current = pending
            .register_approval(
                "current".into(),
                "browser-a".into(),
                "target-a".into(),
                "diagnose-current".into(),
                false,
            )
            .unwrap();

        assert_eq!(
            pending.cancel_approvals_for_diagnosis("browser-a", "diagnose-old"),
            1
        );
        assert!(old.await.is_err());
        assert!(pending.resolve("browser-a", &resolve("current", ApprovalDecision::Approve)));
        assert_eq!(current.await.unwrap().decision, ApprovalDecision::Approve);
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
                error: EdgeExecDisposition::safe_error(
                    AgentErrorKind::PermissionDenied,
                    "test",
                    false,
                ),
            },
        };
        assert!(!pending.deliver_result("edge-b", payload.clone()));
        assert!(pending.deliver_result("edge-a", payload));
        assert!(matches!(
            rx.await.unwrap(),
            EdgeExecDisposition::RejectedBeforeDispatch { .. }
        ));
    }

    #[tokio::test]
    async fn state_reply_is_bound_to_the_queried_host_and_one_shot() {
        let pending = SignalAgentExecPending::new();
        let rx = pending
            .register_state_query("g1".into(), "edge-a".into())
            .unwrap();
        let reply = ExecStateReplyPayload {
            execution_generation: "g1".into(),
            state: ExecState::Running,
            containment_identity: None,
            running_ms: Some(9_000),
            detail: None,
            result_json: None,
        };
        assert!(!pending.deliver_state_reply("edge-b", reply.clone()));
        assert!(pending.deliver_state_reply("edge-a", reply));
        assert_eq!(rx.await.unwrap().state, ExecState::Running);
    }
}
