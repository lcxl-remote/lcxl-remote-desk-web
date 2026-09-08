use super::super::{ClaimedContinuation, ContinuationClaim};
use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use sea_orm::{ActiveModelTrait, ConnectionTrait};

async fn fixture() -> (ScheduleStore, ClaimedContinuation, session_row::Model) {
    let (store, queued, row, _) = super::super::resume_claim::tests::fixture().await;
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
    let source = session_row::Entity::find_by_id(row.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    (store, claimed, source)
}
fn lease(claimed: &ClaimedContinuation) -> ContinuationLease<'_> {
    ContinuationLease {
        owner: 1,
        run_id: &claimed.run.run_id,
        node_id: "node",
        run_epoch: claimed.run.lease_epoch,
        session_token: claimed.session.lease_token,
    }
}
#[tokio::test]
async fn both_deadlines_extend_together_without_changing_session_version_or_json() {
    let (store, claimed, source) = fixture().await;
    assert!(
        store
            .renew_conversation_resume(lease(&claimed), 300)
            .await
            .unwrap()
    );
    let row = session_row::Entity::find_by_id(source.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let run = run::Entity::find_by_id(claimed.run.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state_json, source.state_json);
    assert_eq!(row.version, source.version);
    assert_eq!(row.lease_token, source.lease_token);
    assert_eq!(
        row.lease_deadline.unwrap().timestamp_millis(),
        run.lease_deadline.unwrap()
    );
    assert!(run.lease_deadline > claimed.run.lease_deadline);
    let deadline = run.lease_deadline;
    assert!(
        store
            .renew_conversation_resume(lease(&claimed), 30)
            .await
            .unwrap()
    );
    assert!(
        run::Entity::find_by_id(run.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .lease_deadline
            >= deadline
    );
}
#[tokio::test]
async fn either_expired_lease_or_cancel_blocks_renewal_without_changing_the_other() {
    for reason in 0..3 {
        let (store, claimed, source) = fixture().await;
        if reason == 0 {
            let mut changed: session_row::ActiveModel = source.into();
            changed.lease_deadline = Set(Some(chrono::DateTime::from_timestamp_millis(1).unwrap()));
            changed.update(&store.db).await.unwrap();
        } else {
            let mut changed: run::ActiveModel = claimed.run.clone().into();
            if reason == 1 {
                changed.lease_deadline = Set(Some(1));
            } else {
                changed.cancel_requested_at = Set(Some(1));
            }
            changed.update(&store.db).await.unwrap();
        }
        let before_run = run::Entity::find_by_id(claimed.run.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let before_session = session_row::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !store
                .renew_conversation_resume(lease(&claimed), 300)
                .await
                .unwrap()
        );
        assert_eq!(
            run::Entity::find_by_id(claimed.run.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before_run
        );
        assert_eq!(
            session_row::Entity::find_by_id(before_session.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before_session
        );
    }
}
#[tokio::test]
async fn pause_only_affects_future_runs_but_new_input_stops_this_lease() {
    let (store, claimed, source) = fixture().await;
    let task = store.read(1, &claimed.run.schedule_id).await.unwrap();
    store
        .pause(
            1,
            &task.schedule_id,
            task.revision,
            store.database_time().await.unwrap(),
        )
        .await
        .unwrap();
    assert!(
        store
            .renew_conversation_resume(lease(&claimed), 90)
            .await
            .unwrap()
    );
    let mut session = claimed.session.clone();
    session.begin_focus_epoch(2, Vec::<String>::new()).unwrap();
    session.input_revision = 2;
    session.version += 1;
    let mut changed: session_row::ActiveModel = source.into();
    changed.version = Set(session.version);
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    changed.update(&store.db).await.unwrap();
    assert!(
        !store
            .renew_conversation_resume(lease(&claimed), 300)
            .await
            .unwrap()
    );
}
#[tokio::test]
async fn a_failed_run_renewal_rolls_back_the_session_deadline() {
    let (store, claimed, source) = fixture().await;
    store.db.execute_unprepared("CREATE TRIGGER reject_run_renewal BEFORE UPDATE OF lease_deadline ON agent_schedule_run BEGIN SELECT RAISE(ABORT, 'injected renewal failure'); END").await.unwrap();
    assert!(matches!(
        store.renew_conversation_resume(lease(&claimed), 300).await,
        Err(ScheduleStoreError::Backend(_))
    ));
    assert_eq!(
        session_row::Entity::find_by_id(source.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        source
    );
    assert_eq!(
        run::Entity::find_by_id(claimed.run.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        claimed.run
    );
}

#[tokio::test]
async fn ordinary_step_saves_are_accepted_but_foreign_lease_holders_are_not() {
    let (store, claimed, source) = fixture().await;
    let mut session = claimed.session.clone();
    session.version += 1;
    let mut changed: session_row::ActiveModel = source.into();
    changed.version = Set(session.version);
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    let source = changed.update(&store.db).await.unwrap();
    let mut foreign = lease(&claimed);
    foreign.node_id = "another-node";
    assert!(!store.renew_conversation_resume(foreign, 300).await.unwrap());
    let mut foreign = lease(&claimed);
    foreign.session_token += 1;
    assert!(!store.renew_conversation_resume(foreign, 300).await.unwrap());
    assert_eq!(
        session_row::Entity::find_by_id(source.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        source
    );
    assert!(
        store
            .renew_conversation_resume(lease(&claimed), 300)
            .await
            .unwrap()
    );
    let renewed = session_row::Entity::find_by_id(source.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renewed.version, source.version);
    assert_eq!(renewed.state_json, source.state_json);
}

#[tokio::test]
async fn runtime_heartbeat_propagates_durable_cancel_and_rejects_wrong_session() {
    use super::super::ScheduleHeartbeat;
    use desk_diagnose_core::seam::LeaseHeartbeat;
    use tokio_util::sync::CancellationToken;
    for wrong_session in [false, true] {
        let (store, claimed, _) = fixture().await;
        let cancel = CancellationToken::new();
        let heartbeat =
            ScheduleHeartbeat::new(store.clone(), &claimed, "node".into(), 90, cancel.clone())
                .await
                .unwrap();
        if !wrong_session {
            store.cancel_run(1, &claimed.run.run_id).await.unwrap();
        }
        let guard = heartbeat.start(
            if wrong_session {
                "another-session".into()
            } else {
                claimed.session.conversation_id.clone()
            },
            claimed.session.lease_token,
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), cancel.cancelled())
            .await
            .unwrap();
        assert!(!heartbeat.is_healthy());
        drop(guard);
        let run = run::Entity::find_by_id(claimed.run.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, "running");
        assert!(!run.failure_accounted);
    }
}

#[tokio::test]
async fn execution_boundary_observes_cancel_without_starting_or_waiting_for_ticker() {
    use super::super::ScheduleHeartbeat;
    use desk_diagnose_core::seam::LeaseHeartbeat;
    use tokio_util::sync::CancellationToken;
    let (store, claimed, _) = fixture().await;
    let cancel = CancellationToken::new();
    let heartbeat =
        ScheduleHeartbeat::new(store.clone(), &claimed, "node".into(), 90, cancel.clone())
            .await
            .unwrap();
    assert!(heartbeat.check_current().await);
    store.cancel_run(1, &claimed.run.run_id).await.unwrap();
    assert!(heartbeat.is_healthy(), "no background ticker is running");
    assert!(!heartbeat.check_current().await);
    assert!(!heartbeat.is_healthy());
    assert!(cancel.is_cancelled());
    let row = run::Entity::find_by_id(claimed.run.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "running");
    assert!(!row.failure_accounted);
    assert!(
        !heartbeat.check_current().await,
        "failed checks cannot restore a stopped lease"
    );
}
