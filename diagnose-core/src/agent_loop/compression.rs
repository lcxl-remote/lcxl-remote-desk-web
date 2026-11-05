//! Provider compression and fenced checkpoint acceptance.
use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_checkpoint(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
    turn_id: &str,
    pinned_context: &mut crate::model_context::PinnedContextPolicy,
    compression_attempted: &mut HashSet<String>,
    template: &ModelRequest,
    authority: Option<&crate::grant_disclosure::GrantDisclosureSnapshot>,
    sink: &mut dyn TurnSink,
    planning_policy: &crate::model_context::PinnedContextPolicy,
    authority_observed_at: Option<u64>,
    configuration_changed: bool,
    plan: Box<crate::model_context::CompressionPlan>,
    compatible_state: &crate::model_context::ModelContextState,
) -> Result<crate::model_context::ModelContextView, AgentError> {
    if planning_policy.anthropic_prefix_binding {
        let protected = crate::thinking_context::protected_replay_ids(session);
        if session.conversation.iter().any(|message| protected.contains(&message.message_id)
            && matches!(&message.replay_disposition, Some(crate::replay::ReplayDisposition::Present { envelope })
                if envelope.codec == crate::replay::ReplayCodec::AnthropicContentBlocks)) {
            return Err(AgentError { kind: AgentErrorKind::InvalidInput,
                message: "Anthropic thinking is bound to an active tool-chain prefix. Compression must wait until this chain finishes; no summary request was sent.".into(),
                retryable: false, safe_for_model: true, error_code: None });
        }
    }
    if configuration_changed {
        return Err(AgentError { kind: AgentErrorKind::InvalidInput,
            message: "The current model cannot fit this conversation, including its existing summary, tools and output reserve. Select a model with a larger context or explicitly compress using the previous compatible configuration. No history was removed.".into(),
            retryable: false, safe_for_model: true,
            error_code: Some(desk_utils::error::DeskErrorCode::AI_CONTEXT_ITEM_TOO_LARGE.code()) });
    }
    use crate::seam::ContextCompressionFailureKind as FailureKind;

    let audit_context = compression_audit_context(&plan, &deps.content_safety);
    let compression_identity = format!(
        "compression-{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &session.conversation_id,
                session.input_revision,
                plan.generation,
                &plan.policy.source_context_key,
                &plan.covered_from_message_id,
                &plan.covered_through_message_id,
                &plan.covered_projection_sha256,
            ))
            .map_err(|_| crate::subagent::invalid("compression identity cannot be encoded"))?
        )
    );
    if !compression_attempted.insert(compression_identity.clone()) {
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

    deps.model.on_context_compression_started(
        plan.generation,
        plan.covered_message_count,
        plan.input_model_context_cost,
    );
    let authorized_input = match deps.model.model_egress_policy()? {
        Some(policy) => match crate::model_context::authorize_compression_input(
            &policy,
            &plan,
            &session.model_conversation(),
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
    let mut request = Box::new(ModelRequest {
        observation: deps.model.observation_context(
            crate::model_profile::ModelUseCase::ContextCompression,
            crate::model_observability::Origin::System,
        ),
        messages: authorized_input.as_ref().map_or_else(
            || crate::model_context::compression_request_messages(&plan),
            |input| input.messages.clone(),
        ),
        tools: Vec::new(),
        tool_requirements: crate::model_capability::ModelRequirements::TEXT_ONLY,
        tool_choice: crate::chat::ToolChoice::None,
        response_format: crate::prompt::ResponseFormatSpec::None,
        use_case: crate::model_profile::ModelUseCase::ContextCompression,
        thinking_prefix: None,
        protected_replay_message_ids: Default::default(),
        context_conversation_id: Some(session.conversation_id.clone()),
        pinned_source_context_key: None,
        ui_references: None,
        result_locators: Default::default(),
        previous_cache_projection: None,
        delegation_call: None,
        preparation: None,
        assistant_data: None,
        compression_identity: None,
        issue_reply: None,
        caller_output_hard_cap: Some(i64::from(plan.policy.summary_max_output_tokens)),
    });
    let _observation_preflight =
        crate::model_observability::PreflightObservation::new(request.observation.clone());
    request.compression_identity = Some(compression_identity.clone());
    if let Some(prepared) = deps.model.prepare_request(&request)? {
        prepared.require_fit()?;
        request.preparation = Some(Box::new(prepared));
    }
    let identity = compression_identity;
    crate::future::boxed(|| assistant_data::prepare_request(deps, session, &mut request)).await?;
    let budget = call_budget::reserve_model(
        deps,
        session,
        &mut request,
        &identity,
        crate::subagent::reservation::DelegationCallKind::ContextSummary,
    )
    .await?;
    let mut compression_sink = crate::seam::NullTurnSink;
    let compression_observation = request.observation.clone();
    let dial_result = phases::dial_request(deps.model, request, &mut compression_sink).await;
    call_budget::settle_model(deps, session, budget, &dial_result).await?;
    let turn = match dial_result {
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
    session.record_auxiliary_usage(turn.usage);
    let disposition = match classify_model_turn(&turn) {
        Ok(disposition) => disposition,
        Err(_) => {
            if let Some(context) = &compression_observation {
                context.output(
                    crate::model_observability::OutputOutcome::InvalidProtocol,
                    crate::model_observability::tool::now_ms(),
                );
            }
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
        TurnDisposition::Answer | TurnDisposition::InvokeTools => Some(FailureKind::InvalidSchema),
    };
    if let Some(kind) = disposition_failure {
        if let Some(context) = &compression_observation {
            context.output(
                match disposition {
                    TurnDisposition::Discard => {
                        crate::model_observability::OutputOutcome::OutputTruncated
                    }
                    TurnDisposition::ContextWindowExceeded => {
                        crate::model_observability::OutputOutcome::ContextLimit
                    }
                    _ => crate::model_observability::OutputOutcome::InvalidStructuredOutput,
                },
                crate::model_observability::tool::now_ms(),
            );
        }
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
            if let Some(context) = &compression_observation {
                context.output(
                    crate::model_observability::OutputOutcome::InvalidStructuredOutput,
                    crate::model_observability::tool::now_ms(),
                );
            }
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
            crate::model_context::bind_context_summary_lineage(policy, &mut validated, &turn, input)
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
        Ok(ContentSafetyDecision::Allow) => {
            if let Some(context) = &compression_observation {
                context.output(
                    crate::model_observability::OutputOutcome::Accepted,
                    crate::model_observability::tool::now_ms(),
                );
            }
        }
        Ok(_) | Err(_) => {
            if let Some(context) = &compression_observation {
                context.output(
                    crate::model_observability::OutputOutcome::PolicyRejected,
                    crate::model_observability::tool::now_ms(),
                );
            }
            return Err(report_context_compression_failure(
                deps.model,
                FailureKind::UnsafeOutput,
                Some(audit_context),
                Some(compression_usage),
            )
            .await);
        }
    }
    if !lease_is_current(deps).await {
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
            crate::model_context::bind_context_summary_lineage(policy, &mut validated, &turn, input)
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
        &session.model_conversation(),
        compatible_state,
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
            let cost = u64::try_from(crate::trim::model_context_cost(message)).map_err(|_| ())?;
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
    let candidate_now = authority
        .map(|_| current_unix_ms(deps.clock))
        .transpose()?
        .unwrap_or(0);
    let authority_expired = authority.is_some_and(|snapshot| {
        snapshot.grants.iter().any(|grant| {
            authority_observed_at.is_some_and(|observed| grant.expires_at_unix_ms > observed)
                && grant.expires_at_unix_ms <= candidate_now
        })
    });
    if input_revision_advanced(deps, session).await?.is_some()
        || authority_expired
        || if let Some(expected) = authority {
            deps.tools.current_grant_disclosure().await?.as_ref() != Some(expected)
        } else {
            false
        }
    {
        return Err(report_context_compression_failure(
            deps.model,
            FailureKind::StaleContext,
            Some(audit_context),
            Some(compression_usage),
        )
        .await);
    }
    let candidate_check = assemble_context_request(template, &view, session).and_then(|request| {
        if let Some(prepared) = deps.model.prepare_request(&request)? {
            prepared.require_fit()?;
        }
        Ok(())
    });
    if let Err(error) = candidate_check {
        let failure = if error.error_code
            == Some(desk_utils::error::DeskErrorCode::AI_CONTEXT_ITEM_TOO_LARGE.code())
        {
            FailureKind::InputTooLarge
        } else {
            FailureKind::StaleContext
        };
        let _ = report_context_compression_failure(
            deps.model,
            failure,
            Some(audit_context),
            Some(compression_usage),
        )
        .await;
        return Err(error);
    }
    let previous_state = session.model_context_state.clone();
    let previous_notices = session.context_notices.clone();
    let previous_observation = session.latest_model_context.clone();
    session.model_context_state = next_state;
    if let Some(observation) = &mut session.latest_model_context {
        observation.stale = true;
    }
    session.record_context_notice(
        crate::model_context::ContextNotice::compacted(turn_id, generation, covered_message_count),
        (deps.clock)(),
    );
    if !lease_is_current(deps).await {
        session.model_context_state = previous_state;
        session.context_notices = previous_notices;
        session.latest_model_context = previous_observation;
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
        session.latest_model_context = previous_observation;
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
        .audit_context_compression(crate::seam::ContextCompressionAuditOutcome::Committed {
            context: audit_context,
            usage: compression_usage,
            summary_context_cost,
            final_context_cost,
        })
        .await;
    sink.on_context_compacted(turn_id, generation, covered_message_count);
    *pinned_context = planning_policy.clone();
    Ok(view)
}
