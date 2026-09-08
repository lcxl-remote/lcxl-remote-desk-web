use super::*;
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    schedule::{ScheduleRule, ScheduledTaskKind},
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};
async fn base_fixture() -> (
    ScheduleStore,
    entity::Model,
    session_row::Model,
    PersistedAgentSession,
) {
    let store = super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(session_row::Entity))
        .await
        .unwrap();
    let mut session = PersistedAgentSession::new(
        "source",
        "1",
        "device-1",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-09-06T00:00:00Z",
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    let row = session_row::ActiveModel {
        conversation_id: Set("source".into()),
        actor_id: Set("1".into()),
        device_id: Set("device-1".into()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(0),
        lease_token: Set(0),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    let mut draft = super::super::tests::draft();
    draft.kind = ScheduledTaskKind::ConversationResume;
    draft.source_conversation_id = Some("source".into());
    draft.requirement_revision = Some(1);
    draft.spec.rule = ScheduleRule::Once {
        at: "2099-01-01T00:00:00Z".into(),
    };
    let task = store
        .create_draft(1, &draft, store.database_time().await.unwrap())
        .await
        .unwrap();
    (store, task, row, session)
}

pub(crate) async fn fixture() -> (
    ScheduleStore,
    run::Model,
    session_row::Model,
    PersistedAgentSession,
) {
    let (store, task, row, session) = base_fixture().await;
    let now = store.database_time().await.unwrap();
    let at = now - 1000;
    let mut active: entity::ActiveModel = task.into();
    active.status = Set("active".into());
    active.spec_json = Set(
        serde_json::to_string(&desk_agent_protocol::schedule::ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: chrono::DateTime::from_timestamp_millis(at)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        })
        .unwrap(),
    );
    active.next_run_at = Set(Some(at));
    let active = active.update(&store.db).await.unwrap();
    let queued = store
        .materialize_due(&active.schedule_id, active.revision)
        .await
        .unwrap()
        .unwrap();
    (store, queued, row, session)
}
fn claim(run: &run::Model) -> ContinuationClaim<'_> {
    ContinuationClaim {
        owner: 1,
        run_id: &run.run_id,
        node_id: "scheduler-a",
        lease_seconds: 90,
        policy_revision: 7,
        scope: AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        },
    }
}
#[tokio::test]
async fn occurrence_and_session_are_claimed_once_without_new_user_input() {
    let (store, queued, original, prior) = fixture().await;
    assert!(matches!(
        store.claim_queued(&queued.run_id, "generic", 90).await,
        Err(ScheduleStoreError::Invalid)
    ));
    let claimed = store
        .claim_conversation_resume(claim(&queued))
        .await
        .unwrap();
    assert_eq!(claimed.run.status, "running");
    assert_eq!(claimed.run.lease_owner.as_deref(), Some("scheduler-a"));
    assert_eq!(claimed.run.attempt, 1);
    assert_eq!(claimed.session.conversation_id, queued.conversation_id);
    assert_eq!(
        claimed.session.current_turn_id.as_deref(),
        Some(queued.turn_id.as_str())
    );
    assert_eq!(
        claimed.session.trigger_origin,
        TriggerOrigin::ScheduledContinuation
    );
    assert_eq!(claimed.session.input_revision, prior.input_revision);
    assert_eq!(claimed.session.chain_id, prior.chain_id);
    assert_eq!(claimed.session.conversation, prior.conversation);
    assert_eq!(claimed.session.policy_revision, 7);
    assert_eq!(claimed.session.version, original.version + 1);
    assert!(claimed.session.active_control_connection_id.is_none());
    let row = session_row::Entity::find_by_id(original.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&row.state_json).unwrap(),
        claimed.session
    );
    assert_eq!(
        row.lease_deadline.unwrap().timestamp_millis(),
        claimed.run.lease_deadline.unwrap()
    );
    assert!(matches!(
        store.claim_conversation_resume(claim(&queued)).await,
        Err(ScheduleStoreError::Conflict)
    ));
}
#[tokio::test]
async fn superseded_input_and_busy_session_do_not_consume_the_occurrence() {
    for busy in [false, true] {
        let (store, queued, original, mut session) = fixture().await;
        let task = store.read(1, &queued.schedule_id).await.unwrap();
        if busy {
            session
                .begin_turn(
                    "interactive",
                    None,
                    Some("browser".into()),
                    1,
                    claim(&queued).scope,
                    "2026-09-06T00:00:00Z",
                )
                .unwrap();
        } else {
            session.begin_focus_epoch(2, Vec::<String>::new()).unwrap();
            session.input_revision = 2;
        }
        let mut changed: session_row::ActiveModel = original.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.lease_token = Set(session.lease_token as i64);
        let source = changed.update(&store.db).await.unwrap();
        assert!(matches!(
            store.claim_conversation_resume(claim(&queued)).await,
            Err(ScheduleStoreError::Conflict)
        ));
        assert_eq!(
            run::Entity::find_by_id(queued.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            queued
        );
        assert_eq!(
            session_row::Entity::find_by_id(source.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            source
        );
        assert_eq!(store.read(1, &queued.schedule_id).await.unwrap(), task);
        assert_eq!(
            store
                .supersede_stale_continuation(1, &queued.run_id)
                .await
                .unwrap(),
            !busy
        );
        let after = run::Entity::find_by_id(queued.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.attempt, 0);
        assert_eq!(after.status, if busy { "queued" } else { "superseded" });
        assert_eq!(
            session_row::Entity::find_by_id(source.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            source
        );
    }
}
#[tokio::test]
async fn occurrence_write_failure_rolls_back_the_already_written_session() {
    let (store, queued, source, _) = fixture().await;
    let task = store.read(1, &queued.schedule_id).await.unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_scheduled_claim BEFORE UPDATE OF status ON agent_schedule_run WHEN NEW.status = 'running' BEGIN SELECT RAISE(ABORT, 'injected claim write failure'); END").await.unwrap();
    assert!(matches!(
        store.claim_conversation_resume(claim(&queued)).await,
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
        run::Entity::find_by_id(queued.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        queued
    );
    assert_eq!(store.read(1, &queued.schedule_id).await.unwrap(), task);
}
