//! Explicit rehearsal entry into the ordinary interactive assistant.
use super::*;

#[allow(clippy::too_many_arguments)]
pub async fn run_rehearsal_turn(
    connections: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    target_connection_id: String,
    actor_user_id: i32,
    target_device_id: String,
    rehearsal_id: &str,
    mut ask: DeviceAssistantAsk,
) -> Result<Option<LoopOutcome>, AgentError> {
    let store = crate::schedule_store::ScheduleStore::new(db.clone());
    let reserved = store
        .read_rehearsal(actor_user_id, rehearsal_id)
        .await
        .map_err(|_| transport_error("rehearsal unavailable"))?;
    if reserved.target_device_id != target_device_id
        || reserved.model_id.is_some()
        || !ask.selected_attachment_ids.is_empty()
    {
        return Err(transport_error("invalid rehearsal selection"));
    }
    let original = store
        .claim_rehearsal(actor_user_id, rehearsal_id)
        .await
        .map_err(|_| transport_error("rehearsal already started or changed"))?;
    ask.question = original.prompt;
    ask.locale = original.locale;
    ask.conversation_id = Some(original.client_conversation_id);
    ask.client_message_id = format!("rehearsal:{}:input", original.rehearsal_id);
    // This is a user-requested run. Scope selection and all per-action approvals
    // remain in compose_turn; reserving a rehearsal never supplies a grant.
    compose_turn(
        connections,
        db,
        request_id,
        browser_connection_id,
        target_connection_id,
        actor_user_id,
        target_device_id,
        ask,
        None,
        None,
    )
    .await
}

pub(super) async fn finish_answer(
    db: &DatabaseConnection,
    owner: i32,
    conversation: Option<&str>,
    outcome: &LoopOutcome,
) -> Result<(), AgentError> {
    if let (Some(conversation), LoopOutcome::Answered(answer)) = (conversation, outcome) {
        crate::schedule_store::ScheduleStore::new(db.clone())
            .finish_rehearsal_answer_for_session(owner, conversation, answer)
            .await
            .map_err(|_| transport_error("rehearsal completion needs reconciliation"))?;
    }
    if let (
        Some(conversation),
        LoopOutcome::ProtocolError(_) | LoopOutcome::ContentSafetyUnavailable(_),
    ) = (conversation, outcome)
        && crate::schedule_store::ScheduleStore::new(db.clone())
            .finish_rehearsal_failure_for_session(owner, conversation)
            .await
            .is_err()
    {
        log::warn!("[rehearsal] failed outcome requires reconciliation");
    }
    Ok(())
}

pub(super) async fn finish_error(db: &DatabaseConnection, owner: i32, conversation: Option<&str>) {
    let Some(conversation) = conversation else {
        return;
    };
    let store = crate::schedule_store::ScheduleStore::new(db.clone());
    let settled = store
        .finish_rehearsal_termination_for_session(owner, conversation)
        .await;
    if settled.is_err() {
        // The persisted action ledger may still require reconciliation.
        log::warn!("[rehearsal] unsuccessful turn requires reconciliation");
    }
}
