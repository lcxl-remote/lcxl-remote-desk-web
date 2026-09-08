//! Persist a fresh context under the same transaction as the occurrence claim.
use super::{CurrentTaskAuthority, ScheduleStore, ScheduleStoreError, entity};
use crate::entity::{agent_schedule_run, agent_session};
use desk_agent_protocol::AgentScope;
use desk_diagnose_core::{
    schedule::fresh_session::{FreshSessionInput, initial_session},
    session::PersistedAgentSession,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    /// The caller resolves current subject/device policy and commits only after
    /// all admission checks pass. This writes no grants and starts no model.
    pub async fn insert_fresh_session_on(
        txn: &DatabaseTransaction,
        authority: &CurrentTaskAuthority,
        policy_revision: i64,
        scope: AgentScope,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        let work = authority.run();
        let node = work
            .lease_owner
            .as_deref()
            .ok_or(ScheduleStoreError::Conflict)?;
        let current = Self::lock_run_authority(
            txn,
            work.owner_user_id,
            &authority.contract().contract().target_device_id,
            &work.run_id,
            node,
            work.lease_epoch,
        )
        .await?;
        if current.provenance() != authority.provenance()
            || current.run() != work
            || work.attempt != 1
            || work.lease_epoch != 1
            || work.conversation_id != work.run_id
            || work.turn_id != format!("{}-turn", work.run_id)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
            .one(txn)
            .await?
            .is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        super::budget::reserve_run_budget(txn, &current).await?;
        let task: entity::Model = serde_json::from_str(&work.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let now = chrono::DateTime::from_timestamp_millis(current.verified_at())
            .ok_or(ScheduleStoreError::Invalid)?;
        let deadline = chrono::DateTime::from_timestamp_millis(
            work.lease_deadline.ok_or(ScheduleStoreError::Conflict)?,
        )
        .ok_or(ScheduleStoreError::Invalid)?;
        let mut session = initial_session(
            current.contract(),
            FreshSessionInput {
                run_id: &work.run_id,
                actor_id: &work.owner_user_id.to_string(),
                prompt: &task.prompt,
                locale: task.locale.as_deref(),
                policy_revision,
                scope,
                now: &now.to_rfc3339(),
            },
        )
        .map_err(|_| ScheduleStoreError::Conflict)?;
        session.version = 1;
        agent_session::Entity::insert(agent_session::ActiveModel {
            conversation_id: Set(session.conversation_id.clone()),
            actor_id: Set(session.actor_id.clone()),
            device_id: Set(session.device_id.clone()),
            state_json: Set(session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(Some(deadline)),
            created_at: Set(now),
            updated_at: Set(now),

            ..Default::default()
        })
        .exec_without_returning(txn)
        .await?;
        // Inserting can wait on a uniqueness lock; recheck expiry before returning.
        let after = Self::lock_run_authority(
            txn,
            work.owner_user_id,
            &session.device_id,
            &work.run_id,
            node,
            work.lease_epoch,
        )
        .await?;
        if after.provenance() != current.provenance()
            || agent_schedule_run::Entity::find_by_id(work.id)
                .one(txn)
                .await?
                .as_ref()
                != Some(work)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::super::publication::tests::{Verifier, fixture_on};
    use super::*;
    use crate::entity::{agent_task_authorization, agent_task_budget_reservation as budget_row};
    use desk_agent_protocol::ExecutionMode;
    use sea_orm::{ConnectionTrait, Database, Schema, TransactionTrait};

    #[tokio::test]
    async fn fresh_session_and_claim_commit_or_rollback_as_one_unit() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let (store, task, _, publication) = fixture_on(db).await;
        store
            .db
            .execute(
                &Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(agent_session::Entity),
            )
            .await
            .unwrap();
        store
            .publish_task(1, &publication, &Verifier(true))
            .await
            .unwrap();
        let queued = store
            .enqueue_manual(1, &task.schedule_id, "fresh-session")
            .await
            .unwrap();
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let scope = || AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        for revoked in [true, false] {
            let txn = store.db.begin().await.unwrap();
            let work = ScheduleStore::claim_queued_on(&txn, &queued.run_id, "fresh-node", 90)
                .await
                .unwrap();
            let authority = ScheduleStore::lock_run_authority(
                &txn,
                1,
                &task.target_device_id,
                &work.run_id,
                "fresh-node",
                work.lease_epoch,
            )
            .await
            .unwrap();
            if revoked {
                agent_task_authorization::Entity::update_many()
                    .set(agent_task_authorization::ActiveModel {
                        revoked_at: Set(Some(authority.verified_at())),
                        ..Default::default()
                    })
                    .filter(agent_task_authorization::Column::ScheduleId.eq(&task.schedule_id))
                    .exec(&txn)
                    .await
                    .unwrap();
                assert!(
                    ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
                        .await
                        .is_err()
                );
            } else {
                let session = ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
                    .await
                    .unwrap();
                assert_eq!(session.conversation.len(), 1);
                assert_eq!(session.conversation_id, work.run_id);
                assert!(session.permission_requests.is_empty());
                assert!(
                    ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
                        .await
                        .is_err()
                );
            }
            txn.rollback().await.unwrap();
            assert!(
                budget_row::Entity::find()
                    .all(&store.db)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
            assert_eq!(
                agent_schedule_run::Entity::find_by_id(queued.id)
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                queued
            );
            assert!(
                agent_session::Entity::find()
                    .all(&store.db)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let txn = store.db.begin().await.unwrap();
        let work = ScheduleStore::claim_queued_on(&txn, &queued.run_id, "fresh-node", 90)
            .await
            .unwrap();
        let authority = ScheduleStore::lock_run_authority(
            &txn,
            1,
            &task.target_device_id,
            &work.run_id,
            "fresh-node",
            work.lease_epoch,
        )
        .await
        .unwrap();
        let session = ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let rows = agent_session::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        let reservations = budget_row::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].kind, "run");
        assert_eq!(reservations[0].run_id, work.run_id);
        assert_eq!(reservations[0].reserved_units, 1);
        assert_eq!(
            PersistedAgentSession::decode_json(&rows[0].state_json)
                .unwrap()
                .conversation_id,
            session.conversation_id
        );
        assert_eq!(
            agent_schedule_run::Entity::find_by_id(queued.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            work
        );
        // Fresh executions renew both leases together without rewriting loop state.
        let lease = || super::super::FreshTaskLease {
            owner: 1,
            run_id: &work.run_id,
            node_id: "fresh-node",
            run_epoch: work.lease_epoch,
            session_token: session.lease_token,
        };
        assert!(store.renew_fresh_task(lease(), 300).await.unwrap());
        let renewed_session = agent_session::Entity::find_by_id(rows[0].id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let renewed_run = agent_schedule_run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renewed_session.version, rows[0].version);
        assert_eq!(renewed_session.state_json, rows[0].state_json);
        assert!(renewed_session.lease_deadline > rows[0].lease_deadline);
        assert!(renewed_run.lease_deadline > work.lease_deadline);
        assert!(
            !store
                .renew_conversation_resume(
                    super::super::ContinuationLease {
                        owner: 1,
                        run_id: &work.run_id,
                        node_id: "fresh-node",
                        run_epoch: work.lease_epoch,
                        session_token: session.lease_token,
                    },
                    300
                )
                .await
                .unwrap()
        );
        for case in ["session_expired", "run_expired", "cancelled", "revoked"] {
            match case {
                "session_expired" => {
                    agent_session::Entity::update_many()
                        .set(agent_session::ActiveModel {
                            lease_deadline: Set(Some(
                                chrono::DateTime::from_timestamp_millis(1).unwrap(),
                            )),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                "run_expired" => {
                    agent_schedule_run::Entity::update_many()
                        .set(agent_schedule_run::ActiveModel {
                            lease_deadline: Set(Some(1)),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                "cancelled" => {
                    agent_schedule_run::Entity::update_many()
                        .set(agent_schedule_run::ActiveModel {
                            cancel_requested_at: Set(Some(1)),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                "revoked" => {
                    agent_task_authorization::Entity::update_many()
                        .set(agent_task_authorization::ActiveModel {
                            revoked_at: Set(Some(1)),
                            ..Default::default()
                        })
                        .exec(&store.db)
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let before_task = store.read(1, &task.schedule_id).await.unwrap();
            let before_session = agent_session::Entity::find_by_id(rows[0].id)
                .one(&store.db)
                .await
                .unwrap();
            let before_run = agent_schedule_run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap();
            assert!(
                !matches!(store.renew_fresh_task(lease(), 300).await, Ok(true)),
                "{case}"
            );
            assert_eq!(
                store.read(1, &task.schedule_id).await.unwrap(),
                before_task,
                "{case}"
            );
            assert_eq!(
                agent_session::Entity::find_by_id(rows[0].id)
                    .one(&store.db)
                    .await
                    .unwrap(),
                before_session,
                "{case}"
            );
            assert_eq!(
                agent_schedule_run::Entity::find_by_id(work.id)
                    .one(&store.db)
                    .await
                    .unwrap(),
                before_run,
                "{case}"
            );
            agent_session::Entity::update_many()
                .set(agent_session::ActiveModel {
                    lease_deadline: Set(renewed_session.lease_deadline),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
            agent_schedule_run::Entity::update_many()
                .set(agent_schedule_run::ActiveModel {
                    lease_deadline: Set(renewed_run.lease_deadline),
                    cancel_requested_at: Set(None),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
            agent_task_authorization::Entity::update_many()
                .set(agent_task_authorization::ActiveModel {
                    revoked_at: Set(None),
                    ..Default::default()
                })
                .exec(&store.db)
                .await
                .unwrap();
        }
        use desk_diagnose_core::seam::LeaseHeartbeat;
        let cancel = tokio_util::sync::CancellationToken::new();
        let heartbeat =
            super::super::ScheduleHeartbeat::new_fresh(store.clone(), lease(), 300, cancel.clone())
                .await
                .unwrap();
        assert!(heartbeat.check_current().await);
        agent_schedule_run::Entity::update_many()
            .set(agent_schedule_run::ActiveModel {
                cancel_requested_at: Set(Some(1)),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        assert!(!heartbeat.check_current().await);
        assert!(cancel.is_cancelled());
        agent_schedule_run::Entity::update_many()
            .set(agent_schedule_run::ActiveModel {
                cancel_requested_at: Set(None),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        assert!(
            !heartbeat.check_current().await,
            "lost authority must remain terminal"
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let heartbeat =
            super::super::ScheduleHeartbeat::new_fresh(store.clone(), lease(), 300, cancel.clone())
                .await
                .unwrap();
        let guard = heartbeat.start("another-conversation".into(), session.lease_token);
        assert!(!heartbeat.is_healthy());
        assert!(cancel.is_cancelled());
        drop(guard);
        let renewed_session = agent_session::Entity::find_by_id(rows[0].id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let renewed_run = agent_schedule_run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        // An error after the session update must not leave only one lease renewed.
        store.db.execute_unprepared("CREATE TRIGGER reject_fresh_renewal BEFORE UPDATE OF lease_deadline ON agent_schedule_run BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let before_task = store.read(1, &task.schedule_id).await.unwrap();
        assert!(store.renew_fresh_task(lease(), 300).await.is_err());
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before_task);
        assert_eq!(
            agent_session::Entity::find_by_id(rows[0].id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            renewed_session
        );
        assert_eq!(
            agent_schedule_run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            renewed_run
        );
        let model_budget = super::super::TaskBudgetRequest {
            owner: 1,
            device: &task.target_device_id,
            run_id: &work.run_id,
            node: "fresh-node",
            lease_epoch: work.lease_epoch,
            kind: super::super::TaskBudgetKind::ModelTokens,
            rule_id: None,
            logical_key: "model-step-1",
            input_sha256: &"a".repeat(64),
            units: 10,
        };
        for change in ["version", "token", "message", "origin", "actor", "approval"] {
            let mut altered = session.clone();
            match change {
                "version" => altered.version += 1,
                "token" => altered.lease_token += 1,
                "message" => altered.conversation[0].text.push_str(" modified"),
                "origin" => {
                    altered.trigger_origin = desk_diagnose_core::session::TriggerOrigin::User
                }
                "actor" => altered.actor_id = "2".into(),
                "approval" => {
                    altered.turn_state = desk_diagnose_core::session::TurnState::AwaitingApproval
                }
                _ => unreachable!(),
            }
            let before = store.read(1, &task.schedule_id).await.unwrap();
            let txn = store.db.begin().await.unwrap();
            assert!(
                ScheduleStore::reserve_fresh_model_budget_on(&txn, &altered, &model_budget)
                    .await
                    .is_err(),
                "{change}"
            );
            txn.rollback().await.unwrap();
            assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
            assert_eq!(
                budget_row::Entity::find()
                    .all(&store.db)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
        let txn = store.db.begin().await.unwrap();
        let reserved = ScheduleStore::reserve_fresh_model_budget_on(&txn, &session, &model_budget)
            .await
            .unwrap();
        assert_eq!(reserved.kind, "model_tokens");
        assert_eq!(reserved.reserved_units, 10);
        txn.rollback().await.unwrap();
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            1
        );
        let txn = store.db.begin().await.unwrap();
        ScheduleStore::reserve_fresh_model_budget_on(&txn, &session, &model_budget)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            2
        );
        use crate::entity::model_egress_receipt;
        store
            .db
            .execute(
                &Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(model_egress_receipt::Entity),
            )
            .await
            .unwrap();
        let dispatch_request = super::super::TaskBudgetRequest {
            logical_key: "model-step-2",
            ..model_budget
        };
        let audit = desk_diagnose_core::sink_authorizer::SinkProjectionAudit {
            destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "oss-ai-gateway:1".into(),
                connection_revision: 1,
                model_id: "test-model".into(),
                profile_revision: 1,
            },
            envelope_ids: vec!["original-input".into()],
            digests_sha256: vec!["a".repeat(64)],
            total_bytes: 10,
        };
        store.db.execute_unprepared("CREATE TRIGGER reject_model_intent BEFORE INSERT ON model_egress_receipt BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let budgets = budget_row::Entity::find().all(&store.db).await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::reserve_fresh_model_dispatch_on(
                &txn,
                &session,
                &dispatch_request,
                "export-task",
                2,
                &audit,
                &crate::model_egress_store::test_inputs(&audit),
            )
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
        assert_eq!(
            budget_row::Entity::find().all(&store.db).await.unwrap(),
            budgets
        );
        assert!(
            model_egress_receipt::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .db
            .execute_unprepared("DROP TRIGGER reject_model_intent")
            .await
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        let dispatch = ScheduleStore::reserve_fresh_model_dispatch_on(
            &txn,
            &session,
            &dispatch_request,
            "export-task",
            2,
            &audit,
            &crate::model_egress_store::test_inputs(&audit),
        )
        .await
        .unwrap();
        assert_eq!(
            dispatch.receipt.receipt_id,
            format!("task-model-{}", dispatch.budget.reservation_id)
        );
        assert_eq!(dispatch.budget.reserved_units, 10);
        txn.commit().await.unwrap();
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            3
        );
        let before = store.read(1, &task.schedule_id).await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::reserve_fresh_model_dispatch_on(
                &txn,
                &session,
                &dispatch_request,
                "export-task",
                2,
                &audit,
                &crate::model_egress_store::test_inputs(&audit),
            )
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
        assert_eq!(
            budget_row::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            model_egress_receipt::Entity::find()
                .all(&store.db)
                .await
                .unwrap(),
            vec![dispatch.receipt.clone()]
        );
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::settle_fresh_model_dispatch_on(&txn, 1, &work.run_id, 2)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
        crate::model_egress_store::SignalModelEgressStore::new(store.db.clone())
            .record_terminal_usage(
                &dispatch.receipt.receipt_id,
                &desk_diagnose_core::chat::TokenUsage {
                    input_tokens: Some(2),
                    output_tokens: Some(3),
                    cache_read_tokens: Some(4),
                    cache_write_tokens: Some(5),
                },
            )
            .await
            .unwrap();
        agent_task_authorization::Entity::update_many()
            .set(agent_task_authorization::ActiveModel {
                revoked_at: Set(Some(1)),
                ..Default::default()
            })
            .exec(&store.db)
            .await
            .unwrap();
        let mut expected = store.read(1, &task.schedule_id).await.unwrap();
        expected.revision += 1;
        let txn = store.db.begin().await.unwrap();
        let settled = ScheduleStore::settle_fresh_model_dispatch_on(&txn, 1, &work.run_id, 2)
            .await
            .unwrap();
        assert_eq!(settled.charged_units, 14);
        assert_eq!(settled.state, "overrun");
        txn.commit().await.unwrap();
        let txn = store.db.begin().await.unwrap();
        assert_eq!(
            ScheduleStore::settle_fresh_model_dispatch_on(&txn, 1, &work.run_id, 2)
                .await
                .unwrap(),
            settled
        );
        txn.commit().await.unwrap();
        assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), expected);
        assert!(
            agent_task_authorization::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .iter()
                .all(|authorization| authorization.revoked_at == Some(1))
        );
    }
}
