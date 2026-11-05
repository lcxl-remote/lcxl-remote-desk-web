//! The agentic tool-calling loop (pure orchestration; runtime-agnostic).
//!
//! [`run_agent_turn`] drives one conversational turn over the seams: it claims
//! the turn (atomically, via [`SessionSeam`]), then repeatedly calls the model
//! ([`ModelSeam`]), validates each turn with [`classify_model_turn`], and either
//! returns the final answer or runs the requested tools ([`ToolSeam`]) and loops.
//! An outer wrapper guarantees the turn machine is always settled (`finish_turn`)
//! on every exit path.
//!
//! Read tools run immediately; a mutating tool goes through approval + real
//! execution via [`ToolSeam::confirm_and_exec`], whose terminal outcome the loop
//! turns into the conversation and the execution-reconciliation state — including
//! the unknown-outcome closure (§6): a placeholder tool result keeps the model
//! history well-formed and a late result replaces it in place. Mutating calls in
//! one turn run serially; a rejection or failed dependent action halts the rest of that group.
//! Bounded definitions remain visible independently of authority. Returned calls
//! must be advertised and pass the current exposure matrix, readiness and grant
//! checks; the execution seam atomically reserves each authorized invocation.
//!
//! Circuit breakers are turn-level (reset when the turn is claimed): a per-turn
//! step budget ([`MAX_STEPS_PER_TURN`]) and a same-tool repeat cap
//! ([`MAX_SAME_TOOL_PER_TURN`]).
//!
//! [`MAX_STEPS_PER_TURN`]: crate::MAX_STEPS_PER_TURN
//! [`MAX_SAME_TOOL_PER_TURN`]: crate::MAX_SAME_TOOL_PER_TURN

mod compression;
mod phases;
mod title;

use desk_agent_protocol::browser_control::{
    BrowserActionResult, BrowserElementRef, BrowserElementRole, BrowserPageRef,
};
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
};
use desk_agent_protocol::content_safety::{ContentSafetyDecision, StreamRetractionReason};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use crate::content_safety::{
    ContentSafetyMode, SafetyImage, SafetyModelTurn, SafetyToolCall, content_blocked_error,
    normalize_safety_error, refusal_placeholder_for,
};
use crate::model_message_labels::internal_tool_result_envelope as derive_internal_tool_result_envelope;
use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::chat::{ChatMessage, ChatRole, ModelTurnError, TurnDisposition, classify_model_turn};
use crate::registry::{RegisteredTool, ToolEffect, exposed_for_session, lookup_for_session};
use crate::seam::{
    ClaimError, ClaimTurnParams, ExecContext, ExecOutcome, ModelRequest, ModelSeam, ReadOutcome,
    SessionSeam, ToolSeam, TurnSink, WaitOutcome,
};
use crate::session::{
    AgentSessionSurface, ExecutionState, PersistedAgentSession, SubjectMismatch, TriggerOrigin,
    TurnState,
};

/// The placeholder tool-result text written when a mutating execution's outcome is
/// unknown (§6): it keeps the conversation well-formed and tells the model not to
/// assume the command succeeded. A late real result replaces it in place.
const OUTCOME_UNKNOWN_PLACEHOLDER: &str = "Execution failed to return a conclusive result; effects may have occurred. Read the current target state before choosing the next action. Other authorized operations remain available; no user acknowledgement or record closure is required. Do not infer success or blindly repeat the same action.";

fn image_input_error(error: crate::image_input::ImageInputError) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid model image attachment: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

fn reasoning_output_budget_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::OutputLimitExceeded,
        message: "the model reasoning budget exhausted the runtime output limit before any answer or tool call was produced; increase runtime_max_output_tokens or reduce the model reasoning budget".into(),
        retryable: false,
        safe_for_model: true,
        error_code: Some(
            desk_utils::error::DeskErrorCode::AI_ASSISTANT_RESPONSE_TRUNCATED.code(),
        ),
    }
}

fn empty_end_turn_recovery_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "the model ended twice without any answer or tool call; the bounded automatic recovery was exhausted".into(),
        retryable: false,
        safe_for_model: true,
        error_code: Some(
            desk_utils::error::DeskErrorCode::AI_ASSISTANT_PROTOCOL_VIOLATION.code(),
        ),
    }
}

fn permission_continuation_recovery_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "the model twice answered without invoking the exact tool made available by the owner's permission decision; the bounded automatic recovery was exhausted".into(),
        retryable: false,
        safe_for_model: true,
        error_code: Some(
            desk_utils::error::DeskErrorCode::AI_ASSISTANT_PROTOCOL_VIOLATION.code(),
        ),
    }
}

fn model_context_error(error: crate::model_context::ModelContextError) -> AgentError {
    let oversized = matches!(
        error,
        crate::model_context::ModelContextError::ContextItemTooLarge { .. }
            | crate::model_context::ModelContextError::ProtectedStateTooLarge { .. }
    );
    AgentError {
        kind: if oversized {
            AgentErrorKind::InvalidInput
        } else {
            AgentErrorKind::Internal
        },
        message: error.to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: oversized
            .then(|| desk_utils::error::DeskErrorCode::AI_CONTEXT_ITEM_TOO_LARGE.code()),
    }
}

fn context_compression_error(kind: crate::seam::ContextCompressionFailureKind) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: if kind == crate::seam::ContextCompressionFailureKind::Truncated {
            "model context compression failed: truncated; original history retained. Increase summary maximum output tokens in Context management and check the model runtime output limit before continuing.".into()
        } else {
            format!("model context compression failed: {}", kind.as_str())
        },
        retryable: false,
        safe_for_model: true,
        error_code: Some(
            if kind == crate::seam::ContextCompressionFailureKind::Truncated {
                desk_utils::error::DeskErrorCode::AI_CONTEXT_SUMMARY_OUTPUT_TRUNCATED.code()
            } else {
                desk_utils::error::DeskErrorCode::AI_CONTEXT_COMPRESSION_FAILED.code()
            },
        ),
    }
}

fn compression_failure_for_context_error(
    error: &crate::model_context::ModelContextError,
) -> crate::seam::ContextCompressionFailureKind {
    use crate::model_context::ModelContextError as Error;
    use crate::seam::ContextCompressionFailureKind as Kind;

    match error {
        Error::ProtectedStateTooLarge { .. } | Error::NoCompressiblePrefix => {
            Kind::ProtectedStateTooLarge
        }
        Error::ProtectedReplayUnsafe(_)
        | Error::OrphanToolResult(_)
        | Error::IncompleteToolGroup(_)
        | Error::UnexpectedReplayDisposition(_) => Kind::ProtectedReplayUnsafe,
        Error::CompressionInputTooLarge
        | Error::ContextCostOverflow
        | Error::ContextItemTooLarge { .. } => Kind::InputTooLarge,
        Error::SummaryTooLarge => Kind::SummaryTooLarge,
        Error::InvalidCheckpoint(_) => Kind::InvalidSchema,
        Error::InvalidBudget(_) => Kind::InvalidEffectiveBudget,
        Error::UnsupportedStateSchema(_)
        | Error::TooManyPolicyEntries(_)
        | Error::DuplicatePolicyEntry
        | Error::AmbiguousPersistedFloor
        | Error::UnsupportedStrategy
        | Error::InvalidProfileRevision(_)
        | Error::MissingPersistedFloor(_)
        | Error::FloorRegression
        | Error::InvalidProtectionReference(_)
        | Error::StaleCompressionPlan => Kind::StaleContext,
    }
}

fn compression_failure_for_provider_error(
    error: &AgentError,
) -> crate::seam::ContextCompressionFailureKind {
    use crate::seam::ContextCompressionFailureKind as Kind;

    let message = error.message.to_ascii_lowercase();
    if message.contains("invalid output limit") || message.contains("invalid effective budget") {
        return Kind::InvalidEffectiveBudget;
    }
    if message.contains("timeout")
        || message.contains("timed out")
        || message.contains("no upstream progress")
    {
        return Kind::ProviderTimeout;
    }
    match error.kind {
        AgentErrorKind::Timeout => Kind::ProviderTimeout,
        AgentErrorKind::OutputLimitExceeded => Kind::Truncated,
        AgentErrorKind::UnsupportedCapability | AgentErrorKind::UnsupportedPlatform => {
            Kind::UnsupportedEndpoint
        }
        AgentErrorKind::TransportError
            if message.contains("not implemented") || message.contains("unsupported endpoint") =>
        {
            Kind::UnsupportedEndpoint
        }
        _ => Kind::ProviderRejected,
    }
}

fn compression_audit_context(
    plan: &crate::model_context::CompressionPlan,
    content_safety: &ContentSafetyMode<'_>,
) -> crate::seam::ContextCompressionAuditContext {
    let safety = match content_safety {
        ContentSafetyMode::Disabled => None,
        ContentSafetyMode::Enforced { context, .. } => {
            Some(crate::seam::ContextCompressionSafetyAuditContext {
                provider_identity_sha256: context.safety_provider_identity_sha256.clone(),
                model_identity_sha256: context.safety_model_identity_sha256.clone(),
                connection_revision: context.safety_connection_revision,
                model_profile_revision: context.safety_model_profile_revision,
                policy_revision: context.policy_revision,
                prompt_version: context.safety_prompt_version.clone(),
            })
        }
    };
    crate::seam::ContextCompressionAuditContext {
        generation: plan.generation,
        covered_message_count: plan.covered_message_count,
        covered_from_message_id: plan.covered_from_message_id.clone(),
        covered_through_message_id: plan.covered_through_message_id.clone(),
        input_context_cost: plan.input_model_context_cost,
        platform_context_policy_revision: plan.policy.platform_context_policy_revision,
        safety,
    }
}

async fn report_context_compression_failure(
    model: &dyn ModelSeam,
    kind: crate::seam::ContextCompressionFailureKind,
    context: Option<crate::seam::ContextCompressionAuditContext>,
    usage: Option<crate::seam::ContextCompressionProviderUsage>,
) -> AgentError {
    model.on_context_compression_failed(kind);
    model
        .audit_context_compression(crate::seam::ContextCompressionAuditOutcome::Failed {
            context,
            usage,
            kind,
        })
        .await;
    context_compression_error(kind)
}

fn retain_latest_session_image(
    session: &mut crate::session::PersistedAgentSession,
) -> Result<(), AgentError> {
    crate::image_input::retain_latest_session_image(&mut session.conversation)
        .map_err(image_input_error)
}

/// Why the loop's circuit breaker stopped a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitBreakReason {
    /// The per-turn step budget was exhausted.
    StepBudget,
    /// One tool was called too many times within the turn.
    SameToolRepeat,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// A bounded goal planning segment yielded without a protocol failure.
    GoalYielded,
    /// The model produced a final text answer.
    Answered(String),
    /// A complete model turn or image was rejected by application policy.
    ContentRejected(ContentSafetyDecision),
    /// The required safety dependency did not produce a trustworthy verdict.
    /// This is a retryable failed turn, distinct from a policy rejection.
    ContentSafetyUnavailable(AgentError),
    /// The model turn was truncated (`MaxTokens` / `Other`) and discarded.
    Truncated,
    /// The provider reported that the input exceeded its model context window.
    ContextWindowExceeded,
    /// A circuit breaker stopped the turn.
    CircuitBreak(CircuitBreakReason),
    /// The model violated the wire contract (inconsistent stop reason / bad args).
    ProtocolError(ModelTurnError),
    /// A turn is already running for this conversation.
    TurnBusy,
    /// The owner input was durably accepted for later execution.
    RequestQueued {
        request_id: String,
        session_id: String,
    },
    /// The follow-up came from a different subject than the session.
    SubjectRejected(SubjectMismatch),
    /// New durable user input superseded this planning revision. No stale model
    /// answer, permission request, or new tool dispatch was committed.
    Superseded {
        previous_input_revision: u64,
        current_input_revision: u64,
    },
    /// A normalized permission batch was durably recorded for user decision.
    /// No grant was minted and no requested tool was dispatched.
    PermissionRequested { request_id: String },
    /// A model-proposed long-running goal is awaiting its owner's decision.
    /// The request alone neither starts work nor grants device authority.
    GoalOpenRequested { request_id: String },
    /// A goal-only planning control was accepted and atomically settled with
    /// the session. It does not grant authority or dispatch device work.
    GoalControlled {
        control: crate::goal::GoalControl,
        call_id: String,
    },
    /// A durable main dependency wait releases this turn's lease and model slot.
    SubAgentsWaiting { wait_id: String },
    /// The worker committed a report or wait and released the execution lease.
    SubAgentYielded,
    /// The child's source group closed admission while its original task persists.
    DelegationSourcePaused,
}

/// Recover only the accepted input's first handling turn. The host must first
/// validate its durable input receipt, owner scope and handled input sequence.
pub fn committed_input_outcome(
    session: &PersistedAgentSession,
    message_id: &str,
) -> Option<LoopOutcome> {
    let index = session.conversation.iter().position(|message| {
        message.message_id == message_id
            && message.role == ChatRole::User
            && !crate::permission_resume::is_resume_control_message(message)
    })?;
    // Queued inputs may share the first later owner input's handling turn.
    let turn_id = session.conversation[index..]
        .iter()
        .filter(|message| {
            message.role == ChatRole::User
                && !crate::permission_resume::is_resume_control_message(message)
        })
        .find_map(|message| message.turn_id.as_ref())?;
    session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_ref() == Some(turn_id)
                && message.tool_calls.is_empty()
        })
        .map(|message| LoopOutcome::Answered(message.text.clone()))
}

/// The seams + config the loop runs over, borrowed for one turn.
/// Concrete review refusals of one tool before the owner has to decide.
const MAX_REVIEW_DENIALS_PER_TOOL: u32 = 2;

pub struct LoopDeps<'a> {
    pub session_seam: &'a dyn SessionSeam,
    pub model: &'a dyn ModelSeam,
    pub tools: &'a dyn ToolSeam,
    /// Closed safety mode for this turn. OSS must pass `Disabled`; protected
    /// manager callers freeze a complete `Enforced` context before claiming.
    pub content_safety: ContentSafetyMode<'a>,

    pub registry: &'a [RegisteredTool],
    /// Complete compiled Provider catalog used only to validate discoverable
    /// permission requests. `registry` above remains the callable tool set.
    pub provider_registry: Option<&'a crate::provider_registry::ProviderRegistry>,
    /// Fresh target-scoped readiness used by both the discoverable catalog and
    /// permission-request validation. Static compilation alone is insufficient
    /// for edge capabilities such as the paired Office add-in.
    pub capability_inventory: Option<&'a [crate::capability_availability::CapabilityAvailability]>,
    /// Context-grounded Provider tools that may be requested from the owner but
    /// are not necessarily callable under the current grant scope.
    pub capability_permission_candidates: &'a [RegisteredTool],
    /// Content-free measurements captured when the caller built the current
    /// capability catalog. Non-AI-Assistant callers leave this unset.
    pub capability_catalog_metrics: Option<crate::permission_tools::CapabilityCatalogMetrics>,
    /// Active exact-input Provider tools recovered from the durable permission
    /// decision. On a permission-resumed turn the loop initially exposes only
    /// these mutations (plus internal run projection) so a model cannot replace
    /// an approved ephemeral reference by re-running observation first.
    pub permission_continuation_exact_tools: &'a [String],
    pub response_format: crate::prompt::ResponseFormatSpec,
    /// The system message prepended to the (trimmed) conversation on every model
    /// call. The caller builds it (with the control-end locale) via
    /// [`crate::agentic_prompt::build_agentic_system_message`]; it is never stored
    /// in the persisted conversation, so a prompt-version bump applies to
    /// in-flight conversations.
    pub system_prompt: ChatMessage,
    /// Explicit control-end preference. Missing values preserve the saved locale.
    pub response_locale: Option<String>,
    /// Current interactive worker user's Home directory. This is guidance for
    /// proposing a directory selection, not a selected directory or grant.
    pub interactive_user_home: Option<&'a str>,
    /// Worker incarnation that reported `interactive_user_home`.
    pub interactive_user_home_incarnation: Option<&'a str>,
    /// Per-turn model→tool step budget (circuit breaker). Diagnose passes
    /// [`crate::MAX_STEPS_PER_TURN`]; the latency-sensitive Terminal AI Assistant
    /// passes a tighter bound.
    pub max_steps_per_turn: u32,
    /// Per-turn cap for calls to the same tool. Kept independent of the total
    /// step budget because one model response may request multiple tools.
    pub max_same_tool_per_turn: u32,
    /// Wall-clock source (RFC3339); the core stays free of a time dependency.
    pub clock: &'a dyn Fn() -> String,
    /// Optional background lease renewer. After the turn is claimed the loop starts
    /// it with the claimed lease token and drops it when the turn settles, so a
    /// long-running turn keeps its lease alive and is not reclaimed as an orphan.
    /// `None` disables renewal (test stubs / runtimes without a lease).
    pub heartbeat: Option<&'a dyn crate::seam::LeaseHeartbeat>,
}

/// Run one agent turn end to end. `user_message` is appended after the turn is
/// claimed. Returns the [`LoopOutcome`]; only a model / tool / session backend
/// transport failure surfaces as `Err`.
pub async fn run_agent_turn(
    deps: &LoopDeps<'_>,
    claim: ClaimTurnParams,
    user_message: ChatMessage,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    run_or_resume(deps, claim, Some(user_message), sink).await
}

/// Run one **automation** turn end to end: like [`run_agent_turn`] but with no
/// message appended, because the completion the turn reacts to is already the tail
/// of the conversation (delivered when the background command finished). The model
/// runs against that existing tail. Everything else — claim, lease, loop, settle —
/// is identical.
pub async fn resume_agent_turn(
    deps: &LoopDeps<'_>,
    claim: ClaimTurnParams,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    run_or_resume(deps, claim, None, sink).await
}

mod artifact_registry;
mod assistant_data;
mod call_budget;
mod fresh_task;
mod goal_control;
mod issue_reply;
mod retrieval_accounting;
pub use fresh_task::{
    resume_claimed_fresh_task_children_turn, resume_claimed_fresh_task_notification_turn,
    resume_claimed_fresh_task_permission_turn, resume_claimed_fresh_task_turn,
};

/// Drive a continuation already claimed atomically with its durable occurrence.
/// The central runtime must supply the returned session directly, bind its
/// heartbeat to BOTH leases, and gate model/tool calls against current authority.
/// This function never claims again or interprets the event as new user input.
/// The first save still fences the held session version/token before any model
/// request, and the ordinary loop retains supersession and safety checks.
pub async fn resume_claimed_scheduled_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    scheduled_run_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    resume_claimed_scheduled(deps, session, scheduled_run_id, None, sink).await
}

/// Continue the same claimed occurrence after its original durable owner decision.
/// The central store must validate and consume that receipt with both leases.
pub async fn resume_claimed_scheduled_permission_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    scheduled_run_id: &str,
    permission_request_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    resume_claimed_scheduled(
        deps,
        session,
        scheduled_run_id,
        Some(permission_request_id),
        sink,
    )
    .await
}

async fn resume_claimed_scheduled(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    scheduled_run_id: &str,
    permission_request_id: Option<&str>,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    if scheduled_run_id.is_empty()
        || scheduled_run_id.len() > 256
        || session.trigger_origin != crate::session::TriggerOrigin::ScheduledContinuation
        || !session.agent_role.is_main()
        || session.surface != AgentSessionSurface::AiAssistant
        || session.turn_state != TurnState::Running
        || session.input_revision == 0
        || session.lease_token == 0
        || session.current_request_id.as_deref() != Some(scheduled_run_id)
        || session.active_control_connection_id.is_some()
        || session
            .current_turn_id
            .as_ref()
            .is_none_or(|id| id.is_empty())
        || deps.heartbeat.is_none()
    {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "scheduled continuation requires a bound active claim and heartbeat".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    let turn_id = session.current_turn_id.clone().expect("validated turn id");
    let denied = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "The original requirement is unavailable for this scheduled continuation.".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    if let Some(id) = permission_request_id
        && !session.permission_requests.iter().any(|request| {
            request.request_id == id
                && request.input_revision == session.input_revision
                && request.validate().is_ok()
                && matches!(
                    request.state,
                    crate::dynamic_run::PermissionRequestState::Approved
                        | crate::dynamic_run::PermissionRequestState::PartiallyApproved
                        | crate::dynamic_run::PermissionRequestState::Denied
                        | crate::dynamic_run::PermissionRequestState::NeedsRevalidation
                )
        })
    {
        return Err(denied());
    }
    let policy = deps.model.model_egress_policy()?.ok_or_else(denied)?;
    let task_input = session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::SystemEvent
                && message.message_id == crate::native_ids::scheduled_message(scheduled_run_id)
        })
        .map(|message| {
            let mut message = message.clone();
            message.role = ChatRole::User;
            message
        });
    // Rebuild the same protocol bridge from the original requirement. Owner
    // supplements after its first publication are data for this occurrence;
    // they must not replace the source of a durable bridge with the same id.
    let scheduled_bridge_id = format!("scheduled-resume-{turn_id}");
    let original_prefix = session
        .conversation
        .iter()
        .position(|message| message.message_id == scheduled_bridge_id)
        .unwrap_or(session.conversation.len());
    let original = task_input
        .as_ref()
        .or_else(|| {
            crate::permission_resume::latest_user_requirement(
                &session.conversation[..original_prefix],
            )
        })
        .ok_or_else(denied)?;
    let bridge = if permission_request_id.is_some() {
        crate::permission_resume::authorized_permission_resume_message(
            format!("{turn_id}-decision"),
            &policy,
            original,
        )?
    } else {
        crate::permission_resume::authorized_scheduled_resume_message(
            scheduled_bridge_id,
            &policy,
            original,
        )?
    };
    drive_claimed(deps, session, turn_id, Some(bridge), sink).await
}

/// Resume after an owner permission decision and append one trusted protocol
/// bridge at the conversation tail. Chat-completions providers commonly require
/// a final user-role message to start a new completion, so the bridge uses that
/// wire role while explicitly declaring that it is not user-authored and carries
/// no new requirement. The signal runtime omits it from the product transcript
/// and user-input event stream.
pub async fn resume_agent_turn_after_permission(
    deps: &LoopDeps<'_>,
    claim: ClaimTurnParams,
    decision_message: ChatMessage,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    if decision_message.role != ChatRole::User {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "permission resume bridge must use the user protocol role".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    run_or_resume(deps, claim, Some(decision_message), sink).await
}

/// Drive a segment whose goal and session leases were atomically claimed by a
/// durable coordinator. This entry cannot claim a fresh turn on its own.
pub async fn run_preclaimed_goal_slice(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    if session.trigger_origin != TriggerOrigin::GoalContinuation
        || !session.agent_role.is_main()
        || session.focus_epoch.goal_segment.is_none()
        || !session.turn_state.is_active()
    {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "goal segment was not atomically claimed".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    let turn_id = session.current_turn_id.clone().ok_or_else(|| AgentError {
        kind: AgentErrorKind::Internal,
        message: "claimed goal segment has no turn identity".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Execute only an existing child claimed by its durable coordinator. The role
/// and input are never reconstructed from a completion or model tool argument.
pub async fn run_preclaimed_subagent_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    if session.agent_role.binding().is_none()
        || session.trigger_origin != TriggerOrigin::DelegatedTask
        || session.surface != AgentSessionSurface::AiAssistant
        || !session.turn_state.is_active()
        || session.lease_token == 0
        || deps.heartbeat.is_none()
        || deps.session_seam.subagents(&session).is_none()
    {
        return Err(crate::subagent::invalid(
            "delegated task requires a durable child claim and heartbeat",
        ));
    }
    let turn_id = session
        .current_turn_id
        .clone()
        .ok_or_else(|| crate::subagent::invalid("claimed child has no turn identity"))?;
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Original command interpretation has its own source-qualified claim. It
/// cannot dispatch tools or count an intermediate explanation as a task report.
pub async fn run_preclaimed_child_completion(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    if session.agent_role.binding().is_none()
        || !session.turn_state.is_active()
        || session.lease_token == 0
        || deps.heartbeat.is_none()
        || deps.model.command_completion_event_id().is_none()
        || !matches!(
            session.trigger_origin,
            TriggerOrigin::ExecCompletion
                | TriggerOrigin::WorkCompletion {
                    kind: crate::session::WorkKind::AgentExec
                }
        )
    {
        return Err(crate::subagent::invalid(
            "child command interpretation requires its original qualified claim",
        ));
    }
    let turn_id = session
        .current_turn_id
        .clone()
        .ok_or_else(|| crate::subagent::invalid("child interpretation has no turn identity"))?;
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Resume a dependency or report under its original owner task and budget.
/// The coordinator binds the claim to the original input and control revisions.
pub async fn run_preclaimed_subagent_completion(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    if !session.agent_role.is_main()
        || session.trigger_origin != TriggerOrigin::SubAgentCompletion
        || session.surface != AgentSessionSurface::AiAssistant
        || !session.turn_state.is_active()
        || !session.has_ready_subagent_completion()
        || session.subagent_wait.is_some()
        || session.lease_token == 0
        || deps.heartbeat.is_none()
        || deps.session_seam.subagents(&session).is_none()
    {
        return Err(crate::subagent::invalid(
            "subagent result interpretation requires a durable root claim",
        ));
    }
    let turn_id = session
        .current_turn_id
        .clone()
        .ok_or_else(|| crate::subagent::invalid("root interpretation has no turn identity"))?;
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Shared body of [`run_agent_turn`] / [`resume_agent_turn`]: claim the turn, keep
/// the lease alive, optionally append a message, run the loop, then settle and
/// persist once. `to_append` is the user message for a control-end turn, or `None`
/// for an automation resume that reacts to the conversation's existing tail.
async fn run_or_resume(
    deps: &LoopDeps<'_>,
    claim: ClaimTurnParams,
    to_append: Option<ChatMessage>,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let turn_id = claim.turn_id.clone();
    let session = match deps.session_seam.claim_turn(claim).await {
        Ok(s) => s,
        Err(ClaimError::Busy) => return Ok(LoopOutcome::TurnBusy),
        Err(ClaimError::Subject(m)) => return Ok(LoopOutcome::SubjectRejected(m)),
        Err(ClaimError::Backend(e)) => return Err(e),
    };

    drive_claimed(deps, session, turn_id, to_append, sink).await
}

/// Shared execution/settlement path for ordinary and preclaimed turns.
// Keep the turn state machine off callers' async stacks (including Windows
// test threads and nested permission/resume drivers).
fn drive_claimed<'a, 'd: 'a>(
    deps: &'a LoopDeps<'d>,
    session: PersistedAgentSession,
    turn_id: String,
    to_append: Option<ChatMessage>,
    sink: &'a mut dyn TurnSink,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<LoopOutcome, AgentError>> + 'a>> {
    Box::pin(drive_claimed_inner(deps, session, turn_id, to_append, sink))
}

async fn drive_claimed_inner(
    deps: &LoopDeps<'_>,
    mut session: PersistedAgentSession,
    turn_id: String,
    to_append: Option<ChatMessage>,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let prior_message_ids: HashSet<String> =
        if session.trigger_origin == TriggerOrigin::GoalContinuation {
            session
                .conversation
                .iter()
                .map(|message| message.message_id.clone())
                .collect()
        } else {
            HashSet::new()
        };
    // Keep the lease alive for the (possibly long) turn with the just-claimed
    // token; the guard stops renewal when dropped on every exit path below.
    let _lease_guard = deps
        .heartbeat
        .map(|h| h.start(session.conversation_id.clone(), session.lease_token));

    // Append the user input or server control bridge, when supplied, and persist
    // before the first model call. Completion-only turns use the existing tail.
    let mut newer_user_inputs_after_request = 0_u64;
    if let Some(message) = to_append {
        let message = message.with_turn_id(turn_id.clone());
        if let Some(position) = session
            .conversation
            .iter()
            .position(|existing| existing.message_id == message.message_id)
        {
            newer_user_inputs_after_request = session.conversation[position + 1..]
                .iter()
                .filter(|message| {
                    if message.role != ChatRole::User {
                        return false;
                    }
                    let continuation =
                        crate::permission_resume::is_permission_resume_message(message)
                            || crate::permission_resume::is_scheduled_resume_message(message);
                    !continuation
                })
                .count() as u64;
        }
        if let Some(existing) = session
            .conversation
            .iter_mut()
            .find(|existing| existing.message_id == message.message_id)
        {
            // The runtime can durably append a user follow-up before claiming its
            // model turn. Adopt the turn id without duplicating the message.
            let mut expected = message.clone();
            expected.turn_id = existing.turn_id.clone();
            if *existing != expected {
                return Err(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "durable user message id collision".into(),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                });
            }
            existing.turn_id = Some(turn_id.clone());
        } else {
            session.conversation.push(message);
        }
    }
    if newer_user_inputs_after_request == 0
        && session.trigger_origin == crate::session::TriggerOrigin::User
        && let Some(locale) = deps.response_locale.as_deref().filter(|locale| {
            !locale.is_empty()
                && locale.len() <= 64
                && locale
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        session.response_locale = Some(locale.to_owned());
    }
    if let Err(error) = deps.session_seam.save(&mut session).await {
        return match settle_if_superseded(deps, &session, sink).await? {
            Some(outcome) => Ok(outcome),
            None => Err(error),
        };
    }

    // Run the loop. A concurrently appended user follow-up advances the durable
    // input revision and fences this owner even if cancellation of the provider
    // request is best-effort.
    let result = if newer_user_inputs_after_request > 0 {
        // This request lost the pre-claim race: its durable message is already
        // followed by newer user input. Settle this claim without invoking the
        // model or advancing the handled watermark; the newest request waiter
        // will claim the merged batch next.
        Ok(LoopOutcome::Superseded {
            previous_input_revision: session
                .input_revision
                .saturating_sub(newer_user_inputs_after_request),
            current_input_revision: session.input_revision,
        })
    } else {
        run_inner(deps, &mut session, &turn_id, sink).await
    };
    if matches!(&result, Ok(LoopOutcome::SubAgentYielded)) {
        return result;
    }
    if matches!(&result, Ok(LoopOutcome::DelegationSourcePaused)) {
        // The source-control transaction already released and fenced this child
        // claim. Its old history must not overwrite that committed pause.
        sink.on_turn_discarded();
        return result;
    }
    if let Some(outcome) = settle_if_superseded(deps, &session, sink).await? {
        return Ok(outcome);
    }
    if result.is_err() && deps.content_safety.is_enforced() {
        // The provider/session error is intentionally not copied into a retraction
        // frame. The closed `Incomplete` reason selects local UI text without
        // exposing arbitrary backend detail.
        sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
    }
    let terminal = match &result {
        Err(error)
            if session.trigger_origin == TriggerOrigin::GoalContinuation
                && call_budget::is_admission_rejected(error) =>
        {
            TurnState::Idle
        }
        Ok(LoopOutcome::ProtocolError(_))
        | Ok(LoopOutcome::ContentSafetyUnavailable(_))
        | Err(_) => TurnState::Failed,
        // Policy rejection is a settled Idle turn. Answered / Truncated /
        // CircuitBreak also return to Idle so a follow-up can be claimed.
        _ => TurnState::Idle,
    };
    if matches!(&result, Ok(LoopOutcome::Answered(_))) {
        crate::conversation_attachment::model_read::release_consumed(&mut session.conversation)?;
    }
    let settled_at = (deps.clock)();
    if session.is_subagent_result_turn()
        || session.trigger_origin == TriggerOrigin::SubAgentCompletion
    {
        if matches!(&result, Err(error) if error.kind == AgentErrorKind::ModelUnavailable && error.retryable)
            && session.unclosed_tool_call_ids().is_empty()
            && session.execution_state.states().is_empty()
        {
            if let Some(wait) = session.ready_subagent_wait.as_mut() {
                let now_ms = chrono::DateTime::parse_from_rfc3339(&settled_at)
                    .map_err(|_| crate::subagent::invalid("invalid root interpretation clock"))?
                    .timestamp_millis();
                wait.retry_after_ms = Some(
                    now_ms
                        .checked_add(30_000)
                        .ok_or_else(|| crate::subagent::invalid("root retry clock overflow"))?,
                );
            }
            if let Some(notification) = session.ready_subagent_notification.as_mut() {
                let now_ms = chrono::DateTime::parse_from_rfc3339(&settled_at)
                    .map_err(|_| crate::subagent::invalid("invalid root interpretation clock"))?
                    .timestamp_millis();
                notification.retry_after_ms = Some(
                    now_ms
                        .checked_add(30_000)
                        .ok_or_else(|| crate::subagent::invalid("root retry clock overflow"))?,
                );
            }
            if session
                .ready_subagent_notification
                .as_ref()
                .is_some_and(|notification| notification.accepted_response_message_id.is_some())
            {
                session.ready_subagent_notification = None;
            }
        } else {
            session.ready_subagent_wait = None;
            session.ready_subagent_notification = None;
        }
    }
    if session.agent_role.is_main()
        && session.trigger_origin == TriggerOrigin::ScheduledTask
        && !session.is_subagent_result_turn()
    {
        // A consumed dependency belongs to this segment. A new wait remains in
        // subagent_wait and is acknowledged by the original occurrence coordinator.
        session.ready_subagent_wait = None;
    }

    session.finish_turn(terminal, settled_at.clone());

    session.terminal_permission_request_id = match &result {
        Ok(LoopOutcome::PermissionRequested { request_id }) => Some(request_id.clone()),
        _ => None,
    };
    session.terminal_error = settled_terminal_error(&result);
    if terminal == TurnState::Idle && !matches!(&result, Ok(LoopOutcome::Superseded { .. })) {
        session.handled_input_seq = session.latest_input_seq;
    }
    crate::image_input::strip_session_images(&mut session.conversation);
    crate::visual_evidence::strip_previews(&mut session.visual_evidence);
    // A goal segment has two authorities: the conversation turn and GoalRun.
    // Commit their terminal transition together, including the ledger event.
    // The ordinary save would expose an idle session while the goal is still
    // running after a process crash.
    let save = if session.agent_role.binding().is_some()
        && deps.model.command_completion_event_id().is_none()
        && let Ok(LoopOutcome::Answered(text)) = &result
    {
        let binding = session.agent_role.binding().expect("checked child binding");
        let fence = crate::subagent::state::PlanningFence {
            input_revision: binding.input_revision,
            control_revision: binding.control_revision,
            source_epoch: binding.source_epoch,
        };
        deps.session_seam
            .subagents(&session)
            .ok_or_else(|| crate::subagent::invalid("durable child settlement is unavailable"))?
            .settle_child_answer(&mut session, fence, text.clone())
            .await
            .map(|_| ())
    } else if session.agent_role.binding().is_some() {
        let reason = match &result {
            Err(error)
                if crate::subagent::reservation::CallRejection::from_error(error).is_some() =>
            {
                crate::subagent::reservation::CallRejection::from_error(error)
                    .map(|reason| reason.reason())
            }
            Err(error)
                if error.kind == AgentErrorKind::ModelUnavailable
                    && error.retryable
                    && session.unclosed_tool_call_ids().is_empty()
                    && session.execution_state.states().is_empty() =>
            {
                Some("delegated_model_unavailable")
            }
            Err(_)
            | Ok(LoopOutcome::ProtocolError(_))
            | Ok(LoopOutcome::ContentSafetyUnavailable(_)) => Some("delegated_turn_failed"),
            Ok(LoopOutcome::ContentRejected(_)) => Some("delegated_content_rejected"),
            Ok(LoopOutcome::ContextWindowExceeded) => Some("delegated_context_window_exceeded"),
            Ok(LoopOutcome::PermissionRequested { .. })
            | Ok(LoopOutcome::CircuitBreak(_))
            | Ok(LoopOutcome::Truncated)
            | Ok(LoopOutcome::Answered(_)) => None,
            _ => Some("delegated_turn_unsupported_outcome"),
        };
        let allow_continue = deps.model.command_completion_event_id().is_some()
            && matches!(&result, Ok(LoopOutcome::Answered(_)));
        deps.session_seam
            .subagents(&session)
            .ok_or_else(|| {
                crate::subagent::invalid("durable child turn settlement is unavailable")
            })?
            .settle_child_turn(&mut session, reason, allow_continue)
            .await
    } else if session.trigger_origin == crate::session::TriggerOrigin::GoalContinuation {
        if let Ok(LoopOutcome::GoalControlled { call_id, .. }) = &result {
            record_tool_completion(&mut session, call_id, true);
        }
        let result_fingerprints =
            settled_goal_result_fingerprints(&session, deps.registry, &prior_message_ids);
        let end = match &result {
            Ok(LoopOutcome::GoalYielded) => crate::goal::GoalSegmentEnd::Yield,
            Ok(LoopOutcome::GoalControlled { control, .. }) => {
                crate::goal::GoalSegmentEnd::Control(control.clone())
            }
            Err(error) if call_budget::is_admission_rejected(error) => {
                crate::goal::GoalSegmentEnd::AdmissionRejected(
                    crate::subagent::reservation::CallRejection::from_error(error)
                        .expect("classified call admission"),
                )
            }
            Ok(LoopOutcome::PermissionRequested { request_id }) => {
                crate::goal::GoalSegmentEnd::Control(crate::goal::GoalControl::Wait {
                    reason: crate::goal::GoalWaitReason::Approval,
                    reference_id: request_id.clone(),
                })
            }
            Ok(LoopOutcome::SubAgentsWaiting { wait_id }) => {
                crate::goal::GoalSegmentEnd::Control(crate::goal::GoalControl::Wait {
                    reason: crate::goal::GoalWaitReason::Work,
                    reference_id: wait_id.clone(),
                })
            }
            Ok(LoopOutcome::ContextWindowExceeded) => {
                crate::goal::GoalSegmentEnd::Pause(crate::goal::GoalPauseReason::ContextTooSmall)
            }
            Err(error) if error.kind == AgentErrorKind::AttachmentCapacity => {
                crate::goal::GoalSegmentEnd::Pause(crate::goal::GoalPauseReason::AttachmentCapacity)
            }
            Ok(LoopOutcome::Answered(_)) | Ok(LoopOutcome::CircuitBreak(_)) => {
                crate::goal::GoalSegmentEnd::Pause(crate::goal::GoalPauseReason::NeedsNextStep)
            }
            Err(error)
                if error.kind == AgentErrorKind::ModelUnavailable
                    && session.unclosed_tool_call_ids().is_empty()
                    && !session.execution_state.has_unresolved_outcome() =>
            {
                crate::goal::GoalSegmentEnd::WaitForModel {
                    retry_after_unix_ms: deps.model.retry_after_unix_ms(),
                }
            }
            Err(error)
                if error.kind == AgentErrorKind::ModelRejected
                    && session.unclosed_tool_call_ids().is_empty()
                    && !session.execution_state.has_unresolved_outcome() =>
            {
                crate::goal::GoalSegmentEnd::BlockModel
            }
            _ => crate::goal::GoalSegmentEnd::Pause(crate::goal::GoalPauseReason::Recovery),
        };
        let settled = deps
            .session_seam
            .settle_goal_slice(&mut session, &end, &result_fingerprints, &settled_at)
            .await;
        if settled.is_ok()
            && let Ok(LoopOutcome::GoalControlled { control, call_id }) = &result
        {
            finish_tool(&mut session, call_id, true, sink);
            let message = match control {
                crate::goal::GoalControl::Continue { progress, .. } => progress,
                crate::goal::GoalControl::WaitForWork { message, .. } => message,
                crate::goal::GoalControl::Wait { reference_id, .. } => reference_id,
                crate::goal::GoalControl::Complete { summary, .. } => summary,
                crate::goal::GoalControl::Blocked { reason } => reason,
            };
            sink.on_answer_committed(message);
        }
        settled
    } else {
        deps.session_seam.save(&mut session).await
    };
    if save.is_err()
        && let Some(outcome) = settle_if_superseded(deps, &session, sink).await?
    {
        return Ok(outcome);
    }
    match (result, save) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), _) => Err(e),
    }
}

/// Progress is a new result actually recorded by the server, not the model's
/// `control_goal.message` prose. Ignore volatile receipt metadata for inline
/// JSON so polling the same resource cannot reset the stagnation counter.
fn stable_goal_result_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            fields.retain(|key, _| {
                !matches!(
                    key.as_str(),
                    "observed_at"
                        | "observed_at_unix_ms"
                        | "checked_at"
                        | "checked_at_unix_ms"
                        | "captured_at"
                        | "captured_at_unix_ms"
                        | "fetched_at"
                        | "fetched_at_unix_ms"
                        | "retrieved_at"
                        | "retrieved_at_unix_ms"
                        | "elapsed_ms"
                        | "duration_ms"
                        | "request_id"
                        | "trace_id"
                        | "correlation_id"
                )
            });
            for nested in fields.values_mut() {
                stable_goal_result_json(nested);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items.iter_mut() {
                stable_goal_result_json(nested);
            }
            items.sort_by_key(serde_json::Value::to_string);
        }
        _ => {}
    }
}

fn rejects_required_tool_choice(error: &AgentError) -> bool {
    if error.kind != AgentErrorKind::ModelRejected {
        return false;
    }
    let message = error.message.to_ascii_lowercase();
    (message.contains("tool_choice") || message.contains("tool choice"))
        && [
            "unsupported",
            "not supported",
            "invalid",
            "unknown",
            "required",
            "any",
        ]
        .iter()
        .any(|term| message.contains(term))
}

fn settled_goal_result_fingerprints(
    session: &PersistedAgentSession,
    registry: &[RegisteredTool],
    prior_message_ids: &HashSet<String>,
) -> Vec<String> {
    let mut fingerprints = std::collections::BTreeSet::new();
    for result in &session.conversation {
        if result.role != ChatRole::Tool
            || prior_message_ids.contains(&result.message_id)
            || result.text.trim().is_empty()
            || result.text.starts_with("tool error:")
            || result.text.starts_with("not executed:")
            || result
                .raw_result
                .as_ref()
                .is_some_and(|raw| !raw.restorable)
        {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&result.text)
            && (value.get("ok") == Some(&serde_json::Value::Bool(false))
                || value.get("success") == Some(&serde_json::Value::Bool(false))
                || value.get("result_stored") == Some(&serde_json::Value::Bool(false))
                || value.get("error").is_some_and(|error| !error.is_null()))
        {
            continue;
        }
        let Some(call_id) = result.tool_call_id.as_deref() else {
            continue;
        };
        let Some(call) = session
            .conversation
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|call| call.id == call_id)
        else {
            continue;
        };
        if matches!(call.name.as_str(), "update_task_status" | "control_goal") {
            continue;
        }
        if !registry.iter().any(|tool| {
            tool.name() == call.name
                && matches!(tool.effect, ToolEffect::ReadOnly | ToolEffect::Mutating)
        }) {
            continue;
        }
        let stable_result = if let Some(raw) = &result.raw_result {
            if let Some(mut template) = raw.template.clone() {
                // Large fields have already become null slots. The remaining
                // structured facts can still be compared without attachment
                // IDs, access timestamps, or receipt-order noise.
                stable_goal_result_json(&mut template);
                template.to_string()
            } else {
                // Full externalization has no inline value to normalize. The
                // original digest prevents a fresh attachment ID from making
                // identical content appear new.
                raw.original_sha256.clone()
            }
        } else if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&result.text) {
            stable_goal_result_json(&mut value);
            value.to_string()
        } else {
            result.text.trim().to_owned()
        };
        let arguments = match serde_json::from_str::<serde_json::Value>(&call.arguments_json) {
            Ok(value) => value,
            Err(_) => continue,
        };
        // Stable identity includes the queried resource, so two different
        // resources with the same short result remain distinct facts.
        let identity = format!("{}:{}:{stable_result}", call.name, arguments);
        fingerprints.insert(format!("{:x}", Sha256::digest(identity.as_bytes())));
    }
    fingerprints.into_iter().collect()
}
fn canonical_arguments_json(raw: &str) -> String {
    let value = parsed_tool_arguments(raw);
    serde_json::to_string(&value).expect("JSON values are serializable")
}

fn safety_tool_calls(deps: &LoopDeps<'_>, turn: &crate::chat::ModelTurn) -> Vec<SafetyToolCall> {
    turn.tool_calls
        .iter()
        .map(|call| {
            let effect = deps
                .registry
                .iter()
                .find(|tool| tool.name() == call.name)
                .map(|tool| tool.effect)
                // Unknown tools are never executed; classifying them as mutating is
                // the conservative server-authoritative fallback.
                .unwrap_or(ToolEffect::Mutating);
            SafetyToolCall {
                name: call.name.clone(),
                effect,
                canonical_arguments_json: canonical_arguments_json(&call.arguments_json),
            }
        })
        .collect()
}

async fn review_model_turn(
    deps: &LoopDeps<'_>,
    turn: &crate::chat::ModelTurn,
) -> Result<ContentSafetyDecision, AgentError> {
    match &deps.content_safety {
        ContentSafetyMode::Disabled => Ok(ContentSafetyDecision::Allow),
        ContentSafetyMode::Enforced { seam, context } => seam
            .check_model_turn(SafetyModelTurn {
                surface: context.surface,
                text: crate::reasoning_display::review_text(
                    &turn.text,
                    turn.provider_meta.display_reasoning.as_deref(),
                ),
                tool_calls: safety_tool_calls(deps, turn),
                original_allowed_intent: context.original_allowed_intent.clone(),
            })
            .await
            .map(|verdict| verdict.decision)
            .map_err(|error| normalize_safety_error(&error)),
    }
}

async fn review_context_summary(
    deps: &LoopDeps<'_>,
    text: String,
) -> Result<ContentSafetyDecision, AgentError> {
    match &deps.content_safety {
        ContentSafetyMode::Disabled => Ok(ContentSafetyDecision::Allow),
        ContentSafetyMode::Enforced { seam, context } => seam
            .check_model_turn(SafetyModelTurn {
                surface: context.surface,
                text,
                tool_calls: Vec::new(),
                original_allowed_intent: context.original_allowed_intent.clone(),
            })
            .await
            .map(|verdict| verdict.decision)
            .map_err(|error| normalize_safety_error(&error)),
    }
}

async fn review_image(
    deps: &LoopDeps<'_>,
    image_data_url: &str,
    mime_type: &str,
) -> Result<ContentSafetyDecision, AgentError> {
    match &deps.content_safety {
        ContentSafetyMode::Disabled => Ok(ContentSafetyDecision::Allow),
        ContentSafetyMode::Enforced { seam, context } => seam
            .check_image(SafetyImage {
                surface: context.surface,
                image_data_url: image_data_url.to_string(),
                mime_type: mime_type.to_string(),
                original_allowed_intent: context.original_allowed_intent.clone(),
            })
            .await
            .map(|verdict| verdict.decision)
            .map_err(|error| normalize_safety_error(&error)),
    }
}

#[derive(Debug)]
enum ToolOutputSafetyFailure {
    Rejected(ContentSafetyDecision),
    Unavailable(AgentError),
}

/// Append one tool result only after its optional image has passed the shared
/// image gate. Every tool effect uses this entry point so adding an image to a
/// mutating or wait seam cannot silently bypass manager enforcement later.
// Keep image persistence/review state on the heap for every caller, including
// non-image tools. Otherwise the image branch enlarges the entire loop future.
#[inline(never)]
fn append_reviewed_tool_result<'a, 'd: 'a>(
    deps: &'a LoopDeps<'d>,
    session: &'a mut crate::session::PersistedAgentSession,
    message_id: String,
    call_id: &'a str,
    output: crate::seam::ToolRunOutput,
    data_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<ToolOutputSafetyFailure>, AgentError>> + 'a>,
> {
    Box::pin(async move {
        let mut before = session.clone_boxed();
        let capacity_receipt =
            (session.trigger_origin == TriggerOrigin::GoalContinuation).then(|| {
                (
                    message_id.clone(),
                    data_envelope.clone(),
                    format!("{:x}", Sha256::digest(output.content.as_bytes())),
                )
            });
        let result = append_reviewed_tool_result_inner(
            deps,
            session,
            message_id,
            call_id,
            output,
            data_envelope,
        )
        .await;
        if let Err(error) = &result
            && error.kind == AgentErrorKind::AttachmentCapacity
            && let Some((receipt_id, parent, digest)) = capacity_receipt
        {
            std::mem::swap(session, &mut before);
            let text = serde_json::json!({
                "status": "attachment_capacity",
                "result_stored": false,
                "original_result_sha256": digest,
                "instruction": "The goal is paused. Inspect the original action ledger before considering any retry."
            }).to_string();
            let mut receipt = ChatMessage::tool_result(receipt_id, call_id, text);
            receipt.data_envelope = crate::conversation_attachment::delivery::projection_envelope(
                parent.as_ref(),
                receipt.text.as_bytes(),
                "attachment-capacity",
            )?;
            session.conversation.push(receipt);
        } else if result.is_err() {
            std::mem::swap(session, &mut before);
        }
        result
    })
}

async fn append_reviewed_tool_result_inner(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    message_id: String,
    call_id: &str,
    output: crate::seam::ToolRunOutput,
    data_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
) -> Result<Option<ToolOutputSafetyFailure>, AgentError> {
    let crate::seam::ToolRunOutput {
        format,
        content,
        image_data_url,
        document_preview: _,
    } = output;
    let Some(image_data_url) = image_data_url else {
        // With no new image there is nothing to rotate: any previously retained
        // session image already satisfies the one-image invariant.
        let mut message = ChatMessage::tool_result(message_id, call_id, content);
        message.data_envelope = data_envelope;
        if session.surface == AgentSessionSurface::AiAssistant {
            let policy = deps.model.model_egress_policy()?;
            message = match crate::future::boxed(|| {
                crate::conversation_attachment::delivery::externalize(
                    deps.session_seam,
                    session,
                    message.clone(),
                    format,
                    policy.as_ref(),
                    current_unix_ms(deps.clock),
                )
            })
            .await
            {
                Ok(projected) => projected,
                Err(error) if error.kind == AgentErrorKind::OutputLimitExceeded => {
                    crate::conversation_attachment::delivery::failed_delivery(message, &error)?
                }
                Err(error) => return Err(error),
            };
        }
        session.conversation.push(message);
        return Ok(None);
    };

    let info =
        crate::image_input::validate_image_data_url(&image_data_url).map_err(image_input_error)?;
    // Read the clock before review, but require it only for retained owner evidence.
    let image_received_at_unix_ms = current_unix_ms(deps.clock);
    match review_image(deps, &image_data_url, &info.media_type).await {
        Ok(ContentSafetyDecision::Allow) => {
            let mut message = ChatMessage::tool_result(message_id, call_id, content);
            message.image_data_url = Some(image_data_url);
            message.data_envelope = data_envelope;
            session.conversation.push(message);
            retain_latest_session_image(session)?;
            if session.surface == AgentSessionSurface::AiAssistant {
                let image_data_url = session
                    .conversation
                    .last()
                    .and_then(|message| message.image_data_url.clone())
                    .ok_or_else(|| AgentError {
                        kind: AgentErrorKind::Internal,
                        message: "accepted visual result was not retained for the active turn"
                            .into(),
                        retryable: false,
                        safe_for_model: false,
                        error_code: None,
                    })?;
                let frame = crate::visual_evidence::record_live_observation(
                    session,
                    call_id,
                    &image_data_url,
                    &info,
                    image_received_at_unix_ms?,
                )
                .map_err(|message| AgentError {
                    kind: AgentErrorKind::Internal,
                    message: message.into(),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                })?;
                let (mut attachment, pixels) = crate::conversation_image::ImageAttachment::prepare(
                    session,
                    &frame,
                    deps.model.model_egress_policy()?.as_ref(),
                )?;
                let stored = if session.conversation.last().is_some_and(|message| {
                    message.text.len() > crate::conversation_attachment::INLINE_BYTES
                }) {
                    let authorized_source = attachment.message.data_envelope.clone();
                    let mut image_only = attachment.restore(&pixels)?;
                    image_only.text = "Screenshot; the associated tool body is stored separately in this conversation's attachments.".into();
                    let image_bytes = crate::model_egress::message_content_bytes(&image_only)
                        .map_err(|error| error.agent_error())?;
                    image_only.data_envelope =
                        crate::conversation_attachment::delivery::projection_envelope(
                            authorized_source.as_ref(),
                            &image_bytes,
                            "image-body",
                        )?;
                    image_only.image_data_url = None;
                    attachment.message = image_only;
                    let image_part = attachment.attachment_record(session, &pixels)?;
                    let mut body = session
                        .conversation
                        .last()
                        .cloned()
                        .ok_or_else(invalid_original_result)?;
                    body.image_data_url = None;
                    body.data_envelope =
                        crate::conversation_attachment::delivery::projection_envelope(
                            attachment.message.data_envelope.as_ref(),
                            body.text.as_bytes(),
                            "image-text",
                        )?;
                    let parts = [image_part];
                    let mut delivered = crate::future::boxed(|| {
                        crate::conversation_attachment::delivery::externalize_with(
                            deps.session_seam,
                            session,
                            body,
                            format,
                            None,
                            current_unix_ms(deps.clock),
                            &parts,
                        )
                    })
                    .await?;
                    delivered.image_data_url = Some(image_data_url.clone());
                    *session
                        .conversation
                        .last_mut()
                        .ok_or_else(invalid_original_result)? = delivered;
                    true
                } else {
                    deps.session_seam
                        .store_image(session, &attachment, &pixels)
                        .await?
                };
                if stored {
                    if let Some(stored) = session
                        .visual_evidence
                        .iter_mut()
                        .find(|item| item.evidence_id == frame.evidence_id)
                    {
                        *stored = attachment.frame;
                    }
                    let message = session
                        .conversation
                        .last_mut()
                        .ok_or_else(invalid_original_result)?;
                    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&message.text)
                        && value.is_object()
                    {
                        value["attachment_id"] = serde_json::json!(frame.evidence_id);
                        message.text = value.to_string();
                    } else {
                        message.text.push_str(&format!(
                            "\nScreenshot attachment_id: {}",
                            frame.evidence_id
                        ));
                    }
                    let bytes = crate::model_egress::message_content_bytes(message)
                        .map_err(|error| error.agent_error())?;
                    message.data_envelope =
                        crate::conversation_attachment::delivery::projection_envelope(
                            message.data_envelope.as_ref(),
                            &bytes,
                            "image-reference",
                        )?;
                }
            }
            Ok(None)
        }
        Ok(decision) => {
            session.conversation.push(ChatMessage::tool_result(
                message_id,
                call_id,
                "[image and tool result omitted by content safety policy]",
            ));
            Ok(Some(ToolOutputSafetyFailure::Rejected(decision)))
        }
        Err(error) => {
            session.conversation.push(ChatMessage::tool_result(
                message_id,
                call_id,
                "[image and tool result omitted because content safety review was unavailable]",
            ));
            Ok(Some(ToolOutputSafetyFailure::Unavailable(error)))
        }
    }
}

const fn retraction_reason(decision: ContentSafetyDecision) -> StreamRetractionReason {
    match decision {
        ContentSafetyDecision::Block => StreamRetractionReason::PolicyBlocked,
        ContentSafetyDecision::SafeRedirect => StreamRetractionReason::SafeRedirect,
        ContentSafetyDecision::Allow => StreamRetractionReason::Incomplete,
    }
}

fn append_refusal_placeholder<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    mint: &mut F,
    decision: ContentSafetyDecision,
) {
    if let Some(text) = refusal_placeholder_for(decision) {
        session.conversation.push(ChatMessage::text(
            mint(),
            crate::chat::ChatRole::Assistant,
            text,
        ));
    }
}

async fn finish_tool_output_safety_failure<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call_id: &str,
    remaining_calls: &[crate::chat::ToolCall],
    mint: &mut F,
    sink: &mut dyn TurnSink,
    failure: ToolOutputSafetyFailure,
) -> Result<LoopOutcome, AgentError> {
    let skipped_text = match &failure {
        ToolOutputSafetyFailure::Rejected(_) => {
            "[tool not run because content safety stopped the turn]"
        }
        ToolOutputSafetyFailure::Unavailable(_) => {
            "[tool not run because content safety review was unavailable]"
        }
    };
    for skipped in remaining_calls {
        session
            .conversation
            .push(ChatMessage::tool_result(mint(), &skipped.id, skipped_text));
    }
    if let ToolOutputSafetyFailure::Rejected(decision) = &failure {
        append_refusal_placeholder(session, mint, *decision);
    }
    deps.session_seam.save(session).await?;
    finish_tool(session, call_id, false, sink);

    match failure {
        ToolOutputSafetyFailure::Rejected(decision) => {
            sink.on_turn_retracted(retraction_reason(decision), Some(content_blocked_error()));
            Ok(LoopOutcome::ContentRejected(decision))
        }
        ToolOutputSafetyFailure::Unavailable(error) => {
            sink.on_turn_retracted(
                StreamRetractionReason::SafetyUnavailable,
                Some(error.clone()),
            );
            Ok(LoopOutcome::ContentSafetyUnavailable(error))
        }
    }
}

/// Background publishers persist the immutable original receipt and explicit
/// output format. Delivery is completed under the claimed turn before any model
/// request; storage failure leaves the receipt retryable without redispatch.
async fn deliver_pending_results(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
) -> Result<(), AgentError> {
    if session.surface != AgentSessionSurface::AiAssistant {
        return Ok(());
    }
    let policy = deps.model.model_egress_policy()?;
    let mut changed = false;
    for index in 0..session.conversation.len() {
        let Some(format) = session.conversation[index].pending_delivery_format else {
            continue;
        };
        let mut original = session.conversation[index].clone();
        original.pending_delivery_format = None;
        let result = crate::future::boxed(|| {
            crate::conversation_attachment::delivery::externalize(
                deps.session_seam,
                session,
                original.clone(),
                format,
                policy.as_ref(),
                current_unix_ms(deps.clock),
            )
        })
        .await;
        session.conversation[index] = match result {
            Ok(message) => message,
            Err(error) if error.kind == AgentErrorKind::OutputLimitExceeded => {
                crate::conversation_attachment::delivery::failed_delivery(original, &error)?
            }
            Err(error) => return Err(error),
        };
        changed = true;
    }
    if changed {
        deps.session_seam.save(session).await?;
    }
    Ok(())
}

async fn project_command_completion(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    event_id: &str,
    request: Box<ModelRequest>,
) -> Result<Box<ModelRequest>, AgentError> {
    if session.agent_role.binding().is_some() {
        let seam = deps
            .session_seam
            .subagents(session)
            .ok_or_else(|| crate::subagent::invalid("child runtime seam is unavailable"))?;
        let creation = seam.child_creation_context(session).await?;
        crate::command_completion::project_child_request(*request, session, event_id, &creation)
            .map(Box::new)
    } else {
        crate::command_completion::project_request(*request, session, event_id).map(Box::new)
    }
}

#[inline(never)]
fn assemble_context_request(
    template: &ModelRequest,
    view: &crate::model_context::ModelContextView,
    session: &crate::session::PersistedAgentSession,
) -> Result<Box<ModelRequest>, AgentError> {
    let mut request = clone_context_request(template);
    request.preparation = None;
    let anchors = context_request_anchors(view, session);
    let mut included = request
        .messages
        .iter()
        .chain(&view.messages)
        .map(|message| message.message_id.clone())
        .collect::<HashSet<_>>();
    let mut history = Vec::new();
    for source in session.model_conversation().iter() {
        if anchors.contains(&source.message_id)
            && source.role == ChatRole::User
            && !crate::permission_resume::is_resume_control_message(source)
            && included.insert(source.message_id.clone())
        {
            history.push(source.clone());
        }
    }
    history.extend(view.messages.iter().cloned());
    crate::schedule::review_result::project(&mut history)?;
    let mut at = usize::from(
        request
            .messages
            .first()
            .is_some_and(|message| message.role == ChatRole::System),
    );
    while request.messages.get(at).is_some_and(|message| {
        crate::ai_assistant::settings::is_personalization_projection(message)
            || crate::ai_assistant::current_memory::is_projection(message)
    }) {
        at += 1;
    }
    request.messages.splice(at..at, history);
    Ok(request)
}

#[allow(clippy::too_many_arguments)]
async fn prepare_model_context(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    pinned_context: &mut crate::model_context::PinnedContextPolicy,
    compression_attempted: &mut HashSet<String>,
    template: &ModelRequest,
    authority: Option<&crate::grant_disclosure::GrantDisclosureSnapshot>,
    sink: &mut dyn TurnSink,
) -> Result<crate::model_context::ModelContextView, AgentError> {
    let mut planning_policy = pinned_context.clone();
    let authority_observed_at = authority.map(|_| current_unix_ms(deps.clock)).transpose()?;
    let options = deps.model.context_planning_options();
    let mut last_over_budget: Option<crate::model_preparation::ModelRequestPreparation> = None;
    let mut planning_snapshots = HashSet::new();
    if let Some(event_id) = deps.model.command_completion_event_id() {
        let projected = project_command_completion(
            deps,
            session,
            event_id,
            Box::new(ModelRequest::text_only(
                Vec::new(),
                deps.response_format.clone(),
            )),
        )
        .await?;
        // This is an ephemeral exact-result view, not a replacement history
        // checkpoint. The assembled request below still enforces the byte cap.
        return Ok(crate::model_context::ModelContextView {
            notice_kind: crate::model_context::ContextNoticeKind::Trimmed,
            messages: projected.messages,
            policy_key: pinned_context.key(),
            floor_group_head_message_id: None,
            floor_advanced: false,
        });
    }
    if let Some(base) = deps.model.prepare_request(template)? {
        base.require_fit()?;
        if base.context_bytes == base.maximum_context_bytes
            && deps.model.command_completion_event_id().is_none()
        {
            return Err(AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!(
                    "System instructions, tool definitions and required runtime state already consume the entire {} byte request budget. No room remains for the user requirement or protected tool history; no model request was sent.",
                    base.maximum_context_bytes
                ),
                retryable: false,
                safe_for_model: true,
                error_code: Some(
                    desk_utils::error::DeskErrorCode::AI_CONTEXT_ITEM_TOO_LARGE.code(),
                ),
            });
        }
        planning_policy = planning_policy
            .with_request_overhead_bytes(base.context_bytes)
            .map_err(model_context_error)?;
    }
    if session.repair_recovery_result_labels()? {
        deps.session_seam.save(session).await?;
    }
    loop {
        let snapshot = serde_json::to_string(&(
            planning_policy.key(),
            planning_policy.request_overhead_bytes,
            session
                .model_context_state
                .entries
                .iter()
                .map(|entry| {
                    (
                        &entry.policy_key,
                        &entry.floor_group_head_message_id,
                        &entry.floor_after_group_head_message_id,
                        entry.checkpoint.as_ref().map(|checkpoint| {
                            (
                                checkpoint.v1().generation,
                                &checkpoint.v1().covered_projection_sha256,
                            )
                        }),
                    )
                })
                .collect::<Vec<_>>(),
        ))
        .map_err(|_| crate::subagent::invalid("context planning snapshot cannot be encoded"))?;
        if !planning_snapshots.insert(snapshot) {
            return Err(report_context_compression_failure(
                deps.model,
                crate::seam::ContextCompressionFailureKind::ProtectedStateTooLarge,
                None,
                None,
            )
            .await);
        }
        if planning_policy.strategy
            == crate::model_context::ContextManagementStrategy::CheckpointSummary
            && !lease_is_current(deps).await
        {
            return Err(report_context_compression_failure(
                deps.model,
                crate::seam::ContextCompressionFailureKind::StaleContext,
                None,
                None,
            )
            .await);
        }
        let mut protection = session.context_protection_set();
        if session.trigger_origin == TriggerOrigin::GoalContinuation {
            let goal = deps.session_seam.load_claimed_goal(session).await?;
            if let Some(wait) = goal.work_wait.as_ref().filter(|wait| wait.ready) {
                for id in &wait.ready_event_ids {
                    protection.protect_message(id.clone());
                }
            }
        }
        let compatible_state = crate::model_context::adopt_compatible_checkpoint(
            &session.model_context_state,
            &session.model_conversation(),
            &planning_policy,
            deps.model.model_egress_policy()?.as_ref(),
        )
        .map_err(model_context_error)?;
        let configuration_changed = planning_policy.preserve_history
            && !session.model_context_state.entries.is_empty()
            && !compatible_state
                .entries
                .iter()
                .any(|entry| entry.policy_key == planning_policy.key());
        let eligibility = deps
            .model
            .model_egress_policy()?
            .map(|policy| {
                crate::model_context::reconcile_context_eligibility(
                    &policy,
                    &session.model_conversation(),
                    &compatible_state,
                    &planning_policy,
                    &protection,
                    session.version,
                )
            })
            .transpose();
        let planned = match eligibility {
            Ok(Some(Some(plan))) => {
                Ok(crate::model_context::ContextBuildPlan::NeedsFloorReconciliation(plan))
            }
            Ok(_) => crate::model_context::plan_model_context_with_options(
                &session.model_conversation(),
                &compatible_state,
                &planning_policy,
                &protection,
                session.version,
                options,
            ),
            Err(error) => Err(error),
        };
        let plan = match planned {
            Ok(plan) => plan,
            Err(error)
                if last_over_budget.is_some()
                    && matches!(
                        &error,
                        crate::model_context::ModelContextError::NoCompressiblePrefix
                            | crate::model_context::ModelContextError::ProtectedStateTooLarge { .. }
                            | crate::model_context::ModelContextError::ContextItemTooLarge { .. }
                            | crate::model_context::ModelContextError::InvalidBudget(_)
                    ) =>
            {
                if let Some(prepared) = &last_over_budget {
                    prepared.require_fit()?;
                }
                return Err(model_context_error(error));
            }
            Err(error)
                if planning_policy.strategy
                    == crate::model_context::ContextManagementStrategy::CheckpointSummary
                    && !matches!(
                        error,
                        crate::model_context::ModelContextError::ContextItemTooLarge { .. }
                    ) =>
            {
                let kind = compression_failure_for_context_error(&error);
                return Err(report_context_compression_failure(deps.model, kind, None, None).await);
            }
            Err(error) => return Err(model_context_error(error)),
        };

        if !matches!(
            &plan,
            crate::model_context::ContextBuildPlan::NeedsFloorReconciliation(_)
        ) && let Some(policy) = deps.model.model_egress_policy()?
            && crate::model_context::authorize_context_checkpoint(
                &policy,
                &compatible_state,
                &planning_policy.key(),
                &session.model_conversation(),
            )
            .is_err()
        {
            return Err(report_context_compression_failure(
                deps.model,
                crate::seam::ContextCompressionFailureKind::StaleContext,
                None,
                None,
            )
            .await);
        }

        match plan {
            crate::model_context::ContextBuildPlan::Ready(ready) => {
                let candidate = assemble_context_request(template, &ready.view, session)?;
                if let Some(prepared) = deps.model.prepare_request(&candidate)?
                    && prepared.context_bytes > prepared.maximum_context_bytes
                {
                    let excess = prepared.context_bytes - prepared.maximum_context_bytes;
                    let retained_cost = ready
                        .view
                        .messages
                        .iter()
                        .map(crate::trim::model_context_cost)
                        .sum::<usize>();
                    let next_overhead = planning_policy
                        .request_overhead_bytes
                        .checked_add(excess)
                        .ok_or_else(|| {
                            model_context_error(
                                crate::model_context::ModelContextError::ContextCostOverflow,
                            )
                        })?
                        .max(
                            planning_policy
                                .max_context_bytes
                                .saturating_sub(retained_cost)
                                .saturating_add(1),
                        );
                    if next_overhead >= planning_policy.max_context_bytes {
                        prepared.require_fit()?;
                    }
                    last_over_budget = Some(prepared);
                    planning_policy = planning_policy
                        .clone()
                        .with_request_overhead_bytes(next_overhead)
                        .map_err(model_context_error)?;
                    continue;
                }

                let changed = session.model_context_state != ready.next_state;
                let floor_advanced = ready.view.floor_advanced;
                let previous_state = session.model_context_state.clone();
                let previous_notices = session.context_notices.clone();
                session.model_context_state = ready.next_state;
                if floor_advanced {
                    session.record_context_notice(
                        crate::model_context::ContextNotice::adjusted(
                            turn_id,
                            ready.view.notice_kind,
                        ),
                        (deps.clock)(),
                    );
                }
                if changed || floor_advanced {
                    if planning_policy.strategy
                        == crate::model_context::ContextManagementStrategy::CheckpointSummary
                        && !lease_is_current(deps).await
                    {
                        session.model_context_state = previous_state;
                        session.context_notices = previous_notices;
                        return Err(report_context_compression_failure(
                            deps.model,
                            crate::seam::ContextCompressionFailureKind::StaleContext,
                            None,
                            None,
                        )
                        .await);
                    }
                    if let Err(error) = deps.session_seam.save(session).await {
                        session.model_context_state = previous_state;
                        session.context_notices = previous_notices;
                        return Err(error);
                    }
                }
                if floor_advanced {
                    if ready.view.notice_kind == crate::model_context::ContextNoticeKind::Trimmed {
                        sink.on_context_trimmed(turn_id);
                    } else {
                        sink.on_context_adjusted(turn_id, ready.view.notice_kind);
                    }
                }
                *pinned_context = planning_policy.clone();
                return Ok(ready.view);
            }
            crate::model_context::ContextBuildPlan::NeedsFloorReconciliation(plan) => {
                let previous_state = session.model_context_state.clone();
                let previous_notices = session.context_notices.clone();
                session.model_context_state = match crate::model_context::apply_floor_reconciliation(
                    &plan,
                    &session.model_conversation(),
                    &compatible_state,
                ) {
                    Ok(state) => state,
                    Err(error) => {
                        let kind = compression_failure_for_context_error(&error);
                        return Err(report_context_compression_failure(
                            deps.model, kind, None, None,
                        )
                        .await);
                    }
                };
                let policy_key = planning_policy.key();
                let previous_entry = previous_state
                    .entries
                    .iter()
                    .find(|entry| entry.policy_key == policy_key);
                let next_entry = session
                    .model_context_state
                    .entries
                    .iter()
                    .find(|entry| entry.policy_key == policy_key);
                if previous_entry
                    .zip(next_entry)
                    .is_some_and(|(before, after)| {
                        before.floor_group_head_message_id == after.floor_group_head_message_id
                            && before.floor_after_group_head_message_id
                                == after.floor_after_group_head_message_id
                            && before.checkpoint == after.checkpoint
                    })
                {
                    session.model_context_state = previous_state;
                    return Err(report_context_compression_failure(
                        deps.model,
                        crate::seam::ContextCompressionFailureKind::StaleContext,
                        None,
                        None,
                    )
                    .await);
                }
                session.record_context_notice(
                    crate::model_context::ContextNotice::adjusted(turn_id, plan.notice_kind),
                    (deps.clock)(),
                );
                if !lease_is_current(deps).await {
                    session.model_context_state = previous_state;
                    session.context_notices = previous_notices;
                    return Err(report_context_compression_failure(
                        deps.model,
                        crate::seam::ContextCompressionFailureKind::StaleContext,
                        None,
                        None,
                    )
                    .await);
                }
                if let Err(error) = deps.session_seam.save(session).await {
                    session.model_context_state = previous_state;
                    session.context_notices = previous_notices;
                    let _ = error;
                    return Err(report_context_compression_failure(
                        deps.model,
                        crate::seam::ContextCompressionFailureKind::StaleContext,
                        None,
                        None,
                    )
                    .await);
                }
                sink.on_context_adjusted(turn_id, plan.notice_kind);
                // Rebuild protection and re-plan against the new version/floor.
            }
            crate::model_context::ContextBuildPlan::NeedsCompression(plan) => {
                return crate::future::phase(|| {
                    compression::apply_checkpoint(
                        deps,
                        session,
                        turn_id,
                        pinned_context,
                        compression_attempted,
                        template,
                        authority,
                        sink,
                        &planning_policy,
                        authority_observed_at,
                        configuration_changed,
                        plan,
                        &compatible_state,
                    )
                })
                .await;
            }
        }
    }
}

/// The loop body: model → validate → answer / discard / run read tools → repeat.
async fn lease_is_current(deps: &LoopDeps<'_>) -> bool {
    match deps.heartbeat {
        Some(heartbeat) => heartbeat.check_current().await,
        None => true,
    }
}

async fn ensure_lease_healthy(deps: &LoopDeps<'_>) -> Result<(), AgentError> {
    if !lease_is_current(deps).await {
        return Err(AgentError {
            kind: AgentErrorKind::TransportError,
            message: "turn lease is no longer healthy".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    Ok(())
}

async fn child_source_paused(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
) -> Result<bool, AgentError> {
    if session.agent_role.is_main() {
        return Ok(false);
    }
    let admission = deps
        .session_seam
        .subagents(session)
        .ok_or_else(|| {
            crate::subagent::invalid("child planning requires durable delegation admission")
        })?
        .validate_child_admission(session)
        .await?;
    Ok(admission == crate::subagent::seam::ChildAdmission::SourcePaused)
}

// Keep the growing per-tool state machine on the heap for every entry point,
// including scheduled and permission continuations on small runtime stacks.
#[inline(never)]
fn refresh_reference_sources<'a, 'd: 'a>(
    deps: &'a LoopDeps<'d>,
    session: &'a mut PersistedAgentSession,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<(), AgentError>> + 'a>> {
    Box::pin(refresh_reference_sources_impl(deps, session))
}

async fn refresh_reference_sources_impl(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
) -> Result<(), AgentError> {
    let references_before = session.ui_references.clone();
    let protected_references = session
        .ui_references
        .retained_call_aliases(&session.conversation, &session.permission_requests);
    if session.ui_references.has_attachment_sources() {
        let mut before = None;
        let mut positions = std::collections::BTreeSet::new();
        let mut available = std::collections::BTreeSet::new();
        loop {
            let metadata = deps
                .session_seam
                .list_attachments(session, before.as_deref())
                .await?;
            let Some(last) = metadata.last() else {
                break;
            };
            let next = last.attachment_id.clone();
            if !positions.insert(next.clone()) {
                return Err(crate::conversation_attachment::invalid(
                    "Attachment listing did not advance",
                ));
            }
            available.extend(
                metadata
                    .into_iter()
                    .filter(|part| {
                        part.availability == crate::conversation_attachment::Availability::Available
                    })
                    .map(|part| part.attachment_id),
            );
            before = Some(next);
        }
        session
            .ui_references
            .retain_attachment_sources(&available, &protected_references);
    }
    session
        .ui_references
        .observe_directories(&session.file_scope, &protected_references)?;
    let sources = session
        .conversation
        .iter()
        .map(|message| message.message_id.clone())
        .collect();
    session
        .ui_references
        .retain_sources(&sources, &protected_references);
    if session.ui_references != references_before {
        deps.session_seam.save(session).await?;
    }
    Ok(())
}

#[inline(never)]
fn run_inner<'a, 'd: 'a>(
    deps: &'a LoopDeps<'d>,
    session: &'a mut PersistedAgentSession,
    turn_id: &'a str,
    sink: &'a mut dyn TurnSink,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<LoopOutcome, AgentError>> + 'a>> {
    Box::pin(run_inner_impl(deps, session, turn_id, sink))
}

async fn run_inner_impl(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    crate::conversation_attachment::delivery::resolve_session(deps.session_seam, session).await?;
    crate::conversation_attachment::model_read::validate_pending_reads(deps.session_seam, session)
        .await?;
    refresh_reference_sources(deps, session).await?;
    let mut seq: u32 = 0;
    let mint = move || {
        let id = crate::native_ids::stable(crate::native_ids::Kind::Message, &(turn_id, seq));
        seq += 1;
        id
    };
    let same_tool: HashMap<String, u32> = HashMap::new();
    let compression_attempted = HashSet::new();
    let projection_rebuilds = 0u8;
    use crate::model_observability::protocol_correction::PendingProtocolCorrection;
    let pending_protocol_correction: Option<PendingProtocolCorrection> = None;
    let completion_protocol_retries: u8 = 0;
    let empty_end_turn_retries: u8 = 0;
    let truncated_turn_retries: u8 = 0;
    let permission_protocol_retries: u8 = 0;
    let post_tool_permission_protocol_retries: u8 = 0;
    let invalid_tool_argument_retries: u8 = 0;
    let goal_correction_attempted = false;
    let goal_required_choice_rejected = false;
    let physical_model_attempt = 0u32;
    // A permission decision resumes at the authorization boundary, not at the
    // beginning of the user's workflow. Keep a recency-edge checkpoint in
    // model requests until the model proposes the exact Provider call made
    // available by the decision. Permissioned reads (for example browser
    // snapshots) are included: an approval is never satisfied by prose alone.
    // This prevents weaker OpenAI-compatible models from re-running a read /
    // preview, minting a new ephemeral object reference, and stranding the
    // exact one-shot grant that the owner just approved. Once a mutating call
    // is proposed the normal authorizer remains the sole execution authority;
    // later model calls may inspect again to verify the outcome.
    let permission_continuation_pending =
        session.trigger_origin == crate::session::TriggerOrigin::PermissionDecision;

    let mut state = Box::new(phases::LoopState {
        mint,
        same_tool,
        compression_attempted,
        projection_rebuilds,
        pending_protocol_correction,
        completion_protocol_retries,
        empty_end_turn_retries,
        truncated_turn_retries,
        permission_protocol_retries,
        post_tool_permission_protocol_retries,
        invalid_tool_argument_retries,
        goal_correction_attempted,
        goal_required_choice_rejected,
        physical_model_attempt,
        permission_continuation_pending,
    });
    loop {
        let outcome = phases::run_iteration(deps, session, turn_id, sink, &mut state).await?;
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
    }
}

async fn input_revision_advanced(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
) -> Result<Option<u64>, AgentError> {
    Ok(deps
        .session_seam
        .latest_input_revision(&session.conversation_id)
        .await?
        .filter(|revision| *revision > session.input_revision))
}

/// Cover every save boundary, including initial turn adoption and final settle.
/// A failed CAS alone is not proof of supersession: the runtime must confirm a
/// newer durable input and successfully settle this exact held lease.
async fn settle_if_superseded(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let Some(current_input_revision) = input_revision_advanced(deps, session).await? else {
        return Ok(None);
    };
    sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
    if !deps
        .session_seam
        .settle_superseded(session, &(deps.clock)())
        .await?
    {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "superseded turn could not be fenced and settled".into(),
            retryable: true,
            safe_for_model: false,
            error_code: None,
        });
    }
    Ok(Some(LoopOutcome::Superseded {
        previous_input_revision: session.input_revision,
        current_input_revision,
    }))
}

fn append_superseded_tool_results<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    calls: &[crate::chat::ToolCall],
    parent: Option<&DataEnvelope>,
    mint: &mut F,
) -> Result<(), AgentError> {
    append_unstarted_tool_results(
        session,
        calls,
        parent,
        mint,
        "not executed: superseded by newer user input",
        "supersede_tool_call",
    )
}

fn append_internal_tool_result(
    session: &mut crate::session::PersistedAgentSession,
    parent: Option<&DataEnvelope>,
    message_id: String,
    call_id: &str,
    content: String,
    source_tool_name: &str,
) -> Result<(), AgentError> {
    let mut message = ChatMessage::tool_result(message_id, call_id, content.clone());
    message.data_envelope =
        derive_internal_tool_result_envelope(parent, call_id, &content, source_tool_name)?;
    session.conversation.push(message);
    Ok(())
}

fn append_unstarted_tool_results<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    calls: &[crate::chat::ToolCall],
    parent: Option<&DataEnvelope>,
    mint: &mut F,
    content: &str,
    source_tool_name: &str,
) -> Result<(), AgentError> {
    for call in calls {
        append_internal_tool_result(
            session,
            parent,
            mint(),
            &call.id,
            content.to_string(),
            source_tool_name,
        )?;
    }
    Ok(())
}

fn stable_lineage_id(prefix: &str, value: &impl serde::Serialize) -> String {
    use crate::native_ids::{Kind, stable};
    let kind = match prefix {
        "permission-request" => Kind::PermissionRequest,
        "directory-proposal" => Kind::DirectoryRequest,
        "goal-open-request" => Kind::GoalOpenRequest,
        "status-step" => Kind::StatusStep,
        _ => Kind::Lineage,
    };
    stable(kind, &(prefix, value))
}

fn current_unix_ms(clock: &dyn Fn() -> String) -> Result<u64, AgentError> {
    chrono::DateTime::parse_from_rfc3339(&clock())
        .map_err(|_| AgentError {
            kind: AgentErrorKind::Internal,
            message: "agent clock returned an invalid server timestamp".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })?
        .timestamp_millis()
        .try_into()
        .map_err(|_| AgentError {
            kind: AgentErrorKind::Internal,
            message: "agent clock returned a timestamp outside the supported range".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })
}

pub(crate) fn verified_browser_result(message: &ChatMessage) -> Option<BrowserActionResult> {
    let message = message.trusted_tool_result();
    if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
        return None;
    }
    let source_tool = message
        .data_envelope
        .as_ref()?
        .provenance
        .source_tool_name
        .as_str();
    if !matches!(
        source_tool,
        "browser_open_page"
            | "browser_navigate_page"
            | "browser_take_snapshot"
            | "browser_wait_for"
            | "browser_fill_form"
            | "browser_activate_element"
    ) {
        return None;
    }
    if let Some(result) = verified_completion_browser(&message.trusted_tool_result().text) {
        return Some(result);
    }

    // A foreground provider may return the BrowserActionResult directly. A
    // background completion is appended later as fenced UntrustedOutput and
    // must keep the full ComputerActionCompleted wrapper so arbitrary device
    // bytes can never be promoted into a reusable opaque reference.
    if message.role != ChatRole::Tool {
        return None;
    }
    let result =
        serde_json::from_str::<BrowserActionResult>(&message.trusted_tool_result().text).ok()?;
    result.validate().ok()?;
    Some(result)
}

/// Re-project only typed immutable artifacts that the newest user message names
/// verbatim. This lets a reviewed downstream Provider consume a still-valid
/// artifact after context trimming, a model/profile switch, or process recovery
/// without replaying the historical tool result or exposing a native path.
/// The projection is metadata only and grants no file read, upload, or send
/// authority; the consuming edge must still revalidate the object ref, identity,
/// media type, size and digest under an exact capability grant.
fn requested_artifact_registry_projection(
    conversation: &[ChatMessage],
    message_id: &str,
    now_unix_ms: u64,
) -> Result<Option<ChatMessage>, AgentError> {
    const MAX_REQUESTED_ARTIFACTS: usize = 4;

    let Some(latest_user) = conversation
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::User)
    else {
        return Ok(None);
    };
    let Some(parent) = latest_user.data_envelope.as_ref() else {
        return Ok(None);
    };

    let mut seen_tokens = HashSet::new();
    let mut selected = Vec::new();
    let mut selected_batch = Vec::new();
    let mut completed_text_file_operations = Vec::new();
    let mut source_envelopes = Vec::new();
    for message in conversation.iter().rev() {
        let message = message.trusted_tool_result();
        if selected.len() + selected_batch.len() + completed_text_file_operations.len()
            >= MAX_REQUESTED_ARTIFACTS
        {
            break;
        }
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
            continue;
        }
        let Some(source) = message.data_envelope.as_ref() else {
            continue;
        };
        // This optional index must not revive an expired source or strand a
        // fresh user turn. The owner can request fresh directory metadata and
        // authorize a new read when old model-egress evidence has expired.
        if crate::model_egress::envelope_expires_by(
            source,
            now_unix_ms.saturating_add(crate::model_egress::MODEL_CALL_RETENTION_HEADROOM_MS),
        ) {
            continue;
        }
        let Ok(completion) =
            serde_json::from_str::<ComputerActionCompleted>(&message.trusted_tool_result().text)
        else {
            continue;
        };
        if completion.result
            != desk_agent_protocol::computer_use::ComputerActionResultClass::Verified
        {
            continue;
        }
        if let Some(ComputerActionOutput::TextFileMutation(output)) = &completion.output {
            use desk_agent_protocol::computer_use::TextFileMutationOperation;
            let tool = match output.operation {
                TextFileMutationOperation::Update => "update_text_file",
                TextFileMutationOperation::Delete => "delete_text_file",
            };
            if output.verified
                && !output.original_file_name.is_empty()
                && output.original_file_name.len() <= 512
                && !output.original_file_name.chars().any(char::is_control)
                && source.validate().is_ok()
                && match output.operation {
                    TextFileMutationOperation::Update => {
                        output.updated_file.as_ref().is_some_and(|file| {
                            file.validate().is_ok() && file.file_name == output.original_file_name
                        })
                    }
                    TextFileMutationOperation::Delete => output.updated_file.is_none(),
                }
                && latest_user.text.contains(&output.original_file_name)
                && source.provenance.source_provider_id
                    == crate::ai_assistant::TEXT_FILE_PROVIDER_ID
                && source.provenance.source_tool_name == tool
                && source.digest_sha256
                    == format!("{:x}", sha2::Sha256::digest(message.text.as_bytes()))
                && let Some(call_id) = &message.tool_call_id
                && conversation
                    .iter()
                    .filter(|m| m.role == ChatRole::Assistant)
                    .flat_map(|m| &m.tool_calls)
                    .any(|call| call.id == *call_id && call.name == tool)
            {
                // Durable execution facts are not grant state. In particular,
                // an exhausted one-shot grant must not be read as "not run".
                completed_text_file_operations.push(serde_json::json!({
                    "file_result_call_id": call_id,
                    "file_name": output.original_file_name,
                    "operation": output.operation,
                    "result": "verified",
                }));
                source_envelopes.push(source.clone());
            }
            continue;
        }
        if let Some(ComputerActionOutput::BatchDocumentArtifact(artifact)) = &completion.output {
            if artifact_registry::valid_batch_artifact(artifact, now_unix_ms)
                && latest_user.text.contains(&artifact.file_name)
                && seen_tokens.insert(artifact.file.token.clone())
            {
                selected_batch.push(artifact.clone());
                source_envelopes.push(source.clone());
            }
            continue;
        }
        let Some(ComputerActionOutput::FileArtifact(artifact)) = completion.output else {
            continue;
        };
        if artifact.validate().is_err()
            || !artifact_registry::live_reference(&artifact.file, now_unix_ms)
            || !latest_user.text.contains(&artifact.file_name)
            || !seen_tokens.insert(artifact.file.token.clone())
        {
            continue;
        }
        selected.push(artifact);
        source_envelopes.push(source.clone());
    }
    if selected.is_empty() && selected_batch.is_empty() && completed_text_file_operations.is_empty()
    {
        return Ok(None);
    }
    selected.reverse();
    selected_batch.reverse();

    let payload = serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "kind": "requested_artifact_registry",
        "artifacts": selected,
        "batch_document_artifacts": selected_batch,
        "completed_text_file_operations": completed_text_file_operations,
    }))
    .map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("encode requested artifact registry: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let text = format!(
        "CURRENT REQUESTED ARTIFACT REGISTRY (server authoritative metadata; not a grant): {payload}"
    );
    let mut projection = ChatMessage::system_event(message_id, &text);
    projection.data_envelope = derive_internal_tool_result_envelope(
        Some(parent),
        message_id,
        &text,
        "requested_artifact_registry_projection",
    )?;
    if let Some(envelope) = projection.data_envelope.as_mut() {
        for source in source_envelopes {
            if !envelope
                .provenance
                .source_envelope_ids
                .contains(&source.envelope_id)
            {
                envelope
                    .provenance
                    .source_envelope_ids
                    .push(source.envelope_id);
            }
            envelope.sensitivity = envelope.sensitivity.max(source.sensitivity);
            envelope.retention = envelope.retention.most_restrictive(source.retention);
        }
        envelope.validate().map_err(|error| AgentError {
            kind: AgentErrorKind::Internal,
            message: format!("invalid requested artifact registry envelope: {error}"),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })?;
    }
    Ok(Some(projection))
}

/// Re-project only the bounded opaque references needed to continue a tool
/// chain on every AI Assistant model request. This must also run for an
/// ordinary user follow-up: context compression or a new turn can otherwise
/// retain the requirement while dropping the worker-issued BrowserPageRef.
/// Historical raw Provider results deliberately are not replayed across the
/// egress boundary; the projection keeps only the worker-retained merge
/// preview id, the exact same-run Web Search call reference it must pass to
/// reviewed artifact Providers, and up to two recent validated browser page
/// identities plus at most 32 prioritized closed element refs per origin needed
/// for subsequent semantic browser actions. No rows, snippets, raw DOM, page
/// titles, native paths, arbitrary result text, or authority are copied.
/// Downstream calls must still pass the current grant, selected-object,
/// same-run result, source-pair, page-incarnation and expiry checks.
fn reusable_provider_result_projection(
    conversation: &[ChatMessage],
    message_id: &str,
    now_unix_ms: u64,
    ui_references: Option<&crate::ui_references::UiReferenceState>,
) -> Result<Option<ChatMessage>, AgentError> {
    const MAX_WEB_SOURCES: usize = 8;
    const MAX_BROWSER_RESULTS: usize = 2;
    const MAX_BROWSER_ELEMENTS_PER_RESULT: usize = 32;

    let Some(parent) = conversation
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::User)
        .and_then(|message| message.data_envelope.as_ref())
    else {
        return Ok(None);
    };

    let mut preview = None;
    let mut preview_envelope = None;
    let mut web_search = None;
    let mut web_envelope = None;
    let mut browser_results = Vec::new();
    let mut browser_envelopes = Vec::new();
    let mut seen_browser_pages = HashSet::new();
    for message in conversation.iter().rev() {
        let message = message.trusted_tool_result();
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
            continue;
        }
        let Some(envelope) = message.data_envelope.as_ref() else {
            continue;
        };
        if envelope
            .retention
            .expires_at_unix_ms
            .is_some_and(|expires| expires <= now_unix_ms)
        {
            continue;
        }
        if message.role == ChatRole::Tool
            && preview.is_none()
            && envelope.provenance.source_tool_name == "preview_spreadsheet_merge"
            && let Ok(value) =
                serde_json::from_str::<serde_json::Value>(&message.trusted_tool_result().text)
            && let Some(preview_id) = value
                .pointer("/ReadContext/SpreadsheetMergePreview/preview_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 160)
        {
            preview = Some(serde_json::json!({"preview_id": preview_id}));
            preview_envelope = Some(envelope.clone());
        }
        if message.role == ChatRole::Tool
            && web_search.is_none()
            && envelope.provenance.source_tool_name == "search_public_web"
            && let Some(call_id) = message.tool_call_id.as_deref()
            && conversation.iter().any(|candidate| {
                candidate.role == ChatRole::Assistant
                    && candidate
                        .tool_calls
                        .iter()
                        .any(|call| call.id == call_id && call.name == "search_public_web")
            })
            && let Ok(value) =
                serde_json::from_str::<serde_json::Value>(&message.trusted_tool_result().text)
            && value
                .get("web_search_call_id")
                .and_then(serde_json::Value::as_str)
                == Some(call_id)
            && let Some(results) = value.get("results").and_then(serde_json::Value::as_array)
        {
            let sources = results
                .iter()
                .filter_map(|source| {
                    let title = source.get("title")?.as_str()?;
                    let url = source.get("url")?.as_str()?;
                    (!title.is_empty()
                        && title.chars().count() <= 240
                        && url.starts_with("https://")
                        && url.len() <= 2_048)
                        .then(|| {
                            let pair = serde_json::json!({"title":title,"url":url});
                            let ordinal =
                                crate::result_model_ids::web_source_ordinal(results, &pair);
                            serde_json::json!({"source_id":ordinal,"title":title,"url":url})
                        })
                })
                .take(MAX_WEB_SOURCES)
                .collect::<Vec<_>>();
            if !sources.is_empty() {
                web_search = Some(serde_json::json!({
                    "web_search_result_id": message.message_id,
                    "sources": sources,
                }));
                web_envelope = Some(envelope.clone());
            }
        }
        if browser_results.len() < MAX_BROWSER_RESULTS
            && let Some(result) = verified_browser_result(message)
        {
            // Only the newest page for one adapter origin is useful to a
            // subsequent semantic action. Keeping older incarnations of the
            // same Gmail/Slack surface bloats the model request and makes it
            // easier for the model to copy a stale opaque identity. Two
            // origins are enough for the current cross-application handoff.
            let page_key = format!(
                "{}\0{}\0{}\0{}\0{:?}\0{}\0{}",
                result.page.adapter.device_id,
                result.page.adapter.os_session_id,
                result.page.adapter.profile_incarnation,
                result.page.adapter.connection_revision,
                result.page.origin.kind,
                result.page.origin.host_ascii,
                result.page.origin.port,
            );
            if seen_browser_pages.insert(page_key) {
                let mut elements = result
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.elements.clone())
                    .unwrap_or_default();
                elements.sort_by_key(|element| {
                    let name = element.accessible_name.to_ascii_lowercase();
                    let semantic_priority = if [
                        "to recipients",
                        "subject",
                        "message body",
                        "attach",
                        "file",
                        "compose",
                        "message",
                    ]
                    .iter()
                    .any(|needle| name.contains(needle))
                    {
                        0
                    } else {
                        1
                    };
                    let role_priority = match element.role {
                        BrowserElementRole::Textbox | BrowserElementRole::Combobox => 0,
                        BrowserElementRole::Button
                        | BrowserElementRole::Checkbox
                        | BrowserElementRole::Option => 1,
                        BrowserElementRole::Dialog
                        | BrowserElementRole::Tab
                        | BrowserElementRole::Link => 2,
                        BrowserElementRole::Generic => 3,
                    };
                    (semantic_priority, role_priority)
                });
                elements.truncate(MAX_BROWSER_ELEMENTS_PER_RESULT);
                let mut references = serde_json::json!({
                    "page": crate::browser_model_ids::page_projection(&result.page),
                    "elements": elements.iter().map(crate::browser_model_ids::element_projection).collect::<Vec<_>>(),
                });
                if let Some(aliases) = ui_references {
                    aliases.shorten_value(&mut references, Some(&message.message_id));
                }
                browser_results.push(references);
                browser_envelopes.push(envelope.clone());
            }
        }
        if preview.is_some() && web_search.is_some() && browser_results.len() == MAX_BROWSER_RESULTS
        {
            break;
        }
    }
    if preview.is_none() && web_search.is_none() && browser_results.is_empty() {
        return Ok(None);
    }

    let browser_page_reference_present = !browser_results.is_empty();
    let payload = serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "kind": "reusable_provider_result_registry",
        "spreadsheet_merge_preview": preview,
        "web_search": web_search,
        "browser_results": browser_results,
        "browser_reference_delta": {
            "page_reference_prerequisite_present": browser_page_reference_present,
            "supersedes_turn_start_catalog_for_page_reference_only": browser_page_reference_present,
            "exact_input_may_copy_page_for": [
                "browser_navigate_page",
                "browser_take_snapshot",
                "browser_wait_for"
            ]
        },
    }))
    .map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("encode reusable Provider result registry: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let text = format!(
        "CURRENT REUSABLE PROVIDER RESULTS (server authoritative bounded references; not a grant; copy observed browser page_id and element_id verbatim and never invent them): {payload}. This registry is emitted after the turn-start capability catalog. When browser_reference_delta.page_reference_prerequisite_present=true, it supersedes only an older catalog claim that the page-reference prerequisite is absent. If request_permissions is available and the named downstream browser tool is a registered permission candidate, request its exact permission using the observed IDs and complete remaining input now; do not claim that no browser page exists. Runtime readiness, the tool registry, grants, and final server validation remain authoritative and are not widened by this delta."
    );
    let mut projection = ChatMessage::system_event(message_id, &text);
    projection.data_envelope = derive_internal_tool_result_envelope(
        Some(parent),
        message_id,
        &text,
        "reusable_provider_result_projection",
    )?;
    if let Some(projected) = projection.data_envelope.as_mut() {
        for source in [preview_envelope, web_envelope]
            .into_iter()
            .flatten()
            .chain(browser_envelopes)
        {
            if !projected
                .provenance
                .source_envelope_ids
                .contains(&source.envelope_id)
            {
                projected
                    .provenance
                    .source_envelope_ids
                    .push(source.envelope_id);
            }
            projected.sensitivity = projected.sensitivity.max(source.sensitivity);
            projected.retention = projected.retention.most_restrictive(source.retention);
        }
        projected.validate().map_err(|error| AgentError {
            kind: AgentErrorKind::Internal,
            message: format!("invalid reusable Provider result envelope: {error}"),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })?;
    }
    Ok(Some(projection))
}

/// Browser page/element references are edge-minted evidence, not model-owned
/// identifiers. Before a permission request is ever shown to the owner, require
/// every exact browser reference to be a byte-for-byte semantic match for one
/// unexpired, verified Browser result already persisted in this run. This makes
/// changing only an adapter engine, page incarnation, URL digest, role, or
/// accessible name fail before approval instead of producing an unusable grant.
fn validate_browser_permission_references(
    conversation: &[ChatMessage],
    request: &crate::dynamic_run::PermissionRequest,
) -> Result<(), AgentError> {
    fn collect_elements(value: &serde_json::Value, elements: &mut Vec<BrowserElementRef>) {
        if let Ok(element) = serde_json::from_value::<BrowserElementRef>(value.clone()) {
            elements.push(element);
            return;
        }
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    collect_elements(value, elements);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    collect_elements(value, elements);
                }
            }
            _ => {}
        }
    }

    let now_unix_ms = chrono::DateTime::parse_from_rfc3339(&request.created_at)
        .map_err(|_| AgentError {
            kind: AgentErrorKind::Internal,
            message: "permission request has an invalid server timestamp".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })?
        .timestamp_millis()
        .try_into()
        .map_err(|_| AgentError {
            kind: AgentErrorKind::Internal,
            message: "permission request timestamp is outside the supported range".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })?;

    for item in &request.items {
        if !matches!(
            item.tool_name.as_str(),
            "browser_navigate_page"
                | "browser_take_snapshot"
                | "browser_wait_for"
                | "browser_fill_form"
                | "browser_activate_element"
                | "prepare_gmail_draft"
                | "prepare_slack_message"
                | "send_gmail_message"
                | "send_slack_message"
        ) {
            continue;
        }
        let canonical = item
            .canonical_input_json
            .as_deref()
            .ok_or_else(|| AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!(
                    "invalid request_permissions arguments: tool `{}` requires exact browser input",
                    item.tool_name
                ),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })?;
        let value: serde_json::Value = serde_json::from_str(canonical).map_err(|_| AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!(
                "invalid request_permissions arguments: tool `{}` has invalid exact browser input",
                item.tool_name
            ),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
        let page: BrowserPageRef = serde_json::from_value(
            value
                .get("page")
                .cloned()
                .ok_or_else(|| AgentError {
                    kind: AgentErrorKind::InvalidInput,
                    message: format!(
                        "invalid request_permissions arguments: tool `{}` is missing its exact page reference",
                        item.tool_name
                    ),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                })?,
        )
        .map_err(|_| AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!(
                "invalid request_permissions arguments: tool `{}` has an invalid exact page reference",
                item.tool_name
            ),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
        let mut elements = Vec::new();
        for (key, child) in value.as_object().into_iter().flatten() {
            if key != "page" {
                collect_elements(child, &mut elements);
            }
        }
        let grounded = conversation.iter().rev().any(|message| {
            if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
                || message.data_envelope.as_ref().is_none_or(|envelope| {
                    envelope
                        .retention
                        .expires_at_unix_ms
                        .is_some_and(|expires| expires <= now_unix_ms)
                })
            {
                return false;
            }
            let Some(result) = verified_browser_result(message) else {
                return false;
            };
            if result.validate().is_err() || result.page != page {
                return false;
            }
            elements.is_empty()
                || result.snapshot.as_ref().is_some_and(|snapshot| {
                    elements
                        .iter()
                        .all(|element| snapshot.elements.contains(element))
                })
        });
        if !grounded {
            return Err(AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!(
                    "invalid request_permissions arguments: tool `{}` must copy its exact page and element references from one unexpired verified browser result in this run",
                    item.tool_name
                ),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
    }
    Ok(())
}

/// Validate a permission request against the turn-start callable and explicit
/// permission-candidate sets while
/// allowing one narrow same-turn context transition: a verified browser result
/// can supply the exact fresh page prerequisite for navigate/snapshot/wait after
/// the initial catalog was frozen. The browser-reference validator runs first,
/// so merely naming one of these tools never expands the candidate set. Runtime
/// readiness and the compiled Provider registry are still checked normally.
fn validate_permission_request_availability(
    role: &crate::subagent::AgentRole,
    conversation: &[ChatMessage],
    request: &crate::dynamic_run::PermissionRequest,
    providers: &crate::provider_registry::ProviderRegistry,
    inventory: &[crate::capability_availability::CapabilityAvailability],
    turn_start_callable_tools: &[RegisteredTool],
    permission_candidates: &[RegisteredTool],
) -> Result<(), AgentError> {
    validate_browser_permission_references(conversation, request)?;
    let mut effective_callable_tools = turn_start_callable_tools.to_vec();
    // Candidates are context-grounded by the orchestrator, but deliberately
    // absent from the executable registry until the owner grants permission.
    // Merge them only for request validation, never for tool dispatch.
    for candidate in permission_candidates {
        if !effective_callable_tools
            .iter()
            .any(|tool| tool.name() == candidate.name())
        {
            effective_callable_tools.push(candidate.clone());
        }
    }
    for item in &request.items {
        if !matches!(
            item.tool_name.as_str(),
            "browser_navigate_page" | "browser_take_snapshot" | "browser_wait_for"
        ) || effective_callable_tools
            .iter()
            .any(|tool| tool.name() == item.tool_name)
        {
            continue;
        }
        if let Some(capability) = providers.capability_for_tool(&item.tool_name) {
            effective_callable_tools.push(capability.registered_tool());
        }
    }
    effective_callable_tools.retain(|tool| role.allows_effect(tool.effect));
    crate::permission_tools::validate_request_availability(
        request,
        inventory,
        &effective_callable_tools,
    )
}

/// Emit a tool's terminal UI event from the authoritative result that was just
/// appended to the conversation. Tool output reaches the UI through the same
/// redacted, bounded path used for model context.
fn stage_subagent_observations(
    session: &mut PersistedAgentSession,
    request_message_ids: &HashSet<String>,
    response_message_id: &str,
    interpreted: bool,
) -> Result<(), AgentError> {
    if !session.agent_role.is_main() {
        return Ok(());
    }
    if let Some(notification) = session.ready_subagent_notification.as_mut()
        && notification.parent_input_revision == session.input_revision
        && notification.parent_control_revision == session.control_revision
        && notification.accepted_response_message_id.is_none()
        && request_message_ids.contains(&notification.message_id)
    {
        notification.accepted_response_message_id = Some(response_message_id.into());
    }
    for result in &session.observed_subagent_results {
        if result.parent_input_revision != session.input_revision
            || result.parent_control_revision != session.control_revision
            || !request_message_ids.contains(&result.result_message_id)
        {
            continue;
        }
        let accepted = crate::subagent::seam::AcceptedResultObservation {
            result: result.clone(),
            response_message_id: response_message_id.into(),
        };
        if !session
            .accepted_subagent_observations
            .iter()
            .any(|previous| previous.result == *result)
        {
            if session.accepted_subagent_observations.len() >= 64 {
                return Err(crate::subagent::invalid("too many subagent observations"));
            }
            session
                .accepted_subagent_observations
                .push(accepted.clone());
        }
        if interpreted
            && !session
                .interpreted_subagent_results
                .iter()
                .any(|previous| previous.result == *result)
        {
            if session.interpreted_subagent_results.len() >= 64 {
                return Err(crate::subagent::invalid(
                    "too many subagent interpretations",
                ));
            }
            session.interpreted_subagent_results.push(accepted);
        }
    }
    Ok(())
}

fn finish_tool(
    session: &mut crate::session::PersistedAgentSession,
    call_id: &str,
    ok: bool,
    sink: &mut dyn TurnSink,
) {
    record_tool_completion(session, call_id, ok);
    let result = session
        .conversation
        .iter()
        .rev()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id));
    let output = result
        .map(|message| message.text.as_str())
        .unwrap_or_default();
    let background_task_id = result.and_then(|message| message.background_task_id.as_deref());
    if let Some(evidence) = session
        .visual_evidence
        .iter()
        .rev()
        .find(|evidence| evidence.tool_call_id == call_id)
    {
        sink.on_visual_evidence(evidence);
    }
    sink.on_tool_finished(call_id, ok, output, background_task_id);
}

fn record_tool_completion(
    session: &mut crate::session::PersistedAgentSession,
    call_id: &str,
    ok: bool,
) {
    if let Some(message) = session
        .conversation
        .iter_mut()
        .rev()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id))
    {
        message.tool_ok = Some(ok);
    }
}

fn invalid_original_result() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "the tool result does not match its original data envelope".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

fn interactive_user_home_projection(
    home: Option<&str>,
    worker_incarnation: Option<&str>,
    current_runtime_envelope: Option<&DataEnvelope>,
    turn_id: &str,
    now_unix_ms: u64,
) -> Result<Option<ChatMessage>, AgentError> {
    let text = crate::ai_assistant::interactive_user_home_prompt(home);
    let Some(worker_incarnation) = worker_incarnation.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }
    let allowed_destinations = current_runtime_envelope
        .into_iter()
        .flat_map(|envelope| &envelope.allowed_destinations)
        .filter(|destination| matches!(destination, DestinationIdentity::Model { .. }))
        .cloned()
        .collect::<Vec<_>>();
    if allowed_destinations.len() != 1 {
        return Err(invalid_original_result());
    }
    let text = text.trim().to_string();
    let expires_at_unix_ms = now_unix_ms.saturating_add(5 * 60 * 1_000);
    let message_id = crate::native_ids::stable(
        crate::native_ids::Kind::Message,
        &("runtime-interactive-home", turn_id),
    );
    let digest_sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
    let mut message = ChatMessage::system_event(&message_id, &text);
    message.data_envelope = Some(DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("env-{message_id}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("interactive-home-snapshot-env-{message_id}"),
            sha256: digest_sha256.clone(),
            size_bytes: text.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "session-worker-runtime".into(),
            source_tool_name: "interactive-user-home".into(),
            source_object_id: Some(worker_incarnation.to_string()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256,
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations,
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(expires_at_unix_ms),
            delete_with_run: true,
        },
    });
    message
        .data_envelope
        .as_ref()
        .expect("home projection envelope")
        .validate()
        .map_err(|_| invalid_original_result())?;
    Ok(Some(message))
}

fn original_action_anchor(
    session: &crate::session::PersistedAgentSession,
    action: &crate::session::ActionIdentity,
    call_id: &str,
) -> Result<usize, AgentError> {
    let mut matches = session
        .conversation
        .iter()
        .enumerate()
        .filter(|(_, message)| message.tool_call_id.as_deref() == Some(call_id));
    let (index, message) = matches.next().ok_or_else(invalid_original_result)?;
    if matches.next().is_some()
        || !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput)
        || message
            .background_task_id
            .as_ref()
            .is_some_and(|id| id != &action.action_request_id)
        || !session.execution_state.contains(action)
        || matches!(session.execution_state.execution(&action.execution_id).as_ref(), Some(ExecutionState::OutcomeUnknown { placeholder_message_id, .. })
            if *placeholder_message_id != message.message_id)
    {
        return Err(invalid_original_result());
    }
    Ok(index)
}

fn append_mutating_result(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    mut message: ChatMessage,
) -> Result<(), AgentError> {
    let output = crate::seam::ToolRunOutput {
        format: crate::seam::ToolOutputFormat::Text,
        content: message.text.clone(),
        image_data_url: message.image_data_url.clone(),
        document_preview: None,
    };
    // Control outcomes are not native Provider results. Inherit the original
    // proposal's boundary without inventing a completion receipt or asking a
    // strict Provider to attest to centrally generated text.
    let parent = session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.tool_calls.iter().any(|candidate| {
                    candidate.id == call.id
                        && candidate.name == call.name
                        && crate::ui_model_ids::same_call_input_with_references(
                            &call.name,
                            &candidate.arguments_json,
                            &call.arguments_json,
                            Some(&session.ui_references),
                            &crate::result_model_ids::locators(&session.conversation),
                        )
                })
        })
        .and_then(|message| message.data_envelope.as_ref());
    message.data_envelope = if parent.is_some() {
        crate::model_message_labels::internal_tool_result_envelope(
            parent,
            &call.id,
            &output.content,
            "provider_execution_status",
        )?
    } else {
        deps.tools.mutating_data_envelope(call, &output)?
    };
    session.conversation.push(message);
    Ok(())
}

/// Bind a Provider result to authoritative inputs already present in the
/// durable run. The model supplies no envelope ids and cannot invent them:
/// artifact/preview identities are resolved from prior typed results, selected
/// objects resolve to active context attachments, and the current requirement
/// resolves to the latest user envelope. Unmatched identities add no claim.
pub(crate) fn bind_tool_input_envelopes(
    session: &crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    envelope: &mut Option<DataEnvelope>,
) -> Result<(), AgentError> {
    let Some(envelope) = envelope.as_mut() else {
        return Ok(());
    };
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.arguments_json) else {
        return Ok(());
    };
    fn collect_identity_values(
        value: &serde_json::Value,
        artifact_ids: &mut HashSet<String>,
        preview_ids: &mut HashSet<String>,
    ) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(id) = object
                    .get("artifact_id")
                    .and_then(serde_json::Value::as_str)
                {
                    artifact_ids.insert(id.to_string());
                }
                if let Some(id) = object.get("preview_id").and_then(serde_json::Value::as_str) {
                    preview_ids.insert(id.to_string());
                }
                for value in object.values() {
                    collect_identity_values(value, artifact_ids, preview_ids);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    collect_identity_values(value, artifact_ids, preview_ids);
                }
            }
            _ => {}
        }
    }
    let mut artifact_ids = HashSet::new();
    let mut preview_ids = HashSet::new();
    collect_identity_values(&arguments, &mut artifact_ids, &mut preview_ids);
    let mut source_ids = envelope.provenance.source_envelope_ids.clone();
    if session.surface == AgentSessionSurface::AiAssistant {
        source_ids.extend(crate::result_model_ids::input_envelope_ids(
            &call.name,
            &arguments,
            &session.conversation,
        )?);
    }
    let has_explicit_input_lineage = !source_ids.is_empty();

    if crate::provider_preflight::text_file::TextMutationPreflight::supports(&call.name)
        || crate::provider_preflight::text_file::read_result_id(call)?.is_some()
    {
        let id = arguments
            .get("file_result_call_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(crate::directory_tools::unavailable)?;
        // Freeze authenticated source identity here. Current expiry and consent
        // are independently revalidated by preflight and dispatch.
        let source_id = if call.name == "read_text_file" {
            crate::provider_preflight::text_file::read_source_envelope_id(session, call)?
        } else {
            crate::provider_preflight::text_file::resolve_file_result(session, id, 1)?
                .source_envelope_id()
                .ok_or_else(crate::directory_tools::unavailable)?
                .to_string()
        };
        source_ids.push(source_id);
    }

    if crate::provider_preflight::text_file::selection::supports(&call.name) {
        for selector in crate::provider_preflight::text_file::selection::selectors(call)? {
            source_ids.push(
                crate::provider_preflight::text_file::read_source_envelope_id(session, &selector)?,
            );
        }
    }

    // A Word report may opt into an exact subset of one prior Web Search
    // result. The lookup below has already been enforced before dispatch; bind
    // the server-owned result envelope directly so the artifact lineage does
    // not rely only on the model-turn transitive edge.
    if let Ok(Some(web_source)) = resolve_word_report_web_source_envelope(session, call) {
        source_ids.push(web_source.envelope_id.clone());
    }

    // A tool call is an output of the immediately persisted assistant model
    // turn. That response envelope is the authoritative transitive link to
    // every user/tool/Web envelope projected into that model request. Bind it
    // by the server-owned tool-call id; the model never supplies an envelope id.
    if let Some(model_turn) = session
        .conversation
        .iter()
        .rev()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|tool_call| tool_call.id == call.id)
        })
        .and_then(|message| message.data_envelope.as_ref())
    {
        source_ids.push(model_turn.envelope_id.clone());
    }

    // Every Provider call in this run reacts to the most recent durable owner
    // requirement (including a permission-resume projection derived from it).
    if let Some(user) = session
        .conversation
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::User)
        .and_then(|message| message.data_envelope.as_ref())
    {
        source_ids.push(user.envelope_id.clone());
    }

    // An explicit input lineage comes from the trusted runtime's exact object
    // selection. Never expand it with unrelated, later session attachments.
    // Unlabeled legacy calls retain their conservative context fallback.
    if !has_explicit_input_lineage
        && matches!(
            call.name.as_str(),
            "read_terminal_output"
                | "inspect_files"
                | "read_text_file"
                | "create_text_file"
                | "inspect_spreadsheets"
                | "preview_spreadsheet_merge"
                | "inspect_numbers_file"
                | "patch_numbers_copy"
                | "inspect_pages_file"
                | "replace_pages_copy_body"
                | "inspect_keynote_file"
                | "patch_keynote_copy"
                | "inspect_excel_cell"
                | "patch_excel_copy"
                | "inspect_word_file"
                | "replace_word_copy_body"
                | "inspect_powerpoint_file"
                | "patch_powerpoint_copy"
        )
    {
        source_ids.extend(
            session
                .context_attachments
                .iter()
                .filter(|attachment| {
                    matches!(
                        attachment.state,
                        crate::context_attachment::AttachmentState::Active
                    )
                })
                .map(|attachment| attachment.envelope.envelope_id.clone()),
        );
    }

    source_ids.extend(session.conversation.iter().filter_map(|message| {
        let message = message.trusted_tool_result();
        let source = message.data_envelope.as_ref()?;
        let direct_artifact_id = match &source.content {
            ContentRef::Artifact { artifact_id, .. } => Some(artifact_id.as_str()),
            _ => None,
        };
        let typed_result_artifact_id = serde_json::from_str::<
            desk_agent_protocol::computer_use::ComputerActionCompleted,
        >(&message.text)
        .ok()
        .and_then(|completion| match completion.output {
            Some(desk_agent_protocol::computer_use::ComputerActionOutput::FileArtifact(
                artifact,
            )) if artifact.validate().is_ok() => Some(artifact.file.token),
            _ => None,
        });
        let artifact_match = direct_artifact_id
            .map(str::to_owned)
            .or(typed_result_artifact_id)
            .is_some_and(|artifact_id| artifact_ids.contains(&artifact_id));
        let preview_match =
            serde_json::from_str::<serde_json::Value>(&message.trusted_tool_result().text)
                .ok()
                .is_some_and(|value| {
                    let mut ignored_artifacts = HashSet::new();
                    let mut found_previews = HashSet::new();
                    collect_identity_values(&value, &mut ignored_artifacts, &mut found_previews);
                    found_previews
                        .iter()
                        .any(|preview_id| preview_ids.contains(preview_id))
                });
        (artifact_match || preview_match).then(|| source.envelope_id.clone())
    }));
    source_ids.sort();
    source_ids.dedup();
    envelope.provenance.source_envelope_ids = source_ids;
    envelope.validate().map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("invalid Provider input lineage: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })
}

/// Match optional Word report source entries against one exact Web Search tool
/// result already persisted in the same conversation. The model supplies a
/// server-owned tool call id plus title/URL pairs, but never envelope ids. Both
/// fields may be omitted for a report without Web sources.
fn resolve_word_report_web_source_envelope<'a>(
    session: &'a crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
) -> Result<Option<&'a DataEnvelope>, String> {
    if call.name != "create_word_report" {
        return Ok(None);
    }
    let arguments: serde_json::Value = serde_json::from_str(&call.arguments_json)
        .map_err(|_| "Word report input is not valid JSON".to_string())?;
    let call_id = arguments
        .get("web_search_call_id")
        .and_then(serde_json::Value::as_str);
    let sources = arguments
        .get("web_sources")
        .and_then(serde_json::Value::as_array);
    match (call_id, sources) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {}
        _ => {
            return Err(
                "Word report Web sources require both web_search_call_id and web_sources".into(),
            );
        }
    }
    let call_id = call_id.expect("paired above");
    let sources = sources.expect("paired above");
    if call_id.is_empty() || call_id.len() > 128 || !(1..=8).contains(&sources.len()) {
        return Err(
            "Word report Web sources require one bounded prior Web Search call and 1 to 8 entries"
                .into(),
        );
    }
    let search_call_exists = session.conversation.iter().any(|message| {
        message.role == ChatRole::Assistant
            && message
                .tool_calls
                .iter()
                .any(|tool_call| tool_call.id == call_id && tool_call.name == "search_public_web")
    });
    if !search_call_exists {
        return Err("Word report Web Search call is not part of this durable run".into());
    }
    let result = session
        .conversation
        .iter()
        .find(|message| {
            message.role == ChatRole::Tool && message.tool_call_id.as_deref() == Some(call_id)
        })
        .ok_or_else(|| "Word report Web Search result is not available yet".to_string())?;
    let result = result.trusted_tool_result();
    let result_envelope = result
        .data_envelope
        .as_ref()
        .filter(|envelope| envelope.provenance.source_tool_name == "search_public_web")
        .ok_or_else(|| "Word report Web Search result has no authoritative envelope".to_string())?;
    let result_json: serde_json::Value = serde_json::from_str(&result.trusted_tool_result().text)
        .map_err(|_| {
        "Word report Web Search result is not valid structured data".to_string()
    })?;
    let observed = result_json
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Word report Web Search result has no bounded result list".to_string())?;
    let mut requested = HashSet::new();
    for source in sources {
        let object = source
            .as_object()
            .filter(|object| object.len() == 2)
            .ok_or_else(|| "Word report Web source must contain only title and url".to_string())?;
        let title = object
            .get("title")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Word report Web source title is invalid".to_string())?;
        let url = object
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Word report Web source URL is invalid".to_string())?;
        if title.is_empty()
            || title.chars().count() > 240
            || url.len() > 2_048
            || !url.starts_with("https://")
            || !requested.insert((title, url))
            || !observed.iter().any(|candidate| {
                candidate.get("title").and_then(serde_json::Value::as_str) == Some(title)
                    && candidate.get("url").and_then(serde_json::Value::as_str) == Some(url)
            })
        {
            return Err(
                "Word report Web source was not copied exactly from the referenced search result"
                    .into(),
            );
        }
    }
    Ok(Some(result_envelope))
}

/// Run one validated mutating tool call: approval + execution via the seam, then
/// translate its terminal [`ExecOutcome`] into the conversation + execution state.
///
/// Before the (possibly long) approval wait the turn is persisted as
/// [`TurnState::AwaitingApproval`] so other instances / the UI can observe it; it
/// is restored to `Running` afterward for any read-only follow-up. On a non-success
/// outcome `halted` is set so the rest of the turn's calls are not executed.
#[allow(clippy::too_many_arguments)]
async fn run_mutating<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    call: &crate::chat::ToolCall,
    remaining_calls: &[crate::chat::ToolCall],
    mint: &mut F,
    halted: &mut Option<String>,
    sink: &mut dyn TurnSink,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<Option<LoopOutcome>, AgentError> {
    // Repeated reviewer refusals of one tool need the owner, not another attempt.
    // Other tools stay available, so the assistant may pursue a different method.
    if session.review_denials.get(&call.name).copied().unwrap_or(0) >= MAX_REVIEW_DENIALS_PER_TOOL {
        observation.permission(crate::model_observability::PermissionOutcome::PolicyRejected);
        let note = format!(
            "not executed: independent AI review already refused {} {MAX_REVIEW_DENIALS_PER_TOOL} times for this request. Do not call it again; tell the owner what was refused and ask how to proceed, or use a different method.",
            call.name
        );
        append_mutating_result(
            deps,
            session,
            call,
            ChatMessage::tool_result(mint(), &call.id, note),
        )?;
        finish_tool(session, &call.id, false, sink);
        *halted = Some("not executed: a prior command in this turn was not run".to_string());
        return Ok(None);
    }
    // Defence in depth behind the exposure gate: a mutating tool is never
    // advertised to an automation turn ([`lookup_exposed`] would already reject
    // it), but should one still reach here it is refused before any work is
    // created, so a completion can never self-trigger a new command.
    if !session.allows_new_mutation() {
        observation.permission(crate::model_observability::PermissionOutcome::PolicyRejected);
        append_mutating_result(
            deps,
            session,
            call,
            ChatMessage::tool_result(
                mint(),
                &call.id,
                "not executed: an automation turn cannot start a new command",
            ),
        )?;
        finish_tool(session, &call.id, false, sink);
        *halted = Some("not executed: an automation turn cannot start a new command".to_string());
        return Ok(None);
    }

    let selected = if call.name == crate::command_confirmation::COMMAND_TOOL
        && crate::request_devices::selected_command_turn(session)
    {
        match crate::future::boxed(|| {
            crate::request_devices::resolve_approved_command(deps.tools, session, call)
        })
        .await
        {
            Ok(policy) => Some(policy),
            Err(error) => {
                observation
                    .permission(crate::model_observability::PermissionOutcome::PolicyRejected);
                let text = crate::model_input::describe_error(&call.name, &error.message);
                append_mutating_result(
                    deps,
                    session,
                    call,
                    ChatMessage::tool_result(mint(), &call.id, text),
                )?;
                finish_tool(session, &call.id, false, sink);
                *halted = Some("not executed: the approved command target or input changed".into());
                return Ok(None);
            }
        }
    } else {
        None
    };
    let device = selected
        .as_ref()
        .map_or(session.device_id.as_str(), |policy| {
            policy.target_device_id.as_str()
        })
        .to_owned();
    let mut scope = session.scope_snapshot.clone();
    if selected.is_some() {
        scope.mode = desk_agent_protocol::ExecutionMode::ConfirmEachAction;
        scope.granted = vec![desk_agent_protocol::Capability::ShellExecConfirmed];
    }
    let ctx = ExecContext {
        assistant_turn_fence: crate::action_turn_fence::AssistantTurnFence::for_device(
            session, &device,
        )?,
        conversation_id: session.conversation_id.clone(),
        turn_id: turn_id.to_string(),
        tool_call_id: call.id.clone(),
        actor_id: session.actor_id.clone(),
        policy_revision: session.policy_revision,
        scope,
        // Route the approval preview back to the connection that started the turn.
        connection_id: session.active_control_connection_id.clone(),
    };

    // Persist "awaiting approval" before the wait so the pending decision is
    // observable across instances; restore Running once the seam returns.
    session.turn_state = TurnState::AwaitingApproval;
    deps.session_seam.save(session).await?;
    sink.on_awaiting_approval(&call.name, &call.id, &call.arguments_json);
    let version =
        crate::action_version::ActionVersion::capture_for_device(session, &call.id, &device)?;
    let completion = match &selected {
        Some(policy) => {
            deps.tools
                .execute_selected_command(call, &ctx, version.as_ref(), policy)
                .await
        }
        None => {
            deps.tools
                .confirm_and_exec_versioned(call, &ctx, version.as_ref())
                .await
        }
    };
    if let Some(advance) = completion.version_advance {
        advance.apply(session, version.as_ref())?;
    }
    let outcome = completion.outcome;
    session.turn_state = TurnState::Running;

    // A stable delivery id the foreground path must ack (consume) after its save, so
    // the background completion publisher does not also deliver the same result.
    // Only a foreground terminal result with a delivery id is acked; a
    // Dispatched outcome deliberately leaves the delivery pending for the publisher.
    let mut ack_event_id: Option<String> = None;
    let mut terminal_outcome: Option<LoopOutcome> = None;
    let failed = matches!(&outcome, Ok(ExecOutcome::Failed { .. }));
    match outcome {
        Ok(ExecOutcome::Executed {
            output,
            event_id,
            data_envelope,
        })
        | Ok(ExecOutcome::Failed {
            output,
            event_id,
            data_envelope,
        }) => {
            // Key the result message on the stable delivery id when the runtime has
            // one, so a late completion delivery of the same result is recognized as
            // already present (dedup by message_id) rather than appended twice.
            let message_id = match &event_id {
                Some(id) => id.clone(),
                None => mint(),
            };
            ack_event_id = event_id;
            let data_envelope = if let Some(original) = data_envelope {
                original.validate().map_err(|_| invalid_original_result())?;
                let bytes = crate::model_egress::message_payload_bytes(
                    &output.content,
                    output.image_data_url.as_deref(),
                )
                .map_err(|_| invalid_original_result())?;
                let declared_size = match &original.content {
                    ContentRef::ImmutableBlob { size_bytes, .. }
                    | ContentRef::EphemeralObservation { size_bytes, .. }
                    | ContentRef::Artifact { size_bytes, .. } => *size_bytes,
                };
                if original.digest_sha256 != format!("{:x}", Sha256::digest(&bytes))
                    || declared_size != bytes.len() as u64
                {
                    return Err(invalid_original_result());
                }
                Some(original)
            } else {
                let mut generated = deps.tools.mutating_data_envelope(call, &output)?;
                bind_tool_input_envelopes(session, call, &mut generated)?;
                generated
            };
            let failure = append_reviewed_tool_result(
                deps,
                session,
                message_id,
                &call.id,
                output,
                data_envelope,
            )
            .await?;
            if let Some(failure) = failure {
                terminal_outcome = Some(
                    finish_tool_output_safety_failure(
                        deps,
                        session,
                        &call.id,
                        remaining_calls,
                        mint,
                        sink,
                        failure,
                    )
                    .await?,
                );
            } else {
                finish_tool(session, &call.id, !failed, sink);
                if failed {
                    *halted = Some(format!(
                        "not executed: tool `{}` failed earlier in this group; this tool was not dispatched",
                        call.name
                    ));
                }
            }
        }
        Ok(ExecOutcome::PermissionRequired { request }) => {
            terminal_outcome = Some(
                task_permission::pause(
                    deps,
                    session,
                    call,
                    remaining_calls,
                    request,
                    mint,
                    sink,
                    observation,
                )
                .await?,
            );
        }
        Ok(ExecOutcome::ReviewDenied { reason }) => {
            *session.review_denials.entry(call.name.clone()).or_default() += 1;
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(
                    mint(),
                    &call.id,
                    format!("the command was rejected: {reason}"),
                ),
            )?;
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was not run".to_string());
        }
        Ok(ExecOutcome::Rejected { reason }) => {
            let text = match reason {
                Some(r) => format!("the command was rejected: {r}"),
                None => "the command was rejected".to_string(),
            };
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, text),
            )?;
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was not run".to_string());
        }
        Ok(ExecOutcome::NotExecuted { reason }) => {
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, format!("not executed: {reason}")),
            )?;
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior action did not pass dispatch validation".into());
        }
        Ok(ExecOutcome::Cancelled { reason }) => {
            let text = match reason {
                Some(r) => format!("the command was cancelled before it ran: {r}"),
                None => "the command was cancelled before it ran".to_string(),
            };
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, text),
            )?;
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was cancelled".to_string());
        }
        Ok(ExecOutcome::ApprovalTimeout) => {
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(
                    mint(),
                    &call.id,
                    "approval timed out; the command was not executed",
                ),
            )?;
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was not run".to_string());
        }
        Ok(ExecOutcome::Unknown(id)) => {
            // §6: close the conversation with a placeholder tool result (so the
            // model history stays well-formed) and record the unknown outcome; a
            // late real result replaces the placeholder in place.
            let placeholder_id = mint();
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(
                    placeholder_id.clone(),
                    &call.id,
                    OUTCOME_UNKNOWN_PLACEHOLDER,
                ),
            )?;
            session
                .execution_state
                .insert(ExecutionState::OutcomeUnknown {
                    action: id,
                    placeholder_message_id: placeholder_id,
                    since: (deps.clock)(),
                });
            finish_tool(session, &call.id, false, sink);
        }
        Ok(ExecOutcome::Dispatched(id)) => {
            // Background task model: close the tool call now with a task-id result (a
            // well-formed message the loop never rewrites) and record the outstanding
            // dispatch. The real result is appended later as a completion
            // notification. Further authorized commands remain available; the host
            // atomically enforces capacity across all callers.
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::background_task_running(mint(), &call.id, &id.action_request_id),
            )?;
            session
                .execution_state
                .insert(ExecutionState::Executing { action: id });
            finish_tool(session, &call.id, true, sink);
        }
        // A model-safe execution error becomes an error tool-result; a backend
        // transport error fails the turn. A seam may mark a pre-dispatch error
        // retryable (for example, an unavailable interpreter). In that case no
        // command ran, so let the model inspect the result and choose a valid
        // alternative in the next step. Non-retryable failures retain the
        // conservative stop-the-batch behavior.
        Err(e) if e.safe_for_model => {
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(
                    mint(),
                    &call.id,
                    format!("execution error: {}", e.message),
                ),
            )?;
            finish_tool(session, &call.id, false, sink);
            if !e.retryable {
                *halted = Some(format!(
                    "not executed: tool `{}` failed earlier in this group; this tool was not dispatched",
                    call.name
                ));
            }
        }
        Err(e) => {
            deps.session_seam.save(session).await?;
            return Err(e);
        }
    }

    // Persist the terminal outcome now, before returning to the batch loop, so a
    // crash can never leave the durable execution record ahead of the session (which
    // would strand the tool call and force a conservative unknown-outcome recovery).
    if terminal_outcome.is_none() {
        deps.session_seam.save(session).await?;
    }

    // Post-save ack: the result is safely stored, so tell the seam the foreground
    // consumed this delivery. Best-effort — if the ack is lost (0 rows / transport
    // error) the background publisher delivers instead, and its append dedups by the
    // delivery id the result message is already keyed on, so it never doubles.
    if let Some(event_id) = ack_event_id {
        let _ = deps.tools.ack_delivery(&event_id).await;
    }

    Ok(terminal_outcome)
}

/// Run a `wait_for_task` call: the model actively waits on the background task it
/// dispatched. Validated against the session's own execution identity (a control end
/// can never steer it at another task), then handed to the seam. A completed result
/// retains its original Provider identity when supplied with a receipt; the
/// wait call receives a separate status. Legacy results belong to the wait
/// call. Stable delivery ids deduplicate a racing publisher, and settlement
/// clears the execution machine. A known failure stops the current group.
/// Unknown outcomes retain the original anchor when supplied, otherwise the
/// wait result becomes the reconcile placeholder for a late completion.
#[allow(clippy::too_many_arguments)]
async fn run_wait<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    remaining_calls: &[crate::chat::ToolCall],
    mint: &mut F,
    halted: &mut Option<String>,
    sink: &mut dyn TurnSink,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<Option<LoopOutcome>, AgentError> {
    // A model-safe argument error becomes an error tool result; the turn continues.
    let task_id = match crate::wait_tools::parse_wait_task_id(call) {
        Ok(id) => id,
        Err(e) => {
            observation.input_error(&e);
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, format!("wait error: {}", e.message)),
            )?;
            return Ok(None);
        }
    };
    // Only the session's own in-flight task may be waited on, matched by its stable
    // id. No task, or a mismatched id, is a well-formed error result.
    let Some(action) = session.execution_state.task(&task_id).cloned() else {
        observation.reject(
            crate::model_observability::Stage::Reference,
            crate::model_observability::InputIssue::UnknownReference,
        );
        append_mutating_result(
            deps,
            session,
            call,
            ChatMessage::tool_result(
                mint(),
                &call.id,
                format!("no running background task with id `{task_id}`"),
            ),
        )?;
        return Ok(None);
    };
    if task_id != action.action_request_id {
        observation.reject(
            crate::model_observability::Stage::Reference,
            crate::model_observability::InputIssue::UnknownReference,
        );
        append_mutating_result(
            deps,
            session,
            call,
            ChatMessage::tool_result(
                mint(),
                &call.id,
                format!("no running background task with id `{task_id}`"),
            ),
        )?;
        return Ok(None);
    }

    observation.stage(
        crate::model_observability::Stage::Reference,
        crate::model_observability::StageOutcome::Passed,
    );
    observation.stage(
        crate::model_observability::Stage::Preflight,
        crate::model_observability::StageOutcome::Passed,
    );
    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
    let outcome = deps
        .tools
        .wait_for_task(&action.action_request_id, &action.execution_id)
        .await;
    let mut ack_event_id: Option<String> = None;
    let mut terminal_outcome: Option<LoopOutcome> = None;
    let failed = matches!(&outcome, Ok(WaitOutcome::FailedWithReceipt { .. }));
    match outcome {
        Ok(WaitOutcome::CompletedWithReceipt {
            action: completed_action,
            original_call_id,
            output,
            event_id,
            data_envelope,
        })
        | Ok(WaitOutcome::FailedWithReceipt {
            action: completed_action,
            original_call_id,
            output,
            event_id,
            data_envelope,
        }) => {
            if completed_action != action || output.image_data_url.is_some() {
                return Err(invalid_original_result());
            }
            original_action_anchor(session, &action, &original_call_id)?;
            data_envelope
                .validate()
                .map_err(|_| invalid_original_result())?;
            let declared_size = match &data_envelope.content {
                ContentRef::ImmutableBlob { size_bytes, .. }
                | ContentRef::EphemeralObservation { size_bytes, .. }
                | ContentRef::Artifact { size_bytes, .. } => *size_bytes,
            };
            if declared_size != output.content.len() as u64
                || data_envelope.digest_sha256
                    != format!("{:x}", Sha256::digest(output.content.as_bytes()))
            {
                return Err(invalid_original_result());
            }
            if !remaining_calls.is_empty() {
                // A native completion is an independent untrusted message, not
                // this call's result. Keep its receipt unconsumed until the
                // model closes this group; never dispatch the action again.
                append_mutating_result(
                    deps,
                    session,
                    call,
                    ChatMessage::tool_result(
                        mint(),
                        &call.id,
                        if failed {
                            "the task failed; call wait_for_task on its own to retrieve the original result"
                        } else {
                            "the task has completed; call wait_for_task last in a tool-call group or on its own to retrieve the original result"
                        },
                    ),
                )?;
                finish_tool(session, &call.id, !failed, sink);
                if failed {
                    *halted = Some("not executed: the background action failed".into());
                }
                deps.session_seam.save(session).await?;
                return Ok(None);
            }
            let completion_position = session.conversation.len();
            if !session.apply_completion_with_envelope(
                &event_id,
                &action.execution_id,
                &original_call_id,
                &action.action_request_id,
                &output.content,
                Some(data_envelope.clone()),
                output.format,
                (deps.clock)(),
            ) {
                return Err(invalid_original_result());
            }
            deliver_pending_results(deps, session).await?;
            let text = if failed {
                "background task failed; its original result is recorded in the conversation"
            } else {
                "background task completed; its original result is recorded in the conversation"
            };
            let envelope = crate::model_message_labels::internal_tool_result_envelope(
                Some(&data_envelope),
                &call.id,
                text,
                // Preserve the invoked tool name so history export can bind
                // this closure to its model-authorized wait call. The original
                // receipt retains its own call identity and independent audit.
                &call.name,
            )?;
            let mut message = ChatMessage::tool_result(mint(), &call.id, text);
            message.data_envelope = envelope;
            // Close the wait call before any appended untrusted completion so
            // the model's assistant/tool-result group remains contiguous.
            session.conversation.insert(completion_position, message);
            ack_event_id = Some(event_id);
            finish_tool(session, &call.id, !failed, sink);
            if failed {
                *halted = Some("not executed: the background action failed".into());
            }
        }
        Ok(WaitOutcome::Completed { output, event_id }) => {
            // Key on the stable delivery id so a racing publisher delivery of the
            // same result dedups instead of appending a second copy.
            let message_id = match &event_id {
                Some(id) => id.clone(),
                None => mint(),
            };
            ack_event_id = event_id;
            // The awaited task settled: a follow-up may mutate again.
            session.execution_state.remove(&action);
            let failure =
                append_reviewed_tool_result(deps, session, message_id, &call.id, output, None)
                    .await?;
            if let Some(failure) = failure {
                terminal_outcome = Some(
                    finish_tool_output_safety_failure(
                        deps,
                        session,
                        &call.id,
                        remaining_calls,
                        mint,
                        sink,
                        failure,
                    )
                    .await?,
                );
            } else {
                finish_tool(session, &call.id, true, sink);
            }
        }
        Ok(WaitOutcome::StillRunning) => {
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::background_task_running(mint(), &call.id, &action.action_request_id),
            )?;
            finish_tool(session, &call.id, true, sink);
        }
        Ok(WaitOutcome::UnknownWithIdentity {
            action: unknown_action,
            original_call_id,
        }) => {
            if unknown_action != action {
                return Err(invalid_original_result());
            }
            let index = original_action_anchor(session, &action, &original_call_id)?;
            if matches!(
                session.execution_state.execution(&action.execution_id),
                Some(ExecutionState::Executing { .. })
            ) {
                let original = &mut session.conversation[index];
                original.data_envelope =
                    crate::model_message_labels::internal_tool_result_envelope(
                        original.data_envelope.as_ref(),
                        &original_call_id,
                        OUTCOME_UNKNOWN_PLACEHOLDER,
                        "background_task_outcome_unknown",
                    )?;
                original.text = OUTCOME_UNKNOWN_PLACEHOLDER.into();
                session
                    .execution_state
                    .insert(ExecutionState::OutcomeUnknown {
                        action,
                        placeholder_message_id: original.message_id.clone(),
                        since: (deps.clock)(),
                    });
            }
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, OUTCOME_UNKNOWN_PLACEHOLDER),
            )?;
            finish_tool(session, &call.id, false, sink);
        }
        Ok(WaitOutcome::Unknown) => {
            // The task was recovered without a result. Degrade to an unknown outcome
            // using this call's own result as the reconcile placeholder, and bar
            // further mutation until a late result reconciles it.
            let placeholder_id = mint();
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(
                    placeholder_id.clone(),
                    &call.id,
                    OUTCOME_UNKNOWN_PLACEHOLDER,
                ),
            )?;
            session
                .execution_state
                .insert(ExecutionState::OutcomeUnknown {
                    action,
                    placeholder_message_id: placeholder_id,
                    since: (deps.clock)(),
                });
            finish_tool(session, &call.id, false, sink);
        }
        Err(e) if e.safe_for_model => {
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, format!("wait error: {}", e.message)),
            )?;
            finish_tool(session, &call.id, false, sink);
        }
        Err(e) => {
            deps.session_seam.save(session).await?;
            return Err(e);
        }
    }

    if terminal_outcome.is_none() {
        deps.session_seam.save(session).await?;
    }
    if let Some(event_id) = ack_event_id {
        let _ = deps.tools.ack_delivery(&event_id).await;
    }
    Ok(terminal_outcome)
}

#[cfg(test)]
mod tests;

mod task_permission;

async fn read_conversation_attachment(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    _parent: Option<&desk_agent_protocol::data_lineage::DataEnvelope>,
    remaining_budget: &mut usize,
    maximum_response_bytes: Option<u32>,
    observation: &crate::model_observability::tool::ToolObservation,
) -> Result<ChatMessage, AgentError> {
    use crate::conversation_attachment::{ContentKind, invalid, model_read};
    use crate::model_observability::{InputIssue, PermissionOutcome, Stage, StageOutcome};
    let input: model_read::Input = serde_json::from_str(&call.arguments_json).map_err(|_| {
        observation.reject(Stage::Preflight, InputIssue::Semantic);
        invalid("Invalid attachment read input")
    })?;
    input.request().inspect_err(|error| {
        observation.input_error(error);
    })?;
    observation.stage(Stage::Reference, StageOutcome::Attempted);
    let part = deps
        .session_seam
        .read_attachment(session, &input.attachment_id, false)
        .await?;
    let policy = deps.model.model_egress_policy()?;
    let mut message = if part.metadata.kind == ContentKind::Image {
        input.validate_image().inspect_err(|error| {
            observation.input_error(error);
        })?;
        let source = part.metadata.image_source.as_ref().ok_or_else(|| {
            observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
            invalid("Image source authorization is unavailable")
        })?;
        let message = source.restore(&part.content).inspect_err(|_| {
            observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
        })?;
        crate::conversation_image::authorize_read_observed(&message, policy.as_ref(), observation)?;
        let url = message.image_data_url.as_deref().ok_or_else(|| {
            observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
            invalid("Image pixels unavailable")
        })?;
        let info = crate::image_input::validate_image_data_url(url).map_err(|error| {
            observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
            image_input_error(error)
        })?;
        let review = review_image(deps, url, &info.media_type).await;
        if !matches!(&review, Ok(ContentSafetyDecision::Allow)) {
            if matches!(
                &review,
                Ok(ContentSafetyDecision::Block | ContentSafetyDecision::SafeRedirect)
            ) {
                observation.permission(PermissionOutcome::PolicyRejected);
            }
            return Err(invalid("Stored image was blocked by content safety policy"));
        }
        observation.stage(Stage::Reference, StageOutcome::Passed);
        observation.stage(Stage::Preflight, StageOutcome::Passed);
        message
    } else {
        let envelope = part.metadata.source_envelope.as_ref().ok_or_else(|| {
            observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
            invalid("Attachment source authorization is unavailable")
        })?;
        if let Some(policy) = &policy
            && !policy.has_gateway_authority(envelope)
        {
            observation.permission(PermissionOutcome::PolicyRejected);
            return Err(invalid(
                "Attachment is not authorized for the current model gateway",
            ));
        }
        part.metadata.verify(&part.content).inspect_err(|_| {
            observation.reject(Stage::Reference, InputIssue::ReferenceUnavailable);
        })?;
        let source_envelope = envelope.clone();
        let original_attachment_sha256 = part.metadata.sha256.clone();
        if session.ui_references.observe_attachment(&part)? {
            deps.session_seam.save(session).await?;
        }
        let (content, mut receipt) = model_read::text_page_with_references(
            &part,
            &input,
            &call.id,
            *remaining_budget,
            observation,
            &mut session.ui_references,
        )?;
        deps.session_seam.save(session).await?;
        let mut message = ChatMessage::tool_result("attachment-page", &call.id, content);
        message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            Some(&source_envelope),
            &call.id,
            &message.text,
            crate::conversation_attachment::READ_ATTACHMENT_TOOL,
        )?;
        receipt.original_envelope = message.data_envelope.clone();
        receipt.source_sha256 = original_attachment_sha256;
        message.attachment_read = Some(Box::new(receipt));
        message
    };
    if message.image_data_url.is_some() {
        deps.model
            .context_policy(crate::model_capability::ModelRequirements::for_messages(
                std::slice::from_ref(&message),
            ))
            .await?;
    }
    let cost = crate::trim::model_context_cost(&message);
    if cost > *remaining_budget {
        return Err(invalid("Insufficient model context for attachment reading"));
    }
    if let Some(maximum) = maximum_response_bytes {
        let framed = crate::ai_assistant::history::serialized_tool_response_bytes(
            &call.id,
            &message.text,
            message.image_data_url.as_deref(),
        )
        .map_err(invalid)?;
        if framed > maximum {
            return Err(invalid(
                "Attachment response exceeds the shared framed byte budget; request a smaller text page",
            ));
        }
    }
    session
        .focus_epoch
        .record_history_result(cost)
        .map_err(invalid)?;
    deps.session_seam
        .read_attachment(session, &input.attachment_id, true)
        .await?;
    *remaining_budget -= cost;
    message.tool_call_id = Some(call.id.clone());
    Ok(message)
}

#[inline(never)]
fn settled_terminal_error(result: &Result<LoopOutcome, AgentError>) -> Option<AgentError> {
    match result {
        Err(error) | Ok(LoopOutcome::ContentSafetyUnavailable(error)) => Some(error.clone()),
        Ok(LoopOutcome::ProtocolError(error)) => Some(AgentError {
            kind: AgentErrorKind::Internal,
            message: error.to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }),
        _ => None,
    }
}

#[inline(never)]
fn parsed_tool_arguments(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).expect("classify_model_turn validated tool arguments")
}

#[inline(never)]
fn verified_completion_browser(text: &str) -> Option<BrowserActionResult> {
    let completion = serde_json::from_str::<ComputerActionCompleted>(text).ok()?;
    if completion.result == ComputerActionResultClass::Verified
        && let Some(ComputerActionOutput::Browser(result)) = completion.output
        && result.validate().is_ok()
    {
        return Some(result);
    }
    None
}

#[inline(never)]
fn context_request_anchors(
    view: &crate::model_context::ModelContextView,
    session: &PersistedAgentSession,
) -> std::collections::BTreeSet<String> {
    let mut anchors = std::collections::BTreeSet::new();
    if session.agent_role.is_main()
        && view
            .messages
            .iter()
            .any(|message| message.role == ChatRole::ContextSummary)
    {
        if let Some(source) = session.authorization_requirement() {
            anchors.insert(source.message_id.clone());
        }
        if let Some(segment) = &session.focus_epoch.goal_segment {
            anchors.insert(segment.source_message_id.clone());
        }
        if let Some(source) = session.conversation.iter().find(|message| {
            message.role == ChatRole::User
                && !crate::permission_resume::is_resume_control_message(message)
        }) {
            anchors.insert(source.message_id.clone());
        }
        for message in &view.messages {
            if message.role == ChatRole::ContextSummary
                && let Ok(summary) =
                    serde_json::from_str::<crate::model_context::ContextSummaryV1>(&message.text)
            {
                for fact in summary.goals.iter().chain(&summary.historical_constraints) {
                    anchors.extend(fact.source_message_ids.iter().cloned());
                }
            }
        }
    }
    anchors
}

#[inline(never)]
fn clone_context_request(template: &ModelRequest) -> Box<ModelRequest> {
    Box::new(template.clone())
}
