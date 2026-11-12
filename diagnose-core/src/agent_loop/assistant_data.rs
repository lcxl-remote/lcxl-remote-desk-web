//! Model data ports return labelled observations, never execution authority.

use super::*;
use crate::ai_assistant::{
    data_access::AssistantDataToolAccess,
    data_tools::{self, ParsedDataCall},
    seam::{AssistantDataCall, AssistantDataRead},
};
use crate::chat::ToolCall;
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope, memory::MemoryWriteRequest,
};

pub(super) struct DataToolResult {
    pub message: Box<ChatMessage>,
    pub ok: bool,
    pub replayed: bool,
}

pub(super) mod current;

pub(super) async fn prepare_request(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    request: &mut ModelRequest,
) -> Result<(), AgentError> {
    if session.surface != AgentSessionSurface::AiAssistant || !session.uses_owner_assistant_data() {
        if request.assistant_data.is_some() {
            return Err(invalid(
                "assistant data manifest is invalid on this surface",
            ));
        }
        return Ok(());
    }
    let manifest = deps
        .session_seam
        .assistant_data_request_manifest(session, request, current_unix_ms(deps.clock)?)
        .await?;
    let data_in_use = session.assistant_data.as_ref().is_some_and(|data| {
        !data.read_uses.is_empty()
            || !data.current_memories.is_empty()
            || !data.history_dependencies.is_empty()
    });
    if manifest.is_none()
        && (data_in_use
            || (crate::ai_assistant::data_access::eligible_origin(session)
                && (deps.session_seam.memory().is_some()
                    || deps.session_seam.conversation_search().is_some())))
    {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "audited assistant source preparation is unavailable".into(),
            retryable: true,
            safe_for_model: false,
            error_code: None,
        });
    }
    if manifest.is_none() {
        crate::ai_assistant::seam::AssistantModelDataManifest::validate_absence(request)
            .map_err(invalid)?;
    }
    if let Some(manifest) = &manifest {
        manifest.validate_for(session, request).map_err(invalid)?;
    }
    request.assistant_data = manifest.map(Box::new);
    Ok(())
}

/// Forget commits one atomic batch and returns its tool results to planning.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_forget(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    calls: &[ToolCall],
    targets: &[desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget],
    producer_message_id: &str,
    originally_exposed: bool,
    prior_invocations: u32,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    use desk_agent_protocol::ai_assistant::memory::{DataControlOutcome, DataControlReason};
    let now = current_unix_ms(deps.clock)?;
    let parent = session
        .conversation
        .iter()
        .find(|message| message.message_id == producer_message_id)
        .ok_or_else(|| invalid("missing original control producer"))?;
    let hash = crate::ai_assistant::data_control::tool_group_sha256(std::slice::from_ref(parent))
        .map_err(invalid)?;
    let parent = parent.data_envelope.clone();
    let operation_id = crate::native_ids::stable(
        crate::native_ids::Kind::AssistantDataControl,
        &(
            &session.actor_id,
            &session.conversation_id,
            producer_message_id,
            &hash,
        ),
    );
    if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
        for call in calls {
            append_internal_tool_result(
                session,
                parent.as_ref(),
                crate::native_ids::stable(
                    crate::native_ids::Kind::Message,
                    &(&operation_id, &call.id, "control-superseded"),
                ),
                &call.id,
                "newer owner input superseded this control batch".into(),
                "superseded_tool_call",
            )?;
        }
        return Ok(Some(LoopOutcome::Superseded {
            previous_input_revision: session.input_revision,
            current_input_revision,
        }));
    }
    let lease = ensure_lease_healthy(deps).await;
    let repeat_exceeded = lease.is_ok()
        && u32::try_from(calls.len())
            .ok()
            .and_then(|count| prior_invocations.checked_add(count))
            .is_none_or(|count| count > deps.max_same_tool_per_turn);
    let invoke = async {
        lease?;
        if repeat_exceeded {
            return Err(invalid(
                "memory control batch exceeds the per-turn repeat limit",
            ));
        }
        if !originally_exposed {
            return Err(invalid(
                "memory control was not exposed to the original model request",
            ));
        }
        // The original exposure is binding. The host checks current write
        // authority, or proves an observed target stale without a new read.
        if session.trigger_origin != TriggerOrigin::User {
            return Err(invalid(
                "memory control is unavailable in the original owner scope",
            ));
        }
        let destination = deps
            .model
            .model_egress_policy()?
            .ok_or_else(|| invalid("missing original control destination"))?
            .destination;
        let port = deps
            .session_seam
            .memory_mutation()
            .ok_or_else(|| invalid("memory control storage is unavailable"))?;
        for call in calls {
            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
        }
        let call = crate::ai_assistant::seam::MemoryForgetCall {
            producer_message_id,
            targets,
            operation_id: &operation_id,
            destination: &destination,
            now_unix_ms: now,
        };
        port.forget_group(session, &call).await
    }
    .await;
    let receipt = match invoke {
        Ok(receipt) => receipt,
        Err(error) => {
            for call in calls {
                append_internal_tool_result(
                    session,
                    parent.as_ref(),
                    crate::native_ids::stable(
                        crate::native_ids::Kind::Message,
                        &(&operation_id, &call.id, "control-rejected"),
                    ),
                    &call.id,
                    "memory control did not return a committed receipt".into(),
                    "memory_control_rejected",
                )?;
            }
            deps.session_seam.save(session).await?;
            for call in calls {
                finish_tool(session, &call.id, false, sink);
            }
            if repeat_exceeded {
                return Ok(Some(LoopOutcome::CircuitBreak(
                    CircuitBreakReason::SameToolRepeat,
                )));
            }
            return Err(error);
        }
    };
    if receipt.operation_id != operation_id
        || receipt.reason != DataControlReason::MemoryForget
        || (receipt.outcome != DataControlOutcome::StaleTarget
            && receipt.affected_count as usize != targets.len())
    {
        return Err(invalid(
            "memory control storage returned a different receipt",
        ));
    }
    if receipt.outcome == DataControlOutcome::StaleTarget {
        crate::ai_assistant::data_control::record_stale_memory_forget(
            session,
            producer_message_id,
            &receipt,
        )
        .map_err(invalid)?;
        deps.session_seam.save(session).await?;
        for call in calls {
            finish_tool(session, &call.id, false, sink);
        }
        return Ok(None);
    }
    for call in calls {
        finish_tool(session, &call.id, true, sink);
    }
    Ok(None)
}

pub(super) async fn dispatch(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    call: &ToolCall,
    producer_message_id: &str,
    result_message_id: String,
    parent: Option<&DataEnvelope>,
) -> Result<DataToolResult, AgentError> {
    let now = current_unix_ms(deps.clock)?;
    let mutation = call.name == data_tools::MANAGE_MEMORIES;
    let access = if mutation {
        if crate::ai_assistant::memory_access::usable_memory_scope(session, 0).is_none()
            || deps.session_seam.memory_mutation().is_none()
        {
            return Err(invalid("memory mutation has no original owner scope"));
        }
        // The host may return an authenticated no-write result after revocation.
        // Do not rewrite the producer's saved generations before that check.
        deps.session_seam
            .assistant_data_access(session, now)
            .await?
            .ok_or_else(|| invalid("assistant data access is unavailable"))?
    } else {
        fresh_access(deps, session, now).await?
    };
    let advertised = data_tools::for_session(
        session,
        &access.settings,
        deps.session_seam.memory().is_some(),
        deps.session_seam.memory_mutation().is_some(),
        deps.session_seam.conversation_search().is_some(),
        now,
    );
    if !mutation && !advertised.iter().any(|tool| tool.name() == call.name) {
        return Err(invalid(
            "assistant data tool is unavailable in the original scope",
        ));
    }
    let reservation = if call.name != data_tools::MANAGE_MEMORIES {
        match retrieval_accounting::reserve(deps, session, producer_message_id, call).await? {
            retrieval_accounting::Admission::New(reservation) => Some(reservation),
            retrieval_accounting::Admission::Replay(message) => {
                let ok = message.tool_ok.unwrap_or(true);
                return Ok(DataToolResult {
                    message,
                    ok,
                    replayed: true,
                });
            }
        }
    } else {
        None
    };
    let maximum = reservation.as_ref().map_or_else(
        || {
            session.assistant_data.as_ref().map_or_else(
                || {
                    desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits::default()
                        .response_bytes
                },
                |data| data.limits.retrieval.episode.response_bytes,
            )
        },
        |value| value.upper_bytes,
    );
    let logical_id = reservation
        .as_ref()
        .and_then(|value| value.id.as_deref())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crate::native_ids::stable(
                crate::native_ids::Kind::AssistantReadAuthorization,
                &(&session.conversation_id, producer_message_id, &call.id),
            )
        });
    let result = invoke(
        deps,
        session,
        call,
        producer_message_id,
        &logical_id,
        maximum,
        parent,
        now,
    )
    .await;
    let (mut message, ok) = match result {
        Ok((content, sources)) => {
            let mut message = Box::new(ChatMessage::tool_result(
                result_message_id.clone(),
                &call.id,
                content,
            ));
            message.data_envelope =
                if mutation && data_tools::is_memory_write_acknowledgement(&message.text) {
                    crate::model_message_labels::internal_tool_result_envelope(
                        parent,
                        &call.id,
                        &message.text,
                        &call.name,
                    )?
                } else {
                    crate::model_message_labels::assistant_data_result_envelope(
                        parent,
                        &sources,
                        &call.id,
                        &message.text,
                        &call.name,
                        &session
                            .assistant_data
                            .as_ref()
                            .ok_or_else(|| invalid("missing assistant data state"))?
                            .retrieval_budget
                            .episode_id,
                    )?
                };
            (message, true)
        }
        Err(error) if error.safe_for_model => (
            Box::new(error_result(
                parent,
                call,
                &result_message_id,
                &error.message,
            )?),
            false,
        ),
        Err(error) => return Err(error),
    };
    message.turn_id = session.current_turn_id.clone();
    if let Some(reservation) = &reservation {
        if !retrieval_accounting::fits(reservation, &message) {
            message = Box::new(error_result(
                parent,
                call,
                &result_message_id,
                "Response exceeds the framed byte limit; narrow the source window. No source content was returned.",
            )?);
            message.turn_id = session.current_turn_id.clone();
            message.tool_ok = Some(false);
            retrieval_accounting::settle(session, reservation, &message)?;
            return Ok(DataToolResult {
                message,
                ok: false,
                replayed: false,
            });
        }
        retrieval_accounting::settle(session, reservation, &message)?;
    } else if crate::ai_assistant::history::serialized_tool_response_bytes(
        &call.id,
        &message.text,
        None,
    )
    .map_err(invalid)?
        > maximum
    {
        return Err(invalid("memory maintenance response is too large"));
    }
    message.tool_ok = Some(ok);
    Ok(DataToolResult {
        message,
        ok,
        replayed: false,
    })
}

pub(super) async fn fresh_access(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    now: u64,
) -> Result<AssistantDataToolAccess, AgentError> {
    let access = deps
        .session_seam
        .assistant_data_access(session, now)
        .await?
        .ok_or_else(|| invalid("assistant data access is unavailable"))?;
    let can_read = crate::ai_assistant::data_access::eligible_origin(session);
    let data = session
        .assistant_data
        .as_mut()
        .ok_or_else(|| invalid("missing assistant data state"))?;
    if data.control_generation != access.control_generation
        || data.memory_state_generation != access.memory_state_generation
        || (!can_read
            && (!data.read_uses.is_empty()
                || !data.current_memories.is_empty()
                || !data.history_dependencies.is_empty()))
    {
        data.read_uses.clear();
        data.current_memories.clear();
        data.history_dependencies.clear();
        data.control_generation = access.control_generation;
        data.memory_state_generation = access.memory_state_generation;
        deps.session_seam.save(session).await?;
    }
    Ok(access)
}

#[allow(clippy::too_many_arguments)]
async fn invoke(
    deps: &LoopDeps<'_>,
    session: &PersistedAgentSession,
    call: &ToolCall,
    producer_message_id: &str,
    logical_call_id: &str,
    maximum_response_bytes: u32,
    parent: Option<&DataEnvelope>,
    now: u64,
) -> Result<(String, Vec<DataEnvelope>), AgentError> {
    let policy = deps
        .model
        .model_egress_policy()?
        .ok_or_else(|| invalid("assistant data needs an audited model destination"))?;
    let parent =
        parent.ok_or_else(|| invalid("assistant data invocation has no producer evidence"))?;
    let retrieval_reservation = if call.name != data_tools::MANAGE_MEMORIES {
        let reservation = session
            .assistant_data
            .as_ref()
            .and_then(|data| {
                data.retrieval_budget
                    .reservations
                    .iter()
                    .find(|entry| entry.logical_call_id == logical_call_id)
            })
            .ok_or_else(|| {
                invalid("assistant data invocation has no durable retrieval reservation")
            })?;
        Some(reservation)
    } else {
        None
    };
    let limits = &session
        .assistant_data
        .as_ref()
        .ok_or_else(|| invalid("missing assistant data policy snapshot"))?
        .limits;
    let arguments = data_tools::parse_with_limits(call, limits, &session.ui_references)?;
    let context = AssistantDataCall {
        session,
        tool_call: call,
        arguments: &arguments,
        producer_message: session
            .conversation
            .iter()
            .find(|message| message.message_id == producer_message_id)
            .ok_or_else(|| invalid("missing original data producer"))?,
        logical_call_id,
        retrieval_reservation,
        producer_output_envelope: parent,
        maximum_response_bytes,
        destination: &policy.destination,
        now_unix_ms: now,
    };
    match &arguments {
        ParsedDataCall::SearchMemories(request) => {
            // A frozen narrower scope is an answerable limit, not a turn failure.
            if request.scope == AssistantDeviceScope::AllMyDevices
                && crate::ai_assistant::memory_access::usable_memory_scope(session, now)
                    .is_some_and(|scope| scope.device_rule != AssistantDeviceScope::AllMyDevices)
            {
                return Err(invalid(
                    "memory device scope exceeds original authorization; search current_device",
                ));
            }
            project(
                deps.session_seam
                    .memory()
                    .ok_or_else(|| invalid("memory storage is unavailable"))?
                    .search_memories(context, request)
                    .await?,
            )
        }
        ParsedDataCall::ReadMemoryHistory(request) => project(
            deps.session_seam
                .memory()
                .ok_or_else(|| invalid("memory storage is unavailable"))?
                .read_memory_history(context, request)
                .await?,
        ),
        ParsedDataCall::ManageMemory(request) => {
            if matches!(request, MemoryWriteRequest::Forget { .. }) {
                return Err(invalid(
                    "forget requires an independent complete control batch",
                ));
            }
            let result = deps
                .session_seam
                .memory_mutation()
                .ok_or_else(|| invalid("memory maintenance is unavailable"))?
                .manage_memory(context, request)
                .await?;
            project(result)
        }
        ParsedDataCall::ReadMemorySource { source_id, .. } => project(
            deps.session_seam
                .memory()
                .ok_or_else(|| invalid("memory storage is unavailable"))?
                .read_memory_source(context, source_id)
                .await?,
        ),
        ParsedDataCall::SearchConversations(request) => {
            crate::ai_assistant::history::validate_search_scope(
                crate::ai_assistant::data_access::usable_history_scope(session, now)
                    .ok_or_else(|| invalid("original history scope is unavailable"))?,
                request,
                now,
            )
            .map_err(invalid)?;
            project(
                deps.session_seam
                    .conversation_search()
                    .ok_or_else(|| invalid("conversation search is unavailable"))?
                    .search_conversations(context, request)
                    .await?,
            )
        }
        ParsedDataCall::ReadConversation(request) => project(
            deps.session_seam
                .conversation_search()
                .ok_or_else(|| invalid("conversation search is unavailable"))?
                .read_conversation(context, request)
                .await?,
        ),
    }
}

fn project<T: serde::Serialize>(
    read: AssistantDataRead<T>,
) -> Result<(String, Vec<DataEnvelope>), AgentError> {
    let content = serde_json::to_string(&read.result)
        .map_err(|_| invalid("assistant result serialization failed"))?;
    Ok(match read.projection {
        Some(proof) => (content, vec![proof.snapshot_envelope().map_err(invalid)?]),
        None => (content, Vec::new()),
    })
}

fn error_result(
    parent: Option<&DataEnvelope>,
    call: &ToolCall,
    message_id: &str,
    error: &str,
) -> Result<ChatMessage, AgentError> {
    let content = crate::model_input::describe_error(&call.name, error);
    let mut message = ChatMessage::tool_result(message_id, &call.id, content);
    message.tool_ok = Some(false);
    message.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        parent,
        &call.id,
        &message.text,
        &call.name,
    )?;
    Ok(message)
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests;
