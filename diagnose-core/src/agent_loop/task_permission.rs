//! Publish an exact, undispatched scheduled operation for owner approval.
use super::*;

pub(super) async fn pause<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    held_session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    remaining: &[crate::chat::ToolCall],
    request: crate::dynamic_run::PermissionRequest,
    mint: &mut F,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    // Publish from a staged copy. Validation or persistence failure must not
    // leave an in-memory pending request for the outer error path to save.
    let mut staged = held_session.clone();
    let session = &mut staged;
    let invalid = || AgentError {
        kind: AgentErrorKind::Internal,
        message: "invalid scheduled permission candidate".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    };
    request.validate().map_err(|_| invalid())?;
    let canonical = crate::permission_tools::canonical_tool_permission_input_json(
        &call.name,
        serde_json::from_str(&call.arguments_json).map_err(|_| invalid())?,
    )
    .map_err(|_| invalid())?;
    if session.trigger_origin != crate::session::TriggerOrigin::ScheduledTask
        || request.state != crate::dynamic_run::PermissionRequestState::Pending
        || request.input_revision != session.input_revision
        || request.items.len() != 1
        || request.items[0].item_id != call.id
        || request.items[0].tool_name != call.name
        || request.items[0].suggested_max_uses != 1
        || request.items[0].canonical_input_json.as_deref() != Some(canonical.as_str())
        || !crate::schedule::contract::exception::unique_original_call(&session.conversation, call)
        || !session.unclosed_tool_call_ids().contains(&call.id)
    {
        return Err(invalid());
    }
    let parent = session
        .conversation
        .iter()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|original| original == &call.to_ref())
        })
        .and_then(|message| message.data_envelope.clone());
    let content =
        serde_json::json!({ "status": "pending_user_decision", "request_id": request.request_id,
        "item_count": 1, "authority": "none", "executed": false })
        .to_string();
    append_mutating_result(
        deps,
        session,
        call,
        ChatMessage::tool_result(mint(), &call.id, content),
    )?;
    let result_ids = session
        .conversation
        .last()
        .and_then(|message| message.data_envelope.as_ref())
        .map(|label| vec![label.envelope_id.clone()])
        .unwrap_or_default();
    append_unstarted_tool_results(
        session,
        remaining,
        parent.as_ref(),
        mint,
        "not executed: waiting for user permission decision",
        "permission_pause_tool_call",
    )?;
    let event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
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
            source_envelope_ids: parent
                .map(|label| vec![label.envelope_id])
                .unwrap_or_default(),
            result_envelope_ids: result_ids,
            created_at: request.created_at.clone(),
        },
        request: request.clone(),
    };
    event.validate().map_err(|_| invalid())?;
    session
        .add_permission_request(request.clone())
        .map_err(|_| invalid())?;
    session.last_event_seq = event_seq;
    deps.session_seam
        .save_permission_request(session, &event)
        .await?;
    finish_tool(session, &call.id, true, sink);
    sink.on_permission_requested(&request.request_id, 1);
    *held_session = staged;
    Ok(LoopOutcome::PermissionRequested {
        request_id: request.request_id,
    })
}
