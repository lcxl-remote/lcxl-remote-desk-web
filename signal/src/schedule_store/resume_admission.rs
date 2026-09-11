//! Task-first locks for actions originating in a scheduled continuation.
use super::entity;
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::{
    schedule::lifecycle::FailureState,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter, sea_query::Expr};

/// A scheduled continuation acquires the SQLite write lock by touching its task.
/// Validation and the caller's action/outbox mutation share that transaction;
/// an intervening writer cannot turn a stale read snapshot into a valid dispatch.
// Keep child execution state on the heap instead of embedding it in each caller.
#[inline(never)]
pub(crate) fn lock_action_session<'a>(
    txn: &'a DatabaseTransaction,
    conversation_id: &'a str,
) -> std::pin::Pin<
    Box<impl std::future::Future<Output = Result<Option<agent_session::Model>, DbErr>> + 'a>,
> {
    Box::pin(lock_action_session_inner(txn, conversation_id))
}

async fn lock_action_session_inner(
    txn: &DatabaseTransaction,
    conversation_id: &str,
) -> Result<Option<agent_session::Model>, DbErr> {
    let peek = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .one(txn)
        .await?;
    let Some(peek) = peek else {
        return Ok(None);
    };
    let initial = decode(&peek)?;
    if initial.trigger_origin == TriggerOrigin::ScheduledTask {
        fresh_action_authority_on(txn, &initial).await?;
        let row = agent_session::Entity::find_by_id(peek.id)
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let current = decode(&row)?;
        if current != initial {
            return Err(invalid());
        }
        let authority = fresh_action_authority_on(txn, &current).await?;
        if row
            .lease_deadline
            .is_none_or(|deadline| deadline.timestamp_millis() <= authority.verified_at())
        {
            return Err(invalid());
        }
        return Ok(Some(row));
    }
    let binding = if initial.trigger_origin == TriggerOrigin::ScheduledContinuation {
        let run_id = initial.current_request_id.as_deref().ok_or_else(invalid)?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(work.owner_user_id))
            .exec(txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(invalid());
        }
        Some((work.schedule_id, run_id.to_owned()))
    } else {
        None
    };
    let row = agent_session::Entity::find_by_id(peek.id)
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let session = decode(&row)?;
    if (session.trigger_origin == TriggerOrigin::ScheduledContinuation) != binding.is_some() {
        return Err(invalid());
    }
    if let Some((schedule_id, run_id)) = binding {
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&schedule_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(&run_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let failures: FailureState =
            serde_json::from_str(&task.failure_state_json).map_err(|_| invalid())?;
        let now = super::authority::authority_now(txn)
            .await
            .map_err(|error| match error {
                super::ScheduleStoreError::Backend(error) => error,
                _ => invalid(),
            })?;
        if task.kind != "conversation_resume"
            || task.calc_version != desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION
            || task.contract_revision.is_some()
            || task.authorization_revision.is_some()
            || session.lease_token == 0
            || !matches!(task.status.as_str(), "active" | "triggered" | "paused")
            || task.active_run_id.as_deref() != Some(run_id.as_str())
            || task.owner_user_id != work.owner_user_id
            || task.owner_user_id.to_string() != session.actor_id
            || task.target_device_id != session.device_id
            || task.source_conversation_id.as_deref() != Some(conversation_id)
            || task.requirement_revision != i64::try_from(session.input_revision).ok()
            || failures
                .pause_reasons
                .iter()
                .any(|reason| *reason != SchedulePauseReason::User)
            || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
            || work.schedule_id != schedule_id
            || work.status != "running"
            || work.cancel_requested_at.is_some()
            || work.failure_accounted
            || work.lease_epoch <= 0
            || work.lease_owner.as_ref().is_none_or(|node| node.is_empty())
            || work.lease_deadline.is_none_or(|deadline| deadline <= now)
            || work.conversation_id != conversation_id
            || session.current_request_id.as_deref() != Some(run_id.as_str())
            || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            || session.active_control_connection_id.is_some()
            || !session.turn_state.is_active()
            || session.surface != AgentSessionSurface::DeviceAssistant
            || row
                .lease_deadline
                .is_none_or(|deadline| deadline.timestamp_millis() <= now)
        {
            return Err(invalid());
        }
    }
    Ok(Some(row))
}

/// Join task authority to the action transaction before taking the session lock.
/// A task grant cannot substitute for the current run, cancellation or lease.
pub(crate) async fn fresh_action_authority_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<super::CurrentTaskAuthority, DbErr> {
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.surface != AgentSessionSurface::DeviceAssistant
        || session.input_revision != 1
        || session.lease_token == 0
        || session.current_request_id.as_deref() != Some(session.conversation_id.as_str())
        || session.active_control_connection_id.is_some()
        || !matches!(
            session.turn_state,
            desk_diagnose_core::session::TurnState::Running
                | desk_diagnose_core::session::TurnState::AwaitingApproval
        )
    {
        return Err(invalid());
    }
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(&session.conversation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if work.owner_user_id.to_string() != session.actor_id
        || work.conversation_id != session.conversation_id
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
    {
        return Err(invalid());
    }
    super::ScheduleStore::lock_run_authority(
        txn,
        work.owner_user_id,
        &session.device_id,
        &work.run_id,
        work.lease_owner.as_deref().ok_or_else(invalid)?,
        work.lease_epoch,
    )
    .await
    .map_err(|error| match error {
        super::ScheduleStoreError::Backend(error) => error,
        _ => invalid(),
    })
}

fn decode(row: &agent_session::Model) -> Result<PersistedAgentSession, DbErr> {
    let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if matches!(
        session.trigger_origin,
        TriggerOrigin::ScheduledContinuation | TriggerOrigin::ScheduledTask
    ) && (session.conversation_id != row.conversation_id
        || session.actor_id != row.actor_id
        || session.device_id != row.device_id
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token))
    {
        return Err(invalid());
    }
    Ok(session)
}
fn invalid() -> DbErr {
    DbErr::Custom("scheduled action is no longer bound to an active run".into())
}

#[cfg(test)]
mod tests {
    use super::super::{ClaimedContinuation, ContinuationClaim, ScheduleStore};
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::session::ExecutionState;
    use desk_diagnose_core::session::TurnState;
    use sea_orm::{ActiveModelTrait, Set, TransactionTrait};

    async fn fixture() -> (ScheduleStore, ClaimedContinuation) {
        let (store, queued, _, _) = super::super::resume_claim::tests::fixture().await;
        let claimed = store
            .claim_conversation_resume(ContinuationClaim {
                owner: 1,
                run_id: &queued.run_id,
                node_id: "node",
                lease_seconds: 90,
                policy_revision: 1,
                scope: AgentScope {
                    granted: vec![],
                    mode: ExecutionMode::ReadOnly,
                    expires_at: None,
                    policy_name: None,
                },
            })
            .await
            .unwrap();
        (store, claimed)
    }

    #[tokio::test]
    async fn scheduled_action_locks_current_task_without_advancing_session_or_leases() {
        let (store, claimed) = fixture().await;
        let txn = store.db.begin().await.unwrap();
        let row = lock_action_session(&txn, &claimed.session.conversation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.version, claimed.session.version);
        assert_eq!(row.lease_token, claimed.session.lease_token as i64);
        txn.commit().await.unwrap();
        let work = run::Entity::find_by_id(claimed.run.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work, claimed.run);
    }

    #[tokio::test]
    async fn cancelled_expired_or_rebound_schedule_cannot_authorize_action_transaction() {
        for change in 0..5 {
            let (store, claimed) = fixture().await;
            match change {
                0 => {
                    store.cancel_run(1, &claimed.run.run_id).await.unwrap();
                }
                1 => {
                    run::Entity::update_many()
                        .col_expr(run::Column::LeaseDeadline, Expr::value(0_i64))
                        .filter(run::Column::Id.eq(claimed.run.id))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                2 => {
                    entity::Entity::update_many()
                        .col_expr(entity::Column::ActiveRunId, Expr::value("other-run"))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                3 => {
                    entity::Entity::update_many()
                        .col_expr(entity::Column::RequirementRevision, Expr::value(2_i64))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                _ => {
                    run::Entity::update_many()
                        .col_expr(run::Column::TurnId, Expr::value("other-turn"))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
            }
            let txn = store.db.begin().await.unwrap();
            assert!(
                lock_action_session(&txn, &claimed.session.conversation_id)
                    .await
                    .is_err(),
                "change={change}"
            );
            txn.rollback().await.unwrap();
            let row = agent_session::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.version, claimed.session.version);
        }
    }

    #[tokio::test]
    async fn active_action_boundary_ignores_prior_failure_but_rejects_settled_turn() {
        for (state, interrupted, allowed) in [
            (TurnState::Running, false, true),
            (TurnState::AwaitingApproval, false, true),
            (TurnState::Idle, false, false),
            (TurnState::Failed, false, false),
            (TurnState::Cancelled, false, false),
            (TurnState::AwaitingApproval, true, true),
        ] {
            let (store, claimed) = fixture().await;
            let row = agent_session::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
            session.turn_state = state;
            if interrupted {
                session.execution_state = ExecutionState::Interrupted {
                    since: "2026-09-06T00:00:00Z".into(),
                };
            }
            let mut row: agent_session::ActiveModel = row.into();
            row.state_json = Set(session.encode_json_for_storage().unwrap());

            row.update(&store.db).await.unwrap();
            let txn = store.db.begin().await.unwrap();
            let result = lock_action_session(&txn, &claimed.session.conversation_id).await;
            assert_eq!(
                result.is_ok(),
                allowed,
                "{state:?}, interrupted={interrupted}"
            );
            txn.rollback().await.unwrap();
        }
    }

    #[tokio::test]
    async fn pausing_future_occurrences_preserves_current_action_admission() {
        let (store, claimed) = fixture().await;
        let task = entity::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut failures: FailureState = serde_json::from_str(&task.failure_state_json).unwrap();
        failures.pause_reasons.insert(SchedulePauseReason::User);
        let mut task: entity::ActiveModel = task.into();
        task.status = Set("paused".into());
        task.failure_state_json = Set(serde_json::to_string(&failures).unwrap());
        task.update(&store.db).await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert!(
            lock_action_session(&txn, &claimed.session.conversation_id)
                .await
                .unwrap()
                .is_some()
        );
        txn.rollback().await.unwrap();
    }
}
