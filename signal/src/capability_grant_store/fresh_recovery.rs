//! Restore original results or proven undispatched calls in the scheduler transaction.
use super::*;
mod unknown;
use desk_diagnose_core::session::{ExecutionState, TriggerOrigin};
use sea_orm::{DatabaseTransaction, QuerySelect};

fn invalid() -> DbErr {
    DbErr::Custom("invalid completed task recovery receipt".into())
}

pub(crate) async fn restore_completed_calls(
    txn: &DatabaseTransaction,
    session: &mut PersistedAgentSession,
    context: &crate::schedule_store::TaskReceiptContext,
    now: &str,
) -> Result<bool, DbErr> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.conversation_id != context.run_id()
    {
        return Err(invalid());
    }
    let rows = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(&session.conversation_id))
        .lock_exclusive()
        .all(txn)
        .await?;
    if rows.iter().any(|row| {
        !matches!(
            row.status.as_str(),
            CAPABILITY_WORK_SUCCEEDED
                | CAPABILITY_WORK_FAILED
                | CAPABILITY_WORK_PREPARED
                | CAPABILITY_WORK_SUPERSEDED
                | CAPABILITY_WORK_REVOKED
                | CAPABILITY_WORK_OUTCOME_UNKNOWN
        )
    }) {
        return Ok(false);
    }
    let mut next = session.clone();
    let now_ms = chrono::DateTime::parse_from_rfc3339(now)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .ok_or_else(invalid)?;
    for row in &rows {
        if !matches!(
            row.status.as_str(),
            CAPABILITY_WORK_PREPARED | CAPABILITY_WORK_SUPERSEDED | CAPABILITY_WORK_REVOKED
        ) {
            continue;
        }
        if row.actor_id != session.actor_id
            || row.target_device_id != session.device_id
            || session.current_turn_id.as_deref() != Some(row.turn_id.as_str())
        {
            return Err(invalid());
        }
        let payload = decode_prepared_payload(row)?;
        let grant_row = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq(&payload.grant_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let grant = decode_grant(&grant_row)?;
        if !matches!(&grant.issued_by,
            desk_agent_protocol::capability_grant::CapabilityGrantIssuer::TaskAuthorization(parent)
                if parent == context.provenance())
        {
            return Err(invalid());
        }
        super::scheduled_recovery::prepared::close_on(txn, &mut next, row, now_ms).await?;
    }
    for row in &rows {
        if row.status == CAPABILITY_WORK_OUTCOME_UNKNOWN {
            unknown::restore(txn, &mut next, row, context).await?;
        }
    }
    let mut calls = next.unclosed_tool_call_ids();
    for action in session
        .execution_state
        .tasks()
        .into_iter()
        .filter(|action| {
            !rows.iter().any(|row| {
                row.id == action.work_id && row.status == CAPABILITY_WORK_OUTCOME_UNKNOWN
            })
        })
    {
        let row = rows
            .iter()
            .find(|row| row.id == action.work_id)
            .ok_or_else(invalid)?;
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let (outbox, work, payload) =
            super::computer_binding::original_on(txn, &outbox.dispatch_id).await?;
        let original_action = desk_diagnose_core::session::ActionIdentity::new(
            work.id,
            &payload.call_id,
            &payload.dispatch_id,
            desk_diagnose_core::session::WorkKind::ComputerAction,
        );
        if *action != original_action {
            return Err(invalid());
        }
        if outbox.computer_binding_json.is_none() {
            return Ok(false);
        }
        let binding = super::computer_background::bound(&outbox, &work, &payload)?;
        if !calls.contains(&binding.origin.tool_call_id) {
            calls.push(binding.origin.tool_call_id.clone());
        }
    }
    for call_id in calls {
        let mut found = None;
        for row in &rows {
            if matches!(
                row.status.as_str(),
                CAPABILITY_WORK_PREPARED
                    | CAPABILITY_WORK_SUPERSEDED
                    | CAPABILITY_WORK_REVOKED
                    | CAPABILITY_WORK_OUTCOME_UNKNOWN
            ) {
                continue;
            }
            if row.kind != CAPABILITY_WORK_KIND
                || row.actor_id != session.actor_id
                || row.target_device_id != session.device_id
                || session.current_turn_id.as_deref() != Some(row.turn_id.as_str())
            {
                return Err(invalid());
            }
            let outbox = agent_capability_dispatch_outbox::Entity::find()
                .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(row.id))
                .one(txn)
                .await?
                .ok_or_else(invalid)?;
            let (outbox, work, payload) =
                super::computer_binding::original_on(txn, &outbox.dispatch_id).await?;
            if outbox.computer_binding_json.is_none() {
                return Ok(false);
            }
            let binding = super::computer_background::bound(&outbox, &work, &payload)?;
            if binding.origin.tool_call_id != call_id {
                continue;
            }
            if found.is_some() {
                return Err(invalid());
            }
            let original = SignalCapabilityGrantStore::original_task_terminal_on(
                txn,
                &outbox.dispatch_id,
                context.provenance(),
            )
            .await?
            .ok_or_else(invalid)?;
            super::computer_delivery::validate_destination(&next, &original)?;
            found = Some(original);
        }
        let Some(original) = found else {
            return Ok(false);
        };
        let already_present = super::computer_delivery::validate_destination(&next, &original)?;
        if !already_present {
            let content = desk_diagnose_core::image_input::recovered_result_text(
                &original.output.content,
                original.output.image_data_url.as_deref(),
            )
            .map_err(|_| invalid())?;
            next.apply_completion_with_envelope(
                &original.work.completion_event_id,
                &original.receipt.action.execution_id,
                &call_id,
                &original.receipt.action.action_request_id,
                &content,
                Some(original.receipt.envelope.clone()),
                now,
            );
        }
        next.execution_state.remove(&original.receipt.action);
    }
    if !next.unclosed_tool_call_ids().is_empty()
        || next.execution_state.is_running()
        || next.execution_state.interrupted()
    {
        return Ok(false);
    }
    *session = next;
    Ok(true)
}
