//! Plan, publish and pause on one bounded permission proposal.
use super::*;

/// Turn-local inputs the permission planner needs besides the session itself.
pub(super) struct ProposalContext<'a> {
    pub authority: Option<&'a crate::grant_disclosure::GrantDisclosureSnapshot>,
    pub request_devices: &'a [crate::request_devices::RequestDevice],
    pub selected_reads: &'a [(RegisteredTool, bool)],
    pub step_inventory: Option<&'a [crate::capability_availability::CapabilityAvailability]>,
    pub permission_candidates: &'a [RegisteredTool],
    pub remaining: &'a [crate::chat::ToolCall],
    pub parent_envelope: Option<&'a desk_agent_protocol::data_lineage::DataEnvelope>,
}

/// Validate `call` as a permission proposal, record it and pause the turn.
/// A rejected proposal is returned to the model as this call's result.
/// `shown` is the model's own call: the explicit `request_permissions` call, or
/// the direct tool call from which `call` was synthesized with the same id.
#[allow(clippy::too_many_arguments)]
pub(super) async fn propose<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    shown: &crate::chat::ToolCall,
    context: ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    let ProposalContext {
        authority,
        request_devices,
        selected_reads,
        step_inventory,
        permission_candidates,
        remaining,
        parent_envelope,
    } = context;
    let synthesized = shown.name != crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME;
    let source_name = if synthesized {
        crate::permission_tools::CALL_PERMISSION_RESULT_SOURCE
    } else {
        crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME
    };
    sink.on_tool_started(&shown.name, &shown.id, &shown.arguments_json);
    let created_at = (deps.clock)();
    let request_id = stable_lineage_id(
        "permission-request",
        &(
            &session.conversation_id,
            session.input_revision,
            session.last_event_seq.saturating_add(1),
            &call.id,
        ),
    );
    let selected_request = crate::future::boxed(|| {
        crate::request_devices::plan_device_permission(
            deps.tools,
            deps.provider_registry,
            authority.as_ref().map(|snapshot| snapshot.surface),
            session,
            request_devices,
            selected_reads,
            call,
            &request_id,
            &created_at,
        )
    })
    .await;
    let request = selected_request.unwrap_or_else(|| deps.provider_registry.ok_or_else(|| AgentError {
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
                            }).and_then(|mut request| {
                                let providers = deps.provider_registry.ok_or_else(|| AgentError {
                                    kind: AgentErrorKind::Internal,
                                    message: "permission planning has no Provider catalog".into(),
                                    retryable: false,
                                    safe_for_model: false,
                                    error_code: None,
                                })?;
                                let inventory = step_inventory.ok_or_else(|| AgentError {
                                    kind: AgentErrorKind::Internal,
                                    message: "permission planning has no live capability inventory".into(),
                                    retryable: false,
                                    safe_for_model: false,
                                    error_code: None,
                                })?;
                                // An interactive command needs the owner's live terminal
                                // at approval time; unattended turns cannot provide one.
                                if session.trigger_origin != crate::session::TriggerOrigin::User
                                    && request.items.iter().any(|item| {
                                        item.command_confirmation
                                            .as_ref()
                                            .is_some_and(|confirmation| confirmation.is_interactive())
                                    })
                                {
                                    return Err(AgentError {
                                        kind: AgentErrorKind::PermissionDenied,
                                        message: "interactive (PTY) commands need the owner's live terminal and cannot be requested from a goal or scheduled run; use a non-interactive command".into(),
                                        retryable: false,
                                        safe_for_model: true,
                                        error_code: None,
                                    });
                                }
                                crate::application_ui::bind_request(&mut request, &session.conversation)?;
                                crate::permission_tools::include_desktop_action_reads(&mut request, providers)?;
                                validate_permission_request_availability(
                                    &session.agent_role,
                                    &session.conversation,
                                    &request,
                                    providers,
                                    inventory,
                                    deps.registry,
                                    permission_candidates,
                                )?;
                                let now = chrono::DateTime::parse_from_rfc3339(&request.created_at)
                                    .ok()
                                    .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
                                    .unwrap_or(0);
                                if request.items.iter().any(|item| crate::provider_preflight::ArtifactCallPreflight::supports(&item.tool_name)) {
                                    let surface = authority.ok_or_else(|| crate::subagent::invalid("artifact permission has no current authorization context"))?.surface;
                                    crate::permission_tools::bind_artifact_request(&mut request, session, providers, surface, now)?;
                                }
                                for item in &request.items {
                                    crate::provider_preflight::text_file::validate_mutation_permission_input(
                                        session,
                                        &item.tool_name,
                                        item.canonical_input_json.as_deref(),
                                        now,
                                    )?;
                                    crate::provider_preflight::text_file::validate_read_permission_input_with_context(
                                        session,
                                        &item.tool_name,
                                        item.canonical_input_json.as_deref(),
                                        now,
                                        deps.tools.original_read_context(),
                                    )?;
                                }
                                Ok(request)
                            }));
    let request = match request {
        Ok(mut request) => crate::application_launch::bind_permission_request(
            &mut request,
            session,
            authority.map_or(0, |snapshot| snapshot.readiness_revision),
            deps.tools,
        )
        .await
        .map(|()| request),
        Err(error) => Err(error),
    };
    let request = match request {
        Ok(request) => deps
            .session_seam
            .validate_task_permission_request(
                session,
                &request,
                deps.provider_registry.ok_or_else(|| AgentError {
                    kind: AgentErrorKind::InvalidInput,
                    message: "Permission provider catalog is unavailable".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                })?,
            )
            .await
            .map(|()| request),
        Err(error) => Err(error),
    };
    tool_observation.preflight_result(&request);
    let request = match request {
        Ok(request) => {
            if let Some((denied_request, denied_item)) =
                crate::permission_tools::unchanged_ai_denied_item(
                    &session.permission_requests,
                    &session.permission_decisions,
                    &request,
                )
            {
                tool_observation
                    .permission(crate::model_observability::PermissionOutcome::PolicyRejected);
                Err(AgentError {
                    kind: AgentErrorKind::InvalidInput,
                    message: format!(
                        "permission_denied: this action is unchanged from AI-denied item `{denied_item}` in request `{denied_request}`. Do not rename or repeat it on the same owner input. Narrow the actual action or ask the owner about this specific rejected operation."
                    ),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                })
            } else {
                Ok(request)
            }
        }
        Err(error) => Err(error),
    };
    match request {
        Ok(request) => {
            // An explicit proposal owns its complete input, so its
            // preparatory schemas retire. A direct call keeps its
            // schema: the model calls it again after approval.
            if !synthesized {
                session
                    .capability_disclosure
                    .loaded_tool_names
                    .retain(|name| !request.items.iter().any(|item| item.tool_name == *name));
            }
            let existing = session
                .permission_requests
                .iter()
                .rev()
                .find(|existing| {
                    crate::permission_tools::equivalent_permission_request(existing, &request)
                })
                .cloned();
            let existing = match existing {
                Some(existing)
                    if existing.state == crate::dynamic_run::PermissionRequestState::Approved
                        && deps
                            .session_seam
                            .permission_request_can_renew(session, &existing)
                            .await? =>
                {
                    None
                }
                other => other,
            };
            if let Some(existing) = existing {
                let actor = session
                    .permission_decisions
                    .iter()
                    .find(|decision| {
                        decision.request_id == existing.request_id
                            && decision.input_revision == existing.input_revision
                    })
                    .map(|decision| decision.actor);
                tool_observation.permission(crate::model_observability::permission::state_outcome(
                    existing.state,
                    actor,
                ));
                let decision_state = match existing.state {
                    crate::dynamic_run::PermissionRequestState::Pending => "pending",
                    crate::dynamic_run::PermissionRequestState::NeedsRevalidation => {
                        "needs_revalidation"
                    }
                    crate::dynamic_run::PermissionRequestState::Approved => "approved",
                    crate::dynamic_run::PermissionRequestState::PartiallyApproved => {
                        "partially_approved"
                    }
                    crate::dynamic_run::PermissionRequestState::Denied => "denied",
                    crate::dynamic_run::PermissionRequestState::Replaced => "replaced",
                    crate::dynamic_run::PermissionRequestState::Withdrawn => "withdrawn",
                };
                let content = crate::permission_tools::existing_request_result(
                    &existing.request_id,
                    decision_state,
                )
                .to_string();
                let envelope = derive_internal_tool_result_envelope(
                    parent_envelope,
                    &call.id,
                    &content,
                    source_name,
                )?;
                let mut message = ChatMessage::tool_result(mint(), &call.id, content);
                message.data_envelope = envelope;
                session.conversation.push(message);
                finish_tool(session, &call.id, true, sink);
                if existing.state.can_user_decide() {
                    append_unstarted_tool_results(
                        session,
                        remaining,
                        parent_envelope,
                        &mut *mint,
                        "not executed: waiting for the existing user permission decision",
                        "permission_pause_tool_call",
                    )?;
                    deps.session_seam.save(session).await?;
                    sink.on_permission_requested(&existing.request_id, existing.items.len());
                    return Ok(Some(LoopOutcome::PermissionRequested {
                        request_id: existing.request_id,
                    }));
                }
                deps.session_seam.save(session).await?;
                return Ok(None);
            }
            let content = if synthesized {
                crate::permission_tools::call_pending_result(
                    &request.request_id,
                    request.items.len(),
                )
            } else {
                serde_json::json!({
                    "status": "pending_user_decision",
                    "request_id": request.request_id,
                    "item_count": request.items.len(),
                    "authority": "none"
                })
            }
            .to_string();
            let result_envelope = derive_internal_tool_result_envelope(
                parent_envelope,
                &call.id,
                &content,
                source_name,
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
                approval_mode_revision: None,
                event: crate::dynamic_run::AgentRunEvent {
                    schema_version: crate::dynamic_run::AGENT_RUN_EVENT_SCHEMA_VERSION,
                    event_id: stable_lineage_id(
                        "permission-event",
                        &(&session.conversation_id, event_seq, &request.request_id),
                    ),
                    run_id: session.conversation_id.clone(),
                    event_seq,
                    input_revision: session.input_revision,
                    kind: crate::dynamic_run::AgentRunEventKind::PermissionRequested,
                    correlation_id: Some(request.request_id.clone()),
                    source_envelope_ids: parent_envelope
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
                message: format!("invalid permission request event: {error}"),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })?;
            let mut message = ChatMessage::tool_result(mint(), &call.id, content);
            message.data_envelope = result_envelope;
            session.conversation.push(message);
            append_unstarted_tool_results(
                session,
                remaining,
                parent_envelope,
                &mut *mint,
                "not executed: waiting for user permission decision",
                "permission_pause_tool_call",
            )?;
            session
                .add_permission_request(request.clone())
                .map_err(|error| AgentError {
                    kind: AgentErrorKind::Internal,
                    message: format!("invalid persisted permission request: {error}"),
                    retryable: false,
                    safe_for_model: false,
                    error_code: None,
                })?;
            session.last_event_seq = event_seq;
            deps.session_seam
                .save_permission_request(session, &event)
                .await?;
            if let Some(alias) = crate::model_observability::ObservationAlias::permission_request(
                &session.conversation_id,
                &request.request_id,
            ) {
                tool_observation.bind(alias);
            }
            tool_observation.permission(crate::model_observability::PermissionOutcome::Waiting);
            finish_tool(session, &call.id, true, sink);
            sink.on_permission_requested(&request.request_id, request.items.len());
            return Ok(Some(LoopOutcome::PermissionRequested {
                request_id: request.request_id,
            }));
        }
        Err(error) => {
            let content = crate::model_input::describe_error(&call.name, &error.message);
            let envelope = derive_internal_tool_result_envelope(
                parent_envelope,
                &call.id,
                &content,
                source_name,
            )?;
            let mut message = ChatMessage::tool_result(mint(), &call.id, content);
            message.data_envelope = envelope;
            session.conversation.push(message);
            if session.trigger_origin == crate::session::TriggerOrigin::ScheduledTask {
                append_unstarted_tool_results(
                    session,
                    remaining,
                    parent_envelope,
                    &mut *mint,
                    "not executed: task permission request was rejected",
                    "task_permission_denied",
                )?;
                finish_tool(session, &call.id, false, sink);
                return Err(error);
            }
            deps.session_seam.save(session).await?;
            finish_tool(session, &call.id, false, sink);
        }
    }

    Ok(None)
}

/// Turn a direct call that lacks a matching grant into a call-time proposal.
/// An owner refusal of this tool for the current input stays final; other
/// refusals are judged again by the planner against the concrete new action.
#[allow(clippy::too_many_arguments)]
pub(super) async fn propose_call<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    context: ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if crate::permission_tools::owner_denied_tool(
        &session.permission_requests,
        &session.permission_decisions,
        &call.name,
        session.input_revision,
    ) {
        tool_observation.permission(crate::model_observability::PermissionOutcome::Denied);
        append_internal_tool_result(
            session,
            context.parent_envelope,
            mint(),
            &call.id,
            "permission_denied: the owner refused this tool for the current request. No action was dispatched. Do not retry it; explain the blocker or ask the owner.".into(),
            "permission_denied",
        )?;
        finish_tool(session, &call.id, false, sink);
        return Ok(None);
    }
    let application_scope_uses = deps.session_seam.application_scope_default_uses().await;
    let planning = deps
        .provider_registry
        .ok_or_else(|| AgentError {
            kind: AgentErrorKind::Internal,
            message: "permission planning has no Provider catalog".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        })
        .and_then(|registry| {
            crate::permission_tools::synthesize_request_call(
                call,
                registry,
                &session.conversation,
                application_scope_uses,
            )
        });
    let planning = planning.and_then(|planning| bind_request_device(planning, call, session));
    let planning = match planning {
        Ok(planning) => planning,
        Err(error) if error.safe_for_model => {
            tool_observation.reject(
                crate::model_observability::Stage::Preflight,
                crate::model_observability::InputIssue::Semantic,
            );
            sink.on_tool_started(&call.name, &call.id, &call.arguments_json);
            append_internal_tool_result(
                session,
                context.parent_envelope,
                mint(),
                &call.id,
                crate::model_input::describe_error(&call.name, &error.message),
                "permission_proposal_rejected",
            )?;
            finish_tool(session, &call.id, false, sink);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    propose(
        deps,
        session,
        &planning,
        call,
        context,
        mint,
        tool_observation,
        sink,
    )
    .await
}

/// Resolve a host's `GrantRequired` answer: published tasks never receive it,
/// and an exact-input mismatch is explained once per turn before a new
/// proposal for the changed arguments is accepted.
#[allow(clippy::too_many_arguments)]
pub(super) async fn grant_required<F: FnMut() -> String>(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
    input_mismatch: bool,
    hinted: &mut HashSet<String>,
    context: ProposalContext<'_>,
    mint: &mut F,
    tool_observation: &crate::model_observability::tool::ToolObservation,
    sink: &mut dyn TurnSink,
) -> Result<Option<LoopOutcome>, AgentError> {
    if !session.trigger_origin.uses_call_time_approval() {
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "a published task received a call-time approval outcome".into(),
            retryable: false,
            safe_for_model: false,
            error_code: None,
        });
    }
    if input_mismatch && hinted.insert(call.name.clone()) {
        tool_observation.permission(crate::model_observability::PermissionOutcome::PolicyRejected);
        append_internal_tool_result(
            session,
            context.parent_envelope,
            mint(),
            &call.id,
            crate::capability_grant::APPROVED_INPUT_MISMATCH_MESSAGE.into(),
            "approved_input_mismatch",
        )?;
        finish_tool(session, &call.id, false, sink);
        return Ok(None);
    }
    propose_call(deps, session, call, context, mint, tool_observation, sink).await
}

/// A direct device call names its device the same way an explicit proposal
/// would: a single admitted target is inferred, and a native UI call takes the
/// device that observed its application.
fn bind_request_device(
    planning: crate::chat::ToolCall,
    call: &crate::chat::ToolCall,
    session: &crate::session::PersistedAgentSession,
) -> Result<crate::chat::ToolCall, AgentError> {
    let mut arguments: serde_json::Value = serde_json::from_str(&planning.arguments_json)
        .map_err(|_| crate::subagent::invalid("invalid proposal"))?;
    if !session.uses_request_device_tools() || arguments.get("target_ref").is_some() {
        return Ok(planning);
    }
    if let Some(inferred) = crate::request_devices::infer_single_target(&planning, session)? {
        return Ok(inferred);
    }
    if !crate::request_devices::implies_target(&call.name, session) {
        return Ok(planning);
    }
    let input: serde_json::Value = serde_json::from_str(&call.arguments_json)
        .map_err(|_| crate::subagent::invalid("invalid tool arguments"))?;
    let mut observed = session
        .assistant_request_targets()
        .unwrap_or_default()
        .iter()
        .filter(|target| {
            crate::request_devices::confirm_implied_target(session, &target.device_id, &input)
                .is_ok()
        });
    if let (Some(target), None) = (observed.next(), observed.next()) {
        arguments["target_ref"] = serde_json::json!(target.reference);
        return Ok(crate::chat::ToolCall {
            arguments_json: arguments.to_string(),
            ..planning
        });
    }
    Ok(planning)
}
