use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::session::TurnState;
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

async fn fixture() -> (ScheduleStore, run::Model, PersistedAgentSession) {
    let (store, queued, _, _) = super::super::resume_claim::tests::fixture().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(agent_action_item::Entity))
        .await
        .unwrap();
    store
        .db
        .execute(&schema.create_table_from_entity(agent_exec_task::Entity))
        .await
        .unwrap();
    store
        .db
        .execute(&schema.create_table_from_entity(crate::entity::agent_run_event::Entity))
        .await
        .unwrap();
    let claimed = store
        .claim_conversation_resume(super::super::ContinuationClaim {
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
    let mut session = claimed.session;
    session.turn_state = TurnState::Idle;
    session.handled_input_seq = session.latest_input_seq;
    session.terminal_permission_request_id = Some("wait".into());
    session.permission_requests.push(serde_json::from_value(serde_json::json!({
        "schema_version":1, "request_id":"wait", "input_revision":session.input_revision,
        "state":"pending", "created_at":"2026-09-06T00:00:00Z", "items":[{
            "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
            "expected_effect":"read_device", "resource_scope":["target:current_device"],
            "operation_scope":["observe"], "suggested_ttl_seconds":120,
            "suggested_max_uses":1, "reason":"Inspect current device"
        }]
    })).unwrap());
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut row: agent_session::ActiveModel = row.into();
    row.state_json = Set(session.encode_json_for_storage().unwrap());
    row.lease_deadline = Set(None);
    row.update(&store.db).await.unwrap();
    let work = store
        .await_continuation_permission(
            super::super::ContinuationLease {
                owner: 1,
                run_id: &claimed.run.run_id,
                node_id: "node",
                run_epoch: claimed.run.lease_epoch,
                session_token: session.lease_token,
            },
            "wait",
        )
        .await
        .unwrap();
    (store, work, session)
}

#[tokio::test]
async fn cancellation_withdraws_wait_and_releases_slot_once() {
    let (store, work, session) = fixture().await;
    let before = store.read(1, &work.schedule_id).await.unwrap();
    let txn = crate::db::begin_write(&store.db, entity::Entity)
        .await
        .unwrap();
    lock_approval_on(&txn, &session, "wait").await.unwrap();
    txn.commit().await.unwrap();
    let cancelled = store.cancel_run(1, &work.run_id).await.unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert!(cancelled.finished_at.is_some() && cancelled.failure_accounted);
    assert_eq!(store.cancel_run(1, &work.run_id).await.unwrap(), cancelled);
    let after = store.read(1, &work.schedule_id).await.unwrap();
    assert!(after.active_run_id.is_none());
    assert_eq!(after.failure_state_json, before.failure_state_json);
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let closed = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(closed.turn_state, TurnState::Cancelled);
    assert_eq!(
        closed.permission_requests[0].state,
        desk_diagnose_core::dynamic_run::PermissionRequestState::Withdrawn
    );
    let txn = crate::db::begin_write(&store.db, entity::Entity)
        .await
        .unwrap();
    assert!(lock_approval_on(&txn, &session, "wait").await.is_err());
}

#[tokio::test]
async fn durable_cancel_intent_blocks_late_approval_before_cleanup() {
    let (store, work, session) = fixture().await;
    let mut row: run::ActiveModel = work.clone().into();
    row.cancel_requested_at = Set(Some(store.database_time().await.unwrap()));
    row.update(&store.db).await.unwrap();
    let txn = crate::db::begin_write(&store.db, entity::Entity)
        .await
        .unwrap();
    assert!(lock_approval_on(&txn, &session, "wait").await.is_err());
    txn.rollback().await.unwrap();
    assert_eq!(store.scan_approval_expiry_once(0).await.unwrap().expired, 1);
    assert_eq!(store.scan_approval_expiry_once(0).await.unwrap().expired, 0);
}

#[tokio::test]
async fn changed_input_cannot_be_closed_by_original_occurrence() {
    let (store, work, mut session) = fixture().await;
    session.input_revision += 1;
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut row: agent_session::ActiveModel = row.into();
    row.state_json = Set(session.encode_json_for_storage().unwrap());
    let before = row.update(&store.db).await.unwrap();
    let pending = store.cancel_run(1, &work.run_id).await.unwrap();
    assert_eq!(pending.status, "awaiting_permission");
    assert!(pending.cancel_requested_at.is_some());
    assert_eq!(
        agent_session::Entity::find_by_id(before.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn cancellation_between_session_pause_and_occurrence_checkpoint_is_settled() {
    let (store, work, session) = fixture().await;
    // The loop has persisted Idle and the exact permission request; the
    // scheduler has not yet committed its matching awaiting_permission row.
    let mut row: run::ActiveModel = work.clone().into();
    row.status = Set("running".into());
    row.result_ref = Set(None);
    row.lease_deadline = Set(Some(store.database_time().await.unwrap() + 90_000));
    row.update(&store.db).await.unwrap();
    assert_eq!(
        store.cancel_run(1, &work.run_id).await.unwrap().status,
        "running"
    );
    let waiting = store
        .await_continuation_permission(
            super::super::ContinuationLease {
                owner: 1,
                run_id: &work.run_id,
                node_id: "node",
                run_epoch: work.lease_epoch,
                session_token: session.lease_token,
            },
            "wait",
        )
        .await
        .unwrap();
    assert!(waiting.cancel_requested_at.is_some());
    assert_eq!(waiting.status, "awaiting_permission");
    assert_eq!(store.scan_approval_expiry_once(0).await.unwrap().expired, 1);
    let cancelled = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(cancelled.attempt, 1);
    assert_eq!(cancelled.lease_epoch, work.lease_epoch);
    assert!(
        store
            .read(1, &work.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .is_none()
    );
}
