use super::*;
use desk_diagnose_core::{
    session::{TriggerOrigin, TurnState},
    subagent::runtime::RuntimeTurn,
};

async fn child(db: &DatabaseConnection) -> (SubAgentStore, RuntimeTurn) {
    child_source(db, false).await
}

async fn child_source(db: &DatabaseConnection, goal_source: bool) -> (SubAgentStore, RuntimeTurn) {
    let (mut parent, calls) = super::creation::runnable_parent(db).await;
    if goal_source {
        let now = chrono::Utc::now();
        let goal = desk_diagnose_core::goal::GoalRun::new(
            "recovery-goal".into(),
            "root".into(),
            "1".into(),
            "1".into(),
            "Investigate these independent symptoms".into(),
            "source-message".into(),
            desk_diagnose_core::goal::GoalOpening::OwnerRequest,
            desk_diagnose_core::goal::GoalModelBinding::from_destination(
                &super::creation::destination(),
            )
            .unwrap(),
            1,
            now.timestamp_millis() as u64,
            desk_diagnose_core::goal::GoalLimits::default(),
        )
        .unwrap();
        let txn = db.begin().await.unwrap();
        initialize_goal_group_on(&txn, &mut parent, &goal, now.timestamp_millis())
            .await
            .unwrap();
        parent.version += 1;
        session_row::Entity::update_many()
            .set(session_row::ActiveModel {
                state_json: Set(parent.encode_json_for_storage().unwrap()),
                version: Set(parent.version),
                ..Default::default()
            })
            .filter(session_row::Column::ConversationId.eq("root"))
            .exec(&txn)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
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
    (store, runtime)
}

#[tokio::test]
async fn child_source_survives_history_loss_and_does_not_adopt_new_parent_input() {
    let db = database().await;
    let (store, runtime) = child(&db).await;
    let RuntimeTurn::Child {
        session,
        run,
        creation,
    } = runtime
    else {
        panic!("child context");
    };
    let mut compacted = session.clone();
    compacted.conversation.clear();
    compacted.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(compacted.encode_json_for_storage().unwrap()),
            version: Set(compacted.version),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&compacted.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let frozen = store
        .child_creation_context(&compacted)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frozen, creation);
    assert_eq!(
        frozen.instruction.role,
        desk_diagnose_core::chat::ChatRole::SystemEvent
    );
    assert!(
        desk_diagnose_core::permission_resume::latest_user_requirement(&compacted.conversation)
            .is_none()
    );
    assert_eq!(
        frozen.source.owner_requirement.text,
        "Investigate these independent symptoms"
    );
    assert!(
        store
            .child_context_for_subject(
                &compacted.conversation_id,
                "different-owner",
                "1",
                compacted.version
            )
            .await
            .is_err()
    );
    assert!(
        store
            .child_context_for_subject(&compacted.conversation_id, "1", "1", session.version)
            .await
            .is_err()
    );
    let authoritative = store
        .child_context_for_subject(&compacted.conversation_id, "1", "1", compacted.version)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        authoritative.0.source.model_destination,
        super::creation::destination()
    );
    assert_eq!(load(&db, &run.binding.task_id).await.binding, run.binding);
}

#[tokio::test]
async fn lapsed_child_lease_recovers_only_the_finite_task_and_keeps_its_budget_and_deadline() {
    let db = database().await;
    let (store, runtime) = child(&db).await;
    let RuntimeTurn::Child { run, .. } = runtime else {
        panic!("child context");
    };
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&run),
            &run.binding.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("child lease");
    };
    assert!(!store.reconcile_task(&run.binding.task_id).await.unwrap());
    let before_group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let old_deadline = chrono::Utc::now() - chrono::Duration::seconds(1);
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            lease_deadline: Set(Some(old_deadline)),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .exec(&db)
        .await
        .unwrap();
    assert!(store.reconcile_task(&run.binding.task_id).await.unwrap());
    assert!(!store.reconcile_task(&run.binding.task_id).await.unwrap());
    let recovered = load(&db, &run.binding.task_id).await;
    assert_eq!(recovered.state, SubAgentState::Queued);
    assert_eq!(recovered.binding, run.binding);
    assert!(recovered.terminal_report.is_none());
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(session.turn_state, TurnState::Idle);
    assert_eq!(session.trigger_origin, TriggerOrigin::DelegatedTask);
    assert_eq!(session.agent_role, claimed.session.agent_role);
    assert_eq!(session.lease_token, claimed.session.lease_token + 1);
    assert!(row.lease_deadline.is_none());
    let after_group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        decode_group(&after_group).unwrap().budget,
        decode_group(&before_group).unwrap().budget
    );
}

#[tokio::test]
async fn recovery_observes_a_paused_source_without_claiming_or_resetting_it() {
    let db = database().await;
    let (store, runtime) = child_source(&db, true).await;
    let RuntimeTurn::Child { run, session, .. } = runtime else {
        panic!("child context");
    };
    let txn = db.begin().await.unwrap();
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut group = decode_group(&row).unwrap();
    let epoch = group.set_source_admission(SourceAdmission::Paused).unwrap();
    let now = chrono::Utc::now();
    replace_group_on(&txn, &row, &group, now.timestamp_millis())
        .await
        .unwrap();
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&run.binding.task_id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut paused = run.clone();
    paused.pause_source(epoch, &now.to_rfc3339()).unwrap();
    replace_run_on(&txn, &row, &paused, now.timestamp_millis())
        .await
        .unwrap();
    synchronize_control_on(&txn, &paused, now.timestamp_millis())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(!store.reconcile_task(&run.binding.task_id).await.unwrap());
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let frozen = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(!store.child_resume_available(&frozen).await.unwrap());
    assert_eq!(
        load(&db, &run.binding.task_id).await.state,
        SubAgentState::WaitingSource
    );
    assert_eq!(frozen.input_revision, session.input_revision);
    assert_eq!(
        frozen.agent_role.binding().unwrap().deadline_ms,
        run.binding.deadline_ms
    );
    assert!(
        store
            .child_runtime_candidate(&run.binding.task_id)
            .await
            .unwrap()
            .is_none()
    );
}
