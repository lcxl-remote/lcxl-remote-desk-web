//! Require original durable owner disposition; a note alone is not evidence.
use super::{ScheduleStoreError, entity};
use crate::entity::{agent_action_item as action, agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{
    ExecutionState, ManualOutcomeDisposition, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Set};

pub(super) async fn evidence(
    txn: &DatabaseTransaction,
    task: &entity::Model,
    run: &run::Model,
    now: i64,
    requested: Option<(i64, &str)>,
) -> Result<ManualOutcomeDisposition, ScheduleStoreError> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&run.run_id))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::Conflict)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.turn_state != TurnState::Failed
        || (requested.is_none() && session.execution_state != ExecutionState::None)
        || row.lease_deadline.is_some()
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
    let items = action::Entity::find()
        .filter(action::Column::ConversationId.eq(&run.run_id))
        .lock_exclusive()
        .all(txn)
        .await?;
    let item = items
        .iter()
        .find(|item| item.id == evidence.action.work_id)
        .ok_or(ScheduleStoreError::Conflict)?;
    let outbox = crate::entity::agent_capability_dispatch_outbox::Entity::find()
        .filter(crate::entity::agent_capability_dispatch_outbox::Column::WorkId.eq(item.id))
        .filter(
            crate::entity::agent_capability_dispatch_outbox::Column::DispatchId
                .eq(&evidence.action.execution_id),
        )
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::Conflict)?;
    let payload: crate::capability_grant_store::CapabilityDispatchPayload =
        serde_json::from_str(&outbox.payload_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if payload.work_id != item.id
        || payload.dispatch_id != evidence.action.execution_id
        || (requested.is_some()
            && outbox.state != crate::capability_grant_store::DISPATCH_OUTBOX_OUTCOME_UNKNOWN)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let anchor = session
        .conversation
        .iter()
        .find(|message| message.message_id == evidence.placeholder_message_id)
        .ok_or(ScheduleStoreError::Conflict)?;
    if anchor.tool_call_id.as_deref() != Some(item.tool_call_id.as_str()) {
        return Err(ScheduleStoreError::Conflict);
    }
    let disposed = item.manual_resolved_at;
    if requested.is_none() && disposed.is_none() {
        return Err(ScheduleStoreError::Conflict);
    }
    if item.actor_id != session.actor_id
        || item.target_device_id != session.device_id
        || item.turn_id != run.turn_id
        || item.action_request_id != evidence.action.action_request_id
        || item.execution_id.as_deref() != Some(evidence.action.execution_id.as_str())
        || item.kind != evidence.action.kind.as_str()
        || disposed.is_some_and(|value| value.timestamp_millis() > at)
        || items.iter().any(|other| {
            other.id != item.id
                && !matches!(
                    other.status.as_str(),
                    crate::capability_grant_store::CAPABILITY_WORK_SUCCEEDED
                        | crate::capability_grant_store::CAPABILITY_WORK_FAILED
                        | crate::capability_grant_store::CAPABILITY_WORK_SUPERSEDED
                        | crate::capability_grant_store::CAPABILITY_WORK_REVOKED
                )
        })
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let evidence = evidence.clone();
    if requested.is_some() {
        if item.status != crate::capability_grant_store::CAPABILITY_WORK_OUTCOME_UNKNOWN
            || item.result_json.is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = action::Entity::update_many()
            .set(action::ActiveModel {
                manual_resolved_at: Set(Some(timestamp)),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(action::Column::Id.eq(item.id))
            .filter(
                action::Column::Status
                    .eq(crate::capability_grant_store::CAPABILITY_WORK_OUTCOME_UNKNOWN),
            )
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        session.version = session
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        let changed = agent_session::Entity::update_many()
            .set(agent_session::ActiveModel {
                state_json: Set(session
                    .encode_json_for_storage()
                    .map_err(|_| ScheduleStoreError::Invalid)?),
                version: Set(session.version),
                updated_at: Set(timestamp),
                ..Default::default()
            })
            .filter(agent_session::Column::Id.eq(row.id))
            .filter(agent_session::Column::Version.eq(row.version))
            .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    Ok(evidence)
}
