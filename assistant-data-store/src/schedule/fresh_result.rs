//! Settle fresh runs from persisted terminal state and original effect evidence.
use super::{
    FreshTaskLease, ScheduleStoreError, lock_run_authority,
    session::{
        ActionClass, ActionFilter, Finish, NativeDisposition, OrgMode, ScheduleHostStores,
        SessionKey,
    },
    settlement::{Settlement, settle_on},
};
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::ScheduledRunStatus;
use desk_diagnose_core::{
    chat::ChatRole,
    session::{ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// A fresh occurrence result its loop already committed.
#[derive(Clone, Copy)]
pub enum FreshResult<'a> {
    Answer(&'a str),
    Permission(&'a str),
    Children(&'a str),
    Failure(&'a desk_agent_protocol::AgentError),
}

/// Settles or parks the leased fresh occurrence with its loop's committed result.
pub async fn finish_fresh_result_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    lease: FreshTaskLease<'_>,
    result: FreshResult<'_>,
) -> Result<Finish<run::Model>, ScheduleStoreError> {
    if matches!(result, FreshResult::Answer(answer) if answer.trim().is_empty())
        || lease.session_token == 0
        || lease.session_token > i64::MAX as u64
    {
        return Err(ScheduleStoreError::Invalid);
    }
    let (state, expected_error) = match result {
        FreshResult::Answer(_) | FreshResult::Permission(_) | FreshResult::Children(_) => {
            (TurnState::Idle, None)
        }
        FreshResult::Failure(error) => (TurnState::Failed, Some(error)),
    };
    let initial = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: lease.run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    host.lock_task_subject(txn, lease.owner, &initial.device_id, lease.run_id)
        .await?;
    host.lock_conversation(txn, lease.run_id).await?;
    host.lock_root_children(
        txn,
        lease.run_id,
        &lease.owner.to_string(),
        &initial.device_id,
    )
    .await?;
    let authority = lock_run_authority(
        txn,
        lease.owner,
        &initial.device_id,
        lease.run_id,
        lease.node_id,
        lease.run_epoch,
    )
    .await?;
    let row = host
        .find_session(txn, SessionKey::Id(initial.id), OrgMode::Any, true)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if !row.personal
        || row.actor_id != lease.owner.to_string()
        || row.version != session.version
        || row.lease_token != lease.session_token as i64
        || session.lease_token != lease.session_token
        || session.actor_id != row.actor_id
        || session.device_id != row.device_id
        || session.conversation_id != lease.run_id
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.input_revision != 1
        || session.turn_state != state
        || !row.stored_turn_state_is(state)
        || session.execution_state != ExecutionState::None
        || session.terminal_error.as_ref() != expected_error
        || match result {
            FreshResult::Permission(id) => session
                .terminal_permission_request_id
                .as_deref()
                .is_some_and(|stored| stored != id),
            _ => session.terminal_permission_request_id.is_some(),
        }
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || (state == TurnState::Idle && session.handled_input_seq != session.latest_input_seq)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let mut child_planning_pending = false;
    let mut delegated_unknown = false;
    let mut delegated_pending = false;
    if let Some(group_id) = &session.delegation_group_id {
        use crate::entity::{agent_delegation_group as source, agent_subagent_run as child};
        let group = source::Entity::find()
            .filter(source::Column::GroupId.eq(group_id))
            .filter(source::Column::RootConversationId.eq(lease.run_id))
            .filter(source::Column::SourceOccurrenceId.eq(lease.run_id))
            .filter(source::Column::ActorId.eq(&session.actor_id))
            .filter(source::Column::DeviceId.eq(&session.device_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        child_planning_pending = child::Entity::find()
            .filter(child::Column::GroupId.eq(group_id))
            .filter(child::Column::RootConversationId.eq(lease.run_id))
            .filter(child::Column::ActorId.eq(&session.actor_id))
            .filter(child::Column::DeviceId.eq(&session.device_id))
            .filter(child::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            .one(txn)
            .await?
            .is_some();
        match host.scheduled_native(txn, &group).await? {
            NativeDisposition::Pending => delegated_pending = true,
            NativeDisposition::Unknown => delegated_unknown = true,
            NativeDisposition::Settled => {}
        }
    }
    let notification_retry = matches!(result, FreshResult::Failure(error)
        if error.kind == desk_agent_protocol::AgentErrorKind::ModelUnavailable && error.retryable)
        && session.is_subagent_result_turn()
        && (session
            .ready_subagent_notification
            .as_ref()
            .is_some_and(|notice| {
                notice.accepted_response_message_id.is_none() && notice.retry_after_ms.is_some()
            })
            || session
                .ready_subagent_wait
                .as_ref()
                .is_some_and(|wait| wait.retry_after_ms.is_some()))
        && authority.run().result_ref.as_deref().is_some_and(|value| {
            value.starts_with("answer-children:") || value.starts_with("children:")
        })
        && !delegated_unknown;
    if matches!(result, FreshResult::Failure(_)) && delegated_pending && !notification_retry {
        return Err(ScheduleStoreError::Conflict);
    }
    let answered_children_pending = matches!(result, FreshResult::Answer(_))
        && !delegated_unknown
        && (child_planning_pending || delegated_pending);
    if matches!(result, FreshResult::Answer(_))
        && session.delegation_group_id.is_some()
        && !host
            .required_children_complete(txn, &session)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let mut reference = match result {
        FreshResult::Answer(answer) => {
            let message = session
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == ChatRole::Assistant)
                .filter(|message| {
                    message.text == answer
                        && message.tool_calls.is_empty()
                        && message.turn_id.as_deref() == Some(authority.run().turn_id.as_str())
                })
                .ok_or(ScheduleStoreError::Conflict)?;
            let reference = format!("message:{}", message.message_id);
            if reference.len() > 512 {
                return Err(ScheduleStoreError::Invalid);
            }
            Some(reference)
        }
        FreshResult::Permission(id) => Some(
            desk_diagnose_core::schedule::permission_wait::reference(&session, id)
                .ok_or(ScheduleStoreError::Conflict)?,
        ),
        FreshResult::Children(id) => {
            let wait = session
                .subagent_wait
                .as_ref()
                .or(session.ready_subagent_wait.as_ref())
                .ok_or(ScheduleStoreError::Conflict)?;
            wait.validate().map_err(|_| ScheduleStoreError::Invalid)?;
            if wait.wait_id != id
                || wait.parent_input_revision != session.input_revision
                || wait.parent_control_revision != session.control_revision
                || session.delegation_group_id.as_deref() != Some(wait.group_id.as_str())
            {
                return Err(ScheduleStoreError::Conflict);
            }
            let reference = format!("children:{id}");
            if reference.len() > 512 {
                return Err(ScheduleStoreError::Invalid);
            }
            Some(reference)
        }
        FreshResult::Failure(_) => None,
    };
    if notification_retry {
        reference = authority.run().result_ref.clone();
    } else if answered_children_pending {
        let message = reference
            .as_deref()
            .and_then(|value| value.strip_prefix("message:"))
            .ok_or(ScheduleStoreError::Conflict)?;
        reference = Some(format!("answer-children:{message}"));
    } else if delegated_unknown {
        reference = Some(format!(
            "delegated-effects:{}",
            session
                .delegation_group_id
                .as_deref()
                .ok_or(ScheduleStoreError::Conflict)?
        ));
    }
    if reference.as_ref().is_some_and(|value| value.len() > 512) {
        return Err(ScheduleStoreError::Invalid);
    }
    let actions = host
        .find_actions(txn, lease.run_id, ActionFilter::default(), true)
        .await?;
    let mut unknown = delegated_unknown;
    for work in &actions {
        unknown |= action_leaves_unknown(
            work.class,
            matches!(result, FreshResult::Failure(_)),
            work.is_side_effecting && work.dispatch_intent_at_ms.is_some(),
        )?;
    }
    let (outcome, error_kind) = match result {
        FreshResult::Answer(_) if unknown => (
            ScheduledRunStatus::OutcomeUnknown,
            Some("outcome_unknown".into()),
        ),
        FreshResult::Answer(_) if answered_children_pending => {
            (ScheduledRunStatus::AwaitingChildren, None)
        }
        FreshResult::Answer(_) => (ScheduledRunStatus::Succeeded, None),
        FreshResult::Permission(_) => (ScheduledRunStatus::AwaitingPermission, None),
        FreshResult::Children(_) => (ScheduledRunStatus::AwaitingChildren, None),
        FreshResult::Failure(_) if notification_retry => {
            (ScheduledRunStatus::AwaitingChildren, None)
        }
        FreshResult::Failure(_) if unknown => (
            ScheduledRunStatus::OutcomeUnknown,
            Some("outcome_unknown".into()),
        ),
        FreshResult::Failure(error) => {
            let kind = if error.kind == desk_agent_protocol::AgentErrorKind::PermissionDenied {
                "policy_denied".to_owned()
            } else {
                serde_json::to_string(&error.kind).map_err(|_| ScheduleStoreError::Invalid)?
            };
            (
                ScheduledRunStatus::Failed,
                Some(kind.trim_matches('"').to_owned()),
            )
        }
    };
    let current = lock_run_authority(
        txn,
        lease.owner,
        &session.device_id,
        lease.run_id,
        lease.node_id,
        lease.run_epoch,
    )
    .await?;
    if matches!(result, FreshResult::Permission(_))
        && reference.as_deref() == current.run().result_ref.as_deref()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    if let Some(request_id) = current
        .run()
        .result_ref
        .as_deref()
        .and_then(|value| value.strip_prefix("permission:"))
    {
        host.settle_fresh_decision(txn, &session, request_id, current.verified_at())
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    if matches!(
        result,
        FreshResult::Permission(_) | FreshResult::Children(_)
    ) || answered_children_pending
        || notification_retry
    {
        let updated = run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set(
                    if matches!(result, FreshResult::Children(_))
                        || answered_children_pending
                        || notification_retry
                    {
                        "awaiting_children".into()
                    } else {
                        "awaiting_permission".into()
                    },
                ),
                lease_owner: Set(
                    if matches!(result, FreshResult::Children(_))
                        || answered_children_pending
                        || notification_retry
                    {
                        None
                    } else {
                        current.run().lease_owner.clone()
                    },
                ),
                result_ref: Set(reference),
                lease_deadline: Set(None),
                updated_at: Set(current.verified_at()),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(current.run().id))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::LeaseEpoch.eq(lease.run_epoch))
            .filter(run::Column::LeaseOwner.eq(lease.node_id))
            .filter(run::Column::CancelRequestedAt.is_null())
            .exec(txn)
            .await?;
        if updated.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let waiting = run::Entity::find_by_id(current.run().id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        return Ok(Finish::Commit(waiting));
    }
    settle_on(
        host,
        txn,
        Settlement {
            work: current.run().clone(),
            now: current.verified_at(),
            outcome,
            offline_timeout: false,
            error_kind,
            result_ref: reference,
        },
    )
    .await
    .map(Finish::Commit)
}

/// Whether an action leaves a fresh result's external effects unknown. Only a
/// failed result may carry unsuccessful actions, and only finished ones.
fn action_leaves_unknown(
    class: ActionClass,
    failure: bool,
    effect_after_intent: bool,
) -> Result<bool, ScheduleStoreError> {
    if class == ActionClass::Succeeded {
        return Ok(false);
    }
    if !failure
        || !matches!(
            class,
            ActionClass::Failed | ActionClass::Withdrawn | ActionClass::OutcomeUnknown
        )
    {
        return Err(ScheduleStoreError::Conflict);
    }
    // Terminal transport failure after intent cannot prove that nothing
    // happened externally. Preserve uncertainty for owner reconciliation.
    Ok(class == ActionClass::OutcomeUnknown || effect_after_intent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsuccessful_actions_block_answers_and_mark_unknown_effects_on_failure() {
        use ActionClass::*;
        for class in [
            Succeeded,
            Failed,
            Withdrawn,
            LegacyClosed,
            OutcomeUnknown,
            Dispatching,
            Open,
        ] {
            assert_eq!(
                action_leaves_unknown(class, false, false).is_ok(),
                class == Succeeded,
                "{class:?}"
            );
        }
        assert_eq!(
            action_leaves_unknown(Succeeded, true, true).ok(),
            Some(false)
        );
        assert_eq!(action_leaves_unknown(Failed, true, false).ok(), Some(false));
        assert_eq!(action_leaves_unknown(Failed, true, true).ok(), Some(true));
        assert_eq!(
            action_leaves_unknown(Withdrawn, true, false).ok(),
            Some(false)
        );
        assert_eq!(
            action_leaves_unknown(OutcomeUnknown, true, false).ok(),
            Some(true)
        );
        for class in [LegacyClosed, Dispatching, Open] {
            assert!(
                action_leaves_unknown(class, true, false).is_err(),
                "{class:?}"
            );
        }
    }
}
