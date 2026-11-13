//! Require original durable owner disposition; a note alone is not evidence.
use super::super::session::{
    ActionClass, ActionFilter, OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch,
};
use super::{ScheduleStoreError, entity, run};
use desk_diagnose_core::session::{
    ExecutionState, ManualOutcomeDisposition, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::ConnectionTrait;

pub(super) async fn evidence<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    task: &entity::Model,
    run: &run::Model,
    now: i64,
    requested: Option<(i64, &str)>,
) -> Result<ManualOutcomeDisposition, ScheduleStoreError> {
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &run.run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            true,
        )
        .await?
        .ok_or(ScheduleStoreError::Conflict)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.turn_state != TurnState::Failed
        || (requested.is_none() && session.execution_state != ExecutionState::None)
        || row.lease_deadline_ms.is_some()
        || !row.personal
        || session.actor_id != run.owner_user_id.to_string()
        || session.actor_id != row.actor_id
        || session.device_id != task.target_device_id
        || session.device_id != row.device_id
        || session.conversation_id != run.run_id
        || run.conversation_id != run.run_id
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || session.current_request_id.as_deref() != Some(run.run_id.as_str())
        || session.current_turn_id.as_deref() != Some(run.turn_id.as_str())
        || session.input_revision != 1
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if let Some((work_id, execution_id)) = requested
        && !session.manually_dispose_unknown(work_id, execution_id, timestamp.to_rfc3339())
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let evidence = session
        .manual_outcome_disposition
        .as_ref()
        .ok_or(ScheduleStoreError::Conflict)?;
    let at = chrono::DateTime::parse_from_rfc3339(&evidence.disposed_at)
        .map_err(|_| ScheduleStoreError::Invalid)?
        .timestamp_millis();
    if at > now
        || !session
            .conversation
            .iter()
            .any(|message| message.message_id == evidence.placeholder_message_id)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let items = host
        .find_actions(txn, &run.run_id, ActionFilter::default(), true)
        .await?;
    let item = items
        .iter()
        .find(|item| item.id == evidence.action.work_id)
        .ok_or(ScheduleStoreError::Conflict)?;
    host.verify_manual_outbox(
        txn,
        item,
        &evidence.action.execution_id,
        requested.is_some(),
    )
    .await?;
    let anchor = session
        .conversation
        .iter()
        .find(|message| message.message_id == evidence.placeholder_message_id)
        .ok_or(ScheduleStoreError::Conflict)?;
    if anchor.tool_call_id.as_deref() != Some(item.tool_call_id.as_str()) {
        return Err(ScheduleStoreError::Conflict);
    }
    let disposed = item.manual_resolved_at_ms;
    if requested.is_none() && disposed.is_none() {
        return Err(ScheduleStoreError::Conflict);
    }
    if item.actor_id != session.actor_id
        || item.target_device_id != session.device_id
        || item.turn_id != run.turn_id
        || item.action_request_id.as_deref() != Some(evidence.action.action_request_id.as_str())
        || item.execution_id.as_deref() != Some(evidence.action.execution_id.as_str())
        || item.kind != evidence.action.kind.as_str()
        || disposed.is_some_and(|value| value > at)
        || items.iter().any(|other| {
            other.id != item.id
                && !matches!(
                    other.class,
                    ActionClass::Succeeded | ActionClass::Failed | ActionClass::Withdrawn
                )
        })
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let evidence = evidence.clone();
    if requested.is_some() {
        if item.class != ActionClass::OutcomeUnknown || item.has_result {
            return Err(ScheduleStoreError::Conflict);
        }
        if !host.resolve_unknown_action(txn, item, now).await? {
            return Err(ScheduleStoreError::Conflict);
        }
        session.version = session
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let state_json = session
            .encode_json_for_storage()
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let changed = host
            .write_session(
                txn,
                SessionGuard {
                    id: Some(row.id),
                    version: Some(row.version),
                    lease_token: Some(row.lease_token),
                    ..SessionGuard::any()
                },
                SessionPatch {
                    state_json: Some(&state_json),
                    version: Some(session.version),
                    updated_at_ms: Some(now),
                    ..Default::default()
                },
            )
            .await?;
        if changed != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    Ok(evidence)
}
