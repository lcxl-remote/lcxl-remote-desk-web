//! End original child waits without reclaiming planning or inventing native results.
use super::{ScheduleStore, ScheduleStoreError, entity};
#[cfg(test)]
use crate::agent_subagent_store as children;
use crate::entity::agent_schedule_run as run;
#[cfg(test)]
use crate::entity::{
    agent_delegation_group as group_row, agent_session, agent_subagent_run as child_row,
};
#[cfg(test)]
use desk_diagnose_core::session::{PersistedAgentSession, TurnState};
#[cfg(test)]
use sea_orm::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

impl ScheduleStore {
    /// Owner-only stop leaves finite children running until terminal or source
    /// expiry. Withdrawal closes child planning immediately; actual native work
    /// is then reconciled before releasing the original schedule's active slot.
    pub async fn settle_fresh_children_wait(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        desk_diagnose_core::future::send_phase(|| {
            self.settle_fresh_children_wait_with_observer(
                run_id,
                crate::model_metrics::runtime::submit,
            )
        })
        .await
    }

    async fn settle_fresh_children_wait_with_observer(
        &self,
        run_id: &str,
        mut submit: impl FnMut(desk_diagnose_core::model_observability::ObservationEvent),
    ) -> Result<bool, ScheduleStoreError> {
        let peek = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let snapshot: entity::Model = serde_json::from_str(&peek.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let txn = crate::db::begin_write(&self.db, entity::Entity).await?;
        let subject_withdrawn = false;
        let decision = desk_diagnose_core::future::send_phase(|| {
            desk_assistant_data_store::schedule::settle_fresh_children_wait_on(
                &super::SignalScheduleHost,
                &txn,
                peek,
                snapshot,
                subject_withdrawn,
                run_id,
            )
        })
        .await?;
        let Some(closed) = super::session_host::finish(txn, decision).await? else {
            return Ok(false);
        };
        closed.permission_ends.submit(closed.now, &mut submit);
        Ok(closed.settled)
    }

    /// Late original results backfill genuine root receipts and close the marker.
    /// The original outcome, failure policy and source admission stay unchanged.
    pub(super) fn reconcile_fresh_children_receipts<'a>(
        &'a self,
        run_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<bool, ScheduleStoreError>> + Send + 'a>,
    > {
        desk_diagnose_core::future::send_phase(|| {
            self.reconcile_fresh_children_receipts_inner(run_id)
        })
    }

    async fn reconcile_fresh_children_receipts_inner(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let peek = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let snapshot: entity::Model = serde_json::from_str(&peek.task_snapshot_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let txn = crate::db::begin_write(&self.db, entity::Entity).await?;
        let decision = desk_diagnose_core::future::send_phase(|| {
            desk_assistant_data_store::schedule::reconcile_fresh_children_receipts_on(
                &super::SignalScheduleHost,
                &txn,
                peek,
                snapshot,
                run_id,
            )
        })
        .await?;
        super::session_host::finish(txn, decision).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/model_metrics_scheduled_permissions.rs"
    ));
    use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};

    async fn original_run(
        db: &crate::config::connection::DatabaseConnection,
        root: &str,
    ) -> run::Model {
        run::Entity::find()
            .filter(run::Column::RunId.eq(root))
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn healthy_child_wait_is_not_a_completed_or_expired_occurrence() {
        let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
        let before = original_run(&db, &parent.conversation_id).await;
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert_eq!(original_run(&db, &parent.conversation_id).await, before);
        assert_eq!(
            schedule
                .read(1, &before.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(parent.conversation_id.as_str())
        );
    }

    #[tokio::test]
    async fn cancel_closes_children_and_releases_the_original_active_slot_once() {
        let (db, schedule, _, parent, task_id) = children::scheduled_test_wait_fixture().await;
        let before = original_run(&db, &parent.conversation_id).await;
        run::Entity::update_many()
            .set(run::ActiveModel {
                cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let after = original_run(&db, &parent.conversation_id).await;
        assert_eq!(after.status, "cancelled");
        assert!(after.failure_accounted && after.finished_at.is_some());
        assert_eq!(after.started_at, before.started_at);
        assert_eq!(after.attempt, before.attempt);
        assert_eq!(after.lease_epoch, before.lease_epoch);
        assert_eq!(after.result_ref, before.result_ref);
        assert!(
            schedule
                .read(1, &after.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, "cancelled");
        let group = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&child.group_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(group.source_admission, "closed");
        assert!(!group.parent_active);
        assert!(!group.creation_envelope_json.is_empty());
    }

    #[tokio::test]
    async fn main_only_stop_keeps_the_child_source_until_it_finishes_without_reviving_main() {
        let (db, schedule, store, parent, task_id) = children::scheduled_test_wait_fixture().await;
        store
            .stop_for_owner(
                &parent.conversation_id,
                &parent.actor_id,
                &parent.device_id,
                &AiAssistantStopControl {
                    client_request_id: "scheduled-main-only".into(),
                    expected_input_revision: parent.input_revision,
                    expected_control_revision: parent.control_revision,
                    subagent_choice: Some(SubAgentStopChoice::MainOnly),
                },
            )
            .await
            .unwrap();
        let stopped = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let saved = PersistedAgentSession::decode_json(&stopped.state_json).unwrap();
        assert!(saved.main_stopped);
        assert!(
            !schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let work = original_run(&db, &parent.conversation_id).await;
        assert_eq!(work.status, "awaiting_children");
        assert!(work.cancel_requested_at.is_none());
        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, "queued");
        children::complete_scheduled_test_child(&db, &task_id).await;
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let current = agent_session::Entity::find_by_id(stopped.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let current = PersistedAgentSession::decode_json(&current.state_json).unwrap();
        assert!(current.main_stopped);
        assert_eq!(current.lease_token, saved.lease_token);
        assert_eq!(current.input_revision, saved.input_revision);
        assert_eq!(current.control_revision, saved.control_revision);
        assert_eq!(current.turn_state, TurnState::Cancelled);
        assert_eq!(
            original_run(&db, &parent.conversation_id).await.status,
            "cancelled"
        );
    }

    #[tokio::test]
    async fn native_running_and_unknown_facts_survive_source_closure_without_replanning() {
        exercise_original_native_wait(false).await;
    }

    #[tokio::test]
    async fn owner_stop_retains_running_native_work_and_unknown_outcomes() {
        exercise_original_native_wait(true).await;
    }

    async fn exercise_original_native_wait(owner_stop: bool) {
        let (db, schedule, store, parent, task_id) = children::scheduled_test_wait_fixture().await;

        let child = child_row::Entity::find()
            .filter(child_row::Column::TaskId.eq(&task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        use crate::entity::agent_exec_task as native;
        let exec = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
        exec.create(
            "scheduled-native-action",
            "scheduled-native-generation",
            &child.child_conversation_id,
            "original-child-call",
            "original-host",
            "device-1",
            chrono::Utc::now() + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        exec.mark_running("scheduled-native-generation")
            .await
            .unwrap();
        let command = native::Entity::find()
            .filter(native::Column::ExecutionGeneration.eq("scheduled-native-generation"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let metric_aliases = if owner_stop {
            None
        } else {
            Some(seed_observed_schedule_waits(&db, &parent, &child.child_conversation_id).await)
        };
        let original = original_run(&db, &parent.conversation_id).await;
        if owner_stop {
            store
                .stop_for_owner(
                    &parent.conversation_id,
                    &parent.actor_id,
                    &parent.device_id,
                    &AiAssistantStopControl {
                        client_request_id: "stop-context-with-original-native-work".into(),
                        expected_input_revision: parent.input_revision,
                        expected_control_revision: parent.control_revision,
                        subagent_choice: Some(SubAgentStopChoice::IncludeSubAgents),
                    },
                )
                .await
                .unwrap();
        } else {
            run::Entity::update_many()
                .set(run::ActiveModel {
                    cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                    ..Default::default()
                })
                .filter(run::Column::Id.eq(original.id))
                .exec(&db)
                .await
                .unwrap();
        }
        let mut metric_events = Vec::new();
        assert!(
            !schedule
                .settle_fresh_children_wait_with_observer(&parent.conversation_id, |event| {
                    metric_events.push(event)
                })
                .await
                .unwrap()
        );
        if let Some(aliases) = metric_aliases {
            assert_observed_schedule_waits_ended(&metric_events, &aliases);
        }
        let before_unknown = original_run(&db, &parent.conversation_id).await;
        assert_eq!(before_unknown.status, "awaiting_children");
        let retained = native::Entity::find_by_id(command.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.cancel_requested_at.is_some());
        assert_eq!(retained.status, command.status);
        native::Entity::update_many()
            .set(native::ActiveModel {
                status: Set("unknown".into()),
                ..Default::default()
            })
            .filter(native::Column::Id.eq(command.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        let unknown = original_run(&db, &parent.conversation_id).await;
        assert_eq!(unknown.status, "outcome_unknown");
        assert!(unknown.receipts_reconciled_at.is_none());
        assert!(
            schedule
                .read(1, &unknown.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        assert!(
            !schedule
                .reconcile_late_task_receipts(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert_eq!(original_run(&db, &parent.conversation_id).await, unknown);
        let retained = native::Entity::find_by_id(command.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.cancel_requested_at.is_some());
        assert_eq!(retained.status, "unknown");
    }
    #[tokio::test]
    async fn settled_historical_child_receipts_close_only_the_marker_without_reviving_original_planning()
     {
        let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
        let before = original_run(&db, &parent.conversation_id).await;
        run::Entity::update_many()
            .set(run::ActiveModel {
                cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            schedule
                .settle_fresh_children_wait(&parent.conversation_id)
                .await
                .unwrap()
        );
        // Exercise a historical uncertain outcome with all retained native facts
        // already settled. Receipt reconciliation must not rewrite that outcome.
        run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set("outcome_unknown".into()),
                error_kind: Set(Some("outcome_unknown".into())),
                receipts_reconciled_at: Set(None),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(before.id))
            .exec(&db)
            .await
            .unwrap();
        let unknown = original_run(&db, &parent.conversation_id).await;
        let root_before = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            schedule
                .reconcile_late_task_receipts(&parent.conversation_id)
                .await
                .unwrap()
        );
        assert!(
            !schedule
                .reconcile_late_task_receipts(&parent.conversation_id)
                .await
                .unwrap()
        );
        let after = original_run(&db, &parent.conversation_id).await;
        assert_eq!(after.status, unknown.status);
        assert_eq!(after.error_kind, unknown.error_kind);
        assert_eq!(after.failure_accounted, unknown.failure_accounted);
        assert_eq!(after.finished_at, unknown.finished_at);
        assert_eq!(after.result_ref, unknown.result_ref);
        assert_eq!(after.lease_epoch, unknown.lease_epoch);
        assert!(after.receipts_reconciled_at.is_some());
        let root_after = agent_session::Entity::find_by_id(root_before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_before, root_after);
        let current = PersistedAgentSession::decode_json(&root_after.state_json).unwrap();
        assert!(current.main_stopped && !current.turn_state.is_active());
        assert!(
            schedule
                .read(1, &after.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
    }
}
