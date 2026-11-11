//! Run the call an approval resume released, before the model is called again.
//!
//! The model issued this exact call once. Its approval decides whether it runs;
//! the model never has to repeat it. Every host authorization and dispatch check
//! still applies, and a call the approval no longer covers is closed, never
//! proposed again.
use super::*;

/// Closed result when the approval no longer covers the released call.
const NOT_COVERED: &str = "not executed: the approval no longer covers this call (it expired, was used up, or its target changed); call it again if still needed";

/// Dispatch the released call, record its only result and clear the release.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    exposed: &[&RegisteredTool],
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    hinted: &mut HashSet<String>,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let Some(released) = session.released_permission_call.clone() else {
        return Ok(None);
    };
    // Providers may reuse call ids across turns: use the latest issuing
    // message and only results recorded after it.
    let issued = session
        .conversation
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (message.role == ChatRole::Assistant)
                .then(|| {
                    message
                        .tool_calls
                        .iter()
                        .find(|call| call.id == released.tool_call_id)
                        .map(|call| (index, call.clone(), message.data_envelope.clone()))
                })
                .flatten()
        });
    let outcome = match issued {
        Some((index, call, parent))
            if !session.conversation[index + 1..].iter().any(|message| {
                message.tool_call_id.as_deref() == Some(released.tool_call_id.as_str())
            }) =>
        {
            let call = crate::chat::ToolCall {
                id: call.id,
                name: call.name,
                arguments_json: call.arguments_json,
            };
            let context = permission_proposal::ProposalContext {
                remaining: &[],
                parent_envelope: parent.as_ref(),
                ..context
            };
            dispatch(
                deps, session, turn_id, exposed, &released, &call, context, mint, hinted, sink,
            )
            .await?
        }
        _ => None,
    };
    session.released_permission_call = None;
    deps.session_seam.save(session).await?;
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
async fn dispatch<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    exposed: &[&RegisteredTool],
    released: &crate::session::AwaitingPermission,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    hinted: &mut HashSet<String>,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if let Some(refusal) = refusal(session, released) {
        close(session, call, context, mint, sink, refusal)?;
        return Ok(None);
    }
    let Some(tool) = exposed
        .iter()
        .find(|tool| tool.name() == call.name)
        .copied()
    else {
        close(session, call, context, mint, sink, NOT_COVERED)?;
        return Ok(None);
    };
    let resolved = match crate::ui_model_ids::resolve_session_call_with_context(
        call,
        session,
        if crate::ui_model_ids::needs_resolution(&call.name) {
            current_unix_ms(deps.clock)?
        } else {
            0
        },
        deps.tools.original_read_context(),
    ) {
        Ok(resolved) => resolved,
        Err(_) => {
            close(session, call, context, mint, sink, NOT_COVERED)?;
            return Ok(None);
        }
    };
    let observation = crate::model_observability::tool::ToolObservation::default();
    match tool.effect {
        ToolEffect::ReadOnly | ToolEffect::ExternalRead | ToolEffect::ActionPreview => {
            crate::future::boxed(|| {
                read_call::run(
                    deps,
                    session,
                    &resolved,
                    read_call::ReadCall {
                        turn_id,
                        tool,
                        selected_device_read: crate::request_devices::is_selected_read_tool(
                            tool, session,
                        ),
                        selected_browser: crate::request_devices::is_selected_action_tool(
                            tool, session,
                        ),
                        replay: true,
                    },
                    context,
                    mint,
                    hinted,
                    &observation,
                    sink,
                )
            })
            .await
        }
        ToolEffect::Mutating => {
            let mut halted = None;
            let mut grant_required = None;
            let outcome = crate::future::boxed(|| {
                run_mutating(
                    deps,
                    session,
                    turn_id,
                    &resolved,
                    &[],
                    mint,
                    &mut halted,
                    &mut grant_required,
                    sink,
                    &observation,
                )
            })
            .await?;
            if grant_required.is_some() {
                close(session, call, context, mint, sink, NOT_COVERED)?;
            }
            Ok(outcome)
        }
        _ => {
            close(session, call, context, mint, sink, NOT_COVERED)?;
            Ok(None)
        }
    }
}

/// The recorded refusal of this call's approval item, if any.
fn refusal(
    session: &crate::session::PersistedAgentSession,
    released: &crate::session::AwaitingPermission,
) -> Option<&'static str> {
    let item = session
        .permission_decisions
        .iter()
        .find(|decision| decision.request_id == released.request_id)
        .and_then(|decision| {
            decision
                .items
                .iter()
                .find(|item| item.item_id == released.tool_call_id)
        });
    match item {
        Some(item) if item.approved => None,
        Some(item) if item.actor == crate::dynamic_run::PermissionDecisionActor::Owner => {
            Some("not executed: the owner refused this call")
        }
        Some(item) if item.actor == crate::dynamic_run::PermissionDecisionActor::AiApproval => {
            Some(
                "not executed: AI review refused this call; see the approval record for the reason",
            )
        }
        _ => Some("not executed: this call was not approved"),
    }
}

/// A released call that cannot run under its approval is closed, not proposed.
pub(super) fn close_unauthorized<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    sink: &mut dyn TurnSink,
) -> Result<(), AgentError> {
    close(session, call, context, mint, sink, NOT_COVERED)
}

fn close<F: FnMut() -> String>(
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: permission_proposal::ProposalContext<'_>,
    mint: &mut F,
    sink: &mut dyn TurnSink,
    text: &str,
) -> Result<(), AgentError> {
    append_internal_tool_result(
        session,
        context.parent_envelope,
        mint(),
        &call.id,
        text.into(),
        "permission_resume_not_executed",
    )?;
    finish_tool(session, &call.id, false, sink);
    Ok(())
}
