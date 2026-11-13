//! Reclaim the original occurrence only after its durable child dependency resolves.
use super::{
    FreshTaskClaim, ScheduleStoreError,
    authority::authority_now,
    lock_delegation_source_authority, lock_run_authority,
    session::{OrgMode, ScheduleHostStores, SessionGuard, SessionKey, SessionPatch},
};
use crate::entity::agent_schedule_run as run;
use desk_diagnose_core::session::{PersistedAgentSession, TriggerOrigin, TurnState};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// Caller holds current owner/device and root control. Neither the dependency
/// result nor this claim mints an operation grant or a new occurrence quota.
pub async fn claim_fresh_children_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    input: FreshTaskClaim<'_>,
) -> Result<PersistedAgentSession, ScheduleStoreError> {
    if input.owner <= 0
        || input.node_id.is_empty()
        || !(30..=300).contains(&input.lease_seconds)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(input.run_id))
        .filter(run::Column::OwnerUserId.eq(input.owner))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let mut row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: input.run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if work.status != "awaiting_children"
        || work.failure_accounted
        || work.finished_at.is_some()
        || work.cancel_requested_at.is_some()
        || work.lease_owner.is_some()
        || work.lease_deadline.is_some()
        || work.attempt != 1
        || work.started_at.is_none()
        || session.actor_id != input.owner.to_string()
        || !row.personal
        || row.actor_id != session.actor_id
        || row.device_id != session.device_id
        || row.version != session.version
        || row.lease_token != session.lease_token as i64
        || row.lease_deadline_ms.is_some()
        || !(session.turn_state == TurnState::Idle
            || (session.turn_state == TurnState::Failed
                && (session
                    .ready_subagent_notification
                    .as_ref()
                    .is_some_and(|notice| {
                        notice.accepted_response_message_id.is_none()
                            && notice.retry_after_ms.is_some()
                    })
                    || (session.subagent_result_only
                        && session
                            .ready_subagent_wait
                            .as_ref()
                            .is_some_and(|wait| wait.retry_after_ms.is_some())))))
        || !session.agent_role.is_main()
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.policy_revision != input.policy_revision
        || session.main_stopped
        || session.input_revision != 1
        || session.subagent_wait.is_some()
        || session.terminal_error.as_ref().is_some_and(|error| {
            error.kind != desk_agent_protocol::AgentErrorKind::ModelUnavailable
                || !error.retryable
                || (!session.subagent_result_only
                    && session.ready_subagent_notification.is_none())
        })
        || session.terminal_permission_request_id.is_some()
        || !session.unclosed_tool_call_ids().is_empty()
        || !session.execution_state.states().is_empty()
        || session
            .permission_requests
            .iter()
            .any(|request| !request.state.is_terminal())
        || session.current_request_id.as_deref() != Some(input.run_id)
        || session.current_turn_id.as_deref() != Some(format!("{}-turn", input.run_id).as_str())
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let answer_reference = work
        .result_ref
        .as_deref()
        .and_then(|value| value.strip_prefix("answer-children:"));
    let (group, source) = if let Some(answer_id) = answer_reference {
        if !session.conversation.iter().any(|message| {
            message.message_id == answer_id
                && message.role == desk_diagnose_core::chat::ChatRole::Assistant
                && message.turn_id == session.current_turn_id
                && message.tool_calls.is_empty()
        }) {
            return Err(ScheduleStoreError::Conflict);
        }
        let source_row = crate::entity::agent_delegation_group::Entity::find()
            .filter(
                crate::entity::agent_delegation_group::Column::GroupId.eq(session
                    .delegation_group_id
                    .as_deref()
                    .ok_or(ScheduleStoreError::Conflict)?),
            )
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        let source = host.decode_creation(&source_row)?;
        let frozen = source
            .scheduled_source
            .as_ref()
            .ok_or(ScheduleStoreError::Invalid)?;
        lock_delegation_source_authority(
            txn,
            input.owner,
            &session.device_id,
            frozen,
        )
        .await?;
        host.prepare_notification(txn, &mut session).await?;
        row = host
            .find_session(txn, SessionKey::Id(row.id), OrgMode::Any, false)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.version != session.version {
            return Err(ScheduleStoreError::Conflict);
        }
        host.ready_notification_source(txn, &session)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?
            .ok_or(ScheduleStoreError::Conflict)?
    } else {
        let (wait, group, source) =
            host.ready_wait_source(txn, &session)
                .await
                .map_err(|_| ScheduleStoreError::Conflict)?
                .ok_or(ScheduleStoreError::Conflict)?;
        if work.result_ref.as_deref() != Some(format!("children:{}", wait.wait_id).as_str()) {
            return Err(ScheduleStoreError::Conflict);
        }
        (group, source)
    };
    if !matches!(&group.source, desk_diagnose_core::subagent::DelegationSource::ScheduledOccurrence {
            schedule_id, occurrence_id,
        } if schedule_id == &work.schedule_id && occurrence_id == input.run_id)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let frozen = source
        .scheduled_source
        .as_ref()
        .ok_or(ScheduleStoreError::Invalid)?;
    let authority = lock_delegation_source_authority(
        txn,
        input.owner,
        &session.device_id,
        frozen,
    )
    .await?;
    // Task -> session lock order agrees with cancellation and native dispatch.
    let locked = host
        .find_session(txn, SessionKey::Id(row.id), OrgMode::Any, true)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if locked != row {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = authority_now(txn).await?;
    let until = now
        .checked_add(i64::from(input.lease_seconds) * 1_000)
        .ok_or(ScheduleStoreError::Invalid)?
        .min(authority.deadline_ms())
        .min(group.limits.deadline_ms);
    if now < authority.verified_at() || until <= now {
        return Err(ScheduleStoreError::Conflict);
    }
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    let epoch = work
        .lease_epoch
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.lease_token = session
        .lease_token
        .checked_add(1)
        .filter(|token| *token <= i64::MAX as u64)
        .ok_or(ScheduleStoreError::Invalid)?;
    if answer_reference.is_some() {
        session.subagent_result_only = true;
    }
    session.turn_state = TurnState::Running;
    session.terminal_error = None;
    session.scope_snapshot =
        desk_diagnose_core::session::narrow_scope(&session.turn_start_scope, &input.scope);
    session.updated_at = timestamp.to_rfc3339();
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("running".into()),
            lease_owner: Set(Some(input.node_id.into())),
            lease_epoch: Set(epoch),
            lease_deadline: Set(Some(until)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::Status.eq("awaiting_children"))
        .filter(run::Column::LeaseOwner.is_null())
        .filter(run::Column::CancelRequestedAt.is_null())
        .filter(run::Column::FailureAccounted.eq(false))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
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
                lease_deadline_null: true,
                ..SessionGuard::any()
            },
            SessionPatch {
                state_json: Some(&state_json),
                version: Some(session.version),
                lease_token: Some(session.lease_token as i64),
                lease_deadline_ms: Some(Some(until)),
                updated_at_ms: Some(now),
                turn_state: Some(TurnState::Running.as_str()),
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    host.mark_notification_attempt(txn, &session).await?;
    let after = lock_run_authority(
        txn,
        input.owner,
        &session.device_id,
        input.run_id,
        input.node_id,
        epoch,
    )
    .await?;
    if after.provenance() != &frozen.provenance {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(session)
}
