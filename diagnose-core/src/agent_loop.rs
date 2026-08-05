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

use desk_agent_protocol::content_safety::{ContentSafetyDecision, StreamRetractionReason};
use std::collections::{HashMap, HashSet};

use crate::content_safety::{
    ContentSafetyMode, SafetyImage, SafetyModelTurn, SafetyToolCall, content_blocked_error,
    normalize_safety_error, refusal_placeholder_for,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::chat::{ChatMessage, ModelTurnError, TurnDisposition, classify_model_turn};
use crate::registry::{RegisteredTool, ToolEffect, exposed_tools, lookup_exposed};
use crate::seam::{
    ClaimError, ClaimTurnParams, ExecContext, ExecOutcome, ModelRequest, ModelSeam, SessionSeam,
    ToolSeam, TurnSink, WaitOutcome,
};
use crate::session::{ExecutionState, SubjectMismatch, TurnState};

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
    /// A circuit breaker stopped the turn.
    CircuitBreak(CircuitBreakReason),
    /// The model violated the wire contract (inconsistent stop reason / bad args).
    ProtocolError(ModelTurnError),
    /// A turn is already running for this conversation.
    TurnBusy,
    /// The follow-up came from a different subject than the session.
    SubjectRejected(SubjectMismatch),
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
    pub response_format: crate::prompt::ResponseFormatSpec,
    /// The system message prepended to the (trimmed) conversation on every model
    /// call. The caller builds it (with the control-end locale) via
    /// [`crate::agentic_prompt::build_agentic_system_message`]; it is never stored
    /// in the persisted conversation, so a prompt-version bump applies to
    /// in-flight conversations.
    pub system_prompt: ChatMessage,
    /// Byte budget for the conversation history sent to the model (§ trimming).
    /// The system prompt is prepended on top of this and is not counted against it.
    pub max_context_bytes: usize,
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
    if let Some(message) = to_append {
        session.conversation.push(message);
    }
    deps.session_seam.save(&mut session).await?;

    // Run the loop; whatever happens, settle the turn machine and persist once.
    let result = run_inner(deps, &mut session, &turn_id, sink).await;
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
    crate::image_input::strip_session_images(&mut session.conversation);
    // Surface a save failure only if the loop itself otherwise succeeded.
    let save = deps.session_seam.save(&mut session).await;
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

    loop {
        if session.turn_step_budget_exhausted(deps.max_steps_per_turn) {
            return Ok(LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget));
        }

        let exposed = exposed_tools(
            deps.registry,
            &session.scope_snapshot,
            &session.execution_state,
            session.trigger_origin,
        );
        let tool_requirements = crate::model_capability::ModelRequirements::for_registered_tools(
            exposed.iter().copied(),
        );
        let specs = exposed.iter().map(|tool| tool.spec.clone()).collect();
        // Assemble the model request: a freshly built system prompt prepended to a
        // trailing, budget-trimmed window of the stored conversation. The system
        // prompt is never persisted, so it is added here on every call.
        let mut messages = Vec::with_capacity(session.conversation.len() + 1);
        messages.push(deps.system_prompt.clone());
        messages.extend(crate::trim::trim_conversation(
            &session.conversation,
            deps.max_context_bytes,
        ));
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
            max_output_tokens: None,
        };

        let turn = deps.model.call(request, sink).await?;
        session.record_step(turn.usage);

        let disposition = match classify_model_turn(&turn) {
            Ok(disposition) => disposition,
            Err(error) => {
                if deps.content_safety.is_enforced() {
                    sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
                }
                return Ok(LoopOutcome::ProtocolError(error));
            }
        };
        if disposition == TurnDisposition::Discard {
            if deps.content_safety.is_enforced() {
                sink.on_turn_retracted(StreamRetractionReason::Incomplete, None);
            } else {
                sink.on_turn_discarded();
            }
            return Ok(LoopOutcome::Truncated);
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

        match disposition {
            TurnDisposition::Answer => {
                session.conversation.push(ChatMessage::text(
                    mint(),
                    crate::chat::ChatRole::Assistant,
                    turn.text.clone(),
                ));
                // The model reacted to this request; drop any pending auto-trigger
                // whose completion it saw here so it does not also fire a turn.
                session.clear_reacted_auto_triggers(&request_message_ids);
                deps.session_seam.save(session).await?;
                sink.on_answer_committed(&turn.text);
                return Ok(LoopOutcome::Answered(turn.text));
            }
            TurnDisposition::Discard => unreachable!("discard returned before safety review"),
            TurnDisposition::InvokeTools => {
                // Record the assistant's tool-call message so the conversation
                // stays well-formed when replayed to the model.
                let refs = turn.tool_calls.iter().map(|c| c.to_ref()).collect();
                session.conversation.push(ChatMessage::assistant_tool_calls(
                    mint(),
                    turn.text.clone(),
                    refs,
                ));
                // The model reacted to this request (with tool calls); drop any
                // pending auto-trigger whose completion it saw here. Persisted with
                // the tool results at the save below.
                session.clear_reacted_auto_triggers(&request_message_ids);
                if deps.content_safety.is_enforced() {
                    // Enforced turns persist the reviewed assistant tool-call
                    // message before any tool lifecycle event or durable action.
                    deps.session_seam.save(session).await?;
                    sink.on_partial_committed();
                }

                // Mutating tools in one turn run serially; once one is rejected,
                // times out, or goes to an unknown outcome, the rest of the turn's
                // calls are not executed (§3). `halted` holds the skip note.
                let mut halted: Option<String> = None;
                for (call_index, call) in turn.tool_calls.iter().enumerate() {
                    if let Some(note) = &halted {
                        session.conversation.push(ChatMessage::tool_result(
                            mint(),
                            &call.id,
                            note.clone(),
                        ));
                        continue;
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
                            session.conversation.push(ChatMessage::tool_result(
                                mint(),
                                &skipped.id,
                                format!(
                                    "tool `{}` was not run because the per-turn repeat limit was reached",
                                    skipped.name
                                ),
                            ));
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
                        session.conversation.push(ChatMessage::tool_result(
                            mint(),
                            &call.id,
                            format!("tool `{}` is not available in the current scope", call.name),
                        ));
                        continue;
                    };

                    match tool.effect {
                        ToolEffect::ReadOnly => {
                            // A read tool error is reported back as a tool result;
                            // the backend transport itself does not fail the turn.
                            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
                            let (out, ok) = match deps.tools.run_read(call).await {
                                Ok(out) => (out, true),
                                Err(e) => (
                                    crate::seam::ToolRunOutput {
                                        content: format!("tool error: {}", e.message),
                                        image_data_url: None,
                                    },
                                    false,
                                ),
                            };
                            let crate::seam::ToolRunOutput {
                                content,
                                image_data_url,
                            } = out;
                            if let Some(image_data_url) = image_data_url {
                                let info =
                                    crate::image_input::validate_image_data_url(&image_data_url)
                                        .map_err(image_input_error)?;
                                match review_image(deps, &image_data_url, &info.media_type).await {
                                    Ok(ContentSafetyDecision::Allow) => {
                                        let mut msg =
                                            ChatMessage::tool_result(mint(), &call.id, content);
                                        msg.image_data_url = Some(image_data_url);
                                        session.conversation.push(msg);
                                        retain_latest_session_image(session)?;
                                        finish_tool(session, &call.id, ok, sink);
                                    }
                                    Ok(decision) => {
                                        session.conversation.push(ChatMessage::tool_result(
                                            mint(),
                                            &call.id,
                                            "[image and tool result omitted by content safety policy]",
                                        ));
                                        for skipped in turn.tool_calls.iter().skip(call_index + 1) {
                                            session.conversation.push(ChatMessage::tool_result(
                                                mint(),
                                                &skipped.id,
                                                "[tool not run because content safety stopped the turn]",
                                            ));
                                        }
                                        append_refusal_placeholder(session, &mut mint, decision);
                                        deps.session_seam.save(session).await?;
                                        finish_tool(session, &call.id, false, sink);
                                        sink.on_turn_retracted(
                                            retraction_reason(decision),
                                            Some(content_blocked_error()),
                                        );
                                        return Ok(LoopOutcome::ContentRejected(decision));
                                    }
                                    Err(error) => {
                                        session.conversation.push(ChatMessage::tool_result(
                                            mint(),
                                            &call.id,
                                            "[image and tool result omitted because content safety review was unavailable]",
                                        ));
                                        for skipped in turn.tool_calls.iter().skip(call_index + 1) {
                                            session.conversation.push(ChatMessage::tool_result(
                                                mint(),
                                                &skipped.id,
                                                "[tool not run because content safety review was unavailable]",
                                            ));
                                        }
                                        deps.session_seam.save(session).await?;
                                        finish_tool(session, &call.id, false, sink);
                                        sink.on_turn_retracted(
                                            StreamRetractionReason::SafetyUnavailable,
                                            Some(error.clone()),
                                        );
                                        return Ok(LoopOutcome::ContentSafetyUnavailable(error));
                                    }
                                }
                            } else {
                                session.conversation.push(ChatMessage::tool_result(
                                    mint(),
                                    &call.id,
                                    content,
                                ));
                                finish_tool(session, &call.id, ok, sink);
                            }
                        }
                        ToolEffect::Mutating => {
                            run_mutating(
                                deps,
                                session,
                                turn_id,
                                call,
                                &mut mint,
                                &mut halted,
                                sink,
                            )
                            .await?;
                        }
                        ToolEffect::WaitTask => {
                            run_wait(deps, session, call, &mut mint, &mut halted, sink).await?;
                        }
                    }
                }
                deps.session_seam.save(session).await?;
                // Loop again with the tool results in context.
            }
        }
    }
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
    mint: &mut F,
    halted: &mut Option<String>,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    // Defence in depth behind the exposure gate: a mutating tool is never
    // advertised to an automation turn ([`lookup_exposed`] would already reject
    // it), but should one still reach here it is refused before any work is
    // created, so a completion can never self-trigger a new command.
    if !session.trigger_origin.allows_new_mutation() {
        session.conversation.push(ChatMessage::tool_result(
            mint(),
            &call.id,
            "not executed: an automation turn cannot start a new command",
        ));
        finish_tool(session, &call.id, false, sink);
        *halted = Some("not executed: an automation turn cannot start a new command".to_string());
        return Ok(());
    }

    let ctx = ExecContext {
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
    let outcome = deps.tools.confirm_and_exec(call, &ctx).await;
    session.turn_state = TurnState::Running;

    // A stable delivery id the foreground path must ack (consume) after its save, so
    // the background completion publisher does not also deliver the same result.
    // Only the foreground-win result (Executed with a delivery id) is acked; a
    // Dispatched outcome deliberately leaves the delivery pending for the publisher.
    let mut ack_event_id: Option<String> = None;

    match outcome {
        Ok(ExecOutcome::Executed { output, event_id }) => {
            // Key the result message on the stable delivery id when the runtime has
            // one, so a late completion delivery of the same result is recognized as
            // already present (dedup by message_id) rather than appended twice.
            let message_id = match &event_id {
                Some(id) => id.clone(),
                None => mint(),
            };
            ack_event_id = event_id;
            let mut msg = ChatMessage::tool_result(message_id, &call.id, output.content);
            msg.image_data_url = output.image_data_url;
            session.conversation.push(msg);
            retain_latest_session_image(session)?;
            finish_tool(session, &call.id, true, sink);
        }
        Ok(ExecOutcome::Rejected { reason }) => {
            let text = match reason {
                Some(r) => format!("the operator rejected this command: {r}"),
                None => "the operator rejected this command".to_string(),
            };
            session
                .conversation
                .push(ChatMessage::tool_result(mint(), &call.id, text));
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was not run".to_string());
        }
        Ok(ExecOutcome::Cancelled { reason }) => {
            let text = match reason {
                Some(r) => format!("the command was cancelled before it ran: {r}"),
                None => "the command was cancelled before it ran".to_string(),
            };
            session
                .conversation
                .push(ChatMessage::tool_result(mint(), &call.id, text));
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was cancelled".to_string());
        }
        Ok(ExecOutcome::ApprovalTimeout) => {
            session.conversation.push(ChatMessage::tool_result(
                mint(),
                &call.id,
                "approval timed out; the command was not executed",
            ));
            finish_tool(session, &call.id, false, sink);
            *halted = Some("not executed: a prior command in this turn was not run".to_string());
        }
        Ok(ExecOutcome::Unknown(id)) => {
            // §6: close the conversation with a placeholder tool result (so the
            // model history stays well-formed) and record the unknown outcome; a
            // late real result replaces the placeholder in place.
            let placeholder_id = mint();
            session.conversation.push(ChatMessage::tool_result(
                placeholder_id.clone(),
                &call.id,
                OUTCOME_UNKNOWN_PLACEHOLDER,
            ));
            session.execution_state = ExecutionState::OutcomeUnknown {
                work_id: id.work_id,
                execution_id: id.execution_id,
                exec_request_id: id.exec_request_id,
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
            session
                .conversation
                .push(ChatMessage::background_task_running(
                    mint(),
                    &call.id,
                    &id.exec_request_id,
                ));
            session.execution_state = ExecutionState::Executing {
                work_id: id.work_id,
                execution_id: id.execution_id,
                exec_request_id: id.exec_request_id,
            };
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
            session.conversation.push(ChatMessage::tool_result(
                mint(),
                &call.id,
                format!("execution error: {}", e.message),
            ));
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
    deps.session_seam.save(session).await?;

    // Post-save ack: the result is safely stored, so tell the seam the foreground
    // consumed this delivery. Best-effort — if the ack is lost (0 rows / transport
    // error) the background publisher delivers instead, and its append dedups by the
    // delivery id the result message is already keyed on, so it never doubles.
    if let Some(event_id) = ack_event_id {
        let _ = deps.tools.ack_delivery(&event_id).await;
    }

    Ok(())
}

/// Run a `wait_for_task` call: the model actively waits on the background task it
/// dispatched. Validated against the session's own execution identity (a control end
/// can never steer it at another task), then handed to the seam. A completed result
/// becomes this call's real tool result — keyed on the completion's delivery id so a
/// racing publisher delivery dedups — and clears the execution machine so a follow-up
/// may mutate again. A still-running wait closes the call with a "still running"
/// note, leaving the task in flight. An unknown outcome degrades to
/// [`ExecutionState::OutcomeUnknown`] with this call's result as the reconcile
/// placeholder, so a late real result can still land.
#[allow(clippy::too_many_arguments)]
async fn run_wait<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    mint: &mut F,
    halted: &mut Option<String>,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    // A model-safe argument error becomes an error tool result; the turn continues.
    let task_id = match crate::wait_tools::parse_wait_task_id(call) {
        Ok(id) => id,
        Err(e) => {
            session.conversation.push(ChatMessage::tool_result(
                mint(),
                &call.id,
                format!("wait error: {}", e.message),
            ));
            return Ok(());
        }
    };
    // Only the session's own in-flight task may be waited on, matched by its stable
    // id. No task, or a mismatched id, is a well-formed error result.
    let Some((work_id, execution_id, exec_request_id)) = session
        .execution_state
        .waitable_task()
        .map(|(w, e, r)| (w, e.to_string(), r.to_string()))
    else {
        session.conversation.push(ChatMessage::tool_result(
            mint(),
            &call.id,
            "no background task is running; there is nothing to wait for",
        ));
        return Ok(());
    };
    if task_id != exec_request_id {
        session.conversation.push(ChatMessage::tool_result(
            mint(),
            &call.id,
            format!("no running background task with id `{task_id}`"),
        ));
        return Ok(());
    }

    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
    let outcome = deps
        .tools
        .wait_for_task(&exec_request_id, &execution_id)
        .await;
    let mut ack_event_id: Option<String> = None;
    match outcome {
        Ok(WaitOutcome::Completed { output, event_id }) => {
            // Key on the stable delivery id so a racing publisher delivery of the
            // same result dedups instead of appending a second copy.
            let message_id = match &event_id {
                Some(id) => id.clone(),
                None => mint(),
            };
            ack_event_id = event_id;
            let mut msg = ChatMessage::tool_result(message_id, &call.id, output.content);
            msg.image_data_url = output.image_data_url;
            session.conversation.push(msg);
            retain_latest_session_image(session)?;
            // The awaited task settled: a follow-up may mutate again.
            session.execution_state = ExecutionState::None;
            finish_tool(session, &call.id, true, sink);
        }
        Ok(WaitOutcome::StillRunning) => {
            session
                .conversation
                .push(ChatMessage::background_task_running(
                    mint(),
                    &call.id,
                    &exec_request_id,
                ));
            finish_tool(session, &call.id, true, sink);
        }
        Ok(WaitOutcome::Unknown) => {
            // The task was recovered without a result. Degrade to an unknown outcome
            // using this call's own result as the reconcile placeholder, and bar
            // further mutation until a late result reconciles it.
            let placeholder_id = mint();
            session.conversation.push(ChatMessage::tool_result(
                placeholder_id.clone(),
                &call.id,
                OUTCOME_UNKNOWN_PLACEHOLDER,
            ));
            session.execution_state = ExecutionState::OutcomeUnknown {
                work_id,
                execution_id,
                exec_request_id,
                placeholder_message_id: placeholder_id,
                since: (deps.clock)(),
            };
            finish_tool(session, &call.id, false, sink);
            *halted = Some("a prior command's outcome is unknown".to_string());
        }
        Err(e) if e.safe_for_model => {
            session.conversation.push(ChatMessage::tool_result(
                mint(),
                &call.id,
                format!("wait error: {}", e.message),
            ));
            finish_tool(session, &call.id, false, sink);
        }
        Err(e) => {
            deps.session_seam.save(session).await?;
            return Err(e);
        }
    }

    deps.session_seam.save(session).await?;
    if let Some(event_id) = ack_event_id {
        let _ = deps.tools.ack_delivery(&event_id).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
