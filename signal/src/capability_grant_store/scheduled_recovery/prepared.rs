//! Close a reserved call only while durable records prove no dispatch intent.
use super::*;
use sea_orm::ExprTrait;

pub(in crate::capability_grant_store) async fn close_on(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    work: &agent_action_item::Model,
    now_ms: u64,
) -> Result<String, DbErr> {
    let payload = decode_prepared_payload(work)?;
    let (reservation, stored) = load_prepared(txn, &payload.call_id)
        .await?
        .ok_or_else(invalid)?;
    let outbox = agent_capability_dispatch_outbox::Entity::find()
        .filter(
            agent_capability_dispatch_outbox::Column::WorkId
                .eq(work.id)
                .or(agent_capability_dispatch_outbox::Column::CallId.eq(&payload.call_id))
                .or(agent_capability_dispatch_outbox::Column::ReservationId
                    .eq(&payload.reservation_id)),
        )
        .one(txn)
        .await?;
    let digest = format!(
        "{:x}",
        Sha256::digest(payload.canonical_input_json.as_bytes())
    );
    if stored != *work
        || outbox.is_some()
        || payload.generation == 0
        || payload.input_revision != session.input_revision
        || payload.input_watermark != session.latest_input_seq
        || payload.call_id != work.action_request_id
        || payload.call_id != work.tool_call_id
        || payload.grant_id != reservation.grant_id
        || payload.reservation_id != reservation.reservation_id
        || payload.reservation_id
            != stable_id(
                "reservation",
                &format!("{}:{}", payload.grant_id, payload.call_id),
            )
        || reservation.run_id != session.conversation_id
        || reservation.generation != i64::try_from(payload.generation).map_err(|_| invalid())?
        || reservation.canonical_input_digest_sha256 != digest
        || payload.canonical_input_digest_sha256 != digest
        || work.draft_hash != digest
        || work.payload_schema_version != 1
        || work.execution_id.as_deref()
            != Some(format!("capability:{}:{}", payload.call_id, payload.generation).as_str())
        || work.attempt != 0
        || work.dispatched_attempt.is_some()
        || work.dispatch_intent_at.is_some()
        || work.result_json.is_some()
        || work.result_schema_version.is_some()
        || session
            .execution_state
            .tasks()
            .into_iter()
            .any(|action| action.work_id == work.id)
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
        || grant.provider_id != payload.provider_id
        || grant.capability_id != payload.capability_id
        || grant.tool_name != payload.tool_name
        || grant.policy_revision != work.policy_revision
        || grant.effect.is_side_effecting() != work.is_side_effecting
        || grant
            .canonical_input_digest_sha256
            .as_ref()
            .is_some_and(|expected| expected != &digest)
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
    let (proposal, call) = proposals[0];
    let call_id = call.id.clone();
    let mut message = ChatMessage::tool_result(
        format!("scheduled-unissued:{}", work.id),
        &call_id,
        "The original prepared action was not dispatched. Its unused reservation was released; do not automatically retry this interrupted turn.",
    );
    message.turn_id = Some(work.turn_id.clone());
    message.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            Some(proposal.data_envelope.as_ref().ok_or_else(invalid)?),
            &call_id,
            &message.text,
            "scheduled_unissued_action",
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
    match (work.status.as_str(), reservation.state.as_str()) {
        (CAPABILITY_WORK_PREPARED, RESERVATION_STATUS_RESERVED) => {
            release_before_intent(
                txn,
                &reservation,
                work,
                now_ms,
                CAPABILITY_WORK_SUPERSEDED,
                "scheduled_executor_interrupted_before_intent",
            )
            .await?;
        }
        (CAPABILITY_WORK_SUPERSEDED | CAPABILITY_WORK_REVOKED, RESERVATION_STATUS_RELEASED) => {}
        _ => return Err(invalid()),
    }
    if append {
        session.conversation.push(message);
    }
    Ok(call_id)
}
