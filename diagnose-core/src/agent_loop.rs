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
//! one turn run serially; a rejection / timeout / unknown outcome halts the rest.
//! The same exposure matrix ([`registry::exposed_tools`] /
//! [`registry::lookup_exposed`]) both advertises tools to the model and validates
//! a returned call, so a model can never invoke a tool it was not shown.
//!
//! Circuit breakers are turn-level (reset when the turn is claimed): a per-turn
//! step budget ([`MAX_STEPS_PER_TURN`]) and a same-tool repeat cap
//! ([`MAX_SAME_TOOL_PER_TURN`]).
//!
//! [`MAX_STEPS_PER_TURN`]: crate::MAX_STEPS_PER_TURN
//! [`MAX_SAME_TOOL_PER_TURN`]: crate::MAX_SAME_TOOL_PER_TURN

use desk_agent_protocol::browser_control::{
    BrowserActionResult, BrowserElementRef, BrowserElementRole, BrowserPageRef,
};
use desk_agent_protocol::computer_use::{
    ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
};
use desk_agent_protocol::content_safety::{ContentSafetyDecision, StreamRetractionReason};
use desk_agent_protocol::data_lineage::{ContentRef, DataEnvelope};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use crate::content_safety::{
    ContentSafetyMode, SafetyImage, SafetyModelTurn, SafetyToolCall, content_blocked_error,
    normalize_safety_error, refusal_placeholder_for,
};
use crate::model_message_labels::internal_tool_result_envelope as derive_internal_tool_result_envelope;
use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::chat::{ChatMessage, ChatRole, ModelTurnError, TurnDisposition, classify_model_turn};
use crate::registry::{RegisteredTool, ToolEffect, exposed_tools, lookup_exposed};
use crate::seam::{
    ClaimError, ClaimTurnParams, ExecContext, ExecOutcome, ModelRequest, ModelSeam, SessionSeam,
    ToolSeam, TurnSink, WaitOutcome,
};
use crate::session::{AgentSessionSurface, ExecutionState, SubjectMismatch, TurnState};

/// The placeholder tool-result text written when a mutating execution's outcome is
/// unknown (§6): it keeps the conversation well-formed and tells the model not to
/// assume the command succeeded. A late real result replaces it in place.
const OUTCOME_UNKNOWN_PLACEHOLDER: &str =
    "execution outcome unknown; the command may have executed; do not assume success";

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
            desk_utils::error::DeskErrorCode::COPILOT_RESPONSE_TRUNCATED.code(),
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
            desk_utils::error::DeskErrorCode::COPILOT_PROTOCOL_VIOLATION.code(),
        ),
    }
}

fn model_context_error(error: crate::model_context::ModelContextError) -> AgentError {
    let oversized = matches!(
        error,
        crate::model_context::ModelContextError::ContextItemTooLarge { .. }
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
        message: format!("model context compression failed: {}", kind.as_str()),
        retryable: false,
        safe_for_model: true,
        error_code: Some(desk_utils::error::DeskErrorCode::AI_CONTEXT_COMPRESSION_FAILED.code()),
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
}

/// The seams + config the loop runs over, borrowed for one turn.
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
    /// Per-turn model→tool step budget (circuit breaker). Diagnose passes
    /// [`crate::MAX_STEPS_PER_TURN`]; the latency-sensitive terminal copilot
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
    let mut session = match deps.session_seam.claim_turn(claim).await {
        Ok(s) => s,
        Err(ClaimError::Busy) => return Ok(LoopOutcome::TurnBusy),
        Err(ClaimError::Subject(m)) => return Ok(LoopOutcome::SubjectRejected(m)),
        Err(ClaimError::Backend(e)) => return Err(e),
    };

    // Keep the lease alive for the (possibly long) turn with the just-claimed
    // token; the guard stops renewal when dropped on every exit path below.
    let _lease_guard = deps
        .heartbeat
        .map(|h| h.start(session.conversation_id.clone(), session.lease_token));

    // Append the control-end message (if any) and persist before the first model
    // call. An automation resume appends nothing — it reacts to the existing tail.
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
                    message.role == ChatRole::User
                        && !crate::permission_resume::is_permission_resume_message(message)
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
        Ok(LoopOutcome::ProtocolError(_))
        | Ok(LoopOutcome::ContentSafetyUnavailable(_))
        | Err(_) => TurnState::Failed,
        // Policy rejection is a settled Idle turn. Answered / Truncated /
        // CircuitBreak also return to Idle so a follow-up can be claimed.
        _ => TurnState::Idle,
    };
    session.finish_turn(terminal, (deps.clock)());
    if terminal == TurnState::Idle && !matches!(&result, Ok(LoopOutcome::Superseded { .. })) {
        session.handled_input_seq = session.latest_input_seq;
    }
    crate::image_input::strip_session_images(&mut session.conversation);
    // Surface a save failure only if the loop itself otherwise succeeded.
    let save = deps.session_seam.save(&mut session).await;
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
fn canonical_arguments_json(raw: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(raw).expect("classify_model_turn validated tool arguments");
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
                text: turn.text.clone(),
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
async fn append_reviewed_tool_result(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    message_id: String,
    call_id: &str,
    output: crate::seam::ToolRunOutput,
    data_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
) -> Result<Option<ToolOutputSafetyFailure>, AgentError> {
    let crate::seam::ToolRunOutput {
        content,
        image_data_url,
    } = output;
    let Some(image_data_url) = image_data_url else {
        // With no new image there is nothing to rotate: any previously retained
        // session image already satisfies the one-image invariant.
        let mut message = ChatMessage::tool_result(message_id, call_id, content);
        message.data_envelope = data_envelope;
        session.conversation.push(message);
        return Ok(None);
    };

    let info =
        crate::image_input::validate_image_data_url(&image_data_url).map_err(image_input_error)?;
    match review_image(deps, &image_data_url, &info.media_type).await {
        Ok(ContentSafetyDecision::Allow) => {
            let mut message = ChatMessage::tool_result(message_id, call_id, content);
            message.image_data_url = Some(image_data_url);
            message.data_envelope = data_envelope;
            session.conversation.push(message);
            retain_latest_session_image(session)?;
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

async fn prepare_model_context(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    pinned_context: &crate::model_context::PinnedContextPolicy,
    compression_attempted: &mut bool,
    sink: &mut dyn TurnSink,
) -> Result<crate::model_context::ModelContextView, AgentError> {
    loop {
        if pinned_context.strategy
            == crate::model_context::ContextManagementStrategy::CheckpointSummary
            && deps
                .heartbeat
                .is_some_and(|heartbeat| !heartbeat.is_healthy())
        {
            return Err(report_context_compression_failure(
                deps.model,
                crate::seam::ContextCompressionFailureKind::StaleContext,
                None,
                None,
            )
            .await);
        }
        let protection = session.context_protection_set();
        let plan = match crate::model_context::plan_model_context(
            &session.conversation,
            &session.model_context_state,
            pinned_context,
            &protection,
            session.version,
        ) {
            Ok(plan) => plan,
            Err(error)
                if pinned_context.strategy
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

        if let Some(policy) = deps.model.model_egress_policy()?
            && crate::model_context::authorize_context_checkpoint(
                &policy,
                &session.model_context_state,
                &pinned_context.key(),
                &session.conversation,
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
                let changed = session.model_context_state != ready.next_state;
                let floor_advanced = ready.view.floor_advanced;
                let previous_state = session.model_context_state.clone();
                let previous_notices = session.context_notices.clone();
                session.model_context_state = ready.next_state;
                if floor_advanced {
                    session
                        .add_context_notice(crate::model_context::ContextNotice::trimmed(turn_id));
                }
                if changed || floor_advanced {
                    if pinned_context.strategy
                        == crate::model_context::ContextManagementStrategy::CheckpointSummary
                        && deps
                            .heartbeat
                            .is_some_and(|heartbeat| !heartbeat.is_healthy())
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
                    sink.on_context_trimmed(turn_id);
                }
                return Ok(ready.view);
            }
            crate::model_context::ContextBuildPlan::NeedsFloorReconciliation(plan) => {
                let previous_state = session.model_context_state.clone();
                let previous_notices = session.context_notices.clone();
                session.model_context_state = match crate::model_context::apply_floor_reconciliation(
                    &plan,
                    &session.conversation,
                    &session.model_context_state,
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
                session.add_context_notice(crate::model_context::ContextNotice::trimmed(turn_id));
                if deps
                    .heartbeat
                    .is_some_and(|heartbeat| !heartbeat.is_healthy())
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
                    let _ = error;
                    return Err(report_context_compression_failure(
                        deps.model,
                        crate::seam::ContextCompressionFailureKind::StaleContext,
                        None,
                        None,
                    )
                    .await);
                }
                sink.on_context_trimmed(turn_id);
                // Rebuild protection and re-plan against the new version/floor.
            }
            crate::model_context::ContextBuildPlan::NeedsCompression(plan) => {
                use crate::seam::ContextCompressionFailureKind as FailureKind;

                let audit_context = compression_audit_context(&plan, &deps.content_safety);
                if *compression_attempted {
                    return Err(report_context_compression_failure(
                        deps.model,
                        FailureKind::AttemptExhausted,
                        Some(audit_context),
                        None,
                    )
                    .await);
                }
                // The attempt is spent before the durable provider-call path. The
                // manager seam's call fence is the cross-crash authority.
                *compression_attempted = true;
                deps.model.on_context_compression_started(
                    plan.generation,
                    plan.covered_message_count,
                    plan.input_model_context_cost,
                );
                let authorized_input = match deps.model.model_egress_policy()? {
                    Some(policy) => match crate::model_context::authorize_compression_input(
                        &policy,
                        &plan,
                        &session.conversation,
                    ) {
                        Ok(input) => Some(input),
                        Err(_) => {
                            return Err(report_context_compression_failure(
                                deps.model,
                                FailureKind::StaleContext,
                                Some(audit_context),
                                None,
                            )
                            .await);
                        }
                    },
                    None => None,
                };
                let request = ModelRequest {
                    messages: authorized_input.as_ref().map_or_else(
                        || crate::model_context::compression_request_messages(&plan),
                        |input| input.messages.clone(),
                    ),
                    tools: Vec::new(),
                    tool_requirements: crate::model_capability::ModelRequirements::TEXT_ONLY,
                    tool_choice: crate::chat::ToolChoice::None,
                    response_format: crate::prompt::ResponseFormatSpec::None,
                    use_case: crate::model_profile::ModelUseCase::ContextCompression,
                    caller_output_hard_cap: Some(
                        crate::model_context::CONTEXT_SUMMARY_OUTPUT_HARD_CAP_TOKENS,
                    ),
                };
                let mut compression_sink = crate::seam::NullTurnSink;
                let turn = match deps.model.call(request, &mut compression_sink).await {
                    Ok(turn) => turn,
                    Err(error) => {
                        let kind = compression_failure_for_provider_error(&error);
                        return Err(report_context_compression_failure(
                            deps.model,
                            kind,
                            Some(audit_context),
                            None,
                        )
                        .await);
                    }
                };
                let compression_usage = crate::seam::ContextCompressionProviderUsage {
                    tokens: turn.usage,
                    reasoning_tokens: turn.provider_meta.reasoning_tokens,
                };
                session.record_compression_usage(turn.usage);
                let disposition = match classify_model_turn(&turn) {
                    Ok(disposition) => disposition,
                    Err(_) => {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::InvalidSchema,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                };
                let disposition_failure = match disposition {
                    TurnDisposition::Answer if turn.tool_calls.is_empty() => None,
                    TurnDisposition::Discard => Some(FailureKind::Truncated),
                    TurnDisposition::ContextWindowExceeded => Some(FailureKind::InputTooLarge),
                    TurnDisposition::Answer | TurnDisposition::InvokeTools => {
                        Some(FailureKind::InvalidSchema)
                    }
                };
                if let Some(kind) = disposition_failure {
                    return Err(report_context_compression_failure(
                        deps.model,
                        kind,
                        Some(audit_context),
                        Some(compression_usage),
                    )
                    .await);
                }
                let created_at = (deps.clock)();
                let provenance = match deps
                    .model
                    .context_compression_provenance(turn_id, &created_at)
                {
                    Ok(provenance) => provenance,
                    Err(_) => {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::StaleContext,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                };
                let mut validated = match crate::model_context::parse_validated_context_summary(
                    &turn.text, &plan, provenance,
                ) {
                    Ok(validated) => validated,
                    Err(error) => {
                        let kind = compression_failure_for_context_error(&error);
                        return Err(report_context_compression_failure(
                            deps.model,
                            kind,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                };
                if let Some(input) = &authorized_input {
                    let policy = deps.model.model_egress_policy()?;
                    if policy.as_ref().is_none_or(|policy| {
                        crate::model_context::bind_context_summary_lineage(
                            policy,
                            &mut validated,
                            &turn,
                            input,
                        )
                        .is_err()
                    }) {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::StaleContext,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                }
                let summary_context_cost = validated.summary_model_context_cost;
                let candidate = match serde_json::to_string(&validated.summary) {
                    Ok(candidate) => candidate,
                    Err(_) => {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::InvalidSchema,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                };
                match review_context_summary(deps, candidate).await {
                    Ok(ContentSafetyDecision::Allow) => {}
                    Ok(_) | Err(_) => {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::UnsafeOutput,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                }
                if deps
                    .heartbeat
                    .is_some_and(|heartbeat| !heartbeat.is_healthy())
                {
                    return Err(report_context_compression_failure(
                        deps.model,
                        FailureKind::StaleContext,
                        Some(audit_context),
                        Some(compression_usage),
                    )
                    .await);
                }
                // Content review may wait on another service. Recheck expiry
                // immediately before applying the checkpoint, without extending
                // the accepted model output's retention boundary.
                if let Some(input) = &authorized_input {
                    let policy = deps.model.model_egress_policy()?;
                    if policy.as_ref().is_none_or(|policy| {
                        crate::model_context::bind_context_summary_lineage(
                            policy,
                            &mut validated,
                            &turn,
                            input,
                        )
                        .is_err()
                    }) {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::StaleContext,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                }
                let generation = plan.generation;
                let covered_message_count = plan.covered_message_count;
                let (next_state, view) = match crate::model_context::apply_validated_checkpoint(
                    &plan,
                    validated,
                    &session.conversation,
                    &session.model_context_state,
                    session.version,
                ) {
                    Ok(value) => value,
                    Err(_) => {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::StaleContext,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                };
                let final_context_cost: Result<u64, ()> =
                    view.messages.iter().try_fold(0u64, |sum, message| {
                        let cost = u64::try_from(crate::trim::model_context_cost(message))
                            .map_err(|_| ())?;
                        sum.checked_add(cost).ok_or(())
                    });
                let final_context_cost = match final_context_cost {
                    Ok(cost) => cost,
                    Err(()) => {
                        return Err(report_context_compression_failure(
                            deps.model,
                            FailureKind::InputTooLarge,
                            Some(audit_context),
                            Some(compression_usage),
                        )
                        .await);
                    }
                };
                let previous_state = session.model_context_state.clone();
                let previous_notices = session.context_notices.clone();
                session.model_context_state = next_state;
                session.add_context_notice(crate::model_context::ContextNotice::compacted(
                    turn_id,
                    generation,
                    covered_message_count,
                ));
                if deps
                    .heartbeat
                    .is_some_and(|heartbeat| !heartbeat.is_healthy())
                {
                    session.model_context_state = previous_state;
                    session.context_notices = previous_notices;
                    return Err(report_context_compression_failure(
                        deps.model,
                        FailureKind::StaleContext,
                        Some(audit_context),
                        Some(compression_usage),
                    )
                    .await);
                }
                if let Err(error) = deps.session_seam.save(session).await {
                    session.model_context_state = previous_state;
                    session.context_notices = previous_notices;
                    let _ = error;
                    return Err(report_context_compression_failure(
                        deps.model,
                        FailureKind::StaleContext,
                        Some(audit_context),
                        Some(compression_usage),
                    )
                    .await);
                }
                deps.model.on_context_compression_succeeded(
                    generation,
                    summary_context_cost,
                    final_context_cost,
                );
                deps.model
                    .audit_context_compression(
                        crate::seam::ContextCompressionAuditOutcome::Committed {
                            context: audit_context,
                            usage: compression_usage,
                            summary_context_cost,
                            final_context_cost,
                        },
                    )
                    .await;
                sink.on_context_compacted(turn_id, generation, covered_message_count);
                return Ok(view);
            }
        }
    }
}

/// The loop body: model → validate → answer / discard / run read tools → repeat.
async fn run_inner(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let mut seq: u32 = 0;
    let mut mint = move || {
        let id = format!("{turn_id}-{seq}");
        seq += 1;
        id
    };
    let mut same_tool: HashMap<String, u32> = HashMap::new();
    let mut compression_attempted = false;
    let mut empty_end_turn_retries: u8 = 0;
    let mut truncated_turn_retries: u8 = 0;
    // A permission decision resumes at the authorization boundary, not at the
    // beginning of the user's workflow. Keep a recency-edge checkpoint in
    // model requests until the model proposes a real mutating Provider call.
    // This prevents weaker OpenAI-compatible models from re-running a read /
    // preview, minting a new ephemeral object reference, and stranding the
    // exact one-shot grant that the owner just approved. Once a mutating call
    // is proposed the normal authorizer remains the sole execution authority;
    // later model calls may inspect again to verify the outcome.
    let mut permission_continuation_pending =
        session.trigger_origin == crate::session::TriggerOrigin::PermissionDecision;

    loop {
        if session.turn_step_budget_exhausted(deps.max_steps_per_turn) {
            return Ok(LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget));
        }
        if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
            return Ok(LoopOutcome::Superseded {
                previous_input_revision: session.input_revision,
                current_input_revision,
            });
        }

        let mut exposed = exposed_tools(
            deps.registry,
            &session.scope_snapshot,
            &session.execution_state,
            session.trigger_origin,
        );
        if permission_continuation_pending && !deps.permission_continuation_exact_tools.is_empty() {
            exposed.retain(|tool| {
                deps.permission_continuation_exact_tools
                    .iter()
                    .any(|name| name == tool.name())
                    || tool.effect == ToolEffect::RunProjection
            });
        }
        let tool_requirements = crate::model_capability::ModelRequirements::for_registered_tools(
            exposed.iter().copied(),
        );
        let specs = exposed.iter().map(|tool| tool.spec.clone()).collect();
        let request_requirements = tool_requirements.union(
            crate::model_capability::ModelRequirements::for_messages(&session.conversation),
        );
        let pinned_context = deps.model.context_policy(request_requirements).await?;
        let context_view = prepare_model_context(
            deps,
            session,
            turn_id,
            &pinned_context,
            &mut compression_attempted,
            sink,
        )
        .await?;
        // Assemble the model request: a freshly built system prompt prepended to a
        // trailing, budget-trimmed window of the stored conversation. The system
        // prompt is never persisted, so it is added here on every call.
        let mut messages = Vec::with_capacity(session.conversation.len() + 3);
        messages.push(deps.system_prompt.clone());
        messages.extend(context_view.messages);
        if session.surface == AgentSessionSurface::DeviceAssistant {
            // Put the server-owned input watermark at the recency edge of the
            // request. Some OpenAI-compatible models underweight a system prompt
            // when several durable user follow-ups are batched; this marker makes
            // the latest-input rule explicit without copying any user content.
            let marker_id = format!(
                "runtime-input-watermark-{turn_id}-{}",
                session.input_revision
            );
            let marker_text = format!(
                "RUNTIME INPUT WATERMARK (server authoritative): input_revision={} latest_input_seq={}. The newest user message in the transcript is the active requirement and overrides conflicting earlier requests. Do not continue a superseded plan. If update_task_status already succeeded for this requirement, do not call it again unless actual task progress materially changed; continue the work or answer.",
                session.input_revision, session.latest_input_seq
            );
            let latest_user = session
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == ChatRole::User)
                .cloned();
            let parent = latest_user
                .as_ref()
                .and_then(|message| message.data_envelope.as_ref());
            let mut marker = ChatMessage::system_event(&marker_id, &marker_text);
            marker.data_envelope = derive_internal_tool_result_envelope(
                parent,
                &marker_id,
                &marker_text,
                "input_watermark",
            )?;
            messages.push(marker);
            if let Some(artifact_projection) = requested_artifact_registry_projection(
                &session.conversation,
                &format!(
                    "runtime-requested-artifacts-{turn_id}-{}",
                    session.input_revision
                ),
            )? {
                messages.push(artifact_projection);
            }
            if let Some(result_projection) = reusable_provider_result_projection(
                &session.conversation,
                &format!(
                    "runtime-reusable-results-{turn_id}-{}",
                    session.input_revision
                ),
                current_unix_ms(deps.clock)?,
            )? {
                messages.push(result_projection);
            }
            let consecutive_user_inputs = session
                .conversation
                .iter()
                .rposition(|message| message.role == ChatRole::User)
                .map(|position| {
                    session.conversation[..=position]
                        .iter()
                        .rev()
                        .take_while(|message| message.role == ChatRole::User)
                        .count()
                })
                .unwrap_or_default();
            if consecutive_user_inputs > 1
                && let Some(mut latest_user) = latest_user
            {
                // Repeat the exact latest bytes at the recency edge only for a
                // batched follow-up. The repeated bytes need their own derived
                // DataEnvelope: a sink projection intentionally rejects duplicate
                // envelope ids, even when the bytes are identical. Inheriting the
                // parent's sensitivity/destination while recording lineage keeps
                // that fail-closed invariant intact. Older messages remain in
                // context for additive requirements, but cannot outweigh recency.
                let projection_id =
                    format!("runtime-latest-input-{turn_id}-{}", session.input_revision);
                latest_user.data_envelope = derive_internal_tool_result_envelope(
                    latest_user.data_envelope.as_ref(),
                    &projection_id,
                    &latest_user.text,
                    "latest_input_projection",
                )?;
                latest_user.message_id = projection_id;
                messages.push(latest_user);
            }
            if permission_continuation_pending {
                let marker_id = format!(
                    "runtime-permission-continuation-{turn_id}-{}",
                    session.input_revision
                );
                let marker_text = "PERMISSION CONTINUATION CHECKPOINT (server authoritative): resume at the authorization boundary; do not restart the workflow. Re-read CURRENT AUTHORIZED GRANTS. If a required tool has state=active with approved_exact_input, call that tool now with exactly approved_exact_input and no changed fields. Do not inspect again, create another preview, or request the same permission before that call, because doing so can replace the approved ephemeral object reference. If no matching active grant exists, adapt to the recorded decision or explain the blocker. This checkpoint grants no authority; the server authorizer still performs the final match.";
                let parent = session
                    .conversation
                    .iter()
                    .rev()
                    .find(|message| message.role == ChatRole::User)
                    .and_then(|message| message.data_envelope.as_ref());
                let mut marker = ChatMessage::system_event(&marker_id, marker_text);
                marker.data_envelope = derive_internal_tool_result_envelope(
                    parent,
                    &marker_id,
                    marker_text,
                    "permission_continuation_checkpoint",
                )?;
                messages.push(marker);
            }
        }
        if empty_end_turn_retries > 0 {
            // Some reasoning-capable OpenAI-compatible providers can end a
            // response after emitting only opaque reasoning, especially after
            // a denied tool call. Retry once with a server-owned recency marker
            // instead of persisting an empty assistant message. The marker does
            // not grant authority or request a tool; it only asks the model to
            // produce a protocol-visible answer or call on the next response.
            let marker_id =
                format!("runtime-empty-end-turn-retry-{turn_id}-{empty_end_turn_retries}");
            let marker_text = "RUNTIME RECOVERY NOTICE (server authoritative): the previous provider response contained reasoning but no assistant text or tool call. Continue the same requirement now with either a visible answer or a valid exposed tool call. This notice grants no permission. If a prior tool was denied for missing authorization, request the exact bounded grant before retrying it.";
            let parent = session
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == ChatRole::User)
                .and_then(|message| message.data_envelope.as_ref());
            let mut marker = ChatMessage::system_event(&marker_id, marker_text);
            marker.data_envelope = derive_internal_tool_result_envelope(
                parent,
                &marker_id,
                marker_text,
                "empty_end_turn_retry",
            )?;
            messages.push(marker);
        }
        if truncated_turn_retries > 0 {
            // A MaxTokens response can contain partial text or an incomplete
            // tool call. Nothing from that response is persisted or
            // dispatched. Give the provider one bounded chance to continue
            // with a concise, protocol-visible result while preserving the
            // same durable requirement and already-authorized grants.
            let marker_id =
                format!("runtime-truncated-turn-retry-{turn_id}-{truncated_turn_retries}");
            let marker_text = "RUNTIME RECOVERY NOTICE (server authoritative): the previous provider response reached its output-token limit and was discarded before any assistant text or tool call was committed. Continue the same requirement now. Do not repeat prior reasoning. Produce only the minimum valid exposed tool call(s) needed for the next step, or a concise visible answer if no tool is needed. Re-read current authorized grants and preserve approved exact inputs. This notice grants no permission.";
            let parent = session
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == ChatRole::User)
                .and_then(|message| message.data_envelope.as_ref());
            let mut marker = ChatMessage::system_event(&marker_id, marker_text);
            marker.data_envelope = derive_internal_tool_result_envelope(
                parent,
                &marker_id,
                marker_text,
                "truncated_turn_retry",
            )?;
            messages.push(marker);
        }
        // The ids the model is about to see. A pending auto-trigger whose completion
        // message is in this request is cleared once the model reacts to it (the
        // assistant answer / tool-call save below), so it never fires an automation
        // turn for a result the model already handled.
        let request_message_ids: HashSet<String> =
            messages.iter().map(|m| m.message_id.clone()).collect();
        let request = ModelRequest {
            messages,
            tools: specs,
            tool_requirements,
            tool_choice: crate::chat::ToolChoice::Auto,
            response_format: deps.response_format.clone(),
            use_case: crate::model_profile::ModelUseCase::Agent,
            caller_output_hard_cap: None,
        };

        if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
            return Ok(LoopOutcome::Superseded {
                previous_input_revision: session.input_revision,
                current_input_revision,
            });
        }
        let turn = deps.model.call(request, sink).await?;
        if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
            return Ok(LoopOutcome::Superseded {
                previous_input_revision: session.input_revision,
                current_input_revision,
            });
        }
        session.record_step(turn.usage);

        // Reasoning models may spend the entire completion allowance on opaque
        // reasoning and return neither user-visible text nor a tool call. Treat
        // that as an actionable configuration failure instead of the generic
        // retryable truncation: retrying the same budget is deterministic churn.
        if turn.stop_reason == crate::chat::StopReason::MaxTokens
            && turn.provider_meta.reasoning_observed
            && turn.text.trim().is_empty()
            && turn.tool_calls.is_empty()
        {
            if deps.content_safety.is_enforced() {
                sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
            } else {
                sink.on_turn_discarded();
            }
            return Err(reasoning_output_budget_error());
        }

        if turn.stop_reason == crate::chat::StopReason::EndTurn
            && turn.text.trim().is_empty()
            && turn.tool_calls.is_empty()
        {
            if deps.content_safety.is_enforced() {
                sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
            } else {
                sink.on_turn_discarded();
            }
            if empty_end_turn_retries >= 1 {
                return Err(empty_end_turn_recovery_error());
            }
            empty_end_turn_retries += 1;
            continue;
        }

        let disposition = match classify_model_turn(&turn) {
            Ok(disposition) => disposition,
            Err(error) => {
                if deps.content_safety.is_enforced() {
                    sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
                }
                return Ok(LoopOutcome::ProtocolError(error));
            }
        };
        if matches!(
            disposition,
            TurnDisposition::Discard | TurnDisposition::ContextWindowExceeded
        ) {
            if deps.content_safety.is_enforced() {
                sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
            } else {
                sink.on_turn_discarded();
            }
            if disposition == TurnDisposition::Discard && truncated_turn_retries < 1 {
                truncated_turn_retries += 1;
                continue;
            }
            return Ok(match disposition {
                TurnDisposition::Discard => LoopOutcome::Truncated,
                TurnDisposition::ContextWindowExceeded => LoopOutcome::ContextWindowExceeded,
                _ => unreachable!("only non-actionable dispositions enter this branch"),
            });
        }

        let safety_decision = match review_model_turn(deps, &turn).await {
            Ok(decision) => decision,
            Err(error) => {
                sink.on_turn_retracted(
                    StreamRetractionReason::SafetyUnavailable,
                    Some(error.clone()),
                );
                return Ok(LoopOutcome::ContentSafetyUnavailable(error));
            }
        };
        if safety_decision != ContentSafetyDecision::Allow {
            append_refusal_placeholder(session, &mut mint, safety_decision);
            session.clear_reacted_auto_triggers(&request_message_ids);
            deps.session_seam.save(session).await?;
            sink.on_turn_retracted(
                retraction_reason(safety_decision),
                Some(content_blocked_error()),
            );
            return Ok(LoopOutcome::ContentRejected(safety_decision));
        }

        if permission_continuation_pending
            && turn.tool_calls.iter().any(|call| {
                deps.permission_continuation_exact_tools
                    .iter()
                    .any(|name| name == &call.name)
                    && exposed
                        .iter()
                        .any(|tool| tool.name() == call.name && tool.effect == ToolEffect::Mutating)
            })
        {
            permission_continuation_pending = false;
        }

        match disposition {
            TurnDisposition::Answer => {
                let mut message =
                    ChatMessage::text(mint(), crate::chat::ChatRole::Assistant, turn.text.clone())
                        .with_turn_id(session.current_turn_id.clone().unwrap_or_default());
                message.data_envelope = turn.provider_meta.data_envelope.clone();
                session.conversation.push(message);
                // The model reacted to this request; drop any pending auto-trigger
                // whose completion it saw here so it does not also fire a turn.
                session.clear_reacted_auto_triggers(&request_message_ids);
                deps.session_seam.save(session).await?;
                sink.on_answer_committed(&turn.text);
                return Ok(LoopOutcome::Answered(turn.text));
            }
            TurnDisposition::Discard => unreachable!("discard returned before safety review"),
            TurnDisposition::ContextWindowExceeded => {
                unreachable!("context-window stop returned before safety review")
            }
            TurnDisposition::InvokeTools => {
                // Record the assistant's tool-call message so the conversation
                // stays well-formed when replayed to the model.
                let refs = turn.tool_calls.iter().map(|c| c.to_ref()).collect();
                let replay = turn
                    .provider_meta
                    .replay
                    .clone()
                    .expect("classify_model_turn requires tool-call replay disposition");
                let mut message = ChatMessage::assistant_tool_calls_with_replay(
                    mint(),
                    turn.text.clone(),
                    refs,
                    replay,
                )
                .with_turn_id(session.current_turn_id.clone().unwrap_or_default());
                message.data_envelope = turn.provider_meta.data_envelope.clone();
                session.conversation.push(message);
                // The model reacted to this request (with tool calls); drop any
                // pending auto-trigger whose completion it saw here. Persisted with
                // the tool results at the save below.
                session.clear_reacted_auto_triggers(&request_message_ids);
                // Persist every assistant tool-call and its replay disposition
                // before any tool lifecycle event or external action.
                deps.session_seam.save(session).await?;
                if deps.content_safety.is_enforced() {
                    sink.on_partial_committed();
                }

                // Mutating tools in one turn run serially; once one is rejected,
                // times out, or goes to an unknown outcome, the rest of the turn's
                // calls are not executed (§3). `halted` holds the skip note.
                let mut halted: Option<String> = None;
                for (call_index, call) in turn.tool_calls.iter().enumerate() {
                    if let Some(note) = &halted {
                        append_internal_tool_result(
                            session,
                            turn.provider_meta.data_envelope.as_ref(),
                            mint(),
                            &call.id,
                            note.clone(),
                            "halted_tool_call",
                        )?;
                        continue;
                    }

                    if let Some(current_input_revision) =
                        input_revision_advanced(deps, session).await?
                    {
                        append_superseded_tool_results(
                            session,
                            &turn.tool_calls[call_index..],
                            turn.provider_meta.data_envelope.as_ref(),
                            &mut mint,
                        )?;
                        return Ok(LoopOutcome::Superseded {
                            previous_input_revision: session.input_revision,
                            current_input_revision,
                        });
                    }

                    // Same-tool repeat circuit breaker.
                    let count = same_tool.entry(call.name.clone()).or_insert(0);
                    *count += 1;
                    if *count > deps.max_same_tool_per_turn {
                        // Keep the persisted conversation valid for a follow-up:
                        // every tool call in the assistant message must have a
                        // corresponding result, including calls skipped by this
                        // circuit breaker.
                        for skipped in &turn.tool_calls[call_index..] {
                            append_internal_tool_result(
                                session,
                                turn.provider_meta.data_envelope.as_ref(),
                                mint(),
                                &skipped.id,
                                format!(
                                    "tool `{}` was not run because the per-turn repeat limit was reached",
                                    skipped.name
                                ),
                                "repeat_limit_tool_call",
                            )?;
                        }
                        deps.session_seam.save(session).await?;
                        return Ok(LoopOutcome::CircuitBreak(
                            CircuitBreakReason::SameToolRepeat,
                        ));
                    }

                    // A call naming a tool not exposed under the current scope/state
                    // becomes an error tool-result so the conversation stays
                    // well-formed and the model can adjust.
                    let Some(tool) = lookup_exposed(
                        deps.registry,
                        &call.name,
                        &session.scope_snapshot,
                        &session.execution_state,
                        session.trigger_origin,
                    ) else {
                        append_internal_tool_result(
                            session,
                            turn.provider_meta.data_envelope.as_ref(),
                            mint(),
                            &call.id,
                            format!("tool `{}` is not available in the current scope", call.name),
                            "unavailable_tool_call",
                        )?;
                        continue;
                    };

                    // Some mutation inputs name evidence produced earlier in
                    // this durable run. Resolve those references before any
                    // approval/reservation work so the model cannot turn a
                    // fabricated or cross-run Web source into trusted report
                    // content. The resulting action still carries only the
                    // bounded title/URL projection, never raw search HTML.
                    if let Err(validation_error) =
                        resolve_word_report_web_source_envelope(session, call)
                    {
                        append_internal_tool_result(
                            session,
                            turn.provider_meta.data_envelope.as_ref(),
                            mint(),
                            &call.id,
                            validation_error,
                            "invalid_server_bound_web_evidence",
                        )?;
                        continue;
                    }

                    match tool.effect {
                        ToolEffect::ReadOnly => {
                            // A read tool error is reported back as a tool result;
                            // the backend transport itself does not fail the turn.
                            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                            let (out, ok) = match deps.tools.run_read(call).await {
                                Ok(out) => (out, true),
                                Err(e) => (
                                    crate::seam::ToolRunOutput {
                                        content: if e.safe_for_model {
                                            format!("tool error: {}", e.message)
                                        } else {
                                            "tool error: the tool could not complete".into()
                                        },
                                        image_data_url: None,
                                    },
                                    false,
                                ),
                            };
                            // A model-visible tool error is still data produced by
                            // the selected source. Information-flow-enforced
                            // surfaces must label it before the next model call in
                            // exactly the same way as a successful read result.
                            let mut data_envelope = deps.tools.read_data_envelope(call, &out)?;
                            bind_tool_input_envelopes(session, call, &mut data_envelope)?;
                            let failure = append_reviewed_tool_result(
                                deps,
                                session,
                                mint(),
                                &call.id,
                                out,
                                data_envelope,
                            )
                            .await?;
                            if let Some(failure) = failure {
                                return finish_tool_output_safety_failure(
                                    deps,
                                    session,
                                    &call.id,
                                    &turn.tool_calls[call_index + 1..],
                                    &mut mint,
                                    sink,
                                    failure,
                                )
                                .await;
                            }
                            if let Some(current_input_revision) =
                                input_revision_advanced(deps, session).await?
                            {
                                append_superseded_tool_results(
                                    session,
                                    &turn.tool_calls[call_index + 1..],
                                    turn.provider_meta.data_envelope.as_ref(),
                                    &mut mint,
                                )?;
                                return Ok(LoopOutcome::Superseded {
                                    previous_input_revision: session.input_revision,
                                    current_input_revision,
                                });
                            }
                            finish_tool(session, &call.id, ok, sink);
                            // Persist each read-only result before advancing to the
                            // next call. On a crash, the OSS recovery layer can then
                            // distinguish the still-unstarted calls from any later
                            // durable mutating task instead of treating the whole
                            // assistant batch as an unknown execution.
                            deps.session_seam.save(session).await?;
                        }
                        ToolEffect::Mutating => {
                            if let Some(outcome) = run_mutating(
                                deps,
                                session,
                                turn_id,
                                call,
                                &turn.tool_calls[call_index + 1..],
                                &mut mint,
                                &mut halted,
                                sink,
                            )
                            .await?
                            {
                                return Ok(outcome);
                            }
                        }
                        ToolEffect::WaitTask => {
                            if let Some(outcome) = run_wait(
                                deps,
                                session,
                                call,
                                &turn.tool_calls[call_index + 1..],
                                &mut mint,
                                &mut halted,
                                sink,
                            )
                            .await?
                            {
                                return Ok(outcome);
                            }
                        }
                        ToolEffect::RunProjection => {
                            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                            let updated_at = (deps.clock)();
                            let step_id = stable_lineage_id("status-step", &call.id);
                            let current_revision = session
                                .task_status_projection
                                .as_ref()
                                .map_or(0, |projection| projection.revision);
                            match crate::task_status_tools::build_task_status_projection(
                                call,
                                current_revision,
                                updated_at.clone(),
                                step_id,
                            ) {
                                Ok(projection) => {
                                    let content = serde_json::json!({
                                        "status": "updated",
                                        "revision": projection.revision,
                                        "item_count": projection.items.len(),
                                    })
                                    .to_string();
                                    let result_envelope = derive_internal_tool_result_envelope(
                                        turn.provider_meta.data_envelope.as_ref(),
                                        &call.id,
                                        &content,
                                        crate::task_status_tools::UPDATE_TASK_STATUS_TOOL_NAME,
                                    )?;
                                    let event_seq = session
                                        .last_event_seq
                                        .checked_add(1)
                                        .ok_or_else(|| AgentError {
                                            kind: AgentErrorKind::Internal,
                                            message: "agent run event sequence exhausted".into(),
                                            retryable: false,
                                            safe_for_model: false,
                                            error_code: None,
                                        })?;
                                    let event = crate::dynamic_run::TaskStatusUpdatedEvent {
                                        event: crate::dynamic_run::AgentRunEvent {
                                            schema_version: crate::dynamic_run::AGENT_RUN_EVENT_SCHEMA_VERSION,
                                            event_id: stable_lineage_id(
                                                "status-event",
                                                &format!(
                                                    "{}:{event_seq}:{}",
                                                    session.conversation_id, call.id
                                                ),
                                            ),
                                            run_id: session.conversation_id.clone(),
                                            event_seq,
                                            input_revision: session.input_revision,
                                            kind: crate::dynamic_run::AgentRunEventKind::TaskStatusUpdated,
                                            correlation_id: Some(call.id.clone()),
                                            source_envelope_ids: turn
                                                .provider_meta
                                                .data_envelope
                                                .as_ref()
                                                .map(|envelope| vec![envelope.envelope_id.clone()])
                                                .unwrap_or_default(),
                                            result_envelope_ids: result_envelope
                                                .as_ref()
                                                .map(|envelope| vec![envelope.envelope_id.clone()])
                                                .unwrap_or_default(),
                                            created_at: updated_at,
                                        },
                                        projection: projection.clone(),
                                    };
                                    event.validate().map_err(|error| AgentError {
                                        kind: AgentErrorKind::Internal,
                                        message: format!(
                                            "invalid task-status update event: {error}"
                                        ),
                                        retryable: false,
                                        safe_for_model: false,
                                        error_code: None,
                                    })?;
                                    session.task_status_projection = Some(projection);
                                    session.last_event_seq = event_seq;
                                    let mut message =
                                        ChatMessage::tool_result(mint(), &call.id, content);
                                    message.data_envelope = result_envelope;
                                    session.conversation.push(message);
                                    deps.session_seam
                                        .save_task_status_update(session, &event)
                                        .await?;
                                    finish_tool(session, &call.id, true, sink);
                                }
                                Err(error) => {
                                    let content = format!("tool error: {}", error.message);
                                    let envelope = derive_internal_tool_result_envelope(
                                        turn.provider_meta.data_envelope.as_ref(),
                                        &call.id,
                                        &content,
                                        crate::task_status_tools::UPDATE_TASK_STATUS_TOOL_NAME,
                                    )?;
                                    let mut message =
                                        ChatMessage::tool_result(mint(), &call.id, content);
                                    message.data_envelope = envelope;
                                    session.conversation.push(message);
                                    deps.session_seam.save(session).await?;
                                    finish_tool(session, &call.id, false, sink);
                                }
                            }
                        }
                        ToolEffect::PermissionPlanning => {
                            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                            let created_at = (deps.clock)();
                            let request_id = stable_lineage_id(
                                "permission-request",
                                &format!(
                                    "{}:{}:{}:{}",
                                    session.conversation_id,
                                    session.input_revision,
                                    session.last_event_seq.saturating_add(1),
                                    call.id
                                ),
                            );
                            let request = deps.provider_registry.ok_or_else(|| AgentError {
                                kind: AgentErrorKind::Internal,
                                message: "permission planning has no Provider catalog".into(),
                                retryable: false,
                                safe_for_model: false,
                                error_code: None,
                            }).and_then(|registry| {
                                crate::permission_tools::build_permission_request(
                                    call,
                                    registry,
                                    request_id.clone(),
                                    session.input_revision,
                                    created_at.clone(),
                                )
                            }).and_then(|request| {
                                validate_browser_permission_references(
                                    &session.conversation,
                                    &request,
                                )?;
                                let inventory = deps.capability_inventory.ok_or_else(|| AgentError {
                                    kind: AgentErrorKind::Internal,
                                    message: "permission planning has no live capability inventory".into(),
                                    retryable: false,
                                    safe_for_model: false,
                                    error_code: None,
                                })?;
                                crate::permission_tools::validate_request_availability(
                                    &request,
                                    inventory,
                                    deps.registry,
                                )?;
                                Ok(request)
                            });
                            match request {
                                Ok(request) => {
                                    if let Some(existing) = session
                                        .permission_requests
                                        .iter()
                                        .rev()
                                        .find(|existing| {
                                            crate::permission_tools::equivalent_permission_request(
                                                existing, &request,
                                            )
                                        })
                                        .cloned()
                                    {
                                        let decision_state = match existing.state {
                                            crate::dynamic_run::PermissionRequestState::Pending => {
                                                "pending"
                                            }
                                            crate::dynamic_run::PermissionRequestState::NeedsRevalidation => {
                                                "needs_revalidation"
                                            }
                                            crate::dynamic_run::PermissionRequestState::Approved => {
                                                "approved"
                                            }
                                            crate::dynamic_run::PermissionRequestState::PartiallyApproved => {
                                                "partially_approved"
                                            }
                                            crate::dynamic_run::PermissionRequestState::Denied => {
                                                "denied"
                                            }
                                            crate::dynamic_run::PermissionRequestState::Replaced => {
                                                "replaced"
                                            }
                                            crate::dynamic_run::PermissionRequestState::Withdrawn => {
                                                "withdrawn"
                                            }
                                        };
                                        let content = serde_json::json!({
                                            "status": "existing_permission_request",
                                            "decision_state": decision_state,
                                            "request_id": existing.request_id,
                                            "authority": "unchanged",
                                            "message": "An authority-equivalent permission batch already exists for this input revision. Do not request it again; use the current authorization snapshot or adapt to the recorded decision."
                                        })
                                        .to_string();
                                        let envelope = derive_internal_tool_result_envelope(
                                            turn.provider_meta.data_envelope.as_ref(),
                                            &call.id,
                                            &content,
                                            crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                                        )?;
                                        let mut message =
                                            ChatMessage::tool_result(mint(), &call.id, content);
                                        message.data_envelope = envelope;
                                        session.conversation.push(message);
                                        finish_tool(session, &call.id, true, sink);
                                        if existing.state.can_user_decide() {
                                            append_unstarted_tool_results(
                                                session,
                                                &turn.tool_calls[call_index + 1..],
                                                turn.provider_meta.data_envelope.as_ref(),
                                                &mut mint,
                                                "not executed: waiting for the existing user permission decision",
                                                "permission_pause_tool_call",
                                            )?;
                                            deps.session_seam.save(session).await?;
                                            sink.on_permission_requested(
                                                &existing.request_id,
                                                existing.items.len(),
                                            );
                                            return Ok(LoopOutcome::PermissionRequested {
                                                request_id: existing.request_id,
                                            });
                                        }
                                        deps.session_seam.save(session).await?;
                                        continue;
                                    }
                                    let content = serde_json::json!({
                                        "status": "pending_user_decision",
                                        "request_id": request.request_id,
                                        "item_count": request.items.len(),
                                        "authority": "none"
                                    })
                                    .to_string();
                                    let result_envelope = derive_internal_tool_result_envelope(
                                        turn.provider_meta.data_envelope.as_ref(),
                                        &call.id,
                                        &content,
                                        crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                                    )?;
                                    let event_seq = session
                                        .last_event_seq
                                        .checked_add(1)
                                        .ok_or_else(|| AgentError {
                                            kind: AgentErrorKind::Internal,
                                            message: "agent run event sequence exhausted".into(),
                                            retryable: false,
                                            safe_for_model: false,
                                            error_code: None,
                                        })?;
                                    let event = crate::dynamic_run::PermissionRequestedEvent {
                                        event: crate::dynamic_run::AgentRunEvent {
                                            schema_version: crate::dynamic_run::AGENT_RUN_EVENT_SCHEMA_VERSION,
                                            event_id: stable_lineage_id(
                                                "permission-event",
                                                &format!(
                                                    "{}:{event_seq}:{}",
                                                    session.conversation_id, request.request_id
                                                ),
                                            ),
                                            run_id: session.conversation_id.clone(),
                                            event_seq,
                                            input_revision: session.input_revision,
                                            kind: crate::dynamic_run::AgentRunEventKind::PermissionRequested,
                                            correlation_id: Some(request.request_id.clone()),
                                            source_envelope_ids: turn
                                                .provider_meta
                                                .data_envelope
                                                .as_ref()
                                                .map(|envelope| vec![envelope.envelope_id.clone()])
                                                .unwrap_or_default(),
                                            result_envelope_ids: result_envelope
                                                .as_ref()
                                                .map(|envelope| vec![envelope.envelope_id.clone()])
                                                .unwrap_or_default(),
                                            created_at,
                                        },
                                        request: request.clone(),
                                    };
                                    event.validate().map_err(|error| AgentError {
                                        kind: AgentErrorKind::Internal,
                                        message: format!(
                                            "invalid permission request event: {error}"
                                        ),
                                        retryable: false,
                                        safe_for_model: false,
                                        error_code: None,
                                    })?;
                                    let mut message =
                                        ChatMessage::tool_result(mint(), &call.id, content);
                                    message.data_envelope = result_envelope;
                                    session.conversation.push(message);
                                    append_unstarted_tool_results(
                                        session,
                                        &turn.tool_calls[call_index + 1..],
                                        turn.provider_meta.data_envelope.as_ref(),
                                        &mut mint,
                                        "not executed: waiting for user permission decision",
                                        "permission_pause_tool_call",
                                    )?;
                                    session.add_permission_request(request.clone()).map_err(
                                        |error| AgentError {
                                            kind: AgentErrorKind::Internal,
                                            message: format!(
                                                "invalid persisted permission request: {error}"
                                            ),
                                            retryable: false,
                                            safe_for_model: false,
                                            error_code: None,
                                        },
                                    )?;
                                    session.last_event_seq = event_seq;
                                    deps.session_seam
                                        .save_permission_request(session, &event)
                                        .await?;
                                    finish_tool(session, &call.id, true, sink);
                                    sink.on_permission_requested(
                                        &request.request_id,
                                        request.items.len(),
                                    );
                                    return Ok(LoopOutcome::PermissionRequested {
                                        request_id: request.request_id,
                                    });
                                }
                                Err(error) => {
                                    let content = format!("tool error: {}", error.message);
                                    let envelope = derive_internal_tool_result_envelope(
                                        turn.provider_meta.data_envelope.as_ref(),
                                        &call.id,
                                        &content,
                                        crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
                                    )?;
                                    let mut message =
                                        ChatMessage::tool_result(mint(), &call.id, content);
                                    message.data_envelope = envelope;
                                    session.conversation.push(message);
                                    deps.session_seam.save(session).await?;
                                    finish_tool(session, &call.id, false, sink);
                                }
                            }
                        }
                    }
                }
                deps.session_seam.save(session).await?;
                // Loop again with the tool results in context.
            }
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

fn stable_lineage_id(prefix: &str, value: &str) -> String {
    format!("{prefix}-{:x}", Sha256::digest(value.as_bytes()))
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

fn verified_browser_result(message: &ChatMessage) -> Option<BrowserActionResult> {
    if message.role != ChatRole::Tool {
        return None;
    }
    if let Ok(completion) = serde_json::from_str::<ComputerActionCompleted>(&message.text)
        && completion.result == ComputerActionResultClass::Verified
        && let Some(ComputerActionOutput::Browser(result)) = completion.output
        && result.validate().is_ok()
    {
        return Some(result);
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
    let result = serde_json::from_str::<BrowserActionResult>(&message.text).ok()?;
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
    let mut source_envelopes = Vec::new();
    for message in conversation.iter().rev() {
        if selected.len() >= MAX_REQUESTED_ARTIFACTS {
            break;
        }
        let Ok(completion) = serde_json::from_str::<ComputerActionCompleted>(&message.text) else {
            continue;
        };
        let Some(ComputerActionOutput::FileArtifact(artifact)) = completion.output else {
            continue;
        };
        if artifact.validate().is_err()
            || !latest_user.text.contains(&artifact.file_name)
            || !seen_tokens.insert(artifact.file.token.clone())
        {
            continue;
        }
        selected.push(artifact);
        if let Some(envelope) = message.data_envelope.as_ref() {
            source_envelopes.push(envelope.clone());
        }
    }
    if selected.is_empty() {
        return Ok(None);
    }
    selected.reverse();

    let payload = serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "kind": "requested_artifact_registry",
        "artifacts": selected,
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
/// chain on every Device Assistant model request. This must also run for an
/// ordinary user follow-up: context compression or a new turn can otherwise
/// retain the requirement while dropping the worker-issued BrowserPageRef.
/// Historical raw Provider results deliberately are not replayed across the
/// egress boundary; the projection keeps only the worker-retained merge
/// preview id, the exact same-run Web Search call reference it must pass to
/// reviewed artifact Providers, and up to four recent validated browser page
/// identities plus at most 32 prioritized closed element refs per origin needed
/// for subsequent semantic browser actions. No rows, snippets, raw DOM, page
/// titles, native paths, arbitrary result text, or authority are copied.
/// Downstream calls must still pass the current grant, selected-object,
/// same-run result, source-pair, page-incarnation and expiry checks.
fn reusable_provider_result_projection(
    conversation: &[ChatMessage],
    message_id: &str,
    now_unix_ms: u64,
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
        if message.role != ChatRole::Tool {
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
        if preview.is_none()
            && envelope.provenance.source_tool_name == "preview_spreadsheet_merge"
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.text)
            && let Some(preview_id) = value
                .pointer("/ReadContext/SpreadsheetMergePreview/preview_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 160)
        {
            preview = Some(serde_json::json!({"preview_id": preview_id}));
            preview_envelope = Some(envelope.clone());
        }
        if web_search.is_none()
            && envelope.provenance.source_tool_name == "search_public_web"
            && let Some(call_id) = message.tool_call_id.as_deref()
            && conversation.iter().any(|candidate| {
                candidate.role == ChatRole::Assistant
                    && candidate
                        .tool_calls
                        .iter()
                        .any(|call| call.id == call_id && call.name == "search_public_web")
            })
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.text)
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
                        .then(|| serde_json::json!({"title": title, "url": url}))
                })
                .take(MAX_WEB_SOURCES)
                .collect::<Vec<_>>();
            if !sources.is_empty() {
                web_search = Some(serde_json::json!({
                    "web_search_call_id": call_id,
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
                    let semantic_priority = [
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
                    .then_some(0)
                    .unwrap_or(1);
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
                browser_results.push(serde_json::json!({
                    "page": result.page,
                    "elements": elements,
                }));
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

    let payload = serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "kind": "reusable_provider_result_registry",
        "spreadsheet_merge_preview": preview,
        "web_search": web_search,
        "browser_results": browser_results,
    }))
    .map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("encode reusable Provider result registry: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let text = format!(
        "CURRENT REUSABLE PROVIDER RESULTS (server authoritative bounded references; not a grant; copy opaque ids verbatim and never invent them): {payload}"
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
                | "browser_fill_form"
                | "browser_activate_element"
                | "prepare_gmail_web_draft_handoff"
                | "prepare_slack_web_message_handoff"
        ) {
            continue;
        }
        let canonical = item.canonical_input_json.as_deref().ok_or_else(|| AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!(
                "invalid request_capability_grants arguments: tool `{}` requires exact browser input",
                item.tool_name
            ),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })?;
        let value: serde_json::Value = serde_json::from_str(canonical).map_err(|_| AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!(
                "invalid request_capability_grants arguments: tool `{}` has invalid exact browser input",
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
                        "invalid request_capability_grants arguments: tool `{}` is missing its exact page reference",
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
                "invalid request_capability_grants arguments: tool `{}` has an invalid exact page reference",
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
            if message.role != ChatRole::Tool
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
                    "invalid request_capability_grants arguments: tool `{}` must copy its exact page and element references from one unexpired verified browser result in this run",
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

/// Emit a tool's terminal UI event from the authoritative result that was just
/// appended to the conversation. Tool output reaches the UI through the same
/// redacted, bounded path used for model context.
fn finish_tool(
    session: &crate::session::PersistedAgentSession,
    call_id: &str,
    ok: bool,
    sink: &mut dyn TurnSink,
) {
    let result = session
        .conversation
        .iter()
        .rev()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id));
    let output = result
        .map(|message| message.text.as_str())
        .unwrap_or_default();
    let background_task_id = result.and_then(|message| message.background_task_id.as_deref());
    sink.on_tool_finished(call_id, ok, output, background_task_id);
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
        || session.execution_state.waitable_task() != Some(action)
        || matches!(&session.execution_state, ExecutionState::OutcomeUnknown { placeholder_message_id, .. }
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
        content: message.text.clone(),
        image_data_url: message.image_data_url.clone(),
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
                        && candidate.arguments_json == call.arguments_json
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

    // Selected-object tools consume the edge-held objects represented by the
    // active context attachments, not model-supplied paths or envelope ids.
    if call.name.contains("selected") || call.name == "preview_spreadsheet_merge" {
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
        let preview_match = serde_json::from_str::<serde_json::Value>(&message.text)
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
    if call.name != "create_word_report_from_merge_preview" {
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
    let result_envelope = result
        .data_envelope
        .as_ref()
        .filter(|envelope| envelope.provenance.source_tool_name == "search_public_web")
        .ok_or_else(|| "Word report Web Search result has no authoritative envelope".to_string())?;
    let result_json: serde_json::Value = serde_json::from_str(&result.text)
        .map_err(|_| "Word report Web Search result is not valid structured data".to_string())?;
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
) -> Result<Option<LoopOutcome>, AgentError> {
    // Defence in depth behind the exposure gate: a mutating tool is never
    // advertised to an automation turn ([`lookup_exposed`] would already reject
    // it), but should one still reach here it is refused before any work is
    // created, so a completion can never self-trigger a new command.
    if !session.trigger_origin.allows_new_mutation() {
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

    let ctx = ExecContext {
        assistant_turn_fence: crate::action_turn_fence::AssistantTurnFence::from_session(session)?,
        conversation_id: session.conversation_id.clone(),
        turn_id: turn_id.to_string(),
        tool_call_id: call.id.clone(),
        actor_id: session.actor_id.clone(),
        policy_revision: session.policy_revision,
        scope: session.scope_snapshot.clone(),
        // Route the approval preview back to the connection that started the turn.
        connection_id: session.active_control_connection_id.clone(),
    };

    // Persist "awaiting approval" before the wait so the pending decision is
    // observable across instances; restore Running once the seam returns.
    session.turn_state = TurnState::AwaitingApproval;
    deps.session_seam.save(session).await?;
    sink.on_awaiting_approval(&call.name, &call.id, &call.arguments_json);
    let version = crate::action_version::ActionVersion::capture(session, &call.id)?;
    let completion = deps
        .tools
        .confirm_and_exec_versioned(call, &ctx, version.as_ref())
        .await;
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
                    *halted = Some("not executed: a prior action in this group failed".into());
                }
            }
        }
        Ok(ExecOutcome::Rejected { reason }) => {
            let text = match reason {
                Some(r) => format!("the operator rejected this command: {r}"),
                None => "the operator rejected this command".to_string(),
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
            session.execution_state = ExecutionState::OutcomeUnknown {
                action: id,
                placeholder_message_id: placeholder_id,
                since: (deps.clock)(),
            };
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command's outcome is unknown".to_string());
        }
        Ok(ExecOutcome::Dispatched(id)) => {
            // Background task model: close the tool call now with a task-id result (a
            // well-formed message the loop never rewrites) and record the outstanding
            // dispatch. The real result is appended later as a completion
            // notification. The conversation stays usable — a result is coming — but
            // no second mutation starts until this one finishes (`Executing` blocks
            // `allows_new_mutation`).
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::background_task_running(mint(), &call.id, &id.action_request_id),
            )?;
            session.execution_state = ExecutionState::Executing { action: id };
            finish_tool(session, &call.id, true, sink);
            *halted = Some(
                "a prior command in this turn is still running as a background task".to_string(),
            );
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
                *halted = Some("not executed: a prior command failed".to_string());
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
) -> Result<Option<LoopOutcome>, AgentError> {
    // A model-safe argument error becomes an error tool result; the turn continues.
    let task_id = match crate::wait_tools::parse_wait_task_id(call) {
        Ok(id) => id,
        Err(e) => {
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
    let Some(action) = session.execution_state.waitable_task().cloned() else {
        append_mutating_result(
            deps,
            session,
            call,
            ChatMessage::tool_result(
                mint(),
                &call.id,
                "no background task is running; there is nothing to wait for",
            ),
        )?;
        return Ok(None);
    };
    if task_id != action.action_request_id {
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
                (deps.clock)(),
            ) {
                return Err(invalid_original_result());
            }
            let text = if failed {
                "background task failed; its original result is recorded in the conversation"
            } else {
                "background task completed; its original result is recorded in the conversation"
            };
            let envelope = crate::model_message_labels::internal_tool_result_envelope(
                Some(&data_envelope),
                &call.id,
                text,
                "wait_for_task_status",
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
            session.execution_state = ExecutionState::None;
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
            if matches!(session.execution_state, ExecutionState::Executing { .. }) {
                let original = &mut session.conversation[index];
                original.data_envelope =
                    crate::model_message_labels::internal_tool_result_envelope(
                        original.data_envelope.as_ref(),
                        &original_call_id,
                        OUTCOME_UNKNOWN_PLACEHOLDER,
                        "background_task_outcome_unknown",
                    )?;
                original.text = OUTCOME_UNKNOWN_PLACEHOLDER.into();
                session.execution_state = ExecutionState::OutcomeUnknown {
                    action,
                    placeholder_message_id: original.message_id.clone(),
                    since: (deps.clock)(),
                };
            }
            append_mutating_result(
                deps,
                session,
                call,
                ChatMessage::tool_result(mint(), &call.id, OUTCOME_UNKNOWN_PLACEHOLDER),
            )?;
            finish_tool(session, &call.id, false, sink);
            *halted = Some("a prior command's outcome is unknown".into());
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
            session.execution_state = ExecutionState::OutcomeUnknown {
                action,
                placeholder_message_id: placeholder_id,
                since: (deps.clock)(),
            };
            finish_tool(session, &call.id, false, sink);
            *halted = Some("a prior command's outcome is unknown".to_string());
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
