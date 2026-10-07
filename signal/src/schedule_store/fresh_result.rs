//! Settle fresh runs from persisted terminal state and original effect evidence.
use super::{FreshTaskLease, ScheduleStore, ScheduleStoreError};
use crate::entity::{agent_action_item as work_item, agent_schedule_run as run, agent_session};
use desk_diagnose_core::{
    chat::ChatRole,
    session::{ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect, Set};

#[derive(Clone, Copy)]
enum FreshResult<'a> {
    Answer(&'a str),
    Permission(&'a str),
    Children(&'a str),
    Failure(&'a desk_agent_protocol::AgentError),
}

impl ScheduleStore {
    pub async fn finish_answered_fresh_task(
        &self,
        lease: FreshTaskLease<'_>,
        answer: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Answer(answer))
            .await
    }

    pub async fn finish_failed_fresh_task(
        &self,
        lease: FreshTaskLease<'_>,
        error: &desk_agent_protocol::AgentError,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Failure(error))
            .await
    }

    /// Preserve the active occurrence while the owner considers one durable request.
    pub async fn await_fresh_task_permission(
        &self,
        lease: FreshTaskLease<'_>,
        request_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Permission(request_id))
            .await
    }

    /// Waiting releases only the parent's planning holder. Its original
    /// occurrence, source deadline, authorization and shared quota remain fixed.
    pub async fn await_fresh_task_children(
        &self,
        lease: FreshTaskLease<'_>,
        wait_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Children(wait_id))
            .await
    }

    async fn finish_fresh_result(
        &self,
        lease: FreshTaskLease<'_>,
        result: FreshResult<'_>,
    ) -> Result<run::Model, ScheduleStoreError> {
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
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let initial = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(lease.run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let authority = Self::lock_run_authority(
            &txn,
            lease.owner,
            &initial.device_id,
            lease.run_id,
            lease.node_id,
            lease.run_epoch,
        )
        .await?;
        let row = agent_session::Entity::find_by_id(initial.id)
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if row.actor_id != lease.owner.to_string()
            || row.version != session.version
            || row.lease_token != lease.session_token as i64
            || session.lease_token != lease.session_token
            || session.actor_id != row.actor_id
            || session.device_id != row.device_id
            || session.conversation_id != lease.run_id
            || session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.input_revision != 1
            || session.turn_state != state
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
                .one(&txn)
                .await?
                .ok_or(ScheduleStoreError::Conflict)?;
            child_planning_pending = child::Entity::find()
                .filter(child::Column::GroupId.eq(group_id))
                .filter(child::Column::RootConversationId.eq(lease.run_id))
                .filter(child::Column::ActorId.eq(&session.actor_id))
                .filter(child::Column::DeviceId.eq(&session.device_id))
                .filter(child::Column::State.is_not_in(["completed", "failed", "cancelled"]))
                .one(&txn)
                .await?
                .is_some();
            match crate::agent_subagent_store::scheduled_native_on(&txn, &group).await? {
                crate::agent_subagent_store::ScheduledNativeDisposition::Pending => {
                    delegated_pending = true
                }
                crate::agent_subagent_store::ScheduledNativeDisposition::Unknown => {
                    delegated_unknown = true
                }
                crate::agent_subagent_store::ScheduledNativeDisposition::Settled => {}
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
            && !crate::agent_subagent_store::required_children_complete_on(&txn, &session)
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
            FreshResult::Permission(id) => {
                if authority.contract().contract().exception_mode
                    != desk_agent_protocol::schedule::contract::TaskExceptionMode::RequestApproval
                {
                    return Err(ScheduleStoreError::Conflict);
                }
                Some(
                    desk_diagnose_core::schedule::permission_wait::reference(&session, id)
                        .ok_or(ScheduleStoreError::Conflict)?,
                )
            }
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
        let actions = work_item::Entity::find()
            .filter(work_item::Column::ConversationId.eq(lease.run_id))
            .lock_exclusive()
            .all(&txn)
            .await?;
        use crate::capability_grant_store::*;
        let mut unknown = delegated_unknown;
        for work in &actions {
            if work.status == CAPABILITY_WORK_SUCCEEDED {
                continue;
            }
            if matches!(
                result,
                FreshResult::Answer(_) | FreshResult::Permission(_) | FreshResult::Children(_)
            ) {
                return Err(ScheduleStoreError::Conflict);
            }
            if !matches!(
                work.status.as_str(),
                CAPABILITY_WORK_FAILED
                    | CAPABILITY_WORK_SUPERSEDED
                    | CAPABILITY_WORK_REVOKED
                    | CAPABILITY_WORK_OUTCOME_UNKNOWN
            ) {
                return Err(ScheduleStoreError::Conflict);
            }
            // Terminal transport failure after intent cannot prove that nothing
            // happened externally. Preserve uncertainty for owner reconciliation.
            if work.status == CAPABILITY_WORK_OUTCOME_UNKNOWN
                || (work.is_side_effecting && work.dispatch_intent_at.is_some())
            {
                unknown = true;
            }
        }
        if matches!(result, FreshResult::Answer(_))
            && !crate::capability_grant_store::task_grant::all_steps_succeeded_on(&txn, &authority)
                .await?
        {
            return Err(ScheduleStoreError::Conflict);
        }
        use desk_agent_protocol::schedule::ScheduledRunStatus;
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
        let current = Self::lock_run_authority(
            &txn,
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
            let now = chrono::DateTime::from_timestamp_millis(current.verified_at())
                .ok_or(ScheduleStoreError::Invalid)?;
            crate::agent_session_store::permission_resume::settle_fresh_decision_on(
                &txn, &session, request_id, now,
            )
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
                .exec(&txn)
                .await?;
            if updated.rows_affected != 1 {
                return Err(ScheduleStoreError::Conflict);
            }
            let waiting = run::Entity::find_by_id(current.run().id)
                .one(&txn)
                .await?
                .ok_or(ScheduleStoreError::NotFound)?;
            txn.commit().await?;
            return Ok(waiting);
        }
        super::settlement::settle(super::settlement::Settlement {
            work: current.run().clone(),
            now: current.verified_at(),
            txn,
            outcome,
            offline_timeout: false,
            error_kind,
            result_ref: reference,
        })
        .await
    }
}
