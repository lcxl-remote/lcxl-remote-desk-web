//! Persist a committed loop result from this exact continuation without device replay.
use super::{
    ContinuationLease, ScheduleStoreError,
    authority::authority_now,
    session::{
        ActionClass, ActionFilter, Finish, OrgMode, ScheduleHostStores, SessionGuard, SessionKey,
        SessionPatch,
    },
    settlement::{Settlement, settle_on},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_agent_protocol::{AgentError, schedule::ScheduledRunStatus};
use desk_diagnose_core::{
    chat::ChatRole,
    dynamic_run::PermissionRequestState,
    file_scope::DirectoryConsentState,
    session::{
        AgentSessionSurface, ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
    },
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, Set, sea_query::Expr,
};

/// A loop result already committed on the continuation's original turn.
#[derive(Clone, Copy)]
pub enum CommittedResult<'a> {
    Answer(&'a str),
    Failure(&'a AgentError),
    Permission(&'a str),
    Interrupted { cancelled: bool },
}

/// Whether settlement requires a live or an expired occurrence lease.
#[derive(Clone, Copy)]
pub enum SettlementLease {
    Live,
    Expired,
}

impl SettlementLease {
    fn accepts(self, deadline: Option<i64>, now: i64) -> bool {
        deadline.is_some_and(|deadline| match self {
            Self::Live => deadline > now,
            Self::Expired => deadline <= now,
        })
    }
}

/// The result an expired executor left committed on the original turn, if any.
pub fn committed_result<'a>(
    work: &run::Model,
    session: &'a PersistedAgentSession,
) -> Option<CommittedResult<'a>> {
    Some(match session.turn_state {
        TurnState::Idle if session.terminal_permission_request_id.is_some() => {
            CommittedResult::Permission(session.terminal_permission_request_id.as_deref().unwrap())
        }
        TurnState::Idle => {
            let answer = session
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == ChatRole::Assistant)
                .filter(|message| {
                    message.turn_id.as_deref() == Some(work.turn_id.as_str())
                        && message.tool_calls.is_empty()
                        && !message.text.trim().is_empty()
                })?;
            CommittedResult::Answer(&answer.text)
        }
        TurnState::Failed => CommittedResult::Failure(session.terminal_error.as_ref()?),
        TurnState::Running => CommittedResult::Interrupted {
            cancelled: work.cancel_requested_at.is_some(),
        },
        _ => return None,
    })
}

/// Settles the leased continuation with a result its loop already committed.
pub async fn record_committed_continuation_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    lease: ContinuationLease<'_>,
    result: CommittedResult<'_>,
    settlement_lease: SettlementLease,
) -> Result<Finish<run::Model>, ScheduleStoreError> {
    let interrupted = matches!(result, CommittedResult::Interrupted { .. });
    let cancelled = matches!(result, CommittedResult::Interrupted { cancelled: true });
    let (mut outcome, state, expected_error, mut error_kind) = match result {
        CommittedResult::Interrupted { .. } => (
            if cancelled {
                ScheduledRunStatus::Cancelled
            } else {
                ScheduledRunStatus::Failed
            },
            TurnState::Running,
            None,
            Some(
                if cancelled {
                    "cancelled"
                } else {
                    "executor_interrupted"
                }
                .into(),
            ),
        ),
        CommittedResult::Answer(answer) if !answer.trim().is_empty() => {
            (ScheduledRunStatus::Succeeded, TurnState::Idle, None, None)
        }
        CommittedResult::Answer(_) => return Err(ScheduleStoreError::Invalid),
        CommittedResult::Permission(_) => (
            ScheduledRunStatus::AwaitingPermission,
            TurnState::Idle,
            None,
            None,
        ),
        CommittedResult::Failure(error) => (
            ScheduledRunStatus::Failed,
            TurnState::Failed,
            Some(error),
            Some(
                serde_json::to_string(&error.kind)
                    .map_err(|_| ScheduleStoreError::Invalid)?
                    .trim_matches('"')
                    .to_string(),
            ),
        ),
    };
    let waiting = matches!(result, CommittedResult::Permission(_));
    let status = match result {
        CommittedResult::Interrupted { .. } => {
            if cancelled {
                "cancelled"
            } else {
                "failed"
            }
        }
        CommittedResult::Answer(_) => "succeeded",
        CommittedResult::Failure(_) => "failed",
        CommittedResult::Permission(_) => "awaiting_permission",
    };
    if lease.owner <= 0 || lease.run_epoch <= 0 || lease.session_token == 0 {
        return Err(ScheduleStoreError::Invalid);
    }
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(lease.run_id))
        .filter(run::Column::OwnerUserId.eq(lease.owner))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if work.lease_owner.as_deref() != Some(lease.node_id) || work.lease_epoch != lease.run_epoch {
        return Err(ScheduleStoreError::Conflict);
    }
    if let CommittedResult::Permission(id) = result
        && work.status == "awaiting_permission"
        && !work.failure_accounted
        && work.cancel_requested_at.is_none()
        && work.result_ref.as_deref().is_some_and(|reference| {
            reference == format!("permission:{id}") || reference == format!("directory:{id}")
        })
    {
        return Ok(Finish::Rollback(work));
    }
    if work.failure_accounted {
        return if work.status == status && work.error_kind == error_kind {
            Ok(Finish::Rollback(work))
        } else {
            Err(ScheduleStoreError::Conflict)
        };
    }
    // Task -> session -> occurrence -> work matches action admission.
    // Reread the occurrence after locks; a pre-lock peek is not authority.
    let locked = entity::Entity::update_many()
        .col_expr(
            entity::Column::Revision,
            Expr::col(entity::Column::Revision),
        )
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(lease.owner))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: &work.conversation_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            true,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let work = run::Entity::find_by_id(work.id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if work.lease_owner.as_deref() != Some(lease.node_id)
        || work.lease_epoch != lease.run_epoch
        || work.owner_user_id != lease.owner
    {
        return Err(ScheduleStoreError::Conflict);
    }
    if let CommittedResult::Permission(id) = result
        && work.status == "awaiting_permission"
        && !work.failure_accounted
        && work.cancel_requested_at.is_none()
        && work.result_ref.as_deref().is_some_and(|reference| {
            reference == format!("permission:{id}") || reference == format!("directory:{id}")
        })
    {
        return Ok(Finish::Rollback(work));
    }
    if work.failure_accounted {
        return if work.status == status && work.error_kind == error_kind {
            Ok(Finish::Rollback(work))
        } else {
            Err(ScheduleStoreError::Conflict)
        };
    }
    // Cancellation may commit after the loop persisted its permission
    // pause but before this occurrence checkpoint. Preserve that exact
    // pause so cancellation cleanup can close it without redispatch.
    let cancelled = cancelled || (waiting && work.cancel_requested_at.is_some());
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch) {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = authority_now(txn).await?;
    if work.status != "running"
        || work.cancel_requested_at.is_some() != cancelled
        || !settlement_lease.accepts(work.lease_deadline, now)
        || task.kind != "conversation_resume"
        || task.calc_version != desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION
        || task.active_run_id.as_deref() != Some(lease.run_id)
        || task.source_conversation_id.as_deref() != Some(work.conversation_id.as_str())
        || task.requirement_revision != i64::try_from(session.input_revision).ok()
        || session.actor_id != lease.owner.to_string()
        || row.actor_id != session.actor_id
        || session.device_id
            != super::continuation_subject(&task).map_err(|_| ScheduleStoreError::Invalid)?
        || row.device_id != session.device_id
        || !row.personal
        || !row.stored_turn_state_is(state)
        || session.conversation_id != work.conversation_id
        || session.surface != AgentSessionSurface::AiAssistant
        || session.trigger_origin != TriggerOrigin::ScheduledContinuation
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.current_request_id.as_deref() != Some(lease.run_id)
        || session.version != row.version
        || session.lease_token != lease.session_token
        || i64::try_from(lease.session_token).ok() != Some(row.lease_token)
        || session.turn_state != state
        || session.terminal_error.as_ref() != expected_error
        || (!interrupted && session.execution_state != ExecutionState::None)
        || (state == TurnState::Idle && session.handled_input_seq != session.latest_input_seq)
        || !session.pending_auto_triggers.is_empty()
        || (!waiting
            && (session.permission_requests.iter().any(|request| {
                request.input_revision == session.input_revision
                    && request.state == PermissionRequestState::Pending
            }) || session
                .file_scope
                .records()
                .iter()
                .any(|record| record.state == DirectoryConsentState::Pending)))
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let result_ref = match result {
        CommittedResult::Interrupted { .. } => {
            let session_deadline = row.lease_deadline_ms;
            if !matches!(settlement_lease, SettlementLease::Expired)
                || session_deadline.is_some_and(|at| at > now)
                || session.terminal_permission_request_id.is_some()
            {
                return Err(ScheduleStoreError::Conflict);
            }
            None
        }
        CommittedResult::Answer(answer) => {
            let message = session
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == ChatRole::Assistant)
                .filter(|message| {
                    message.text == answer
                        && message.tool_calls.is_empty()
                        && message.turn_id.as_deref() == Some(work.turn_id.as_str())
                })
                .ok_or(ScheduleStoreError::Conflict)?;
            let reference = format!("message:{}", message.message_id);
            if reference.len() > 512 {
                return Err(ScheduleStoreError::Invalid);
            }
            Some(reference)
        }
        CommittedResult::Failure(_) => None,
        CommittedResult::Permission(id) => {
            if matches!(settlement_lease, SettlementLease::Expired)
                && session.terminal_permission_request_id.as_deref() != Some(id)
            {
                return Err(ScheduleStoreError::Conflict);
            }
            Some(
                desk_diagnose_core::schedule::permission_wait::reference(&session, id)
                    .ok_or(ScheduleStoreError::Conflict)?,
            )
        }
    };
    let before_recovery = session.clone();
    if interrupted {
        host.reconcile_scheduled_actions(txn, &mut session, now)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    let awaiting_result = interrupted && session.execution_state.is_running();
    let unknown_result = interrupted && session.execution_state.unknown().is_some();
    if unknown_result {
        outcome = ScheduledRunStatus::OutcomeUnknown;
        error_kind = Some("outcome_unknown".into());
    }
    // Do not infer an outcome from a budget reservation or a rendered answer.
    // All action families must have reached a known terminal transport state.
    let actions = host
        .find_actions(txn, &work.conversation_id, ActionFilter::default(), true)
        .await?;
    let allow_result = awaiting_result || unknown_result;
    if actions.iter().any(|action| {
        let awaited = allow_result
            && session
                .execution_state
                .tasks()
                .into_iter()
                .any(|identity| host.action_matches_identity(action, identity));
        blocks_continuation(action.class, awaited)
    }) {
        return Err(ScheduleStoreError::Conflict);
    }
    if host
        .has_open_exec_tasks(txn, &work.conversation_id, &session, allow_result)
        .await?
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let now = authority_now(txn).await?;
    if !settlement_lease.accepts(work.lease_deadline, now) {
        return Err(ScheduleStoreError::Conflict);
    }
    let at = chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if interrupted {
        if awaiting_result {
            if session == before_recovery {
                return Err(ScheduleStoreError::Conflict);
            }
        } else {
            session.finish_turn(TurnState::Failed, at.to_rfc3339());
        }
        session.terminal_error = if awaiting_result {
            None
        } else {
            let kind = if cancelled && !unknown_result {
                desk_agent_protocol::AgentErrorKind::Cancelled
            } else {
                desk_agent_protocol::AgentErrorKind::Internal
            };
            let message = if unknown_result {
                "The original scheduled action outcome is unknown; do not retry automatically."
            } else if cancelled {
                "Scheduled turn cancelled; original action records were reconciled."
            } else {
                "The scheduled executor lease expired before the turn completed; original action records were reconciled."
            };
            Some(AgentError {
                kind,
                message: message.into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })
        };
    }
    let restored = !waiting
        && !awaiting_result
        && desk_diagnose_core::ai_assistant::scheduled_retrieval::restore_owner_retrieval(
            &mut session,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if interrupted || restored {
        session.version = session
            .version
            .checked_add(1)
            .ok_or(ScheduleStoreError::Invalid)?;
        desk_diagnose_core::image_input::strip_session_images(&mut session.conversation);
        desk_diagnose_core::visual_evidence::strip_previews(&mut session.visual_evidence);
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
                    lease_deadline_ms: Some(None),
                    updated_at_ms: Some(now),
                    turn_state: Some(session.turn_state.as_str()),
                    ..Default::default()
                },
            )
            .await?;
        if changed != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
    }
    if awaiting_result {
        return Ok(Finish::Commit(work));
    }
    host.settle_scheduled_decision(txn, &session, now)
        .await
        .map_err(|_| ScheduleStoreError::Conflict)?;
    if waiting {
        let changed = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("awaiting_permission".into()),
                result_ref: Set(result_ref),
                lease_deadline: Set(None),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::LeaseEpoch.eq(lease.run_epoch))
            .filter(if cancelled {
                run::Column::CancelRequestedAt.is_not_null()
            } else {
                run::Column::CancelRequestedAt.is_null()
            })
            .exec(txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = run::Entity::find_by_id(work.id)
            .one(txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        return Ok(Finish::Commit(result));
    }
    settle_on(
        host,
        txn,
        Settlement {
            work,
            now,
            outcome,
            offline_timeout: false,
            error_kind,
            result_ref,
        },
    )
    .await
    .map(Finish::Commit)
}

/// Whether an action leaves the continuation unsettled. Only the session's own
/// awaited action may still be dispatching or have an unknown outcome.
fn blocks_continuation(class: ActionClass, awaited: bool) -> bool {
    if awaited
        && matches!(
            class,
            ActionClass::Dispatching | ActionClass::OutcomeUnknown
        )
    {
        return false;
    }
    !matches!(
        class,
        ActionClass::Succeeded
            | ActionClass::Failed
            | ActionClass::Withdrawn
            | ActionClass::LegacyClosed
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_finished_or_awaited_actions_let_a_continuation_settle() {
        use ActionClass::*;
        for (class, unawaited, awaited) in [
            (Succeeded, false, false),
            (Failed, false, false),
            (Withdrawn, false, false),
            (LegacyClosed, false, false),
            (OutcomeUnknown, true, false),
            (Dispatching, true, false),
            (Open, true, true),
        ] {
            assert_eq!(blocks_continuation(class, false), unawaited, "{class:?}");
            assert_eq!(blocks_continuation(class, true), awaited, "{class:?}");
        }
    }
}
