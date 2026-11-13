//! Reclaim the original occurrence only after its durable child dependency resolves.
use super::{FreshTaskClaim, ScheduleStore, ScheduleStoreError};
#[cfg(test)]
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::PersistedAgentSession;
#[cfg(test)]
use desk_diagnose_core::session::{TriggerOrigin, TurnState};
#[cfg(test)]
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

impl ScheduleStore {
    /// Caller holds current owner/device and root control. Neither the dependency
    /// result nor this claim mints an operation grant or a new occurrence quota.
    pub(super) async fn claim_fresh_children_on(
        txn: &crate::config::connection::DatabaseTransaction,
        input: FreshTaskClaim<'_>,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        desk_assistant_data_store::schedule::claim_fresh_children_on(
            &super::SignalScheduleHost,
            txn,
            input,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::TransactionTrait;

    pub(super) async fn claim_on(
        db: &crate::config::connection::DatabaseConnection,
        parent: &PersistedAgentSession,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        let txn = crate::db::begin_write(db, agent_session::Entity).await?;
        let result = ScheduleStore::claim_fresh_children_on(
            &txn,
            FreshTaskClaim {
                owner: 1,
                run_id: &parent.conversation_id,
                node_id: "children-node",
                lease_seconds: 90,
                policy_revision: parent.policy_revision,
                scope: parent.scope_snapshot.clone(),
            },
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }

    #[tokio::test]
    async fn dependency_claim_waits_for_real_completion_then_renews_both_holders_once() {
        let (db, schedule, store, parent, task_id) =
            crate::agent_subagent_store::scheduled_test_wait_fixture().await;
        let before = run::Entity::find()
            .filter(run::Column::RunId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(claim_on(&db, &parent).await.is_err());
        crate::agent_subagent_store::complete_scheduled_test_child(&db, &task_id).await;
        let ready = store
            .resolve_parent_wait(&parent.conversation_id, &parent.actor_id, &parent.device_id)
            .await
            .unwrap();
        assert!(matches!(
            ready,
            crate::agent_subagent_store::ParentWaitResolution::Ready(_)
        ));
        let after = claim_on(&db, &parent).await.unwrap();
        assert_eq!(after.lease_token, parent.lease_token + 1);
        assert_eq!(after.input_revision, parent.input_revision);
        assert_eq!(after.delegation_group_id, parent.delegation_group_id);
        assert_eq!(after.trigger_origin, TriggerOrigin::ScheduledTask);
        assert!(after.ready_subagent_wait.is_some());
        assert!(claim_on(&db, &after).await.is_err());
        let current = run::Entity::find_by_id(before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.lease_epoch, before.lease_epoch + 1);
        assert_eq!(current.started_at, before.started_at);
        assert_eq!(current.attempt, 1);
        assert_eq!(current.status, "running");
        let txn = db.begin().await.unwrap();
        let authority = desk_assistant_data_store::schedule::lock_run_authority(
            &txn,
            1,
            &after.device_id,
            &after.conversation_id,
            "children-node",
            current.lease_epoch,
        )
        .await
        .unwrap();
        assert!(
            current.lease_deadline.unwrap()
                <= authority
                    .delegation_source()
                    .unwrap()
                    .deadline_ms()
                    .unwrap()
        );
        txn.rollback().await.unwrap();
        assert_eq!(
            schedule
                .read(1, &current.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(current.run_id.as_str())
        );
    }

    #[tokio::test]
    async fn resolved_result_never_reclaims_a_cancelled_source_or_stopped_parent() {
        for stopped in [false, true] {
            let (db, _, store, parent, task_id) =
                crate::agent_subagent_store::scheduled_test_wait_fixture().await;
            crate::agent_subagent_store::complete_scheduled_test_child(&db, &task_id).await;
            store
                .resolve_parent_wait(&parent.conversation_id, &parent.actor_id, &parent.device_id)
                .await
                .unwrap();
            if stopped {
                let row = agent_session::Entity::find()
                    .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                let mut current = PersistedAgentSession::decode_json(&row.state_json).unwrap();
                current.main_stopped = true;
                current.version += 1;
                agent_session::Entity::update_many()
                    .set(agent_session::ActiveModel {
                        state_json: Set(current.encode_json_for_storage().unwrap()),
                        version: Set(current.version),
                        ..Default::default()
                    })
                    .filter(agent_session::Column::Id.eq(row.id))
                    .exec(&db)
                    .await
                    .unwrap();
            } else {
                run::Entity::update_many()
                    .set(run::ActiveModel {
                        cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                        ..Default::default()
                    })
                    .filter(run::Column::RunId.eq(&parent.conversation_id))
                    .exec(&db)
                    .await
                    .unwrap();
            }
            assert!(claim_on(&db, &parent).await.is_err());
            let current = run::Entity::find()
                .filter(run::Column::RunId.eq(&parent.conversation_id))
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.status, "awaiting_children");
            assert!(current.lease_owner.is_none());
        }
    }
}

#[cfg(test)]
mod notification_tests;
