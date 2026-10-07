use super::*;
use desk_diagnose_core::{
    session::{TriggerOrigin, TurnState},
    subagent::{SubAgentWaitReason, runtime::RuntimeTurn},
};

async fn candidate(db: &DatabaseConnection) -> (PersistedAgentSession, RuntimeTurn) {
    let (parent, calls) = super::creation::runnable_parent(db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let runtime = store
        .child_runtime_candidate(&task.task_id)
        .await
        .unwrap()
        .unwrap();
    (parent, runtime)
}

#[tokio::test]
async fn scanning_prepares_no_lease_and_stale_resource_deferral_cannot_override_a_claim() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    runtime.validate().unwrap();
    assert_eq!(runtime.origin(), TriggerOrigin::DelegatedTask);
    assert_eq!(runtime.session().lease_token, 0);
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    let queued = store.queued_task_candidates(0, 1).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert!(
        store
            .queued_task_candidates(queued[0].id, 1)
            .await
            .unwrap()
            .is_empty()
    );
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(run),
            &run.binding.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("one lease");
    };
    assert!(claimed.session.lease_token > 0);
    assert!(
        !store
            .defer_child_candidate(&runtime, Some(SubAgentWaitReason::ModelCapacity), None)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&db, &run.binding.task_id).await.state,
        SubAgentState::Running
    );
    assert!(
        store
            .child_runtime_candidate(&run.binding.task_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn temporary_resource_wait_preserves_original_task_and_requires_target_availability() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    assert!(
        store
            .defer_child_candidate(&runtime, Some(SubAgentWaitReason::DeviceUnavailable), None)
            .await
            .unwrap()
    );
    let waiting = load(&db, &run.binding.task_id).await;
    assert_eq!(waiting.state, SubAgentState::WaitingResource);
    assert_eq!(waiting.binding, run.binding);
    assert!(
        !store
            .retry_resource_task(&run.binding.task_id, true)
            .await
            .unwrap()
    );
    run_row::Entity::update_many()
        .set(run_row::ActiveModel {
            next_attempt_at_ms: Set(Some(0)),
            ..Default::default()
        })
        .filter(run_row::Column::TaskId.eq(&run.binding.task_id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        !store
            .retry_resource_task(&run.binding.task_id, false)
            .await
            .unwrap()
    );
    assert!(
        store
            .retry_resource_task(&run.binding.task_id, true)
            .await
            .unwrap()
    );
    let resumed = store
        .child_runtime_candidate(&run.binding.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.session().agent_role, runtime.session().agent_role);
    assert_eq!(resumed.session().lease_token, 0);
    assert_eq!(resumed.source(), runtime.source());
}

#[tokio::test]
async fn model_capacity_failure_releases_the_lease_without_completing_or_resetting_the_child() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    let SubAgentClaimOutcome::Claimed(mut claimed) = store
        .claim_child(
            &super::creation::claim_params(run),
            &run.binding.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("one lease");
    };
    claimed
        .session
        .finish_turn(TurnState::Failed, chrono::Utc::now().to_rfc3339());
    store
        .settle_turn_for_task(
            &mut claimed.session,
            Some("delegated_model_unavailable"),
            false,
        )
        .await
        .unwrap();
    let waiting = load(&db, &run.binding.task_id).await;
    assert_eq!(waiting.state, SubAgentState::WaitingResource);
    assert!(waiting.failure_reason.is_none());
    assert!(waiting.terminal_report.is_none());
    assert_eq!(waiting.binding, run.binding);
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.lease_deadline.is_none());
    assert_eq!(
        PersistedAgentSession::decode_json(&row.state_json)
            .unwrap()
            .agent_role,
        runtime.session().agent_role
    );
    assert!(
        store
            .queued_task_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deadline_settlement_is_independent_of_resource_waits_and_never_revives_a_task() {
    let db = database().await;
    let (_, runtime) = candidate(&db).await;
    let RuntimeTurn::Child { run, .. } = &runtime else {
        panic!("one child");
    };
    let store = SubAgentStore::new(db.clone());
    store
        .defer_child_candidate(&runtime, Some(SubAgentWaitReason::ModelCapacity), None)
        .await
        .unwrap();
    let after_deadline =
        chrono::DateTime::from_timestamp_millis(run.binding.deadline_ms + 1).unwrap();
    assert!(
        store
            .expire_task_at(&run.binding.task_id, after_deadline)
            .await
            .unwrap()
    );
    assert!(
        !store
            .expire_task_at(&run.binding.task_id, after_deadline)
            .await
            .unwrap()
    );
    let failed = load(&db, &run.binding.task_id).await;
    assert_eq!(failed.state, SubAgentState::Failed);
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("delegation_deadline_reached")
    );
    assert!(
        !store
            .retry_resource_task(&run.binding.task_id, true)
            .await
            .unwrap()
    );
    assert!(
        store
            .child_runtime_candidate(&run.binding.task_id)
            .await
            .unwrap()
            .is_none()
    );
}
