//! Dispatch one read call and record its result.
use super::*;

/// Per-call inputs of a read besides the session itself.
pub(super) struct ReadCall<'a> {
    pub turn_id: &'a str,
    pub tool: &'a RegisteredTool,
    pub selected_device_read: bool,
    pub selected_browser: bool,
    /// A released approval resume: a missing grant closes the call instead of
    /// proposing again.
    pub replay: bool,
}

/// Run one read. Its result, or a call-time proposal, is recorded on `session`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    read: ReadCall<'_>,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    hinted: &mut HashSet<String>,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let ReadCall {
        turn_id,
        tool,
        selected_device_read,
        selected_browser,
        replay,
    } = read;
    // A read tool error is reported back as a tool result;
    // the backend transport itself does not fail the turn.
    sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
    let external_read = tool.effect == ToolEffect::ExternalRead;
    let browser_device = if selected_browser {
        crate::request_devices::selected_action_device(session, call)?
    } else {
        None
    };
    let pure_preview = tool.effect == ToolEffect::ActionPreview;
    let read_requires_version = !pure_preview
        && (browser_device.is_some()
            || (!external_read && !selected_device_read && deps.tools.read_requires_version(call)));
    let version = if read_requires_version {
        // Durable Provider work compares this persisted action boundary.
        session.turn_state = TurnState::AwaitingApproval;
        deps.session_seam.save(session).await?;
        crate::action_version::ActionVersion::capture_for_device(
            session,
            &call.id,
            browser_device.as_deref().unwrap_or(&session.device_id),
        )?
    } else {
        None
    };
    let completion = if pure_preview {
        crate::seam::ReadCompletion {
            outcome: crate::ai_assistant::validate_preview_call(call).and_then(|content| {
                let data_envelope = derive_internal_tool_result_envelope(
                    context.parent_envelope,
                    &call.id,
                    &content,
                    &call.name,
                )?;
                Ok(crate::seam::ReadOutcome::Completed {
                    output: crate::seam::ToolRunOutput {
                        format: crate::seam::ToolOutputFormat::Json,
                        content,
                        image_data_url: None,
                        document_preview: None,
                    },
                    ok: true,
                    event_id: None,
                    data_envelope,
                    background_task: None,
                })
            }),
            version_advance: None,
        }
    } else if external_read {
        crate::seam::ReadCompletion {
            outcome: deps.tools.run_external_read(session, call).await,
            version_advance: None,
        }
    } else if selected_device_read && !selected_browser {
        crate::seam::ReadCompletion {
            outcome: crate::future::boxed(|| deps.tools.run_request_device_read(session, call))
                .await,
            version_advance: None,
        }
    } else {
        let ctx = ExecContext {
            // Central web reads use the host's original input and lease
            // checks. A device action fence always needs a real device.
            assistant_turn_fence: if session.uses_request_device_tools()
                && crate::web_research::is_web_tool(&call.name)
            {
                None
            } else {
                crate::action_turn_fence::AssistantTurnFence::for_device(
                    session,
                    browser_device.as_deref().unwrap_or(&session.device_id),
                )?
            },
            conversation_id: session.conversation_id.clone(),
            turn_id: turn_id.to_string(),
            tool_call_id: call.id.clone(),
            actor_id: session.actor_id.clone(),
            policy_revision: session.policy_revision,
            scope: session.scope_snapshot.clone(),
            connection_id: session.active_control_connection_id.clone(),
        };
        deps.tools
            .run_read_versioned(call, &ctx, version.as_ref())
            .await
    };
    if let Some(advance) = completion.version_advance {
        advance.apply(session, version.as_ref())?;
    }
    if read_requires_version {
        session.turn_state = TurnState::Running;
    }
    let (mut out, ok, event_id, provided_envelope, background_task) = match completion.outcome {
        Ok(ReadOutcome::GrantRequired { input_mismatch }) => {
            return granted_or_closed(
                deps,
                session,
                call,
                replay,
                input_mismatch,
                hinted,
                context,
                mint,
                tool_observation,
                sink,
            )
            .await;
        }
        Ok(ReadOutcome::PermissionRequired { request }) => {
            return (task_permission::pause(
                deps,
                session,
                call,
                context.remaining,
                request,
                &mut *mint,
                sink,
                tool_observation,
            )
            .await)
                .map(Some);
        }
        Ok(ReadOutcome::Completed {
            output,
            ok,
            event_id,
            data_envelope,
            background_task,
        }) => (output, ok, event_id, data_envelope, background_task),
        Err(e) => (
            crate::model_input::error_output(&call.name, &e),
            false,
            None,
            None,
            None,
        ),
    };
    if let Some(action) = background_task {
        if !ok
            || event_id.is_some()
            || provided_envelope.is_some()
            || out.image_data_url.is_some()
            || out.document_preview.is_some()
        {
            return Err(invalid_original_result());
        }
        append_mutating_result(
            deps,
            session,
            call,
            ChatMessage::background_task_running(mint(), &call.id, &action.action_request_id),
        )?;
        session
            .execution_state
            .insert(ExecutionState::Executing { action });
        finish_tool(session, &call.id, true, sink);
        deps.session_seam.save(session).await?;
        return Ok(None);
    }
    let document_preview = out.document_preview.take();
    if document_preview.is_some() && call.name != "preview_document" {
        return Err(invalid_original_result());
    }
    // A model-visible tool error is still data produced by
    // the selected source. Information-flow-enforced
    // surfaces must label it before the next model call in
    // exactly the same way as a successful read result.
    let mut data_envelope = match provided_envelope {
        Some(envelope) => {
            envelope.validate().map_err(|_| invalid_original_result())?;
            Some(envelope)
        }
        None if selected_device_read && !ok => derive_internal_tool_result_envelope(
            context.parent_envelope,
            &call.id,
            &out.content,
            &call.name,
        )?,
        None => deps.tools.read_data_envelope(call, &out)?,
    };
    bind_tool_input_envelopes(session, call, &mut data_envelope)?;
    let failure =
        append_reviewed_tool_result(deps, session, mint(), &call.id, out, data_envelope).await?;
    if let Some(failure) = failure {
        return (finish_tool_output_safety_failure(
            deps,
            session,
            &call.id,
            context.remaining,
            &mut *mint,
            sink,
            failure,
        )
        .await)
            .map(Some);
    }
    if ok {
        crate::visual_evidence::note_successful_observation(session, &call.id, &call.name)
            .map_err(|message| AgentError {
                kind: AgentErrorKind::Internal,
                message: message.into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })?;
    }
    if let Some(current_input_revision) = input_revision_advanced(deps, session).await? {
        append_superseded_tool_results(
            session,
            context.remaining,
            context.parent_envelope,
            &mut *mint,
        )?;
        return Ok(Some(LoopOutcome::Superseded {
            previous_input_revision: session.input_revision,
            current_input_revision,
        }));
    }
    // Persist each read-only result before advancing to the
    // next call. On a crash, the OSS recovery layer can then
    // distinguish the still-unstarted calls from any later
    // durable mutating task instead of treating the whole
    // assistant batch as an unknown execution.
    deps.session_seam.save(session).await?;
    if let Some(preview) = document_preview.as_ref() {
        let target_ref = selected_device_read
            .then(|| crate::request_devices::selected_read(call).map(|(reference, _)| reference))
            .transpose()?;
        sink.on_document_preview(preview, target_ref.as_deref());
    }
    finish_tool(session, &call.id, ok, sink);
    if let Some(event_id) = event_id {
        let _ = deps.tools.ack_delivery(&event_id).await;
    }

    Ok(None)
}

/// A host found no grant matching this read. A released approval resume never
/// proposes again: the approval no longer covers the call, so it did not run.
#[allow(clippy::too_many_arguments)]
async fn granted_or_closed<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    replay: bool,
    input_mismatch: bool,
    hinted: &mut HashSet<String>,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if !replay {
        return permission_proposal::grant_required(
            deps,
            session,
            call,
            input_mismatch,
            hinted,
            context,
            mint,
            tool_observation,
            sink,
        )
        .await;
    }
    super::released_dispatch::close_unauthorized(session, call, context, mint, sink)?;
    Ok(None)
}
