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
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope};

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
    /// Optional upper bound on the model's output tokens. `None` leaves the seam's
    /// own default in effect. Currently honored only by the signal seam's body
    /// builders (`web/signal/src/model_dial.rs`), used by the provider connectivity
    /// probe to keep the test reply tiny; other seams may ignore it.
    pub max_output_tokens: Option<u32>,
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
            max_output_tokens: None,
        }
    }
}

/// Receives streaming output and lifecycle events from an agent turn as they
/// happen, so a runtime can forward them to the UI (the manager maps these onto
/// `DiagnoseEvent` tool/turn frames; the Direct runtime onto its own stream).
///
/// Text deltas are **provisional** until the turn's [`StopReason`] is known: the
/// loop commits them only on a final answer (via [`on_answer_committed`]) and
/// signals [`on_turn_discarded`] on a truncated turn, so intermediate
/// tool-calling turns never leak half-text to the UI. The tool hooks bracket each
/// dispatched tool call (a read tool, or a mutating tool's approval wait), letting
/// the UI show progress without parsing the conversation. All hooks but
/// [`on_text_delta`] default to no-ops so an existing text-only sink keeps working.
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

    /// A read tool call was dispatched (about to run).
    fn on_tool_started(&mut self, tool_name: &str, call_id: &str) {
        let _ = (tool_name, call_id);
    }

    /// A mutating tool call is waiting for the operator's approval decision.
    fn on_awaiting_approval(&mut self, tool_name: &str, call_id: &str) {
        let _ = (tool_name, call_id);
    }

    /// A dispatched tool call produced its result; `ok` is whether it yielded a
    /// usable result (an executed/read success) rather than an error / rejection /
    /// unknown outcome.
    fn on_tool_finished(&mut self, call_id: &str, ok: bool) {
        let _ = (call_id, ok);
    }

    /// The turn committed a final natural-language answer.
    fn on_answer_committed(&mut self, text: &str) {
        let _ = text;
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
    },
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
        exec_request_id: &str,
        execution_id: &str,
    ) -> Result<WaitOutcome, AgentError> {
        let _ = (exec_request_id, execution_id);
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
}

/// Opaque handle whose `Drop` stops the lease-renewal ticker started by
/// [`LeaseHeartbeat::start`].
pub trait HeartbeatGuard {}
