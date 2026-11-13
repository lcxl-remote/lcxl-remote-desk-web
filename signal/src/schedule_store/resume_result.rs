//! Persist a committed loop result from this exact continuation without device replay.
#[cfg(test)]
use super::entity;
use super::{ContinuationLease, ScheduleStore, ScheduleStoreError};
#[cfg(test)]
use crate::entity::{agent_action_item, agent_exec_task};
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_agent_protocol::AgentError;
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
use desk_assistant_data_store::schedule::{CommittedResult, SettlementLease, committed_result};
use desk_diagnose_core::session::PersistedAgentSession;
#[cfg(test)]
use desk_diagnose_core::{
    chat::ChatRole,
    dynamic_run::PermissionRequestState,
    session::{ExecutionState, TriggerOrigin, TurnState},
};
#[cfg(test)]
use sea_orm::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

impl ScheduleStore {
    /// `answer` is the returned LoopOutcome::Answered text, never a client assertion.
    /// Pending/unknown work is left for the dispatcher to wait for or reconcile.
    pub async fn finish_answered_continuation(
        &self,
        lease: ContinuationLease<'_>,
        answer: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.record_committed_continuation(
            lease,
            CommittedResult::Answer(answer),
            SettlementLease::Live,
        )
        .await
    }

    /// The exact error must already be persisted on the original failed turn.
    /// Transport loss with uncommitted session state still requires reconciliation.
    pub async fn finish_failed_continuation(
        &self,
        lease: ContinuationLease<'_>,
        error: &AgentError,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.record_committed_continuation(
            lease,
            CommittedResult::Failure(error),
            SettlementLease::Live,
        )
        .await
    }

    /// Called with the request ID returned by LoopOutcome::PermissionRequested.
    /// Waiting retains the active slot and does not account a terminal outcome.
    pub async fn await_continuation_permission(
        &self,
        lease: ContinuationLease<'_>,
        request_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.record_committed_continuation(
            lease,
            CommittedResult::Permission(request_id),
            SettlementLease::Live,
        )
        .await
    }

    /// Reconcile persisted results, permission pauses and original action records.
    /// Snapshot reads select evidence only; the settlement transaction rechecks
    /// the exact original turn, lease, input and all action families under locks.
    pub async fn recover_committed_continuation(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<Option<run::Model>, ScheduleStoreError> {
        let work = run::Entity::find()
            .filter(run::Column::OwnerUserId.eq(owner))
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.failure_accounted {
            return Ok(matches!(
                work.status.as_str(),
                "succeeded" | "failed" | "cancelled" | "outcome_unknown"
            )
            .then_some(work));
        }
        if work.status != "running"
            || work.attempt != 1
            || work.started_at.is_none()
            || work.finished_at.is_some()
        {
            return Ok(None);
        }
        let Some(node) = work.lease_owner.as_deref() else {
            return Ok(None);
        };
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&work.conversation_id))
            .filter(agent_session::Column::ActorId.eq(owner.to_string()))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let session = PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        let Some(result) = committed_result(&work, &session) else {
            return Ok(None);
        };
        self.record_committed_continuation(
            ContinuationLease {
                owner,
                run_id,
                node_id: node,
                run_epoch: work.lease_epoch,
                session_token: session.lease_token,
            },
            result,
            SettlementLease::Expired,
        )
        .await
        .map(Some)
    }

    async fn record_committed_continuation(
        &self,
        lease: ContinuationLease<'_>,
        result: CommittedResult<'_>,
        settlement_lease: SettlementLease,
    ) -> Result<run::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let decision = desk_assistant_data_store::schedule::record_committed_continuation_on(
            &super::SignalScheduleHost,
            &txn,
            lease,
            result,
            settlement_lease,
        )
        .await?;
        super::session_host::finish(txn, decision).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

    #[tokio::test]
    async fn permission_wait_keeps_active_slot_and_failure_count_without_a_live_executor() {
        let (store, queued, _, _) = super::super::resume_claim::tests::fixture().await;
        let schema = Schema::new(store.db.get_database_backend());
        store
            .db
            .execute(&schema.entity_table(agent_action_item::Entity))
            .await
            .unwrap();
        store
            .db
            .execute(&schema.entity_table(agent_exec_task::Entity))
            .await
            .unwrap();
        let claimed = store
            .claim_conversation_resume(super::super::ContinuationClaim {
                owner: 1,
                run_id: &queued.run_id,
                node_id: "node",
                lease_seconds: 90,
                policy_revision: 7,
                scope: AgentScope {
                    granted: vec![],
                    mode: ExecutionMode::ReadOnly,
                    expires_at: None,
                    policy_name: None,
                },
            })
            .await
            .unwrap();
        let mut session = claimed.session;
        session.turn_state = TurnState::Idle;
        session.handled_input_seq = session.latest_input_seq;
        session.permission_requests.push(serde_json::from_value(serde_json::json!({
            "schema_version":desk_diagnose_core::dynamic_run::PERMISSION_REQUEST_SCHEMA_VERSION, "request_id":"permission-wait", "input_revision":session.input_revision,
            "state":"pending", "created_at":"2026-09-06T00:00:00Z", "items":[{
                "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
                "expected_effect":"read_device", "resource_scope":["target:current_device"],
                "operation_scope":["observe"], "suggested_ttl_seconds":120,
                "suggested_max_uses":1, "reason":"Inspect current device"
            }]
        })).unwrap());
        session.permission_requests[0].validate().unwrap();
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut row: agent_session::ActiveModel = row.into();
        row.state_json = Set(session.encode_json_for_storage().unwrap());

        let before_session = row.update(&store.db).await.unwrap();
        let before_task = entity::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let held = || ContinuationLease {
            owner: 1,
            run_id: &claimed.run.run_id,
            node_id: "node",
            run_epoch: claimed.run.lease_epoch,
            session_token: session.lease_token,
        };
        assert!(
            store
                .await_continuation_permission(held(), "missing-request")
                .await
                .is_err()
        );
        let waiting = store
            .await_continuation_permission(held(), "permission-wait")
            .await
            .unwrap();
        assert_eq!(waiting.status, "awaiting_permission");
        assert_eq!(
            waiting.result_ref.as_deref(),
            Some("permission:permission-wait")
        );
        assert_eq!(
            store.continuation_candidates(0, 32).await.unwrap(),
            vec![waiting.clone()]
        );
        assert!(waiting.lease_deadline.is_none());
        assert!(waiting.finished_at.is_none());
        assert!(!waiting.failure_accounted);
        assert_eq!(
            store
                .await_continuation_permission(held(), "permission-wait")
                .await
                .unwrap(),
            waiting
        );
        assert!(
            store
                .await_continuation_permission(held(), "different-request")
                .await
                .is_err()
        );
        let mut stale = held();
        stale.run_epoch += 1;
        assert!(
            store
                .await_continuation_permission(stale, "permission-wait")
                .await
                .is_err()
        );
        assert!(
            store
                .finish_answered_continuation(held(), "not a terminal answer")
                .await
                .is_err()
        );
        let task = entity::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.active_run_id, before_task.active_run_id);
        assert_eq!(task.failure_state_json, before_task.failure_state_json);
        assert_eq!(task.revision, before_task.revision + 1);
        assert_eq!(
            agent_session::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before_session
        );
    }
}

#[cfg(test)]
mod terminal_recovery;
