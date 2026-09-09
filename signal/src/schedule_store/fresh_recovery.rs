//! Recover action-free expired executions without replaying their model context.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_action_item as work_item, agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{
    ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

impl ScheduleStore {
    pub(super) async fn expired_fresh_candidates(
        &self,
        after: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        let now = self.database_time().await?;
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::LeaseDeadline.lte(now))
            .filter(run::Column::FailureAccounted.eq(false))
            .filter(run::Column::FinishedAt.is_null())
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    pub async fn recover_action_free_fresh_task(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;

        let initial = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(initial.owner_user_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task" || task.active_run_id.as_deref() != Some(run_id) {
            return Ok(false);
        }
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                sea_orm::sea_query::Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let work = run::Entity::find_by_id(initial.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let now = super::authority::authority_now(&txn).await?;
        if work.status != "running"
            || work.failure_accounted
            || work.finished_at.is_some()
            || work.started_at.is_none()
            || work.lease_deadline.is_none_or(|at| at > now)
            || work.conversation_id != work.run_id
            || work.result_ref.as_deref().is_some_and(|reference| {
                !reference.starts_with("permission:") && !reference.starts_with("directory:")
            })
        {
            return Ok(false);
        }
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(run_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let mut session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if session.trigger_origin != TriggerOrigin::ScheduledTask
            || session.surface != desk_diagnose_core::session::AgentSessionSurface::DeviceAssistant
            || session.conversation_id != work.run_id
            || session.actor_id != work.owner_user_id.to_string()
            || row.actor_id != session.actor_id
            || session.device_id != task.target_device_id
            || row.device_id != session.device_id
            || row.version != session.version
            || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
            || session.current_request_id.as_deref() != Some(run_id)
            || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            || session.input_revision != 1
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if (session.turn_state == TurnState::Idle
            && session.terminal_permission_request_id.is_some())
            || desk_diagnose_core::schedule::permission_wait::unfinished_pause(&session).is_some()
        {
            return super::fresh_permission_recovery::recover(txn, work, row, session, now).await;
        }
        let committed = match session.turn_state {
            TurnState::Idle
                if session.terminal_error.is_none()
                    && session.handled_input_seq == session.latest_input_seq =>
            {
                true
            }
            TurnState::Failed if session.terminal_error.is_some() => true,
            TurnState::Cancelled => true,
            TurnState::Running | TurnState::AwaitingApproval
                if session.terminal_error.is_none() =>
            {
                false
            }
            _ => return Ok(false),
        };
        // A saved terminal result is authoritative even if the execution process
        // disappeared before schedule settlement. Never replace its original error.
        if (committed
            && row
                .lease_deadline
                .is_some_and(|deadline| deadline.timestamp_millis() > now))
            || (!committed
                && row
                    .lease_deadline
                    .is_none_or(|deadline| deadline.timestamp_millis() > now))
            || session.execution_state.interrupted()
            || session.terminal_permission_request_id.is_some()
            || (work.result_ref.is_none() && !session.permission_requests.is_empty())
            || session.permission_requests.iter().any(|request| {
                matches!(request.state,
                desk_diagnose_core::dynamic_run::PermissionRequestState::Pending
                    | desk_diagnose_core::dynamic_run::PermissionRequestState::NeedsRevalidation)
            })
            || !session.pending_auto_triggers.is_empty()
        {
            return Ok(false);
        }
        super::directory_receipt::restore_results(&txn, &mut session).await?;
        if !session.unclosed_tool_call_ids().is_empty()
            || session.execution_state != ExecutionState::None
        {
            if session.turn_state == TurnState::Idle {
                return Ok(false);
            }
            let evidence = super::TaskReceiptContext::load(&txn, &work).await?;
            let timestamp =
                chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
            if !crate::capability_grant_store::fresh_recovery::restore_completed_calls(
                &txn,
                &mut session,
                &evidence,
                &timestamp.to_rfc3339(),
            )
            .await?
            {
                return Ok(false);
            }
        }
        let actions = work_item::Entity::find()
            .filter(work_item::Column::ConversationId.eq(run_id))
            .lock_exclusive()
            .all(&txn)
            .await?;
        use crate::capability_grant_store::*;
        let mut action_failure = false;
        let mut unknown_effect = false;
        for action in &actions {
            if action.status == CAPABILITY_WORK_SUCCEEDED {
                continue;
            }
            if !matches!(
                action.status.as_str(),
                CAPABILITY_WORK_FAILED
                    | CAPABILITY_WORK_SUPERSEDED
                    | CAPABILITY_WORK_REVOKED
                    | CAPABILITY_WORK_OUTCOME_UNKNOWN
            ) {
                return Ok(false);
            }
            action_failure = true;
            if action.status == CAPABILITY_WORK_OUTCOME_UNKNOWN
                || (action.is_side_effecting && action.dispatch_intent_at.is_some())
            {
                unknown_effect = true;
            }
        }
        let mut result_ref = None;
        let mut incomplete_steps = false;
        let answered = session.turn_state == TurnState::Idle;
        if answered {
            let evidence = super::TaskReceiptContext::load(&txn, &work).await?;
            if evidence.contract().contract().target_device_id != session.device_id {
                return Err(ScheduleStoreError::Conflict);
            }
            let message = session
                .conversation
                .last()
                .filter(|message| {
                    message.role == desk_diagnose_core::chat::ChatRole::Assistant
                        && !message.text.trim().is_empty()
                        && message.tool_calls.is_empty()
                        && message.turn_id.as_deref() == Some(work.turn_id.as_str())
                })
                .ok_or(ScheduleStoreError::Conflict)?;
            let reference = format!("message:{}", message.message_id);
            if reference.len() > 512 {
                return Err(ScheduleStoreError::Invalid);
            }
            result_ref = Some(reference);
            if action_failure {
                incomplete_steps = !unknown_effect;
            } else {
                let steps = crate::capability_grant_store::task_grant::step_states_for_recovery(
                    &txn, &evidence,
                )
                .await?;
                use desk_agent_protocol::schedule::contract::TaskStepStatus;
                if steps.values().any(|state| {
                    !matches!(state, TaskStepStatus::Pending | TaskStepStatus::Succeeded)
                }) {
                    return Ok(false);
                }
                incomplete_steps = steps
                    .values()
                    .any(|state| *state != TaskStepStatus::Succeeded);
            }
        }
        let cancelled = if committed {
            session.turn_state == TurnState::Cancelled
                || session.terminal_error.as_ref().is_some_and(|error| {
                    error.kind == desk_agent_protocol::AgentErrorKind::Cancelled
                })
        } else {
            work.cancel_requested_at.is_some()
        };
        let error_kind = if unknown_effect {
            "outcome_unknown".to_owned()
        } else if answered {
            if incomplete_steps {
                "incomplete_steps".to_owned()
            } else {
                String::new()
            }
        } else if committed {
            if cancelled {
                "cancelled".to_owned()
            } else {
                let error = session
                    .terminal_error
                    .as_ref()
                    .ok_or(ScheduleStoreError::Invalid)?;
                if error.kind == desk_agent_protocol::AgentErrorKind::PermissionDenied {
                    "policy_denied".to_owned()
                } else {
                    serde_json::to_string(&error.kind)
                        .map_err(|_| ScheduleStoreError::Invalid)?
                        .trim_matches('"')
                        .to_owned()
                }
            }
        } else if cancelled {
            "cancelled".to_owned()
        } else {
            "lease_expired".to_owned()
        };
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        if let Some(request_id) = work
            .result_ref
            .as_deref()
            .and_then(|reference| reference.strip_prefix("permission:"))
        {
            crate::agent_session_store::permission_resume::settle_fresh_decision_on(
                &txn, &session, request_id, timestamp,
            )
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
        }
        session.version = session
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        session.lease_token = session
            .lease_token
            .checked_add(1)
            .filter(|token| *token <= i64::MAX as u64)
            .ok_or(ScheduleStoreError::Invalid)?;
        if !committed {
            session.finish_turn(
                if cancelled {
                    TurnState::Cancelled
                } else {
                    TurnState::Failed
                },
                timestamp.to_rfc3339(),
            );
            session.terminal_error = Some(desk_agent_protocol::AgentError {
                kind: if cancelled {
                    desk_agent_protocol::AgentErrorKind::Cancelled
                } else {
                    desk_agent_protocol::AgentErrorKind::Timeout
                },
                message: if cancelled {
                    "Scheduled task cancelled; original action records were retained"
                } else {
                    "Scheduled task execution lease expired before completion"
                }
                .into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
        if incomplete_steps {
            session.finish_turn(TurnState::Failed, timestamp.to_rfc3339());
            session.terminal_error = Some(desk_agent_protocol::AgentError {
                kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
                message: "Task returned an answer before its required steps were completed".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
        if unknown_effect {
            session.finish_turn(TurnState::Failed, timestamp.to_rfc3339());
            session.terminal_error = Some(desk_agent_protocol::AgentError {
                kind: desk_agent_protocol::AgentErrorKind::TransportError,
                message:
                    "An original task action has an unknown outcome; automatic retry is forbidden"
                        .into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
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
        super::settlement::settle(super::settlement::Settlement {
            txn,
            work,
            now,
            outcome: if unknown_effect {
                desk_agent_protocol::schedule::ScheduledRunStatus::OutcomeUnknown
            } else if answered && !incomplete_steps {
                desk_agent_protocol::schedule::ScheduledRunStatus::Succeeded
            } else if cancelled {
                desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled
            } else {
                desk_agent_protocol::schedule::ScheduledRunStatus::Failed
            },
            offline_timeout: false,
            error_kind: (!error_kind.is_empty()).then_some(error_kind),
            result_ref,
        })
        .await?;
        Ok(true)
    }
}
