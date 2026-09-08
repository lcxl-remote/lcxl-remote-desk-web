use super::*;
use crate::{
    entity::{
        agent_permission_resume as receipt, agent_schedule as task, agent_schedule_run as run,
    },
    schedule_store::{
        ContinuationClaim, ContinuationLease, ContinuationPermissionClaim, ScheduleStore,
    },
};
use desk_agent_protocol::schedule::*;

#[tokio::test]
async fn scheduled_approval_claim_consumes_original_receipt_and_preserves_occurrence() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut settled = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    settled.handled_input_seq = settled.latest_input_seq;
    store.save(&mut settled).await.unwrap();
    let scheduler = ScheduleStore::new(store.db.clone());
    let now = Utc::now().timestamp() * 1000;
    let draft = ScheduleDraft {
        time_confirmation: None,
        client_create_key: "scheduled-permission".into(),
        kind: ScheduledTaskKind::ConversationResume,
        target_device_id: "device-1".into(),
        title: "Continue".into(),
        prompt: "Continue original request".into(),
        locale: None,
        model_id: None,
        spec: ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: "2099-01-01T00:00:00Z".into(),
            },
        },
        source_conversation_id: Some("conversation-1".into()),
        requirement_revision: Some(1),
        creation_source: ScheduleCreationSource::Manual,
    };
    let task = scheduler.create_draft(1, &draft, now).await.unwrap();
    let mut active: task::ActiveModel = task.into();
    active.status = Set("active".into());
    active.next_run_at = Set(Some(now - 1000));
    active.spec_json = Set(serde_json::to_string(&ScheduleSpec {
        schema_version: 1,
        rule: ScheduleRule::Once {
            at: chrono::DateTime::from_timestamp_millis(now - 1000)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        },
    })
    .unwrap());
    let active = active.update(&store.db).await.unwrap();
    let queued = scheduler
        .materialize_due(&active.schedule_id, active.revision)
        .await
        .unwrap()
        .unwrap();
    let claim = || ContinuationClaim {
        owner: 1,
        run_id: &queued.run_id,
        node_id: "node",
        lease_seconds: 90,
        policy_revision: 1,
        scope: super::super::claim("unused").current_pdp_scope,
    };
    let held = scheduler.claim_conversation_resume(claim()).await.unwrap();
    let mut session = held.session;
    session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    session.handled_input_seq = session.latest_input_seq;
    store.save(&mut session).await.unwrap();
    scheduler
        .await_continuation_permission(
            ContinuationLease {
                owner: 1,
                run_id: &queued.run_id,
                node_id: "node",
                run_epoch: held.run.lease_epoch,
                session_token: session.lease_token,
            },
            "permission-1",
        )
        .await
        .unwrap();
    // The initial start grace does not time out a run already waiting for its owner.
    run::Entity::update_many()
        .col_expr(run::Column::StartDeadline, Expr::value(now - 1))
        .exec(&store.db)
        .await
        .unwrap();
    decide(&store, &decisions, true).await.unwrap();
    let before = state(&store).await;
    let session = PersistedAgentSession::decode_json(&before.0[0].state_json).unwrap();
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
        .list_for_subject("conversation-1", "1", "device-1")
        .await
        .unwrap();
    let before_run = run::Entity::find().one(&store.db).await.unwrap().unwrap();
    let before_task = task::Entity::find().one(&store.db).await.unwrap().unwrap();
    let input = |version, grants| ContinuationPermissionClaim {
        continuation: claim(),
        request_id: "permission-1",
        expected_session_version: version,
        expected_run_epoch: held.run.lease_epoch,
        grants,
    };
    assert!(
        scheduler
            .claim_continuation_permission(input(session.version - 1, &grants))
            .await
            .is_err()
    );
    assert!(
        scheduler
            .claim_continuation_permission(input(session.version, &[]))
            .await
            .is_err()
    );
    let mut wrong = input(session.version, &grants);
    wrong.expected_run_epoch += 1;
    assert!(
        scheduler
            .claim_continuation_permission(wrong)
            .await
            .is_err()
    );
    let mut wrong = input(session.version, &grants);
    wrong.request_id = "another-permission";
    assert!(
        scheduler
            .claim_continuation_permission(wrong)
            .await
            .is_err()
    );
    let mut wrong = input(session.version, &grants);
    wrong.continuation.policy_revision += 1;
    assert!(
        scheduler
            .claim_continuation_permission(wrong)
            .await
            .is_err()
    );
    assert_eq!(
        run::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before_run
    );
    assert_eq!(
        task::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before_task
    );
    assert_eq!(state(&store).await, before);
    assert_eq!(
        receipt::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .state,
        "pending"
    );
    let resumed = scheduler
        .claim_continuation_permission(input(session.version, &grants))
        .await
        .unwrap();
    assert_eq!(resumed.run.run_id, held.run.run_id);
    assert_eq!(
        resumed.run.occurrence_identity,
        held.run.occurrence_identity
    );
    assert_eq!(resumed.run.started_at, held.run.started_at);
    assert_eq!(resumed.run.attempt, 1);
    assert_eq!(resumed.run.lease_epoch, held.run.lease_epoch + 1);
    assert_eq!(resumed.session.lease_token, session.lease_token + 1);
    assert_eq!(
        resumed.session.trigger_origin,
        TriggerOrigin::ScheduledContinuation
    );
    assert_eq!(
        resumed.session.current_request_id.as_deref(),
        Some(queued.run_id.as_str())
    );
    assert_eq!(
        resumed.session.current_turn_id.as_deref(),
        Some(resumed.run.turn_id.as_str())
    );
    assert_eq!(resumed.session.input_revision, session.input_revision);
    assert_eq!(resumed.run.result_ref, None);
    assert_eq!(
        receipt::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .state,
        "started"
    );
    assert!(
        scheduler
            .claim_continuation_permission(input(session.version, &grants))
            .await
            .is_err()
    );
    assert_eq!(run::Entity::find().all(&store.db).await.unwrap().len(), 1);
}
