//! Task-first locks for actions originating in a scheduled continuation.
use super::entity;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_agent_protocol::schedule::SchedulePauseReason;
use desk_diagnose_core::{
    schedule::lifecycle::FailureState,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, sea_query::Expr};

/// A scheduled continuation acquires the SQLite write lock by touching its task.
/// Validation and the caller's action/outbox mutation share that transaction;
/// an intervening writer cannot turn a stale read snapshot into a valid dispatch.
// Keep child execution state on the heap instead of embedding it in each caller.
#[inline(never)]
pub(crate) fn lock_action_session<'a>(
    txn: &'a DatabaseTransaction,
    conversation_id: &'a str,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<agent_session::Model>, DbErr>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(|| lock_action_session_inner(txn, conversation_id))
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
    crate::agent_subagent_store::lock_child_source_on(txn, &initial).await?;
    if initial.trigger_origin == TriggerOrigin::ScheduledTask {
        return desk_diagnose_core::future::send_phase(|| {
            lock_fresh_action_session_on(txn, &peek, &initial)
        })
        .await
        .map(Some);
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
        desk_diagnose_core::future::send_phase(|| {
            validate_continuation_action_on(
                txn,
                conversation_id,
                &row,
                &session,
                &schedule_id,
                &run_id,
            )
        })
        .await?;
    }
    Ok(Some(row))
}

async fn lock_fresh_action_session_on(
    txn: &DatabaseTransaction,
    peek: &agent_session::Model,
    initial: &PersistedAgentSession,
) -> Result<agent_session::Model, DbErr> {
    desk_diagnose_core::future::send_phase(|| fresh_action_authority_on(txn, initial)).await?;
    let row = agent_session::Entity::find_by_id(peek.id)
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let current = decode(&row)?;
    if current.as_ref() != initial {
        return Err(invalid());
    }
    let authority =
        desk_diagnose_core::future::send_phase(|| fresh_action_authority_on(txn, &current)).await?;
    if row
        .lease_deadline
        .is_none_or(|deadline| deadline.timestamp_millis() <= authority.verified_at())
    {
        return Err(invalid());
    }
    Ok(row)
}

async fn validate_continuation_action_on(
    txn: &DatabaseTransaction,
    conversation_id: &str,
    row: &agent_session::Model,
    session: &PersistedAgentSession,
    schedule_id: &str,
    run_id: &str,
) -> Result<(), DbErr> {
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
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
        || task.active_run_id.as_deref() != Some(run_id)
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
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.active_control_connection_id.is_some()
        || !session.turn_state.is_active()
        || session.surface != AgentSessionSurface::AiAssistant
        || row
            .lease_deadline
            .is_none_or(|deadline| deadline.timestamp_millis() <= now)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Join task authority to the action transaction before taking the session lock.
/// A task grant cannot substitute for the current run, cancellation or lease.
pub(crate) async fn fresh_action_authority_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<super::CurrentTaskAuthority, DbErr> {
    if !session.agent_role.is_main()
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.surface != AgentSessionSurface::AiAssistant
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

fn decode(row: &agent_session::Model) -> Result<Box<PersistedAgentSession>, DbErr> {
    let session =
        Box::new(PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?);
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
impl super::ScheduleStore {
    /// Metadata consent never substitutes for the native task and both live
    /// leases. Call before operation/quota locks, and repeat before dispatch.
    pub(crate) async fn assistant_retrieval_deadline_on(
        txn: &crate::config::connection::DatabaseTransaction,
        expected: &PersistedAgentSession,
    ) -> Result<u64, DbErr> {
        use desk_diagnose_core::ai_assistant::scheduled_retrieval::{
            RetrievalOccurrence, ScheduledRetrievalKind,
        };
        if !matches!(
            expected.trigger_origin,
            TriggerOrigin::ScheduledTask | TriggerOrigin::ScheduledContinuation
        ) || !expected.agent_role.is_main()
        {
            return Err(invalid());
        }
        let data = expected.assistant_data.as_ref().ok_or_else(invalid)?;
        let run_id = data.retrieval_task_id.as_deref().ok_or_else(invalid)?;
        if expected.current_request_id.as_deref() != Some(run_id) {
            return Err(DbErr::Custom(
                "scheduled retrieval request differs from its occurrence".into(),
            ));
        }
        let row = desk_diagnose_core::future::send_phase(|| {
            lock_action_session(txn, &expected.conversation_id)
        })
        .await
        .map_err(|error| DbErr::Custom(format!("scheduled retrieval native fence: {error}")))?
        .ok_or_else(invalid)?;
        let mut current = decode(&row)?;
        // Model preparation computes this usage estimate in memory before its
        // next save. It grants no retrieval authority. All other original
        // session fields, including the spent ledger, still compare exactly.
        current.context_usage_basis = expected.context_usage_basis.clone();
        if current.as_ref() != expected {
            return Err(DbErr::Custom(
                "scheduled retrieval session differs from the original native row".into(),
            ));
        }
        // Repeat the original native SELECTs under the task/session fences;
        // transaction identity alone does not make earlier rows current.
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let snapshot: entity::Model =
            serde_json::from_str(&work.task_snapshot_json).map_err(|_| invalid())?;
        if work.finished_at.is_some()
            || work.owner_user_id != task.owner_user_id
            || work.conversation_id != expected.conversation_id
            || !same_retrieval_definition(&task, &snapshot)
        {
            return Err(DbErr::Custom(
                "scheduled retrieval task snapshot changed".into(),
            ));
        }
        let consent = Self::retrieval_consent(&task)
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        let fresh = expected.trigger_origin == TriggerOrigin::ScheduledTask;
        let mut deadline = row.lease_deadline.ok_or_else(invalid)?.timestamp_millis();
        let original_deadline = if fresh {
            let authority =
                desk_diagnose_core::future::send_phase(|| fresh_action_authority_on(txn, expected))
                    .await?;
            deadline = deadline.min(authority.valid_until());
            Some(
                authority
                    .delegation_source()
                    .map_err(|_| invalid())?
                    .deadline_ms()
                    .map_err(|_| invalid())?
                    .try_into()
                    .map_err(|_| invalid())?,
            )
        } else {
            deadline = deadline.min(work.lease_deadline.ok_or_else(invalid)?);
            None
        };
        let occurrence = RetrievalOccurrence {
            schedule_id: &task.schedule_id,
            task_revision: task.task_revision.try_into().map_err(|_| invalid())?,
            run_id,
            kind: if fresh {
                ScheduledRetrievalKind::FreshTask
            } else {
                ScheduledRetrievalKind::ConversationResume
            },
            source_conversation_id: consent.source_conversation_id.as_deref(),
            root_conversation_id: &expected.conversation_id,
            actor_id: &expected.actor_id,
            device_id: &expected.device_id,
            source_device_ref: &data.source_device_ref,
            source_instance_id: data.source_instance_id.as_deref(),
            started_at_unix_ms: work
                .started_at
                .ok_or_else(invalid)?
                .try_into()
                .map_err(|_| invalid())?,
            original_deadline_unix_ms: original_deadline,
        };
        consent
            .validate_occurrence_binding(expected, &occurrence)
            .map_err(|message| DbErr::Custom(message.into()))?;
        let now = super::authority::authority_now(txn)
            .await
            .map_err(|_| invalid())?;
        if deadline <= now {
            return Err(invalid());
        }
        deadline.try_into().map_err(|_| invalid())
    }
}

/// Compare the frozen task definition; mutable occurrence bookkeeping is fenced separately.
pub(super) fn same_retrieval_definition(task: &entity::Model, snapshot: &entity::Model) -> bool {
    task.schedule_id == snapshot.schedule_id
        && task.owner_user_id == snapshot.owner_user_id
        && task.target_device_id == snapshot.target_device_id
        && task.kind == snapshot.kind
        && task.task_revision == snapshot.task_revision
        && task.prompt == snapshot.prompt
        && task.model_id == snapshot.model_id
        && task.locale == snapshot.locale
        && task.spec_json == snapshot.spec_json
        && task.calc_version == snapshot.calc_version
        && task.source_conversation_id == snapshot.source_conversation_id
        && task.requirement_revision == snapshot.requirement_revision
        && task.contract_revision == snapshot.contract_revision
        && task.authorization_revision == snapshot.authorization_revision
        && task.retrieval_consent_json == snapshot.retrieval_consent_json
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

    #[tokio::test]
    async fn scheduled_retrieval_rejects_changed_consent_snapshot_and_original_start() {
        for change in 0..5 {
            let (store, queued, _, _) =
                super::super::resume_claim::tests::retrieval_fixture(true).await;
            let held = store
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
            match change {
                0 | 1 => {
                    let mut snapshot: entity::Model =
                        serde_json::from_str(&held.run.task_snapshot_json).unwrap();
                    if change == 0 {
                        snapshot.retrieval_consent_json = None;
                    } else {
                        snapshot.prompt.push_str(" altered");
                    }
                    run::Entity::update_many()
                        .col_expr(
                            run::Column::TaskSnapshotJson,
                            Expr::value(serde_json::to_string(&snapshot).unwrap()),
                        )
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                2 => {
                    entity::Entity::update_many()
                        .col_expr(
                            entity::Column::RetrievalConsentJson,
                            Expr::value(Option::<String>::None),
                        )
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                3 => {
                    run::Entity::update_many()
                        .col_expr(run::Column::StartedAt, Expr::value(1_i64))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                _ => {
                    run::Entity::update_many()
                        .col_expr(run::Column::FinishedAt, Expr::value(1_i64))
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
            }
            let root = agent_session::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let task = entity::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let work = run::Entity::find().one(&store.db).await.unwrap().unwrap();
            let txn = store.db.begin().await.unwrap();
            assert!(
                desk_diagnose_core::future::send_phase(|| {
                    super::super::ScheduleStore::assistant_retrieval_deadline_on(
                        &txn,
                        &held.session,
                    )
                })
                .await
                .is_err(),
                "change {change}"
            );
            txn.rollback().await.unwrap();
            assert_eq!(
                agent_session::Entity::find()
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                root
            );
            assert_eq!(
                entity::Entity::find()
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                task
            );
            assert_eq!(
                run::Entity::find().one(&store.db).await.unwrap().unwrap(),
                work
            );
        }
    }
}
