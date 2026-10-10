//! Recover a persisted child pause or owner stop without another model request.
use super::ScheduleStoreError;
use crate::agent_subagent_store as children;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{
    agent_delegation_group as group_row, agent_schedule_run as run, agent_session,
    agent_subagent_run as child_row,
};
use desk_diagnose_core::session::{ExecutionState, PersistedAgentSession, TurnState};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

/// A saved provider answer can retain an occurrence while optional children
/// finish. This identifies historical evidence and grants no planning authority.
pub(super) fn answer_wait_reference(
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
pub(super) async fn recover(
    txn: DatabaseTransaction,
    work: run::Model,
    row: agent_session::Model,
    mut session: PersistedAgentSession,
    now: i64,
) -> Result<bool, ScheduleStoreError> {
    if row
        .lease_deadline
        .is_some_and(|until| until.timestamp_millis() > now)
    {
        return Ok(false);
    }
    // Owner stop closes model-visible calls before a background execution has
    // necessarily been projected into execution_state. Inspect the original
    // dispatch records even when those two projections are already empty.
    if session.main_stopped {
        let context = super::TaskReceiptContext::load(&txn, &work).await?;
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        if !crate::capability_grant_store::fresh_recovery::restore_completed_calls(
            &txn,
            &mut session,
            &context,
            &timestamp.to_rfc3339(),
        )
        .await?
        {
            return Ok(false);
        }
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
        return Ok(false);
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
        .one(&txn)
        .await?
        .ok_or(ScheduleStoreError::Conflict)?;
    let group = children::decode_group(&row_group)?;
    let unfinished = child_row::Entity::find()
        .filter(child_row::Column::GroupId.eq(group_id))
        .filter(child_row::Column::RootConversationId.eq(&work.run_id))
        .filter(child_row::Column::ActorId.eq(&session.actor_id))
        .filter(child_row::Column::DeviceId.eq(&session.device_id))
        .filter(child_row::Column::State.is_not_in(["completed", "failed", "cancelled"]))
        .one(&txn)
        .await?
        .is_some();
    let answer_wait = answer_wait_reference(&work, &session, unfinished);
    let reference = if session.main_stopped {
        if session.turn_state != TurnState::Cancelled {
            return Ok(false);
        }
        let tasks = children::group_children_on(&txn, &group).await?;
        if tasks.is_empty() {
            return Ok(false);
        }
        for task in tasks {
            children::decode_run(&task)?;
        }
        // Explicitly distinguish owner stop from a model-visible wait tool.
        format!("stopped-children:{group_id}")
    } else if let Some(reference) = answer_wait {
        if !matches!(
            session.turn_state,
            TurnState::Idle | TurnState::Running | TurnState::Failed
        ) {
            return Ok(false);
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
            return Ok(false);
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
    let changed = agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(None),

            updated_at: Set(timestamp),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(row.id))
        .filter(agent_session::Column::Version.eq(row.version))
        .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
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
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    txn.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};

    #[tokio::test]
    async fn saved_wait_recovers_after_the_original_process_dies_without_another_input_or_occurrence()
     {
        let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
        let before = run::Entity::find()
            .filter(run::Column::RunId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let deadline = chrono::Utc::now().timestamp_millis() - 1;
        run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("running".into()),
                lease_owner: Set(Some("lost-original-node".into())),
                lease_deadline: Set(Some(deadline)),
                result_ref: Set(None),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .recover_action_free_fresh_task(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert!(
            !schedule
                .recover_action_free_fresh_task(&parent.conversation_id)
                .await
                .unwrap()
        );
        let after = run::Entity::find_by_id(before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.status, "awaiting_children");
        assert_eq!(after.result_ref, before.result_ref);
        assert_eq!(after.started_at, before.started_at);
        assert_eq!(after.attempt, before.attempt);
        assert_eq!(after.lease_epoch, before.lease_epoch);
        assert!(after.lease_owner.is_none() && after.lease_deadline.is_none());
        let current = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let saved = PersistedAgentSession::decode_json(&current.state_json).unwrap();
        assert_eq!(saved.lease_token, parent.lease_token + 1);
        assert_eq!(saved.input_revision, parent.input_revision);
        assert_eq!(saved.control_revision, parent.control_revision);
        assert_eq!(saved.subagent_wait, parent.subagent_wait);
        assert_eq!(saved.conversation, parent.conversation);
        assert_eq!(saved.current_turn_steps, parent.current_turn_steps);
        assert_eq!(saved.current_turn_tokens, parent.current_turn_tokens);
        assert_eq!(saved.lifetime_steps, parent.lifetime_steps);
        assert_eq!(saved.lifetime_tokens, parent.lifetime_tokens);
        assert!(current.lease_deadline.is_none());
        assert_eq!(
            schedule
                .read(1, &before.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(parent.conversation_id.as_str())
        );
    }

    #[tokio::test]
    async fn stopped_parent_with_a_lost_run_holder_preserves_finite_children_until_terminal() {
        let (db, schedule, store, parent, task_id) = children::scheduled_test_wait_fixture().await;
        store
            .stop_for_owner(
                &parent.conversation_id,
                &parent.actor_id,
                &parent.device_id,
                &AiAssistantStopControl {
                    client_request_id: "stop-lost-holder".into(),
                    expected_input_revision: parent.input_revision,
                    expected_control_revision: parent.control_revision,
                    subagent_choice: Some(SubAgentStopChoice::MainOnly),
                },
            )
            .await
            .unwrap();
        let before = run::Entity::find()
            .filter(run::Column::RunId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("running".into()),
                lease_owner: Set(Some("lost-original-node".into())),
                lease_deadline: Set(Some(chrono::Utc::now().timestamp_millis() - 1)),
                result_ref: Set(None),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .recover_action_free_fresh_task(&parent.conversation_id)
                .await
                .unwrap()
        );
        let current = run::Entity::find_by_id(before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.status, "awaiting_children");
        assert!(
            current
                .result_ref
                .as_deref()
                .unwrap()
                .starts_with("stopped-children:")
        );
        assert!(current.cancel_requested_at.is_none());
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, "queued");
        let group = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&child.group_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(group.source_admission, "open");
        assert!(!group.parent_active);
        children::complete_scheduled_test_child(&db, &task_id).await;
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert_eq!(
            run::Entity::find_by_id(before.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            "cancelled"
        );
    }
}
