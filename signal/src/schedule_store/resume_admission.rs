//! Task-first locks for actions originating in a scheduled continuation.
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::DbErr;

/// The session row of an action transaction. Scheduled sessions are joined to
/// their task and run fences; other sessions are returned as stored.
pub(crate) fn lock_action_session<'a>(
    txn: &'a crate::config::connection::DatabaseTransaction,
    conversation_id: &'a str,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<crate::entity::agent_session::Model>, DbErr>>
            + Send
            + 'a,
    >,
> {
    desk_assistant_data_store::schedule::lock_action_session(
        &super::SignalScheduleHost,
        txn,
        conversation_id,
    )
}

/// Join task authority to the action transaction before taking the session lock.
pub(crate) async fn fresh_action_authority_on(
    txn: &crate::config::connection::DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<super::CurrentTaskAuthority, DbErr> {
    desk_assistant_data_store::schedule::fresh_action_authority_on(txn, session).await
}

impl super::ScheduleStore {
    /// Return only the immutable task input.
    pub(crate) async fn scheduled_context_on(
        txn: &crate::config::connection::DatabaseTransaction,
        session: &PersistedAgentSession,
    ) -> Result<
        Option<(
            Option<desk_diagnose_core::input_read_context::ReadContextSelection>,
            chrono::DateTime<chrono::Utc>,
        )>,
        DbErr,
    > {
        desk_assistant_data_store::schedule::scheduled_context_on(txn, session).await
    }

    /// Metadata consent never substitutes for the native task and both live
    /// leases. Call before operation/quota locks, and repeat before dispatch.
    pub(crate) async fn assistant_retrieval_deadline_on(
        txn: &crate::config::connection::DatabaseTransaction,
        expected: &PersistedAgentSession,
    ) -> Result<u64, DbErr> {
        desk_assistant_data_store::schedule::assistant_retrieval_deadline_on(
            &super::SignalScheduleHost,
            txn,
            expected,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::entity;
    use super::super::{ClaimedContinuation, ContinuationClaim, ScheduleStore};
    use super::*;
    use crate::entity::{agent_schedule_run as run, agent_session};
    use desk_agent_protocol::schedule::SchedulePauseReason;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use desk_diagnose_core::schedule::lifecycle::FailureState;
    use desk_diagnose_core::session::ExecutionState;
    use desk_diagnose_core::session::TurnState;
    use sea_orm::{ActiveModelTrait, Set, TransactionTrait};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

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
