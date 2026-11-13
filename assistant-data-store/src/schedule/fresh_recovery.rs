//! Recover action-free expired executions without replaying their model context.
mod children;
mod permission;

use super::{
    ScheduleStoreError, TaskReceiptContext,
    authority::authority_now,
    fresh_result::action_leaves_unknown,
    session::{
        ActionClass, ActionFilter, Finish, NativeDisposition, OrgMode, ScheduleHostStores,
        SessionGuard, SessionKey, SessionPatch, SessionRow,
    },
    settlement::{Settlement, settle_on},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_diagnose_core::session::{
    ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

// Own the same original row fences across sequential phases.
struct ExpiredFreshTask<R> {
    work: run::Model,
    row: SessionRow<R>,
    session: Box<PersistedAgentSession>,
    now: i64,
}

/// Recovers an expired fresh occurrence from its saved session and original
/// action records without another model request.
pub async fn recover_action_free_fresh_task_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    run_id: &str,
) -> Result<Finish<bool>, ScheduleStoreError> {
    let initial = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(initial.owner_user_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.kind != "fresh_task" || task.active_run_id.as_deref() != Some(run_id) {
        return Ok(Finish::Rollback(false));
    }
    let locked = entity::Entity::update_many()
        .col_expr(
            entity::Column::Revision,
            sea_orm::sea_query::Expr::col(entity::Column::Revision),
        )
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let work = run::Entity::find_by_id(initial.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let now = authority_now(txn).await?;
    if work.status != "running"
        || work.failure_accounted
        || work.finished_at.is_some()
        || work.started_at.is_none()
        || work.lease_deadline.is_none_or(|at| at > now)
        || work.conversation_id != work.run_id
        || work.result_ref.as_deref().is_some_and(|reference| {
            !reference.starts_with("permission:")
                && !reference.starts_with("directory:")
                && !reference.starts_with("children:")
                && !reference.starts_with("stopped-children:")
                && !reference.starts_with("answer-children:")
                && !reference.starts_with("delegated-effects:")
        })
    {
        return Ok(Finish::Rollback(false));
    }
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            true,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let session = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?,
    );
    if !row.personal
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.surface != desk_diagnose_core::session::AgentSessionSurface::AiAssistant
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
    let stopped_children = if session.main_stopped {
        use crate::entity::agent_subagent_run as child;
        child::Entity::find()
            .filter(child::Column::RootConversationId.eq(run_id))
            .filter(
                child::Column::GroupId.eq(session.delegation_group_id.clone().unwrap_or_default()),
            )
            .filter(child::Column::ActorId.eq(&session.actor_id))
            .filter(child::Column::DeviceId.eq(&session.device_id))
            .one(txn)
            .await?
            .is_some()
    } else {
        false
    };

    let answer_children = if session.delegation_group_id.is_some()
        && session.execution_state == desk_diagnose_core::session::ExecutionState::None
        && session.unclosed_tool_call_ids().is_empty()
    {
        use crate::entity::agent_subagent_run as child;
        let unfinished = child::Entity::find()
            .filter(child::Column::RootConversationId.eq(run_id))
            .filter(
                child::Column::GroupId.eq(session.delegation_group_id.clone().unwrap_or_default()),
            )
            .filter(child::Column::ActorId.eq(&session.actor_id))
            .filter(child::Column::DeviceId.eq(&session.device_id))
            .filter(child::Column::State.is_not_in(["completed", "failed", "cancelled"]))
            .one(txn)
            .await?
            .is_some();
        children::answer_wait_reference(&work, &session, unfinished).is_some()
    } else {
        false
    };
    if session.delegation_group_id.is_some()
        && (stopped_children
            || session.subagent_wait.is_some()
            || session.ready_subagent_wait.is_some()
            || answer_children)
    {
        return desk_diagnose_core::future::send_phase(|| {
            children::recover(host, txn, work, row, session, now)
        })
        .await;
    }
    if (session.turn_state == TurnState::Idle && session.terminal_permission_request_id.is_some())
        || desk_diagnose_core::schedule::permission_wait::unfinished_pause(&session).is_some()
    {
        return desk_diagnose_core::future::send_phase(|| {
            permission::recover(host, txn, work, row, *session, now)
        })
        .await;
    }

    desk_diagnose_core::future::send_phase(|| {
        reconcile_expired_fresh_task(
            host,
            txn,
            Box::new(ExpiredFreshTask {
                work,
                row,
                session,
                now,
            }),
            run_id,
        )
    })
    .await
}

async fn reconcile_expired_fresh_task<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    original: Box<ExpiredFreshTask<H::Row>>,
    run_id: &str,
) -> Result<Finish<bool>, ScheduleStoreError> {
    let ExpiredFreshTask {
        work,
        row,
        mut session,
        now,
    } = *original;
    let committed = match session.turn_state {
        TurnState::Idle
            if session.terminal_error.is_none()
                && session.handled_input_seq == session.latest_input_seq =>
        {
            true
        }
        TurnState::Failed if session.terminal_error.is_some() => true,
        TurnState::Cancelled => true,
        TurnState::Running | TurnState::AwaitingApproval if session.terminal_error.is_none() => {
            false
        }
        _ => return Ok(Finish::Rollback(false)),
    };
    // A saved terminal result is authoritative even if the execution process
    // disappeared before schedule settlement. Never replace its original error.
    if (committed
        && row.lease_deadline_ms.is_some_and(|deadline| deadline > now))
        || (!committed && row.lease_deadline_ms.is_none_or(|deadline| deadline > now))
        || session.execution_state.interrupted()
        || session.terminal_permission_request_id.is_some()
        // Decided per-call approvals are ordinary history of the run; only an
        // undecided request keeps the original wait open.
        || session.permission_requests.iter().any(|request| {
            matches!(request.state,
        desk_diagnose_core::dynamic_run::PermissionRequestState::Pending
            | desk_diagnose_core::dynamic_run::PermissionRequestState::NeedsRevalidation)
        })
        || !session.pending_auto_triggers.is_empty()
    {
        return Ok(Finish::Rollback(false));
    }
    if !session.unclosed_tool_call_ids().is_empty()
        || session.execution_state != ExecutionState::None
    {
        if session.turn_state == TurnState::Idle {
            return Ok(Finish::Rollback(false));
        }
        if !host
            .restore_completed_calls(txn, &work, &mut session, now)
            .await?
        {
            return Ok(Finish::Rollback(false));
        }
    }
    let actions = host
        .find_actions(txn, run_id, ActionFilter::default(), true)
        .await?;
    let mut action_failure = false;
    let mut unknown_effect = false;
    for action in &actions {
        if action.class == ActionClass::Succeeded {
            continue;
        }
        let Ok(unknown) = action_leaves_unknown(
            action.class,
            true,
            action.is_side_effecting && action.dispatch_intent_at_ms.is_some(),
        ) else {
            return Ok(Finish::Rollback(false));
        };
        action_failure = true;
        unknown_effect |= unknown;
    }
    if let Some(group_id) = &session.delegation_group_id {
        use crate::entity::agent_delegation_group as source_group;
        let group = source_group::Entity::find()
            .filter(source_group::Column::GroupId.eq(group_id))
            .filter(source_group::Column::RootConversationId.eq(run_id))
            .filter(source_group::Column::SourceOccurrenceId.eq(run_id))
            .filter(source_group::Column::ActorId.eq(&session.actor_id))
            .filter(source_group::Column::DeviceId.eq(&session.device_id))
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        match host.scheduled_native(txn, &group).await? {
            NativeDisposition::Pending => return Ok(Finish::Rollback(false)),
            NativeDisposition::Unknown => {
                action_failure = true;
                unknown_effect = true;
            }
            NativeDisposition::Settled => {}
        }
    }
    desk_diagnose_core::future::send_phase(|| {
        settle_expired_fresh_task(
            host,
            txn,
            Box::new(ExpiredFreshTask {
                work,
                row,
                session,
                now,
            }),
            committed,
            action_failure,
            unknown_effect,
        )
    })
    .await
}

async fn settle_expired_fresh_task<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    original: Box<ExpiredFreshTask<H::Row>>,
    committed: bool,
    action_failure: bool,
    unknown_effect: bool,
) -> Result<Finish<bool>, ScheduleStoreError> {
    let ExpiredFreshTask {
        work,
        row,
        mut session,
        now,
    } = *original;
    let mut result_ref = None;
    let mut incomplete_steps = false;
    let answered = session.turn_state == TurnState::Idle;
    if answered {
        let evidence = TaskReceiptContext::load(txn, &work).await?;
        if evidence.target_device_id() != session.device_id {
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
        }
    }
    if unknown_effect && session.delegation_group_id.is_some() {
        result_ref = Some(format!(
            "delegated-effects:{}",
            session
                .delegation_group_id
                .as_deref()
                .ok_or(ScheduleStoreError::Conflict)?
        ));
    }
    let cancelled = if committed {
        session.turn_state == TurnState::Cancelled
            || session
                .terminal_error
                .as_ref()
                .is_some_and(|error| error.kind == desk_agent_protocol::AgentErrorKind::Cancelled)
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
        host.settle_fresh_decision(txn, &session, request_id, now)
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
            message: "An original task action has an unknown outcome; automatic retry is forbidden"
                .into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
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
    settle_on(
        host,
        txn,
        Settlement {
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
        },
    )
    .await?;
    Ok(Finish::Commit(true))
}
