//! Recover a persisted child pause or owner stop without another model request.
use super::super::{
    ScheduleStoreError,
    session::{Finish, ScheduleHostStores, SessionGuard, SessionPatch, SessionRow},
};
use crate::entity::{
    agent_delegation_group as group_row, agent_schedule_run as run, agent_subagent_run as child_row,
};
use desk_diagnose_core::session::{ExecutionState, PersistedAgentSession, TurnState};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// A saved provider answer can retain an occurrence while optional children
/// finish. This identifies historical evidence and grants no planning authority.
pub(in crate::schedule) fn answer_wait_reference(
    work: &run::Model,
    session: &PersistedAgentSession,
    unfinished_child: bool,
) -> Option<String> {
    if !session.agent_role.is_main()
        || session.main_stopped
        || session.subagent_wait.is_some()
        || session.ready_subagent_wait.is_some()
    {
        return None;
    }
    if let Some(notice) = &session.ready_subagent_notification
        && notice.accepted_response_message_id.is_none()
    {
        return work
            .result_ref
            .as_ref()
            .filter(|value| value.starts_with("answer-children:"))
            .cloned();
    }
    if !unfinished_child || session.terminal_error.is_some() {
        return None;
    }
    let answer = session.conversation.iter().rev().find(|message| {
        message.role == desk_diagnose_core::chat::ChatRole::Assistant
            && message.turn_id == session.current_turn_id
    })?;
    if !answer.tool_calls.is_empty()
        || answer.text.trim().is_empty()
        || desk_diagnose_core::model_egress::model_output_message_envelope(answer).is_err()
    {
        return None;
    }
    Some(format!("answer-children:{}", answer.message_id))
}

/// Caller holds owner/root/children/task/session control and an expired original
/// run lease. The recorded wait remains evidence, never a new owner instruction.
pub(in crate::schedule) async fn recover<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    work: run::Model,
    row: SessionRow<H::Row>,
    mut session: Box<PersistedAgentSession>,
    now: i64,
) -> Result<Finish<bool>, ScheduleStoreError> {
    if row.lease_deadline_ms.is_some_and(|until| until > now) {
        return Ok(Finish::Rollback(false));
    }
    // Owner stop closes model-visible calls before a background execution has
    // necessarily been projected into execution_state. Inspect the original
    // dispatch records even when those two projections are already empty.
    if session.main_stopped
        && !host
            .restore_completed_calls(txn, &work, &mut session, now)
            .await?
    {
        return Ok(Finish::Rollback(false));
    }
    if session.terminal_permission_request_id.is_some()
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || (!session.main_stopped
            && (session.execution_state != ExecutionState::None
                || session.terminal_error.as_ref().is_some_and(|error| {
                    error.kind != desk_agent_protocol::AgentErrorKind::ModelUnavailable
                        || !error.retryable
                        || !(session.is_subagent_result_turn()
                            && (session.ready_subagent_notification.as_ref().is_some_and(
                                |notice| {
                                    notice.accepted_response_message_id.is_none()
                                        && notice.retry_after_ms.is_some()
                                },
                            ) || session
                                .ready_subagent_wait
                                .as_ref()
                                .is_some_and(|wait| wait.retry_after_ms.is_some())))
                })))
    {
        return Ok(Finish::Rollback(false));
    }
    let group_id = session
        .delegation_group_id
        .as_deref()
        .ok_or(ScheduleStoreError::Conflict)?;
    let row_group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(group_id))
        .filter(group_row::Column::RootConversationId.eq(&work.run_id))
        .filter(group_row::Column::SourceOccurrenceId.eq(&work.run_id))
        .filter(group_row::Column::SourceScheduleId.eq(&work.schedule_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::Conflict)?;
    let group = host.decode_group(&row_group)?;
    let unfinished = child_row::Entity::find()
        .filter(child_row::Column::GroupId.eq(group_id))
        .filter(child_row::Column::RootConversationId.eq(&work.run_id))
        .filter(child_row::Column::ActorId.eq(&session.actor_id))
        .filter(child_row::Column::DeviceId.eq(&session.device_id))
        .filter(child_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
        .one(txn)
        .await?
        .is_some();
    let answer_wait = answer_wait_reference(&work, &session, unfinished);
    let reference = if session.main_stopped {
        if session.turn_state != TurnState::Cancelled {
            return Ok(Finish::Rollback(false));
        }
        let tasks = host.group_children(txn, &group).await?;
        if tasks.is_empty() {
            return Ok(Finish::Rollback(false));
        }

        for task in tasks {
            crate::worker_request::decode_run(&task)?;
        }
        // Explicitly distinguish owner stop from a model-visible wait tool.
        format!("stopped-children:{group_id}")
    } else if let Some(reference) = answer_wait {
        if !matches!(
            session.turn_state,
            TurnState::Idle | TurnState::Running | TurnState::Failed
        ) {
            return Ok(Finish::Rollback(false));
        }
        if session
            .ready_subagent_notification
            .as_ref()
            .is_some_and(|notice| notice.accepted_response_message_id.is_some())
        {
            session.ready_subagent_notification = None;
        }
        reference
    } else {
        if !matches!(session.turn_state, TurnState::Idle | TurnState::Running) {
            return Ok(Finish::Rollback(false));
        }
        let wait = session
            .subagent_wait
            .as_ref()
            .or(session.ready_subagent_wait.as_ref())
            .ok_or(ScheduleStoreError::Conflict)?;
        wait.validate().map_err(|_| ScheduleStoreError::Invalid)?;
        if wait.group_id != group.group_id
            || wait.parent_input_revision != session.input_revision
            || wait.parent_control_revision != session.control_revision
            || group.parent_input_revision != session.input_revision
            || group.parent_control_revision != session.control_revision
            || wait.source_epoch > group.source_epoch
        {
            return Err(ScheduleStoreError::Conflict);
        }
        format!("children:{}", wait.wait_id)
    };
    if reference.len() > 512 {
        return Err(ScheduleStoreError::Invalid);
    }
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.lease_token = session
        .lease_token
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(ScheduleStoreError::Invalid)?;
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if !session.main_stopped {
        session.finish_turn(TurnState::Idle, timestamp.to_rfc3339());
        session.handled_input_seq = session.latest_input_seq;
    }
    session.updated_at = timestamp.to_rfc3339();
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
                lease_token: Some(session.lease_token as i64),
                lease_deadline_ms: Some(None),
                updated_at_ms: Some(now),
                turn_state: Some(session.turn_state.as_str()),
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("awaiting_children".into()),
            result_ref: Set(Some(reference)),
            lease_owner: Set(None),
            lease_deadline: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::Status.eq("running"))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::FailureAccounted.eq(false))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(Finish::Commit(true))
}
