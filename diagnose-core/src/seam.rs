//! The model seam: the agentic loop's abstraction over a model call.
//!
//! The loop (built on this crate) never talks a provider's wire dialect. It
//! hands a [`ModelRequest`] (conversation + advertised tools + steering) to a
//! [`ModelSeam`] and gets back a normalized [`ModelTurn`]. Each runtime supplies
//! its own implementation: the Direct runtime wraps the OpenAI/Anthropic
//! streaming adapters; the Manager runtime wraps its model dialect. Both map onto
//! the same neutral types here, so the two sides can never drift.
//!
//! `?Send`: the Direct adapters use `awc` (`!Send`) on actix's single-threaded
//! runtime, and the manager awaits the model call inline, so a non-`Send` future
//! works for both. The bound is documented here so future implementers don't
//! accidentally require `Send`.

use async_trait::async_trait;
use desk_agent_protocol::content_safety::StreamRetractionReason;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope};

use crate::chat::{ChatMessage, ModelTurn, TokenUsage, ToolCall, ToolChoice, ToolSpec};
use crate::model_context::PinnedContextPolicy;
use crate::model_profile::ModelUseCase;
use crate::prompt::ResponseFormatSpec;
use crate::session::{PersistedAgentSession, SubjectMismatch};

/// A model request in neutral terms: the conversation, the tools the model may
/// call, how it is steered toward them, and the requested response format. The
/// wire shape (OpenAI vs Anthropic) is the [`ModelSeam`] implementation's concern.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    /// Requirements derived from the server-authoritative metadata of the tools
    /// advertised in this request. Keeping this explicit prevents provider seams
    /// from guessing security properties from model-facing tool names.
    pub tool_requirements: crate::model_capability::ModelRequirements,
    pub tool_choice: ToolChoice,
    pub response_format: ResponseFormatSpec,
    /// Purpose used to resolve the configured probe/runtime output budget.
    pub use_case: ModelUseCase,
    /// Optional business hard cap. This may only narrow the configured runtime
    /// limit; probe requests must leave it unset.
    pub caller_output_hard_cap: Option<i64>,
}

impl ModelRequest {
    /// A tool-free request (the single-turn diagnose shape): no tools advertised,
    /// the model is free to answer in text.
    pub fn text_only(messages: Vec<ChatMessage>, response_format: ResponseFormatSpec) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            tool_requirements: crate::model_capability::ModelRequirements::TEXT_ONLY,
            tool_choice: ToolChoice::Auto,
            response_format,
            use_case: ModelUseCase::Agent,
            caller_output_hard_cap: None,
        }
    }

    /// Requirements derived from the authoritative advertised-tool metadata and
    /// the actual message payload immediately before a provider call. Images are
    /// checked again on every later step so a pinned model whose capability was
    /// revoked cannot receive one.
    pub fn requirements(&self) -> crate::model_capability::ModelRequirements {
        self.tool_requirements
            .union(crate::model_capability::ModelRequirements::for_messages(
                &self.messages,
            ))
    }
}

/// Receives streaming output and lifecycle events from an agent turn as they
/// happen, so a runtime can forward them to the UI (the manager maps these onto
/// `DiagnoseEvent` tool/turn frames; the Direct runtime onto its own stream).
///
/// Text deltas are **provisional**. In enforced mode an allowed intermediate
/// tool-calling turn commits its prose with [`on_partial_committed`] before any
/// tool lifecycle event; a rejected, unavailable, truncated, or otherwise failed
/// turn uses [`on_turn_retracted`] so the UI can atomically clear uncommitted text.
/// A final answer commits through [`on_answer_committed`]. Disabled runtimes keep
/// the legacy callbacks and never emit safety-specific frames. All hooks but
/// [`on_text_delta`] default to no-ops.
/// Object-safe so a `&mut dyn TurnSink` can be passed across the seam.
///
/// [`StopReason`]: crate::chat::StopReason
/// [`on_answer_committed`]: TurnSink::on_answer_committed
/// [`on_turn_discarded`]: TurnSink::on_turn_discarded
/// [`on_text_delta`]: TurnSink::on_text_delta
pub trait TurnSink {
    /// An incremental fragment of the assistant's text for the current turn
    /// (provisional until the turn commits).
    fn on_text_delta(&mut self, delta: &str);

    /// Commit provisional prose from an allowed intermediate tool-calling turn.
    /// Enforced loops invoke this only after the assistant tool-call message has
    /// been persisted and before any tool or approval lifecycle event.
    fn on_partial_committed(&mut self) {}

    /// Retract provisional prose without repeating it. `error` is a fixed typed
    /// error for policy/unavailable cases and is absent for incomplete turns whose
    /// ordinary terminal error is selected by the runtime.
    fn on_turn_retracted(&mut self, reason: StreamRetractionReason, error: Option<AgentError>) {
        let _ = (reason, error);
    }

    /// A read tool call was dispatched (about to run).
    fn on_tool_started(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        let _ = (tool_name, call_id, arguments_json);
    }

    /// A mutating tool call is waiting for the operator's approval decision.
    fn on_awaiting_approval(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        let _ = (tool_name, call_id, arguments_json);
    }

    /// A dispatched tool call produced its result; `ok` is whether it yielded a
    /// usable result (an executed/read success) rather than an error / rejection /
    /// unknown outcome.
    fn on_tool_finished(
        &mut self,
        call_id: &str,
        ok: bool,
        output: &str,
        background_task_id: Option<&str>,
    ) {
        let _ = (call_id, ok, output, background_task_id);
    }

    /// The planning turn durably created a permission request and paused before
    /// any requested tool dispatch. The UI fetches full details from session
    /// state and performs a separate trusted decision action.
    fn on_permission_requested(&mut self, request_id: &str, item_count: usize) {
        let _ = (request_id, item_count);
    }

    /// The turn committed a final natural-language answer.
    fn on_answer_committed(&mut self, text: &str) {
        let _ = text;
    }

    /// The persisted floor for this turn advanced before the provider dial.
    fn on_context_trimmed(&mut self, turn_id: &str) {
        let _ = turn_id;
    }

    /// A validated checkpoint and its new raw-history floor were committed.
    fn on_context_compacted(&mut self, turn_id: &str, generation: u32, covered_message_count: u32) {
        let _ = (turn_id, generation, covered_message_count);
    }

    /// The turn was truncated (`MaxTokens` / `Other`) and discarded; any
    /// provisional text streamed for it must be dropped by the UI.
    fn on_turn_discarded(&mut self) {}
}

/// A sink that ignores all streamed output (for non-streaming callers / tests).
pub struct NullTurnSink;

impl TurnSink for NullTurnSink {
    fn on_text_delta(&mut self, _delta: &str) {}
}

/// Closed, content-free failure categories for checkpoint-compression errors,
/// metrics, and audit records. These tokens are part of the operational
/// contract: never replace one with provider text or a serialized summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCompressionFailureKind {
    InputTooLarge,
    ProviderRejected,
    ProviderTimeout,
    Truncated,
    InvalidSchema,
    UnsafeOutput,
    SummaryTooLarge,
    ProtectedStateTooLarge,
    ProtectedReplayUnsafe,
    StaleContext,
    UnsupportedEndpoint,
    InvalidEffectiveBudget,
    AttemptExhausted,
}

impl ContextCompressionFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InputTooLarge => "input_too_large",
            Self::ProviderRejected => "provider_rejected",
            Self::ProviderTimeout => "provider_timeout",
            Self::Truncated => "truncated",
            Self::InvalidSchema => "invalid_schema",
            Self::UnsafeOutput => "unsafe_output",
            Self::SummaryTooLarge => "summary_too_large",
            Self::ProtectedStateTooLarge => "protected_state_too_large",
            Self::ProtectedReplayUnsafe => "protected_replay_unsafe",
            Self::StaleContext => "stale_context",
            Self::UnsupportedEndpoint => "unsupported_endpoint",
            Self::InvalidEffectiveBudget => "invalid_effective_budget",
            Self::AttemptExhausted => "attempt_exhausted",
        }
    }
}

/// Content-free, stable context attached to a compression audit outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompressionAuditContext {
    pub generation: u32,
    pub covered_message_count: u32,
    pub covered_from_message_id: String,
    pub covered_through_message_id: String,
    pub input_context_cost: u64,
    pub platform_context_policy_revision: u64,
    pub safety: Option<ContextCompressionSafetyAuditContext>,
}

/// Credential-free identity of the independently frozen content-safety
/// receiver. This is absent when content safety is disabled for the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCompressionSafetyAuditContext {
    pub provider_identity_sha256: String,
    pub model_identity_sha256: String,
    pub connection_revision: i64,
    pub model_profile_revision: i64,
    pub policy_revision: u64,
    pub prompt_version: String,
}

/// Provider-reported usage for the compression call. `reasoning_tokens` is a
/// diagnostic subset of output tokens on providers that expose it, so it is
/// recorded separately but never added a second time to session/billing totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextCompressionProviderUsage {
    pub tokens: TokenUsage,
    pub reasoning_tokens: Option<u64>,
}

/// Terminal checkpoint-compression audit outcome. The model seam owns provider
/// identity/call-key attribution; the loop supplies only bounded metadata and
/// token counters. Summary or prompt text is intentionally unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCompressionAuditOutcome {
    Committed {
        context: ContextCompressionAuditContext,
        usage: ContextCompressionProviderUsage,
        summary_context_cost: u64,
        final_context_cost: u64,
    },
    Failed {
        context: Option<ContextCompressionAuditContext>,
        usage: Option<ContextCompressionProviderUsage>,
        kind: ContextCompressionFailureKind,
    },
}

/// The model call, abstracted from the wire dialect. Implementations stream text
/// deltas through `sink` and return the fully assembled, normalized [`ModelTurn`]
/// (text + tool calls + stop reason + usage).
#[async_trait(?Send)]
pub trait ModelSeam {
    /// Exact data-egress policy for Device Assistant context transformations.
    /// Ordinary diagnostic/copilot seams leave this unset. A strict seam must
    /// also enforce the same policy inside `call`, before its transport starts.
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(None)
    }

    /// Resolve and pin the model/source/profile needed to build a safe history
    /// view before the provider body is rendered.
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<PinnedContextPolicy, AgentError> {
        let _ = requirements;
        Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "model seam does not expose a pinned context policy".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }

    /// Return the frozen, non-secret provenance of the compression provider call
    /// that just completed. Only seams that can return a checkpoint-summary
    /// policy implement this; the default fails closed.
    fn context_compression_provenance(
        &self,
        turn_id: &str,
        created_at: &str,
    ) -> Result<crate::model_context::CompressorProvenanceV1, AgentError> {
        let _ = (turn_id, created_at);
        Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "model seam does not expose context compression provenance".into(),
            retryable: false,
            safe_for_model: true,
            error_code: Some(
                desk_utils::error::DeskErrorCode::AI_CONTEXT_COMPRESSION_FAILED.code(),
            ),
        })
    }

    /// Low-cardinality observability callbacks. Implementations must never attach
    /// prompt, summary, message, actor, request, or credential content to metrics.
    fn on_context_compression_started(
        &self,
        _generation: u32,
        _covered_message_count: u32,
        _input_context_cost: u64,
    ) {
    }

    fn on_context_compression_succeeded(
        &self,
        _generation: u32,
        _summary_context_cost: u64,
        _final_context_cost: u64,
    ) {
    }

    fn on_context_compression_failed(&self, _kind: ContextCompressionFailureKind) {}

    /// Persist a content-free terminal audit outcome. Implementations must be
    /// fail-open: audit storage failure cannot alter a committed checkpoint or
    /// replace the stable compression error selected by the loop.
    async fn audit_context_compression(&self, _outcome: ContextCompressionAuditOutcome) {}

    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError>;
}

/// The result of running a tool: the (already-redacted) text fed back to the
/// model, plus an optional vision image (e.g. a screenshot read tool).
///
/// Redaction is the seam implementation's responsibility — it happens before the
/// result crosses back into the loop (fail-closed; for a remote edge it happens
/// on the edge). The loop never sees un-redacted tool output.
#[derive(Debug, Clone, Default)]
pub struct ToolRunOutput {
    pub content: String,
    pub image_data_url: Option<String>,
}

/// The immutable identity of a dispatched mutating execution, used to fence a late
/// result and to record the unknown-outcome execution state. The manager fills it
/// from the durable work item; a runtime without durable work (Direct) uses a
/// process-local `work_id` sentinel and synthetic ids.
pub type ExecIdentity = crate::session::ActionIdentity;

/// The terminal outcome of a mutating tool call's approval + execution. The seam
/// owns the whole approval → dispatch → result wait (Direct: a local oneshot
/// confirm + in-process exec; Manager: a durable work item + central approval +
/// cross-instance dispatch). The loop turns this into the conversation and the
/// execution-reconciliation state.
#[derive(Debug, Clone)]
pub enum ExecOutcome {
    /// Approved and executed to a known, already-redacted result.
    ///
    /// `event_id` is the stable delivery id of this result when the runtime records
    /// completions durably (the manager's `work:{work_id}:done`). The loop uses it
    /// as the tool-result's `message_id`, so a later completion delivery of the same
    /// result — a foreground save that crashed before acking its consume — is
    /// recognized as already present and never appended twice. A runtime without
    /// durable completion (Direct) passes `None` and the loop mints an id.
    Executed {
        output: ToolRunOutput,
        event_id: Option<String>,
        /// Original data label already checked against the runtime's durable
        /// result receipt. When present, preserve it instead of relabeling at
        /// delivery time; it grants no implicit export permission.
        data_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
    },
    /// The operator rejected the command; nothing ran.
    Rejected { reason: Option<String> },
    /// The runtime durably refused dispatch before any execution generation was
    /// recorded. This is not an operator rejection or a successful cancellation.
    NotExecuted { reason: String },
    /// The command was cancelled before it dispatched; it provably never ran. Unlike
    /// [`Rejected`](Self::Rejected) (a decision at the approval gate) this is a cancel
    /// arriving while the command was still pending, and unlike
    /// [`Unknown`](Self::Unknown) the outcome is certain: it did not run.
    Cancelled { reason: Option<String> },
    /// Approval expired before any decision; nothing ran.
    ApprovalTimeout,
    /// The command may have run but its outcome is unknown (cancel / timeout /
    /// crash after dispatch). The loop closes the conversation with a placeholder
    /// tool result and records [`ExecutionState::OutcomeUnknown`]; a late result
    /// reconciles it in place.
    ///
    /// [`ExecutionState::OutcomeUnknown`]: crate::session::ExecutionState::OutcomeUnknown
    Unknown(ExecIdentity),
    /// Approved and dispatched, but still running when the foreground wait elapsed:
    /// the command became a background task. The loop closes the tool call
    /// immediately with a task-id result (a well-formed message it never rewrites)
    /// and records [`ExecutionState::Executing`]; the real result arrives later as a
    /// completion notification appended to the conversation. Unlike
    /// [`Unknown`](Self::Unknown) the outcome is not in doubt — a result is coming —
    /// so the conversation is not degraded, only barred from starting a second
    /// mutation until this one completes.
    ///
    /// [`ExecutionState::Executing`]: crate::session::ExecutionState::Executing
    Dispatched(ExecIdentity),
}

/// An execution outcome and the runtime's own committed session-version change.
/// Keep the receipt even when a step after Prepare fails. It is not a refresh
/// from storage, a grant, or evidence that the action executed successfully.
#[derive(Debug)]
pub struct ExecCompletion {
    pub outcome: Result<ExecOutcome, AgentError>,
    pub version_advance: Option<crate::action_version::ActionVersionAdvance>,
}

/// The result of a model actively waiting on a background task via
/// [`wait_for_task`](ToolSeam::wait_for_task). Unlike the passive completion
/// notification the publisher injects, a waited-for result becomes the real
/// tool result of the wait call — the model asked and this is its answer.
#[derive(Debug, Clone)]
pub enum WaitOutcome {
    /// The task reached a terminal result. `event_id` is the stable delivery id of
    /// the completion (the manager's `work:{work_id}:done`), so the loop keys the
    /// wait tool result on it and the background publisher's delivery of the same
    /// result dedups instead of appending a second copy. A runtime without durable
    /// completion passes `None`.
    Completed {
        output: ToolRunOutput,
        event_id: Option<String>,
    },
    /// The wait elapsed while the task was still running: it remains a background
    /// task and the model may wait again or do read-only work meanwhile.
    StillRunning,
    /// The task's outcome became unknown (recovered without a result). The loop
    /// records [`ExecutionState::OutcomeUnknown`] so a late result can still
    /// reconcile it; the conversation stays read-only until then.
    ///
    /// [`ExecutionState::OutcomeUnknown`]: crate::session::ExecutionState::OutcomeUnknown
    Unknown,
}

/// Per-call context the loop hands the mutating seam: the turn's identity, the
/// session subject the durable work item is pinned to, and the authorization it is
/// minted under. The loop owns all of these (they come from the persisted session),
/// so a control end can never influence the work item's subject.
#[derive(Debug, Clone)]
pub struct ExecContext {
    /// Frozen by the loop for Device Assistant, never inferred by an executor
    /// from newer input. Other surfaces retain their existing authorization path.
    pub assistant_turn_fence: Option<crate::action_turn_fence::AssistantTurnFence>,
    pub conversation_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    /// The session's pinned subject actor; the work item is created under it and a
    /// later resolve re-verifies the cookie subject against it.
    pub actor_id: String,
    pub policy_revision: i64,
    pub scope: AgentScope,
    /// The control-end connection that started the turn, if any. The mutating seam
    /// routes the approval preview back to it (Direct: the local browser link;
    /// Manager: the originating browser instance). `None` on a runtime with no live
    /// control connection.
    pub connection_id: Option<String>,
}

/// Runs the loop's tools. Read tools run immediately; a mutating tool goes through
/// approval + real execution via [`confirm_and_exec`](ToolSeam::confirm_and_exec).
#[async_trait(?Send)]
pub trait ToolSeam {
    /// Run a read-only tool call and return its redacted result. The loop has
    /// already validated that the call names an exposed read tool.
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError>;

    /// Produce source metadata for one successful read result before it is
    /// persisted or considered for model export. Default `None` keeps existing
    /// non-Assistant surfaces unchanged; an information-flow-enforced surface
    /// must return a validated envelope with no implicit external destination.
    fn read_data_envelope(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<desk_agent_protocol::data_lineage::DataEnvelope>, AgentError> {
        let _ = (call, output);
        Ok(None)
    }

    /// Approve and execute a mutating tool call, returning its terminal
    /// [`ExecOutcome`]. The loop has already validated the call names an exposed
    /// mutating tool and that no prior execution outcome is still unknown. A
    /// model-safe `Err` is turned into an error tool-result by the loop; a backend
    /// transport `Err` fails the turn. The default rejects, so a read-only runtime
    /// need not implement it.
    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        let _ = (call, ctx);
        Ok(ExecOutcome::Rejected {
            reason: Some("mutating execution is not supported by this runtime".into()),
        })
    }

    /// Execute using the loop's post-save version when the runtime needs to
    /// transact on that session. A committing runtime returns only its own
    /// version advance, including on subsequent failure; all writes must still
    /// compare the original turn/input/lease and held version. The default keeps
    /// nontransactional runtimes on their existing execution path.
    async fn confirm_and_exec_versioned(
        &self,
        call: &ToolCall,
        ctx: &ExecContext,
        version: Option<&crate::action_version::ActionVersion>,
    ) -> ExecCompletion {
        let _ = version;
        ExecCompletion {
            outcome: self.confirm_and_exec(call, ctx).await,
            version_advance: None,
        }
    }

    /// Label a successful mutating result before it is persisted or projected
    /// into the next model request. Mutation authorization governs the effect;
    /// it does not implicitly authorize exporting the result bytes. Enforced
    /// runtimes therefore return a validated envelope with no implicit sink.
    fn mutating_data_envelope(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<desk_agent_protocol::data_lineage::DataEnvelope>, AgentError> {
        let _ = (call, output);
        Ok(None)
    }

    /// Acknowledge that the foreground path has durably saved the result of a
    /// completion delivery, so this runtime may mark that delivery consumed and stop
    /// the background publisher from also delivering it. Called after the session
    /// save succeeds, with the delivery id the result was keyed on. Best-effort: the
    /// loop ignores the outcome, because a lost ack only means the publisher
    /// redelivers, which dedups on the same id. The default is a no-op for runtimes
    /// with no durable completion delivery (Direct / read-only).
    async fn ack_delivery(&self, event_id: &str) -> Result<(), AgentError> {
        let _ = event_id;
        Ok(())
    }

    /// Actively wait for a dispatched background task to finish, identified by its
    /// execution generation (`execution_id`) and stable task id (`exec_request_id`).
    /// The loop has validated that this identity is the session's in-flight task.
    /// Blocks up to a runtime-chosen bound, then reports whether the task completed,
    /// is still running, or became unknown. The default is not supported (a
    /// runtime without durable background tasks — Direct — runs exec synchronously,
    /// so nothing is ever left running to wait on).
    async fn wait_for_task(
        &self,
        action_request_id: &str,
        execution_id: &str,
    ) -> Result<WaitOutcome, AgentError> {
        let _ = (action_request_id, execution_id);
        Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "waiting for background tasks is not supported by this runtime".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }
}

/// Inputs to atomically claim a turn for a conversation. The subject fields pin
/// the session; the routing fields rebind the turn; `current_pdp_scope` is the
/// freshly computed authorization scope adopted at the turn boundary.
#[derive(Debug, Clone)]
pub struct ClaimTurnParams {
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub policy_revision: i64,
    pub current_pdp_scope: AgentScope,
    pub turn_id: String,
    pub request_id: Option<String>,
    pub connection_id: Option<String>,
    /// What caused this turn to be claimed. A control-end request is
    /// [`TriggerOrigin::User`]; a manager-fired automation turn is
    /// [`TriggerOrigin::ExecCompletion`], which the loop bars from starting new
    /// mutations. Adopted onto the session at the turn boundary.
    ///
    /// [`TriggerOrigin::User`]: crate::session::TriggerOrigin::User
    /// [`TriggerOrigin::ExecCompletion`]: crate::session::TriggerOrigin::ExecCompletion
    pub trigger_origin: crate::session::TriggerOrigin,
    pub now: String,
}

/// Why claiming a turn failed.
#[derive(Debug, Clone)]
pub enum ClaimError {
    /// A turn is already running for this conversation.
    Busy,
    /// The follow-up came from a different subject than the session was bound to.
    Subject(SubjectMismatch),
    /// The session backend failed (DB / store error).
    Backend(AgentError),
}

/// Owns the agent session's lifecycle and atomicity. The Direct runtime keeps
/// sessions in process memory (per-conversation lock); the manager persists them
/// in DB with optimistic-concurrency CAS and is the authority across instances.
#[async_trait(?Send)]
pub trait SessionSeam {
    /// Atomically load-or-create the session for `conversation_id` and claim a
    /// turn (settled → `Running`), recomputing scope at the turn boundary,
    /// resetting the turn-level counters, and rotating the lease token. An
    /// orphaned active session whose lease has expired is recovered (settled +
    /// closed) before the claim, so a crashed turn never blocks follow-ups
    /// forever. Returns the claimed session, or a [`ClaimError`] (busy / subject
    /// mismatch / backend). Atomicity is the implementation's responsibility.
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError>;

    /// Persist the session after a step (conversation growth, counters, the
    /// finishing turn-state transition), under a **fencing CAS on both the lease
    /// token and the version**: the held `session.lease_token` must still be the
    /// current owner's and the held `session.version` the latest. On success the
    /// implementation advances the stored version and writes the new value back
    /// into `session.version` so the next save CASes against it. A token mismatch
    /// (the lease was taken over by another owner) or a version conflict fails the
    /// save — the loop ends the turn rather than overwriting the new owner's work.
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError>;

    /// Persist a task-status projection and its append-only run event. Durable
    /// runtimes override this to commit both atomically; simple in-memory seams
    /// may use the default session-only persistence.
    async fn save_task_status_update(
        &self,
        session: &mut PersistedAgentSession,
        event: &crate::dynamic_run::TaskStatusUpdatedEvent,
    ) -> Result<(), AgentError> {
        let _ = event;
        self.save(session).await
    }

    /// Persist a normalized permission request and its append-only event in one
    /// transaction. The default is suitable only for in-memory test seams;
    /// durable runtimes override it so UI visibility cannot diverge from audit.
    async fn save_permission_request(
        &self,
        session: &mut PersistedAgentSession,
        event: &crate::dynamic_run::PermissionRequestedEvent,
    ) -> Result<(), AgentError> {
        let _ = event;
        self.save(session).await
    }

    /// Return the latest durable user-input revision for the run. Runtimes that
    /// do not support concurrent durable follow-ups return `None`.
    async fn latest_input_revision(
        &self,
        conversation_id: &str,
    ) -> Result<Option<u64>, AgentError> {
        let _ = conversation_id;
        Ok(None)
    }

    /// Settle an active read-only turn that was superseded by newer durable
    /// input. Durable runtimes merge any completed read results, close unstarted
    /// calls, preserve the newer user messages, and append a Superseded event in
    /// one fenced transaction. Returns false if the revision did not advance or
    /// the stale owner no longer owns the lease.
    async fn settle_superseded(
        &self,
        stale_session: &PersistedAgentSession,
        now: &str,
    ) -> Result<bool, AgentError> {
        let _ = (stale_session, now);
        Ok(false)
    }

    /// Renew the lease for an active turn: extend its deadline if `lease_token` is
    /// still the current owner's and the turn is still active. It **never** bumps
    /// the version (so a concurrent [`save`](Self::save) is unaffected) and is a
    /// no-op once the session settles. A token mismatch / settled session returns
    /// an error so a background renewer stops. The default is a no-op for runtimes
    /// (and test stubs) that do not lease.
    async fn heartbeat(
        &self,
        conversation_id: &str,
        lease_token: u64,
        now: &str,
    ) -> Result<(), AgentError> {
        let _ = (conversation_id, lease_token, now);
        Ok(())
    }
}

/// Starts a background lease-renewal ticker for one active turn. The core has no
/// timer/runtime, so each runtime supplies it; the loop starts it right after a
/// successful claim (with the claimed `lease_token`) and drops the returned guard
/// when the turn settles, stopping renewal.
pub trait LeaseHeartbeat {
    /// Begin periodically calling [`SessionSeam::heartbeat`] for this turn until the
    /// returned guard is dropped.
    fn start(&self, conversation_id: String, lease_token: u64) -> Box<dyn HeartbeatGuard>;

    /// Whether every attempted renewal for this turn has succeeded. A runtime
    /// with a real lease flips this false at the first renewal error; checkpoint
    /// commits then fail closed even when no competing owner has taken over yet.
    fn is_healthy(&self) -> bool {
        true
    }
}

/// Opaque handle whose `Drop` stops the lease-renewal ticker started by
/// [`LeaseHeartbeat::start`].
pub trait HeartbeatGuard {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatRole, ToolChoice, ToolSpec};

    #[test]
    fn registered_tool_requirement_survives_model_facing_rename() {
        let request = ModelRequest {
            messages: vec![ChatMessage::text(
                "u1",
                ChatRole::User,
                "inspect the screen",
            )],
            tools: vec![ToolSpec {
                name: "renamed_screen_reader".into(),
                description: "read the current screen".into(),
                parameters_schema: serde_json::json!({"type": "object"}),
            }],
            tool_requirements: crate::model_capability::ModelRequirements::IMAGE_INPUT,
            tool_choice: ToolChoice::Auto,
            response_format: ResponseFormatSpec::None,
            use_case: ModelUseCase::Agent,
            caller_output_hard_cap: None,
        };

        assert!(request.requirements().image_input);
    }
}
