//! Direct-runtime wiring for the agentic tool-calling loop.
//!
//! Implements the three seams the shared [`desk_diagnose_core::agent_loop`] runs
//! over, for the Default / DeskServer (daemon-local) runtime:
//!
//! - [`AdapterModelSeam`]: maps a neutral [`ModelRequest`] to the configured
//!   provider's [`ChatRequest`] and calls the streaming adapter, returning the
//!   normalized [`ModelTurn`].
//! - [`DirectToolSeam`]: runs a read tool by mapping the model's call to a
//!   server-stamped read envelope, invoking the in-process [`LocalDeviceAgent`],
//!   redacting the result (fail-closed), and serializing it for the model.
//! - [`InMemorySessionSeam`]: keeps sessions in process memory with a
//!   per-conversation atomic claim (one daemon process owns its sessions).
//!
//! [`read_tool_registry`] is the read-only tool set exposed to the model; each
//! tool maps onto one [`ContextKind`].
//!
//! Only read tools are wired here; the mutating path lands in a later PR. There
//! is no token streaming yet (the loop's `TurnSink` is unused on this path);
//! streaming is added with the UI PR.

use std::collections::HashMap;
use std::sync::Arc;

use desk_agent_protocol::exec::{
    ApprovalId, CommandClassification, ExecDecision, ExecPlan, ExecPlanDraft, ExecRequestId,
};
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentOutcome,
    AgentScope, AuditMeta, CallerRef, CallerType, Capability, DeviceAgent, ExecutionMode,
    OperationInput, ProtocolVersion, RequestId, TargetRef,
};
use desk_diagnose_core::chat::{ModelTurn, StopReason, ToolCall};
use desk_diagnose_core::exec_tools::build_exec_input;
use desk_diagnose_core::read_tools::build_read_operation;
use desk_diagnose_core::registry::RegisteredTool;
use desk_diagnose_core::seam::{
    ClaimError, ClaimTurnParams, ExecContext, ExecIdentity, ExecOutcome, ModelRequest, ModelSeam,
    SessionSeam, ToolRunOutput, ToolSeam, TurnSink,
};
use desk_diagnose_core::session::PersistedAgentSession;

use super::model::{AdapterSelector, ChatRequest};
use super::redaction::{Redactor, redact_snapshot};
use crate::model::settings::SharedSettings;
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::eval::EvidenceSnapshot;

/// Current time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ============================ Mutating exec seams (Direct) ============================

/// What the Direct classifier produced for an exec request.
#[derive(Debug, Clone)]
pub struct DirectClassified {
    pub classification: CommandClassification,
    /// The sealed plan, present only for a `ConfirmRequired` decision.
    pub draft: Option<ExecPlanDraft>,
}

/// Classifies an exec operation locally (whitelist template matching + risk). The
/// production implementation uses the daemon's command templates; tests fake it.
#[async_trait::async_trait(?Send)]
pub trait DirectExecClassifier {
    async fn classify(
        &self,
        input: &OperationInput,
        reason: Option<&str>,
    ) -> Result<DirectClassified, AgentError>;
}

/// The preview shown to the operator for an approval decision.
#[derive(Debug, Clone)]
pub struct ExecApprovalRequest {
    pub exec_request_id: String,
    pub classification: CommandClassification,
    pub draft: ExecPlanDraft,
    pub reason: Option<String>,
}

/// The operator's decision on a previewed Direct execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectApproval {
    Approved,
    Rejected,
    /// No decision before the deadline (nothing ran).
    TimedOut,
    /// The operator cancelled while still awaiting approval (nothing ran).
    Cancelled,
}

/// Requests an operator approval for a previewed execution (a oneshot over the
/// control connection in production; faked in tests).
#[async_trait::async_trait(?Send)]
pub trait DirectExecApprover {
    async fn request_approval(
        &self,
        request: &ExecApprovalRequest,
    ) -> Result<DirectApproval, AgentError>;
}

/// The result of running an approved plan on the local worker.
#[derive(Debug, Clone)]
pub enum DirectRun {
    /// The plan ran and a result came back.
    Sent(AgentOutcome),
    /// The plan may have run but its outcome is unknown (cancel / connection drop).
    OutcomeUnknown,
}

/// Runs a sealed [`ExecPlan`] on the local worker and awaits its result (the worker
/// IPC dispatch in production; faked in tests).
#[async_trait::async_trait(?Send)]
pub trait DirectExecRunner {
    async fn run(&self, plan: ExecPlan) -> Result<DirectRun, AgentError>;
}

/// The Direct runtime's mutating-exec dependencies, injected to enable the path.
/// The trait objects are `Send + Sync` so a [`DirectToolSeam`] holding them stays
/// shareable across the router's tasks (the seam method futures are still `!Send`).
#[derive(Clone)]
pub struct DirectExecParts {
    pub classifier: Arc<dyn DirectExecClassifier + Send + Sync>,
    pub approver: Arc<dyn DirectExecApprover + Send + Sync>,
    pub runner: Arc<dyn DirectExecRunner + Send + Sync>,
}

/// Render an execution outcome into the text fed back to the model (already redacted
/// by the runner).
fn outcome_content(outcome: &AgentOutcome) -> String {
    match outcome {
        AgentOutcome::Ok(output) => {
            serde_json::to_string(output).unwrap_or_else(|_| "{}".to_string())
        }
        AgentOutcome::Err(e) if e.safe_for_model => format!("execution failed: {}", e.message),
        AgentOutcome::Err(_) => "execution failed".to_string(),
    }
}

fn internal(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
    }
}

/// The read-only tools plus the mutating exec tool, for the Direct agent loop.
pub fn agent_tool_registry() -> Vec<RegisteredTool> {
    let mut reg = desk_diagnose_core::read_tools::read_tool_registry();
    reg.extend(desk_diagnose_core::exec_tools::exec_tool_registry());
    reg
}

// ============================ Tool seam (read + exec) ============================

/// Runs read tools against the in-process device agent, redacting each result
/// before it returns to the loop (fail-closed). When exec parts are injected
/// ([`with_exec`](DirectToolSeam::with_exec)), it also runs the mutating path
/// (classify → operator approval → local worker execution).
pub struct DirectToolSeam {
    agent: Arc<LocalDeviceAgent>,
    redactor: Arc<dyn Redactor>,
    actor_id: String,
    exec: Option<DirectExecParts>,
}

impl DirectToolSeam {
    pub fn new(
        agent: Arc<LocalDeviceAgent>,
        redactor: Arc<dyn Redactor>,
        actor_id: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            redactor,
            actor_id: actor_id.into(),
            exec: None,
        }
    }

    /// Enable the mutating exec path with the given local seams.
    pub fn with_exec(mut self, exec: DirectExecParts) -> Self {
        self.exec = Some(exec);
        self
    }

    /// Run the mutating exec flow: classify, request approval, then run on the local
    /// worker, mapping the terminal state to an [`ExecOutcome`].
    async fn run_exec(&self, call: &ToolCall) -> Result<ExecOutcome, AgentError> {
        let Some(exec) = &self.exec else {
            return Ok(ExecOutcome::Rejected {
                reason: Some("execution is not enabled in this runtime".to_string()),
            });
        };
        let (input, reason) = build_exec_input(call)?;
        let classified = exec.classifier.classify(&input, reason.as_deref()).await?;
        let draft = match classified.classification.decision {
            ExecDecision::ConfirmRequired => classified.draft.clone().ok_or_else(|| {
                internal("classifier returned confirm_required without a sealed draft")
            })?,
            ExecDecision::Blocked => {
                return Ok(ExecOutcome::Rejected {
                    reason: Some(format!(
                        "command blocked by policy: {}",
                        classified.classification.impact
                    )),
                });
            }
            ExecDecision::NotExecutable => {
                return Ok(ExecOutcome::Rejected {
                    reason: Some(
                        "command is not executable through the AI path (no matching template)"
                            .to_string(),
                    ),
                });
            }
        };

        let exec_request_id = format!("exec_{}", uuid::Uuid::new_v4().simple());
        let approval = exec
            .approver
            .request_approval(&ExecApprovalRequest {
                exec_request_id: exec_request_id.clone(),
                classification: classified.classification.clone(),
                draft: draft.clone(),
                reason,
            })
            .await?;
        match approval {
            DirectApproval::Rejected => return Ok(ExecOutcome::Rejected { reason: None }),
            // An approval-phase timeout or cancel means nothing ran.
            DirectApproval::TimedOut | DirectApproval::Cancelled => {
                return Ok(ExecOutcome::ApprovalTimeout);
            }
            DirectApproval::Approved => {}
        }

        let approval_id = format!("appr_{}", uuid::Uuid::new_v4().simple());
        let plan = ExecPlan::from_draft(
            ExecRequestId(exec_request_id.clone()),
            ApprovalId(approval_id),
            draft,
        );
        match exec.runner.run(plan).await? {
            DirectRun::Sent(outcome) => Ok(ExecOutcome::Executed(ToolRunOutput {
                content: outcome_content(&outcome),
                image_data_url: None,
            })),
            // No durable work item on this in-process runtime, so the unknown
            // identity is synthetic; the loop still closes the conversation (§6).
            DirectRun::OutcomeUnknown => Ok(ExecOutcome::Unknown(ExecIdentity {
                work_id: 0,
                execution_id: format!("exec_{}", uuid::Uuid::new_v4().simple()),
                exec_request_id,
            })),
        }
    }

    /// A read-only, server-stamped envelope granting exactly `cap` (mirrors the
    /// collector's trusted-field stamp; no control end supplies any of these).
    fn build_envelope(&self, cap: Capability, input: OperationInput) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId(uuid::Uuid::new_v4().to_string()),
            parent_task_id: None,
            target: TargetRef::default(),
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: self.actor_id.clone(),
                tenant_id: None,
            },
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
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input,
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("ai agent loop read".into()),
            },
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ToolSeam for DirectToolSeam {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        let (cap, input) = build_read_operation(call)?;
        let envelope = self.build_envelope(cap, input);
        let output = self.agent.invoke(envelope).await?;

        // Redact via a one-entry snapshot so the exact send-time redaction +
        // screenshot refit run; a redaction failure is fail-closed (the loop
        // turns the Err into an error tool-result, never leaking raw output).
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
            .first()
            .expect("the one entry we recorded is present");
        let content = serde_json::to_string(&entry.outcome).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolRunOutput {
            content,
            image_data_url: entry.image_data_url.clone(),
        })
    }

    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        _ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        self.run_exec(call).await
    }
}

// ============================ Model seam ============================

/// Calls the configured provider via the streaming adapter, mapping the neutral
/// [`ModelRequest`] to a [`ChatRequest`]. Direct provider config only (the
/// manager-proxy gateway is handled on the manager runtime).
pub struct AdapterModelSeam {
    selector: Arc<dyn AdapterSelector>,
    settings: Arc<SharedSettings>,
}

impl AdapterModelSeam {
    pub fn new(selector: Arc<dyn AdapterSelector>, settings: Arc<SharedSettings>) -> Self {
        Self { selector, settings }
    }
}

/// The graceful turn returned when no model gateway is configured.
fn not_configured_turn() -> ModelTurn {
    ModelTurn {
        text: "AI model is not configured; set the provider, model, base URL, and \
               API key in AI model settings."
            .to_string(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for AdapterModelSeam {
    async fn call(
        &self,
        request: ModelRequest,
        _sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        let config = { self.settings.read().await.ai_model.clone() };
        let (Some(model), Some(base_url), Some(api_key)) = (
            config.model.clone(),
            config.base_url.clone(),
            config.api_key.clone(),
        ) else {
            return Ok(not_configured_turn());
        };
        if model.is_empty() || base_url.is_empty() || api_key.is_empty() {
            return Ok(not_configured_turn());
        }
        let adapter = self.selector.select(config.provider.as_deref());
        let chat = ChatRequest {
            base_url,
            api_key,
            model,
            messages: request.messages,
            response_format: request.response_format,
            tools: request.tools,
            tool_choice: request.tool_choice,
        };
        // No token streaming on this path yet (added with the UI PR); the adapter
        // still streams internally, so feed it a no-op delta sink.
        let noop = |_: String| {};
        adapter.stream_chat(chat, &noop).await
    }
}

// ============================ Session seam (in-memory) ============================

/// Keeps agent sessions in process memory, keyed by conversation id. One daemon
/// process owns its sessions, so a single async mutex makes the claim atomic.
#[derive(Default)]
pub struct InMemorySessionSeam {
    sessions: tokio::sync::Mutex<HashMap<String, PersistedAgentSession>>,
}

impl InMemorySessionSeam {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait(?Send)]
impl SessionSeam for InMemorySessionSeam {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut map = self.sessions.lock().await;
        let mut session = match map.get(&params.conversation_id) {
            Some(existing) => {
                existing
                    .check_subject(
                        params.tenant_id.as_deref(),
                        &params.actor_id,
                        &params.device_id,
                    )
                    .map_err(ClaimError::Subject)?;
                existing.clone()
            }
            None => PersistedAgentSession::new(
                params.conversation_id.clone(),
                params.tenant_id.clone(),
                params.actor_id.clone(),
                params.device_id.clone(),
                params.policy_revision,
                params.current_pdp_scope.clone(),
                params.now.clone(),
            ),
        };
        session
            .begin_turn(
                params.turn_id,
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        map.insert(session.conversation_id.clone(), session.clone());
        Ok(session)
    }

    async fn save(&self, session: &PersistedAgentSession) -> Result<(), AgentError> {
        self.sessions
            .lock()
            .await
            .insert(session.conversation_id.clone(), session.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::agent_loop::{LoopDeps, LoopOutcome, run_agent_turn};
    use desk_diagnose_core::chat::{ChatMessage, ChatRole, ModelTurn, StopReason, ToolCall};
    use desk_diagnose_core::prompt::ResponseFormatSpec;
    use desk_diagnose_core::read_tools::read_tool_registry;
    use desk_diagnose_core::seam::TurnSink;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::super::redaction::RegexRedactor;

    /// The direct tool seam runs `read_system_info` against the real in-process
    /// agent and returns a JSON result (succeeds on every CI host).
    #[tokio::test]
    async fn direct_tool_seam_reads_system_info() {
        let seam = DirectToolSeam::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            "agent-loop",
        );
        let out = seam
            .run_read(&ToolCall {
                id: "c1".into(),
                name: "read_system_info".into(),
                arguments_json: String::new(),
            })
            .await
            .expect("read ok");
        assert!(out.content.contains("SystemInfo") || out.content.contains("hostname"));
        assert!(out.image_data_url.is_none());
    }

    struct Collector(RefCell<String>);
    impl TurnSink for Collector {
        fn on_text_delta(&mut self, delta: &str) {
            self.0.borrow_mut().push_str(delta);
        }
    }

    /// A scripted model seam to drive the loop without a network.
    struct ScriptModel(RefCell<VecDeque<ModelTurn>>);
    #[async_trait::async_trait(?Send)]
    impl ModelSeam for ScriptModel {
        async fn call(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            Ok(self.0.borrow_mut().pop_front().expect("a scripted turn"))
        }
    }

    /// End to end on the Direct seams: the model asks for `read_system_info`, the
    /// real tool runs, and the second turn answers — using the in-memory session.
    #[tokio::test]
    async fn loop_runs_read_tool_via_direct_seams() {
        let model = ScriptModel(RefCell::new(
            [
                ModelTurn {
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read_system_info".into(),
                        // Providers always emit a JSON object for arguments.
                        arguments_json: "{}".into(),
                    }],
                    ..Default::default()
                },
                ModelTurn {
                    text: "the host looks healthy".into(),
                    stop_reason: StopReason::EndTurn,
                    ..Default::default()
                },
            ]
            .into(),
        ));
        let tools = DirectToolSeam::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            "agent-loop",
        );
        let session = InMemorySessionSeam::new();
        let registry = read_tool_registry();
        let clock = || now_rfc3339();
        let deps = LoopDeps {
            session_seam: &session,
            model: &model,
            tools: &tools,
            registry: &registry,
            response_format: ResponseFormatSpec::None,
            system_prompt: desk_diagnose_core::agentic_prompt::build_agentic_system_message(None),
            max_context_bytes: desk_diagnose_core::DEFAULT_MAX_CONTEXT_BYTES,
            clock: &clock,
        };
        let claim = ClaimTurnParams {
            conversation_id: "conv-1".into(),
            tenant_id: None,
            actor_id: "actor".into(),
            device_id: "device".into(),
            policy_revision: 1,
            current_pdp_scope: AgentScope {
                granted: vec![Capability::SystemInfo],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            turn_id: "turn-1".into(),
            request_id: Some("req".into()),
            connection_id: Some("conn".into()),
            now: now_rfc3339(),
        };
        let mut sink = Collector(RefCell::new(String::new()));
        let user = ChatMessage::text("u", ChatRole::User, "is the host healthy?");
        let outcome = run_agent_turn(&deps, claim, user, &mut sink).await.unwrap();
        assert_eq!(
            outcome,
            LoopOutcome::Answered("the host looks healthy".into())
        );
    }

    // ---------------------------- Mutating exec path ----------------------------

    use desk_agent_protocol::exec::{ExecEffect, ExecShellKind};
    use desk_agent_protocol::{ExecOutput, OperationOutput, RiskLevel};
    use desk_diagnose_core::seam::{ExecContext, ExecOutcome};

    struct FakeClassifier(DirectClassified);
    #[async_trait::async_trait(?Send)]
    impl DirectExecClassifier for FakeClassifier {
        async fn classify(
            &self,
            _input: &OperationInput,
            _reason: Option<&str>,
        ) -> Result<DirectClassified, AgentError> {
            Ok(self.0.clone())
        }
    }

    struct FakeApprover(DirectApproval);
    #[async_trait::async_trait(?Send)]
    impl DirectExecApprover for FakeApprover {
        async fn request_approval(
            &self,
            _request: &ExecApprovalRequest,
        ) -> Result<DirectApproval, AgentError> {
            Ok(self.0)
        }
    }

    enum RunKind {
        Ok,
        Unknown,
    }
    struct FakeRunner(RunKind);
    #[async_trait::async_trait(?Send)]
    impl DirectExecRunner for FakeRunner {
        async fn run(&self, _plan: ExecPlan) -> Result<DirectRun, AgentError> {
            Ok(match self.0 {
                RunKind::Ok => {
                    DirectRun::Sent(AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
                        exit_code: 0,
                        stdout: "Running".into(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                        duration_ms: 4,
                        redactions: vec![],
                    })))
                }
                RunKind::Unknown => DirectRun::OutcomeUnknown,
            })
        }
    }

    fn confirm_mutating() -> DirectClassified {
        DirectClassified {
            classification: CommandClassification {
                risk: RiskLevel::High,
                matched_template: Some("restart".into()),
                impact: "restarts a service".into(),
                decision: ExecDecision::ConfirmRequired,
                effect: Some(ExecEffect::Mutating),
            },
            draft: Some(ExecPlanDraft {
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
            }),
        }
    }

    fn blocked() -> DirectClassified {
        DirectClassified {
            classification: CommandClassification {
                risk: RiskLevel::Critical,
                matched_template: None,
                impact: "deletes everything".into(),
                decision: ExecDecision::Blocked,
                effect: None,
            },
            draft: None,
        }
    }

    fn seam_with_exec(
        classified: DirectClassified,
        approval: DirectApproval,
        run: RunKind,
    ) -> DirectToolSeam {
        DirectToolSeam::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            "agent-loop",
        )
        .with_exec(DirectExecParts {
            classifier: Arc::new(FakeClassifier(classified)),
            approver: Arc::new(FakeApprover(approval)),
            runner: Arc::new(FakeRunner(run)),
        })
    }

    fn exec_call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "exec_command".into(),
            arguments_json: r#"{"command":"Restart-Service X"}"#.into(),
        }
    }

    fn exec_ctx() -> ExecContext {
        ExecContext {
            conversation_id: "conv-1".into(),
            turn_id: "turn-1".into(),
            tool_call_id: "call-1".into(),
            actor_id: "actor-1".into(),
            policy_revision: 1,
            scope: AgentScope {
                granted: vec![Capability::ShellExecConfirmed],
                mode: ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: None,
            },
        }
    }

    /// The combined registry exposes the read tools plus the exec tool.
    #[test]
    fn agent_registry_includes_exec_tool() {
        let names: Vec<_> = agent_tool_registry()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"exec_command".to_string()));
        assert!(names.contains(&"read_system_info".to_string()));
    }

    /// A read-only seam (no exec parts) rejects a mutating call.
    #[tokio::test]
    async fn exec_disabled_rejects() {
        let seam = DirectToolSeam::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            "agent-loop",
        );
        let out = seam
            .confirm_and_exec(&exec_call(), &exec_ctx())
            .await
            .unwrap();
        assert!(matches!(out, ExecOutcome::Rejected { .. }));
    }

    /// Approve → run → Executed with the result text.
    #[tokio::test]
    async fn exec_approved_runs() {
        let seam = seam_with_exec(confirm_mutating(), DirectApproval::Approved, RunKind::Ok);
        match seam
            .confirm_and_exec(&exec_call(), &exec_ctx())
            .await
            .unwrap()
        {
            ExecOutcome::Executed(out) => assert!(out.content.contains("exit_code")),
            other => panic!("expected Executed, got {other:?}"),
        }
    }

    /// A blocked command is rejected before any approval.
    #[tokio::test]
    async fn exec_blocked_rejected() {
        let seam = seam_with_exec(blocked(), DirectApproval::Approved, RunKind::Ok);
        let out = seam
            .confirm_and_exec(&exec_call(), &exec_ctx())
            .await
            .unwrap();
        assert!(matches!(out, ExecOutcome::Rejected { .. }));
    }

    /// A rejected approval yields Rejected; a timeout yields ApprovalTimeout.
    #[tokio::test]
    async fn exec_rejected_and_timeout() {
        let rejected = seam_with_exec(confirm_mutating(), DirectApproval::Rejected, RunKind::Ok);
        assert!(matches!(
            rejected
                .confirm_and_exec(&exec_call(), &exec_ctx())
                .await
                .unwrap(),
            ExecOutcome::Rejected { .. }
        ));
        let timed = seam_with_exec(confirm_mutating(), DirectApproval::TimedOut, RunKind::Ok);
        assert!(matches!(
            timed
                .confirm_and_exec(&exec_call(), &exec_ctx())
                .await
                .unwrap(),
            ExecOutcome::ApprovalTimeout
        ));
    }

    /// An unknown run outcome reports Unknown (the loop closes the conversation).
    #[tokio::test]
    async fn exec_unknown_outcome() {
        let seam = seam_with_exec(
            confirm_mutating(),
            DirectApproval::Approved,
            RunKind::Unknown,
        );
        match seam
            .confirm_and_exec(&exec_call(), &exec_ctx())
            .await
            .unwrap()
        {
            ExecOutcome::Unknown(id) => {
                assert!(id.exec_request_id.starts_with("exec_"));
                assert_eq!(id.work_id, 0);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
