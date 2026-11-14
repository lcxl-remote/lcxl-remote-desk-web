//! End original child waits without reclaiming planning or inventing native results.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    lock_delegation_source_authority,
    session::{
        Finish, NativeDisposition, OrgMode, ScheduleHostStores, SessionGuard, SessionKey,
        SessionPatch, SessionRow,
    },
    settlement::{Settlement, settle_on},
};
use crate::entity::{
    agent_delegation_group as group_row, agent_schedule as entity, agent_schedule_run as run,
};
use desk_diagnose_core::{
    model_observability::permission::PendingEnds,
    session::{PersistedAgentSession, TriggerOrigin, TurnState},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

// The original rows stay owned across sequential phases of one transaction.
struct FreshWait<R> {
    work: Box<run::Model>,
    row: Box<SessionRow<R>>,
    session: Box<PersistedAgentSession>,
    subject_withdrawn: bool,
}

struct ClosingFreshWait<R> {
    wait: Box<FreshWait<R>>,
    group: Box<group_row::Model>,
    now: i64,
    cancelled: bool,
    expired: bool,
}

struct FreshReceipts<R> {
    work: Box<run::Model>,
    row: Box<SessionRow<R>>,
    session: Box<PersistedAgentSession>,
    now: i64,
}

/// A closed child wait: whether the occurrence settled, and the permission
/// ends the host reports after committing.
pub struct ChildrenWaitClosed {
    pub settled: bool,
    pub now: i64,
    pub permission_ends: PendingEnds,
}

/// Owner-only stop leaves finite children running until terminal or source
/// expiry. Withdrawal closes child planning immediately; actual native work
/// is then reconciled before releasing the original schedule's active slot.
/// Closing commits even while native work is still pending.
pub async fn settle_fresh_children_wait_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    peek: run::Model,
    snapshot: entity::Model,
    subject_withdrawn: bool,
    run_id: &str,
) -> Result<Finish<Option<ChildrenWaitClosed>>, ScheduleStoreError> {
    let Some(wait) = desk_diagnose_core::future::send_phase(|| {
        load_fresh_children_wait(host, txn, peek, snapshot, subject_withdrawn, run_id)
    })
    .await?
    else {
        return Ok(Finish::Rollback(None));
    };
    let Some(closing) = desk_diagnose_core::future::send_phase(|| {
        prepare_fresh_children_close(host, txn, wait, run_id)
    })
    .await?
    else {
        return Ok(Finish::Rollback(None));
    };
    let now = closing.now;
    let (native, permission_ends) =
        desk_diagnose_core::future::send_phase(|| close_fresh_children_source(host, txn, &closing))
            .await?;
    if native == NativeDisposition::Pending {
        return Ok(Finish::Commit(Some(ChildrenWaitClosed {
            settled: false,
            now,
            permission_ends,
        })));
    }
    desk_diagnose_core::future::send_phase(|| {
        persist_fresh_children_close(host, txn, closing, native)
    })
    .await?;
    Ok(Finish::Commit(Some(ChildrenWaitClosed {
        settled: true,
        now,
        permission_ends,
    })))
}

/// Late original results backfill genuine root receipts and close the marker.
/// The original outcome, failure policy and source admission stay unchanged.
pub async fn reconcile_fresh_children_receipts_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    peek: run::Model,
    snapshot: entity::Model,
    run_id: &str,
) -> Result<Finish<bool>, ScheduleStoreError> {
    let Some(wait) = desk_diagnose_core::future::send_phase(|| {
        load_fresh_children_receipts(host, txn, peek, snapshot, run_id)
    })
    .await?
    else {
        return Ok(Finish::Rollback(false));
    };
    let done =
        desk_diagnose_core::future::send_phase(|| persist_fresh_children_receipts(host, txn, wait))
            .await?;
    Ok(if done {
        Finish::Commit(true)
    } else {
        Finish::Rollback(false)
    })
}

async fn load_fresh_children_wait<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    peek: run::Model,
    snapshot: entity::Model,
    subject_withdrawn: bool,
    run_id: &str,
) -> Result<Option<Box<FreshWait<H::Row>>>, ScheduleStoreError> {
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&peek.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(peek.owner_user_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.kind != "fresh_task" || task.active_run_id.as_deref() != Some(run_id) {
        return Ok(None);
    }
    if snapshot.owner_user_id != peek.owner_user_id
        || snapshot.schedule_id != task.schedule_id
        || snapshot.target_device_id != task.target_device_id
        || snapshot.kind != "fresh_task"
    {
        return Err(ScheduleStoreError::Conflict);
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
    let work = Box::new(
        run::Entity::find_by_id(peek.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?,
    );
    if !matches!(work.status.as_str(), "awaiting_children")
        || work.failure_accounted
        || work.finished_at.is_some()
        || work.lease_owner.is_some()
        || work.lease_deadline.is_some()
        || work.attempt != 1
        || work.conversation_id != work.run_id
        || work.started_at.is_none()
    {
        return Ok(None);
    }
    let row = Box::new(
        host.find_session(
            txn,
            SessionKey::Conversation {
                conversation: run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            false,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?,
    );
    let session = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?,
    );
    if session.actor_id != work.owner_user_id.to_string()
        || !row.personal
        || row.actor_id != session.actor_id
        || session.device_id != snapshot.target_device_id
        || row.device_id != session.device_id
        || row.version != session.version
        || row.lease_token != session.lease_token as i64
        || row.lease_deadline_ms.is_some()
        || session.conversation_id != work.run_id
        || !session.agent_role.is_main()
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.input_revision != 1
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || !(matches!(session.turn_state, TurnState::Idle | TurnState::Cancelled)
            || (session.turn_state == TurnState::Failed
                && session.is_subagent_result_turn()
                && (session
                    .ready_subagent_notification
                    .as_ref()
                    .is_some_and(|notice| {
                        notice.accepted_response_message_id.is_none()
                            && notice.retry_after_ms.is_some()
                    })
                    || session
                        .ready_subagent_wait
                        .as_ref()
                        .is_some_and(|wait| wait.retry_after_ms.is_some()))
                && session.terminal_error.as_ref().is_some_and(|error| {
                    error.kind == desk_agent_protocol::AgentErrorKind::ModelUnavailable
                        && error.retryable
                })))
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(Some(Box::new(FreshWait {
        work,
        row,
        session,
        subject_withdrawn,
    })))
}

async fn prepare_fresh_children_close<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    wait: Box<FreshWait<H::Row>>,
    run_id: &str,
) -> Result<Option<Box<ClosingFreshWait<H::Row>>>, ScheduleStoreError> {
    let work = &wait.work;
    let session = &wait.session;
    let subject_withdrawn = wait.subject_withdrawn;

    let reference = work
        .result_ref
        .as_deref()
        .and_then(|value| {
            value
                .strip_prefix("children:")
                .or_else(|| value.strip_prefix("answer-children:"))
                .or_else(|| {
                    session
                        .main_stopped
                        .then(|| value.strip_prefix("stopped-children:"))
                        .flatten()
                })
        })
        .filter(|value| desk_diagnose_core::subagent::valid_id(value))
        .ok_or(ScheduleStoreError::Invalid)?;
    if !session.main_stopped
        && !work
            .result_ref
            .as_deref()
            .is_some_and(|value| value.starts_with("answer-children:"))
        && session
            .subagent_wait
            .as_ref()
            .is_none_or(|wait| wait.wait_id != reference)
        && session
            .ready_subagent_wait
            .as_ref()
            .is_none_or(|wait| wait.wait_id != reference)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let groups = group_row::Entity::find()
        .filter(group_row::Column::RootConversationId.eq(run_id))
        .filter(group_row::Column::SourceOccurrenceId.eq(run_id))
        .filter(group_row::Column::SourceScheduleId.eq(&work.schedule_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .order_by_asc(group_row::Column::Id)
        .limit(2)
        .all(txn)
        .await?;
    let [group_row] = groups.as_slice() else {
        return Err(ScheduleStoreError::Conflict);
    };
    if session.delegation_group_id.as_deref() != Some(group_row.group_id.as_str())
        || (work
            .result_ref
            .as_deref()
            .is_some_and(|value| value.starts_with("stopped-children:"))
            && (!session.main_stopped || reference != group_row.group_id))
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let group = crate::delegation_group::decode_group(group_row)?;

    let source = host
        .decode_creation(group_row)?
        .scheduled_source
        .ok_or(ScheduleStoreError::Invalid)?;
    let now = authority_now(txn).await?;
    let authority_withdrawn = match lock_delegation_source_authority(
        txn,
        work.owner_user_id,
        &session.device_id,
        &source,
    )
    .await
    {
        Ok(_) => false,
        Err(
            ScheduleStoreError::Conflict
            | ScheduleStoreError::NotFound
            | ScheduleStoreError::BudgetExceeded,
        ) => true,
        Err(error) => return Err(error),
    };
    let tasks = crate::delegation_group::group_children_on(txn, &group).await?;
    let mut all_terminal = true;
    for task in &tasks {
        all_terminal &= crate::worker_request::decode_run(task)?.state.is_terminal();
    }
    let expired = now >= group.limits.deadline_ms;
    let cancelled = work.cancel_requested_at.is_some() || (session.main_stopped && all_terminal);
    let withdrawn = authority_withdrawn
        || subject_withdrawn
        || expired
        || group.source_admission == desk_diagnose_core::subagent::group::SourceAdmission::Closed;
    if !cancelled && !withdrawn {
        return Ok(None);
    }
    let group = Box::new(group_row.clone());
    Ok(Some(Box::new(ClosingFreshWait {
        wait,
        group,
        now,
        cancelled,
        expired,
    })))
}

async fn close_fresh_children_source<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    closing: &ClosingFreshWait<H::Row>,
) -> Result<
    (
        NativeDisposition,
        desk_diagnose_core::model_observability::permission::PendingEnds,
    ),
    ScheduleStoreError,
> {
    let session = &closing.wait.session;
    let group_row = &closing.group;
    let ClosingFreshWait {
        now,
        cancelled,
        expired,
        ..
    } = *closing;
    // Closing planning and native stop intents commit even while the real
    // command is still running. Returning false never rolls these back.
    let (_, mut permission_ends) = host.close_scheduled_group(txn, group_row, now).await?;
    permission_ends.extend(
        desk_diagnose_core::model_observability::permission::PendingEnds::waiting(
            session,
            if cancelled {
                desk_diagnose_core::model_observability::PermissionOutcome::Cancelled
            } else if expired {
                desk_diagnose_core::model_observability::PermissionOutcome::Expired
            } else {
                desk_diagnose_core::model_observability::PermissionOutcome::Revoked
            },
        ),
    );
    let current_group = group_row::Entity::find_by_id(group_row.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let native = host.scheduled_native(txn, &current_group).await?;
    Ok((native, permission_ends))
}

async fn persist_fresh_children_close<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    closing: Box<ClosingFreshWait<H::Row>>,
    native: NativeDisposition,
) -> Result<(), ScheduleStoreError> {
    let ClosingFreshWait {
        wait,
        now,
        cancelled,
        expired,
        ..
    } = *closing;
    let FreshWait {
        work,
        mut row,
        mut session,
        ..
    } = *wait;
    let locked = host
        .find_session(txn, SessionKey::Id(row.id), OrgMode::Any, true)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if &locked != row.as_ref() {
        return Err(ScheduleStoreError::Conflict);
    }
    row = Box::new(locked);
    let unknown = native == NativeDisposition::Unknown;
    let outcome = if unknown {
        desk_agent_protocol::schedule::ScheduledRunStatus::OutcomeUnknown
    } else if cancelled {
        desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled
    } else {
        desk_agent_protocol::schedule::ScheduledRunStatus::Failed
    };
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if !session.main_stopped {
        desk_diagnose_core::subagent::control::stop_main_session(
            &mut session,
            &timestamp.to_rfc3339(),
        )
        .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    session.finish_turn(
        if unknown || !cancelled {
            TurnState::Failed
        } else {
            TurnState::Cancelled
        },
        timestamp.to_rfc3339(),
    );
    session.terminal_error = Some(desk_agent_protocol::AgentError {
        kind: if unknown {
            desk_agent_protocol::AgentErrorKind::TransportError
        } else if cancelled {
            desk_agent_protocol::AgentErrorKind::Cancelled
        } else if expired {
            desk_agent_protocol::AgentErrorKind::Timeout
        } else {
            desk_agent_protocol::AgentErrorKind::PermissionDenied
        },
        message: if unknown {
            "An original delegated action has an unknown outcome; automatic retry is forbidden"
        } else if cancelled {
            "Scheduled task stopped; original child records were retained"
        } else {
            "The original scheduled source expired or its authority was withdrawn"
        }
        .into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    });
    session.version = row
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
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
                lease_deadline_null: true,
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
    let result_ref = work.result_ref.clone();
    desk_diagnose_core::future::send_phase(|| {
        settle_on(
            host,
            txn,
            Settlement {
                work: *work,
                now,
                outcome,
                offline_timeout: false,
                result_ref,
                error_kind: Some(
                    if unknown {
                        "outcome_unknown"
                    } else if cancelled {
                        "cancelled"
                    } else if expired {
                        "child_wait_timeout"
                    } else {
                        "source_withdrawn"
                    }
                    .into(),
                ),
            },
        )
    })
    .await?;
    Ok(())
}

async fn load_fresh_children_receipts<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    peek: run::Model,
    snapshot: entity::Model,
    run_id: &str,
) -> Result<Option<Box<FreshReceipts<H::Row>>>, ScheduleStoreError> {
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&peek.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(peek.owner_user_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.kind != "fresh_task"
        || snapshot.kind != "fresh_task"
        || snapshot.schedule_id != task.schedule_id
        || snapshot.owner_user_id != peek.owner_user_id
    {
        return Err(ScheduleStoreError::Conflict);
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
    let work = Box::new(
        run::Entity::find_by_id(peek.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?,
    );
    if work.status != "outcome_unknown"
        || !work.failure_accounted
        || work.finished_at.is_none()
        || work.receipts_reconciled_at.is_some()
        || work.lease_deadline.is_some()
        || work.conversation_id != work.run_id
        || !work.result_ref.as_deref().is_some_and(|value| {
            value.starts_with("children:")
                || value.starts_with("stopped-children:")
                || value.starts_with("answer-children:")
                || value.starts_with("delegated-effects:")
        })
    {
        return Ok(None);
    }
    let groups = group_row::Entity::find()
        .filter(group_row::Column::RootConversationId.eq(run_id))
        .filter(group_row::Column::SourceOccurrenceId.eq(run_id))
        .filter(group_row::Column::SourceScheduleId.eq(&work.schedule_id))
        .filter(group_row::Column::ActorId.eq(work.owner_user_id.to_string()))
        .filter(group_row::Column::DeviceId.eq(&snapshot.target_device_id))
        .limit(2)
        .all(txn)
        .await?;
    let [group] = groups.as_slice() else {
        return Err(ScheduleStoreError::Conflict);
    };
    if group.source_admission != "closed"
        || desk_diagnose_core::future::send_phase(|| host.scheduled_native(txn, group)).await?
            != NativeDisposition::Settled
    {
        return Ok(None);
    }
    desk_diagnose_core::future::send_phase(|| {
        load_fresh_receipt_root(host, txn, work, Box::new(snapshot), run_id)
    })
    .await
    .map(Some)
}

async fn load_fresh_receipt_root<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    work: Box<run::Model>,
    snapshot: Box<entity::Model>,
    run_id: &str,
) -> Result<Box<FreshReceipts<H::Row>>, ScheduleStoreError> {
    let row = Box::new(
        host.find_session(
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
        .ok_or(ScheduleStoreError::NotFound)?,
    );
    let session = Box::new(
        PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?,
    );
    if !row.personal
        || row.actor_id != work.owner_user_id.to_string()
        || row.actor_id != session.actor_id
        || row.device_id != snapshot.target_device_id
        || row.device_id != session.device_id
        || row.version != session.version
        || row.lease_token != session.lease_token as i64
        || row.lease_deadline_ms.is_some()
        || !session.agent_role.is_main()
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.input_revision != 1
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || !matches!(
            session.turn_state,
            TurnState::Idle | TurnState::Failed | TurnState::Cancelled
        )
        || !session.pending_auto_triggers.is_empty()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = authority_now(txn).await?;
    Ok(Box::new(FreshReceipts {
        work,
        row,
        session,
        now,
    }))
}

async fn persist_fresh_children_receipts<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    wait: Box<FreshReceipts<H::Row>>,
) -> Result<bool, ScheduleStoreError> {
    let FreshReceipts {
        work,
        row,
        mut session,
        now,
    } = *wait;
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if !session.execution_state.states().is_empty() || !session.unclosed_tool_call_ids().is_empty()
    {
        let restored = desk_diagnose_core::future::send_phase(|| {
            host.restore_completed_calls(txn, &work, &mut session, now)
        })
        .await?;
        if !restored
            || !session.execution_state.states().is_empty()
            || !session.unclosed_tool_call_ids().is_empty()
        {
            return Ok(false);
        }
        session.version = row
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
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
                    lease_deadline_null: true,
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
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            receipts_reconciled_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::Status.eq("outcome_unknown"))
        .filter(run::Column::FailureAccounted.eq(true))
        .filter(run::Column::ReceiptsReconciledAt.is_null())
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(true)
}
