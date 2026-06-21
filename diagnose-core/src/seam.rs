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
use desk_agent_protocol::{AgentError, AgentScope};

use crate::chat::{ChatMessage, ModelTurn, ToolCall, ToolChoice, ToolSpec};
use crate::prompt::ResponseFormatSpec;
use crate::session::{PersistedAgentSession, SubjectMismatch};

/// A model request in neutral terms: the conversation, the tools the model may
/// call, how it is steered toward them, and the requested response format. The
/// wire shape (OpenAI vs Anthropic) is the [`ModelSeam`] implementation's concern.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub response_format: ResponseFormatSpec,
}

impl ModelRequest {
    /// A tool-free request (the single-turn diagnose shape): no tools advertised,
    /// the model is free to answer in text.
    pub fn text_only(messages: Vec<ChatMessage>, response_format: ResponseFormatSpec) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            response_format,
        }
    }
}

/// Receives streaming output from a model turn as it arrives.
///
/// Text deltas are **provisional** until the turn's [`StopReason`] is known: the
/// loop commits them only on a final answer (`EndTurn`) and discards them
/// otherwise, so intermediate tool-calling turns never leak half-text to the UI.
/// Object-safe so a `&mut dyn TurnSink` can be passed across the seam.
///
/// [`StopReason`]: crate::chat::StopReason
pub trait TurnSink {
    /// An incremental fragment of the assistant's text for the current turn.
    fn on_text_delta(&mut self, delta: &str);
}

/// A sink that ignores all streamed output (for non-streaming callers / tests).
pub struct NullTurnSink;

impl TurnSink for NullTurnSink {
    fn on_text_delta(&mut self, _delta: &str) {}
}

/// The model call, abstracted from the wire dialect. Implementations stream text
/// deltas through `sink` and return the fully assembled, normalized [`ModelTurn`]
/// (text + tool calls + stop reason + usage).
#[async_trait(?Send)]
pub trait ModelSeam {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecIdentity {
    pub work_id: i64,
    pub execution_id: String,
    pub exec_request_id: String,
}

/// The terminal outcome of a mutating tool call's approval + execution. The seam
/// owns the whole approval → dispatch → result wait (Direct: a local oneshot
/// confirm + in-process exec; Manager: a durable work item + central approval +
/// cross-instance dispatch). The loop turns this into the conversation and the
/// execution-reconciliation state.
#[derive(Debug, Clone)]
pub enum ExecOutcome {
    /// Approved and executed to a known, already-redacted result.
    Executed(ToolRunOutput),
    /// The operator rejected the command; nothing ran.
    Rejected { reason: Option<String> },
    /// Approval expired before any decision; nothing ran.
    ApprovalTimeout,
    /// The command may have run but its outcome is unknown (cancel / timeout /
    /// crash after dispatch). The loop closes the conversation with a placeholder
    /// tool result and records [`ExecutionState::OutcomeUnknown`]; a late result
    /// reconciles it in place.
    ///
    /// [`ExecutionState::OutcomeUnknown`]: crate::session::ExecutionState::OutcomeUnknown
    Unknown(ExecIdentity),
}

/// Per-call context the loop hands the mutating seam: the turn's identity plus the
/// authorization the work item is minted under. The subject (actor / device) is
/// already held by the seam (it is built per turn); these are the fields the loop
/// owns and the seam needs to mint a durable work item.
#[derive(Debug, Clone)]
pub struct ExecContext {
    pub conversation_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub policy_revision: i64,
    pub scope: AgentScope,
}

/// Runs the loop's tools. Read tools run immediately; a mutating tool goes through
/// approval + real execution via [`confirm_and_exec`](ToolSeam::confirm_and_exec).
#[async_trait(?Send)]
pub trait ToolSeam {
    /// Run a read-only tool call and return its redacted result. The loop has
    /// already validated that the call names an exposed read tool.
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError>;

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
}

/// Inputs to atomically claim a turn for a conversation. The subject fields pin
/// the session; the routing fields rebind the turn; `current_pdp_scope` is the
/// freshly computed authorization scope adopted at the turn boundary.
#[derive(Debug, Clone)]
pub struct ClaimTurnParams {
    pub conversation_id: String,
    pub tenant_id: Option<String>,
    pub actor_id: String,
    pub device_id: String,
    pub policy_revision: i64,
    pub current_pdp_scope: AgentScope,
    pub turn_id: String,
    pub request_id: Option<String>,
    pub connection_id: Option<String>,
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
    /// turn (`Idle → Running`), recomputing scope at the turn boundary and
    /// resetting the turn-level counters. Returns the claimed session, or a
    /// [`ClaimError`] (busy / subject mismatch / backend). Atomicity is the
    /// implementation's responsibility.
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError>;

    /// Persist the session after a step (conversation growth, counters, the
    /// finishing turn-state transition).
    async fn save(&self, session: &PersistedAgentSession) -> Result<(), AgentError>;
}
