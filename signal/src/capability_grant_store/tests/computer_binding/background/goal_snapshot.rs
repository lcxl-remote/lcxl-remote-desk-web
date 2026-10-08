use super::*;
use desk_diagnose_core::goal::{
    GoalLimits, GoalModelBinding, GoalOpenRequest, GoalOpening, GoalRun,
};

#[tokio::test]
async fn goal_only_changes_advance_coherent_snapshot_without_changing_execution_cas() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("goal-snapshot.db")).await).await;
    let store = SignalAgentSessionStore::new(f.store.db.clone());
    let before = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let first = store
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert!(first.goal.is_none());
    let binding = GoalModelBinding {
        connection_id: "gateway".into(),
        connection_revision: 1,
        profile_revision: 1,
        model_id: "model".into(),
    };
    let goal = GoalRun::new(
        "goal-snapshot".into(),
        "run-1".into(),
        "actor-1".into(),
        "device-1".into(),
        "Calculate 6+8".into(),
        "input".into(),
        GoalOpening::OwnerRequest,
        binding.clone(),
        1,
        1000,
        GoalLimits::default(),
    )
    .unwrap();
    let txn = f.store.db.begin().await.unwrap();
    crate::agent_goal_store::insert_on(&txn, &goal)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let with_goal = store
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert!(with_goal.session.seq > first.session.seq);
    assert_eq!(with_goal.goal, Some(goal));
    assert_eq!(
        (store
            .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap())
        .session
        .seq,
        with_goal.session.seq
    );
    let request = GoalOpenRequest::new(
        "proposal-snapshot".into(),
        "run-1".into(),
        "actor-1".into(),
        "device-1".into(),
        "input".into(),
        1,
        "Refine the result".into(),
        GoalLimits::default(),
        binding,
        1001,
    )
    .unwrap();
    let txn = f.store.db.begin().await.unwrap();
    crate::agent_goal_open_store::insert_on(&txn, &request)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let with_request = store
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert!(with_request.session.seq > with_goal.session.seq);
    assert_eq!(with_request.pending_goal_open_request, Some(request));
    let after = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (before.version, before.lease_token, before.state_json),
        (after.version, after.lease_token, after.state_json)
    );
}
