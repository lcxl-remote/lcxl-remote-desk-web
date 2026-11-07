use super::*;
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    schedule::{ScheduleRule, ScheduledTaskKind},
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema, TransactionTrait};
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
    session.surface = AgentSessionSurface::AiAssistant;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    session.bind_assistant_input_targets(&[]).unwrap();
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
    materialize_fixture(store, task, row, session).await
}

pub(crate) async fn retrieval_fixture(
    enabled: bool,
) -> (
    ScheduleStore,
    run::Model,
    session_row::Model,
    PersistedAgentSession,
) {
    let store = super::super::tests::store().await;
    let session = super::super::tests::retrieval_source(&store, enabled).await;
    let row = session_row::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut draft = super::super::tests::draft();
    draft.kind = ScheduledTaskKind::ConversationResume;
    draft.source_conversation_id = Some(session.conversation_id.clone());
    draft.requirement_revision = Some(session.input_revision);
    draft.spec.rule = ScheduleRule::Once {
        at: "2099-01-01T00:00:00Z".into(),
    };
    let task = store
        .create_draft(1, &draft, store.database_time().await.unwrap())
        .await
        .unwrap();
    materialize_fixture(store, task, row, session).await
}

async fn materialize_fixture(
    store: ScheduleStore,
    task: entity::Model,
    row: session_row::Model,
    session: PersistedAgentSession,
) -> (
    ScheduleStore,
    run::Model,
    session_row::Model,
    PersistedAgentSession,
) {
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

async fn exercise_retrieval_claim(
    store: ScheduleStore,
    queued: run::Model,
    original: session_row::Model,
    prior: PersistedAgentSession,
) {
    use desk_diagnose_core::ai_assistant::history::RetrievalCallKind;
    let task = store.read(1, &queued.schedule_id).await.unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_retrieval_claim BEFORE UPDATE OF status ON agent_schedule_run WHEN NEW.status = 'running' BEGIN SELECT RAISE(ABORT, 'injected'); END").await.unwrap();
    assert!(matches!(
        store.claim_conversation_resume(claim(&queued)).await,
        Err(ScheduleStoreError::Backend(_))
    ));
    assert_eq!(
        session_row::Entity::find_by_id(original.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        original
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
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_retrieval_claim")
        .await
        .unwrap();
    let mut held = store
        .claim_conversation_resume(claim(&queued))
        .await
        .unwrap();
    let data = held.session.assistant_data.as_mut().unwrap();
    let prior_data = prior.assistant_data.as_ref().unwrap();
    assert_eq!(
        data.retrieval_task_id.as_deref(),
        Some(queued.run_id.as_str())
    );
    assert_ne!(
        data.retrieval_budget.episode_id,
        prior_data.retrieval_budget.episode_id
    );
    assert_eq!(
        data.history_access_scope.is_some(),
        prior_data.history_access_scope.is_some()
    );
    assert_eq!(
        data.memory_access_scope.is_some(),
        prior_data.memory_access_scope.is_some()
    );
    assert_eq!(data.instructions, prior_data.instructions);
    assert_eq!(data.read_uses, prior_data.read_uses);
    assert_eq!(data.history_dependencies, prior_data.history_dependencies);
    assert_eq!(
        data.current_actor_input_seq,
        prior_data.current_actor_input_seq
    );
    data.retrieval_budget
        .reserve(
            "spent-call",
            &"b".repeat(64),
            RetrievalCallKind::Relevance,
            4096,
            store.database_time().await.unwrap() as u64,
        )
        .unwrap();
    let spent = data.retrieval_budget.clone();
    let txn = store.db.begin().await.unwrap();
    assert!(
        ScheduleStore::assistant_retrieval_deadline_on(&txn, &held.session)
            .await
            .is_err(),
        "an in-memory budget change cannot replace the original persisted ledger"
    );
    txn.rollback().await.unwrap();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(held.session.encode_json_for_storage().unwrap()),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(original.id))
        .exec(&store.db)
        .await
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    for _ in 0..2 {
        let until = desk_diagnose_core::future::send_phase(|| {
            ScheduleStore::assistant_retrieval_deadline_on(&txn, &held.session)
        })
        .await
        .unwrap();
        assert_eq!(until, held.run.lease_deadline.unwrap() as u64);
    }
    txn.commit().await.unwrap();
    assert!(
        store
            .claim_conversation_resume(claim(&queued))
            .await
            .is_err()
    );
    let persisted = session_row::Entity::find_by_id(original.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&persisted.state_json)
            .unwrap()
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget,
        spent
    );
    store.cancel_run(1, &queued.run_id).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(
        ScheduleStore::assistant_retrieval_deadline_on(&txn, &held.session)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn actual_occurrence_retrieval_binding_rolls_back_and_keeps_original_budget() {
    for enabled in [false, true] {
        let (store, queued, row, session) = retrieval_fixture(enabled).await;
        exercise_retrieval_claim(store, queued, row, session).await;
    }
}

#[test]
fn actual_occurrence_retrieval_fits_small_stacks() {
    const STACK: &str = "LRD_TEST_OCCURRENCE_RETRIEVAL_STACK_BYTES";
    if let Some(bytes) = std::env::var_os(STACK) {
        let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
        assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
        actix_web::rt::System::new().block_on(async {
            let mut cases = Vec::new();
            for enabled in [false, true] {
                cases.push(retrieval_fixture(enabled).await);
            }
            let (done, ready) = tokio::sync::oneshot::channel();
            let thread = std::thread::Builder::new()
                .name("occurrence-retrieval-stack".into())
                .stack_size(bytes)
                .spawn(move || {
                    actix_web::rt::System::new().block_on(async move {
                        eprintln!("actual occurrence retrieval running on {bytes} byte thread");
                        for (store, queued, row, session) in cases {
                            desk_diagnose_core::future::boxed(|| {
                                exercise_retrieval_claim(store, queued, row, session)
                            })
                            .await;
                        }
                        let _ = done.send(());
                    });
                })
                .unwrap();
            ready.await.unwrap();
            thread.join().unwrap();
        });
        return;
    }
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "actual_occurrence_retrieval_fits_small_stacks",
                    "--nocapture",
                    "--test-threads=1"
                ])
                .env(STACK, bytes.to_string())
                .env_remove("RUST_MIN_STACK")
                .status()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn active_goal_prevents_a_scheduled_continuation_from_claiming_its_conversation() {
    let (store, queued, original, _) = fixture().await;
    let goal = desk_diagnose_core::goal::GoalRun::new(
        "goal-1".into(),
        "source".into(),
        "1".into(),
        desk_diagnose_core::dynamic_run::AssistantInputTarget::fixed("device-1".into()),
        "device-1".into(),
        "Finish the report".into(),
        "owner-input".into(),
        desk_diagnose_core::goal::GoalOpening::OwnerRequest,
        desk_diagnose_core::goal::GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        },
        1,
        u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap(),
        desk_diagnose_core::goal::GoalLimits::default(),
    )
    .unwrap();
    let txn = store.db.begin().await.unwrap();
    crate::agent_goal_store::insert_on(&txn, &goal)
        .await
        .unwrap();
    txn.commit().await.unwrap();

    assert!(matches!(
        store.claim_conversation_resume(claim(&queued)).await,
        Err(ScheduleStoreError::Conflict)
    ));
    assert_eq!(
        session_row::Entity::find_by_id(original.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .version,
        original.version
    );
    run::Entity::update_many()
        .set(run::ActiveModel {
            start_deadline: Set(0),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(queued.id))
        .exec(&store.db)
        .await
        .unwrap();
    let expired = store.expire_pending(&queued.run_id).await.unwrap();
    assert_eq!(expired.error_kind.as_deref(), Some("active_goal_conflict"));
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
    assert_eq!(
        &claimed.session.conversation[..prior.conversation.len()],
        &prior.conversation
    );
    assert_eq!(
        claimed.session.conversation.last().unwrap().text,
        store.read(1, &queued.schedule_id).await.unwrap().prompt
    );
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
async fn later_chat_is_preserved_and_bound_only_when_idle() {
    for busy in [false, true] {
        let (store, queued, original, mut session) = fixture().await;
        session.begin_focus_epoch(2, Vec::<String>::new()).unwrap();
        session.input_revision = 2;
        if busy {
            session
                .begin_turn(
                    "interactive",
                    Some("input".into()),
                    Some("browser".into()),
                    1,
                    claim(&queued).scope,
                    "2026-09-06T00:00:00Z",
                )
                .unwrap();
        }
        let mut changed: session_row::ActiveModel = original.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.lease_token = Set(session.lease_token as i64);
        changed.update(&store.db).await.unwrap();
        let result = store.claim_conversation_resume(claim(&queued)).await;
        if busy {
            assert!(matches!(result, Err(ScheduleStoreError::Conflict)));
            assert_eq!(
                run::Entity::find_by_id(queued.id)
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                queued
            );
        } else {
            let claimed = result.unwrap();
            assert_eq!(claimed.session.input_revision, 2);
            let task = store.read(1, &queued.schedule_id).await.unwrap();
            let snapshot: entity::Model =
                serde_json::from_str(&claimed.run.task_snapshot_json).unwrap();
            assert_eq!(task.requirement_revision, Some(2));
            assert_eq!(snapshot.requirement_revision, Some(2));
            assert_eq!(
                claimed.session.conversation.last().unwrap().text,
                task.prompt
            );
            assert_eq!(claimed.run.attempt, 1);
            assert_eq!(claimed.run.status, "running");
        }
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

#[tokio::test]
async fn scheduled_retrieval_consent_cannot_change_between_task_and_occurrence() {
    for change_snapshot in [true, false] {
        let (store, work, original, _) = fixture().await;
        let task = store.read(1, &work.schedule_id).await.unwrap();
        let raw = task.retrieval_consent_json.as_deref().unwrap();
        let mut altered = task.clone();
        altered.retrieval_consent_json = None;
        if change_snapshot {
            run::Entity::update_many()
                .set(run::ActiveModel {
                    task_snapshot_json: Set(serde_json::to_string(&altered).unwrap()),
                    ..Default::default()
                })
                .filter(run::Column::Id.eq(work.id))
                .exec(&store.db)
                .await
                .unwrap();
        } else {
            entity::Entity::update_many()
                .set(entity::ActiveModel {
                    retrieval_consent_json: Set(Some(raw.replace("source", "different-source"))),
                    ..Default::default()
                })
                .filter(entity::Column::Id.eq(task.id))
                .exec(&store.db)
                .await
                .unwrap();
        }
        let before = run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(store.claim_conversation_resume(claim(&work)).await.is_err());
        assert_eq!(
            session_row::Entity::find_by_id(original.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(
            run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
    }
}
