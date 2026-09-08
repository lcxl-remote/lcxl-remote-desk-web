//! A committed intent without an original transport binding is not replayable.
use super::*;

pub(super) async fn reconcile_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    outbox: &agent_capability_dispatch_outbox::Model,
    work: &agent_action_item::Model,
    payload: &CapabilityDispatchPayload,
    now_ms: u64,
) -> Result<(String, ActionIdentity), DbErr> {
    if payload.command_origin.is_some()
        || payload.command_receipt.is_some()
        || outbox.computer_binding_json.is_some()
        || outbox.computer_acceptance_json.is_some()
        || outbox.computer_background_json.is_some()
        || work.result_json.is_some()
        || work.result_schema_version.is_some()
        || payload.input_revision != session.input_revision
        || payload.input_watermark != session.latest_input_seq
        || !matches!(
            (outbox.state.as_str(), work.status.as_str()),
            (DISPATCH_OUTBOX_PENDING, CAPABILITY_WORK_INTENT_RECORDED)
                | (DISPATCH_OUTBOX_SENDING, CAPABILITY_WORK_DISPATCHING)
                | (
                    DISPATCH_OUTBOX_OUTCOME_UNKNOWN,
                    CAPABILITY_WORK_OUTCOME_UNKNOWN
                )
        )
    {
        return Err(invalid());
    }
    let grant_row = agent_capability_grant::Entity::find()
        .filter(agent_capability_grant::Column::GrantId.eq(&payload.grant_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let grant = decode_grant(&grant_row)?;
    if grant.actor_id != session.actor_id
        || grant.run_id != session.conversation_id
        || grant.target_device_id != session.device_id
        || grant.input_revision != session.input_revision
        || grant.provider_id != payload.provider_id
        || grant.capability_id != payload.capability_id
        || grant.tool_name != payload.tool_name
        || grant.policy_revision != work.policy_revision
        || grant.effect.is_side_effecting() != work.is_side_effecting
        || grant
            .canonical_input_digest_sha256
            .as_ref()
            .is_some_and(|digest| digest != &payload.canonical_input_digest_sha256)
    {
        return Err(invalid());
    }
    let proposals: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id.as_deref() == Some(work.turn_id.as_str())
        })
        .flat_map(|message| message.tool_calls.iter().map(move |call| (message, call)))
        .filter(|(_, call)| {
            stable_id(
                "capability-call",
                &format!("{}:{}:{}", session.conversation_id, work.turn_id, call.id),
            ) == payload.call_id
        })
        .collect();
    if proposals.len() != 1 || proposals[0].1.name != payload.tool_name {
        return Err(invalid());
    }
    let (parent, call) = proposals[0];
    let canonical = desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
        &call.name,
        serde_json::from_str(&call.arguments_json).map_err(|_| invalid())?,
    )
    .map_err(|_| invalid())?;
    if canonical != payload.canonical_input_json {
        return Err(invalid());
    }
    let call_id = call.id.clone();
    let action = ActionIdentity::new(
        work.id,
        &payload.call_id,
        &payload.dispatch_id,
        WorkKind::CapabilityProvider,
    );
    let mut message = ChatMessage::tool_result(
        format!("scheduled-unbound:{}", payload.dispatch_id),
        &call_id,
        "The original dispatch intent was committed, but no original transport binding is available. The action outcome is unknown. Do not retry automatically.",
    );
    message.turn_id = Some(work.turn_id.clone());
    message.background_task_id = Some(action.action_request_id.clone());
    message.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            Some(parent.data_envelope.as_ref().ok_or_else(invalid)?),
            &call_id,
            &message.text,
            "scheduled_unbound_action",
        )
        .map_err(|_| invalid())?;
    let results: Vec<_> = session
        .conversation
        .iter()
        .filter(|item| {
            item.tool_call_id.as_deref() == Some(call_id.as_str())
                && matches!(item.role, ChatRole::Tool | ChatRole::UntrustedOutput)
        })
        .collect();
    if results.len() > 1
        || results
            .first()
            .is_some_and(|existing| **existing != message)
    {
        return Err(invalid());
    }
    let append = results.is_empty();
    let now = timestamp(now_ms)?;
    let mut dispatch: agent_capability_dispatch_outbox::ActiveModel = outbox.clone().into();
    dispatch.state = Set(DISPATCH_OUTBOX_OUTCOME_UNKNOWN.into());
    dispatch.updated_at = Set(now);
    dispatch.update(txn).await?;
    let mut record: agent_action_item::ActiveModel = work.clone().into();
    record.status = Set(CAPABILITY_WORK_OUTCOME_UNKNOWN.into());
    record.resolution = Set(Some("scheduled_original_binding_unavailable".into()));
    record.updated_at = Set(now);
    record.update(txn).await?;
    session.execution_state = ExecutionState::OutcomeUnknown {
        action: action.clone(),
        placeholder_message_id: message.message_id.clone(),
        since: work.dispatch_intent_at.ok_or_else(invalid)?.to_rfc3339(),
    };
    if append {
        session.conversation.push(message);
    }
    Ok((call_id, action))
}
