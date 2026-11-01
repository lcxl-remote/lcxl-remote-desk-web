//! Optional naming uses the current caller, lease and physical-call accounting.
use super::*;

pub(super) fn generate<'a, 'b: 'a>(
    deps: &'a LoopDeps<'b>,
    session: &'a mut PersistedAgentSession,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    crate::future::phase(|| async move {
        // Naming never replaces an already committed, reviewed answer.
        let _ = try_generate(deps, session).await;
    })
}

async fn try_generate(
    deps: &LoopDeps<'_>,
    session: &mut PersistedAgentSession,
) -> Result<(), AgentError> {
    if session.surface != AgentSessionSurface::AiAssistant
        || !session.agent_role.is_main()
        || matches!(
            session.trigger_origin,
            TriggerOrigin::ScheduledTask | TriggerOrigin::ScheduledContinuation
        )
        || session.input_revision != 1
        || session.assistant_data.is_none()
    {
        return Ok(());
    }
    let Some(maximum) = deps.session_seam.conversation_title_limit(session).await? else {
        return Ok(());
    };
    let Some(question) = session
        .conversation
        .iter()
        .find(|message| {
            message.role == ChatRole::User
                && message.data_envelope.as_ref().is_some_and(|envelope| {
                    envelope.provenance.source_provider_id == "ai-assistant-user"
                })
        })
        .filter(|message| !message.text.trim().is_empty() && message.image_data_url.is_none())
    else {
        return Ok(());
    };
    let mut request = Box::new(crate::conversation_title::request(
        question.clone(),
        maximum,
    ));
    request.context_conversation_id = Some(session.conversation_id.clone());
    let identity = format!(
        "title:{}",
        crate::conversation_title::call_identity(&request)
            .ok_or_else(|| { crate::subagent::invalid("missing title input identity") })?
    );
    ensure_lease_healthy(deps).await?;
    assistant_data::prepare_request(deps, session, &mut request).await?;
    let context = deps.model.context_policy(request.requirements()).await?;
    let size = deps.model.prepare_request(&request)?.map_or_else(
        || serde_json::to_vec(&request.messages).map_or(usize::MAX, |value| value.len()),
        |prepared| prepared.context_bytes,
    );
    if size > context.max_context_bytes / 2 {
        return Ok(());
    }
    let budget = call_budget::reserve_model(
        deps,
        session,
        &mut request,
        &identity,
        crate::subagent::reservation::DelegationCallKind::Model,
    )
    .await?;
    let result = phases::dial_request(deps.model, request, &mut crate::seam::NullTurnSink).await;
    if let Ok(turn) = &result {
        session.record_auxiliary_usage(turn.usage);
    }
    call_budget::settle_model(deps, session, budget, &result).await?;
    let turn = result?;
    let title = crate::conversation_title::parse(&turn, maximum)
        .map_err(|error| crate::subagent::invalid(&error))?;
    if review_model_turn(deps, &turn).await? != ContentSafetyDecision::Allow {
        return Ok(());
    }
    ensure_lease_healthy(deps).await?;
    deps.session_seam
        .save_conversation_title(session, &title)
        .await
}
