use super::*;
use desk_agent_protocol::data_lineage::{DestinationIdentity, Sensitivity};
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ToolCall, ToolCallRef},
    seam::ClaimTurnParams,
    session::TriggerOrigin,
    subagent::{creation::TaskCreationEnvelope, tools::SpawnRequest},
};

pub(super) fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

pub(super) fn spawn_request() -> SpawnRequest {
    SpawnRequest {
        name: "Independent investigation".into(),
        task: "Investigate one symptom".into(),
        acceptance_criteria: vec!["Cite original evidence".into()],
        required_for_completion: true,
    }
}

pub(super) async fn runnable_parent(
    db: &DatabaseConnection,
) -> (PersistedAgentSession, Vec<ToolCall>) {
    runnable_parent_with_source(db, destination(), Sensitivity::Secret).await
}

pub(super) async fn runnable_parent_with_source(
    db: &DatabaseConnection,
    model_destination: DestinationIdentity,
    sensitivity: Sensitivity,
) -> (PersistedAgentSession, Vec<ToolCall>) {
    seed_parent(db).await;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut parent = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let now = chrono::Utc::now();
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "source-message".into(),
        "Investigate these independent symptoms".into(),
        model_destination,
    )
    .unwrap();
    parent.conversation.push(owner.clone());
    let mut allowed = scope();
    allowed
        .granted
        .push(desk_agent_protocol::Capability::ShellExecConfirmed);
    parent
        .begin_turn(
            "parent-turn",
            Some("parent-request".into()),
            Some("browser".into()),
            1,
            allowed,
            now.to_rfc3339(),
        )
        .unwrap();
    let calls: Vec<_> = (1..=3)
        .map(|index| ToolCall {
            id: format!("spawn-{index}"),
            name: desk_diagnose_core::subagent::tools::SPAWN.into(),
            arguments_json: serde_json::to_string(&spawn_request()).unwrap(),
        })
        .collect();
    let mut caller = ChatMessage::assistant_tool_calls(
        "assistant-calls",
        "Investigate independently",
        calls
            .iter()
            .map(|call| ToolCallRef {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments_json: call.arguments_json.clone(),
            })
            .collect(),
    );
    caller.turn_id = Some("parent-turn".into());
    caller.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            "assistant-calls",
            &caller.text,
            "model-output",
            &[owner.data_envelope.clone().unwrap()],
        )
        .unwrap(),
    );
    caller.data_envelope.as_mut().unwrap().sensitivity = sensitivity;
    parent.conversation.push(caller);
    let txn = db.begin().await.unwrap();
    initialize_input_group_on(&txn, &mut parent, owner, None, None, now.timestamp_millis())
        .await
        .unwrap();
    parent.version = row.version + 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            lease_token: Set(parent.lease_token as i64),
            lease_deadline: Set(Some(now + chrono::Duration::minutes(5))),

            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    (parent, calls)
}

pub(super) fn claim_params(run: &SubAgentRun) -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: run.child_conversation_id.clone(),
        actor_id: "1".into(),
        device_id: "1".into(),
        policy_revision: 2,
        current_pdp_scope: scope(),
        turn_id: "child-turn".into(),
        request_id: Some("child-request".into()),
        connection_id: None,
        trigger_origin: TriggerOrigin::DelegatedTask,
        now: chrono::Utc::now().to_rfc3339(),
    }
}

#[tokio::test]
async fn spawn_replays_one_independent_child_and_capacity_is_root_scoped() {
    let db = database().await;
    let (parent, calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let first = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    assert_eq!(
        store
            .spawn_for_turn(&parent, &calls[0], &spawn_request())
            .await
            .unwrap()
            .task_id,
        first.task_id
    );
    let mut changed = spawn_request();
    changed.name = "Changed request".into();
    let changed_call = ToolCall {
        arguments_json: serde_json::to_string(&changed).unwrap(),
        ..calls[0].clone()
    };
    assert!(
        store
            .spawn_for_turn(&parent, &changed_call, &changed)
            .await
            .is_err()
    );
    let second = store
        .spawn_for_turn(&parent, &calls[1], &spawn_request())
        .await
        .unwrap();
    assert_ne!(first.task_id, second.task_id);
    let capacity = store
        .spawn_for_turn(&parent, &calls[2], &spawn_request())
        .await
        .unwrap_err();
    assert!(matches!(capacity, DbErr::Custom(message)
        if message == desk_diagnose_core::subagent::capacity_storage_message(2)));
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 2);
    let group_row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(parent.delegation_group_id.clone().unwrap()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&group_row).unwrap();
    assert_eq!(group.tasks_created, 2);
    assert_eq!(group.required_task_ids.len(), 2);
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(first.task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let creation: TaskCreationEnvelope = serde_json::from_str(&row.creation_envelope_json).unwrap();
    creation.validate().unwrap();
    assert_eq!(
        creation
            .instruction
            .data_envelope
            .as_ref()
            .unwrap()
            .sensitivity,
        Sensitivity::Secret
    );
    let child_row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(row.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let child = PersistedAgentSession::decode_json(&child_row.state_json).unwrap();
    assert!(!child.agent_role.is_main());
    assert!(child.permission_requests.is_empty());
    assert!(child.context_attachments.is_empty());
    assert!(child.execution_state.states().is_empty());
    assert!(child.pending_auto_triggers.is_empty());
    assert!(
        !child
            .scope_snapshot
            .granted
            .contains(&desk_agent_protocol::Capability::ShellExecConfirmed)
    );
    assert!(
        child
            .conversation
            .iter()
            .all(|message| message.role == ChatRole::SystemEvent)
    );
    assert_eq!(child.lease_token, 0);
    assert_eq!(child.input_revision, 1);
    assert_eq!(child.latest_input_seq, 1);
    assert_eq!(child.handled_input_seq, 0);
}

#[tokio::test]
async fn new_main_input_does_not_invalidate_an_existing_child_claim() {
    let db = database().await;
    let (mut parent, calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let original = load(&db, &task.task_id).await;
    parent.begin_focus_epoch(2, Vec::new()).unwrap();
    parent.input_revision = 2;
    parent.version += 1;
    parent.delegation_group_id = None;
    parent.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),

            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&db)
        .await
        .unwrap();
    let params = claim_params(&original);
    let claimed = match store
        .claim_child(&params, &task.task_id, original.fence(), &destination())
        .await
        .unwrap()
    {
        SubAgentClaimOutcome::Claimed(claimed) => claimed,
        _ => panic!("independent source remains open"),
    };
    assert_eq!(
        claimed.session.agent_role.binding(),
        Some(&original.binding)
    );
    assert_eq!(claimed.session.latest_input_seq, 1);
    assert_eq!(claimed.session.trigger_origin, TriggerOrigin::DelegatedTask);
    assert_eq!(claimed.session.lease_token, 1);
    assert_eq!(claimed.session.version, 1);
    assert_eq!(load(&db, &task.task_id).await.state, SubAgentState::Running);
    assert!(matches!(
        store
            .claim_child(&params, &task.task_id, original.fence(), &destination())
            .await
            .unwrap(),
        SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady)
    ));
}

#[tokio::test]
async fn changed_model_and_expired_source_cannot_claim_or_revive_a_child() {
    let db = database().await;
    let (parent, calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let run = load(&db, &task.task_id).await;
    let mut params = claim_params(&run);
    let changed = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 2,
        model_id: "model".into(),
        profile_revision: 1,
    };
    assert!(
        store
            .claim_child(&params, &task.task_id, run.fence(), &changed)
            .await
            .is_err()
    );
    assert_eq!(load(&db, &task.task_id).await.state, SubAgentState::Queued);
    params.now = chrono::DateTime::from_timestamp_millis(run.binding.deadline_ms)
        .unwrap()
        .to_rfc3339();
    assert!(matches!(
        store
            .claim_child(&params, &task.task_id, run.fence(), &destination())
            .await
            .unwrap(),
        SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::DeadlineReached)
    ));
    assert_eq!(load(&db, &task.task_id).await.state, SubAgentState::Failed);
    assert!(matches!(
        store
            .claim_child(&params, &task.task_id, run.fence(), &destination())
            .await
            .unwrap(),
        SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady)
    ));
}

#[tokio::test]
async fn group_creation_and_parent_binding_roll_back_together() {
    let db = database().await;
    seed_parent(&db).await;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut parent = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let original = parent.clone();
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "owner-input".into(),
        "Investigate".into(),
        destination(),
    )
    .unwrap();
    let txn = db.begin().await.unwrap();
    initialize_input_group_on(
        &txn,
        &mut parent,
        owner,
        None,
        None,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(group_row::Entity::find().count(&db).await.unwrap(), 0);
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&row.state_json).unwrap(),
        original
    );
}

#[tokio::test]
async fn child_locale_is_frozen_when_spawned_and_is_not_a_new_main_preference() {
    let db = database().await;
    let (mut parent, calls) = runnable_parent(&db).await;
    parent.response_locale = Some("zh-CN".into());
    let txn = db.begin().await.unwrap();
    let mut stored = parent.clone();
    stored.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(stored.encode_json_for_storage().unwrap()),
            version: Set(stored.version),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .filter(session_row::Column::Version.eq(parent.version))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    parent = stored;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&task.task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let frozen: TaskCreationEnvelope = serde_json::from_str(&row.creation_envelope_json).unwrap();
    assert_eq!(frozen.response_locale.as_deref(), Some("zh-CN"));
    let run = decode_run(&row).unwrap();
    let SubAgentClaimOutcome::Claimed(claim) = store
        .claim_child(
            &claim_params(&run),
            &task.task_id,
            run.fence(),
            &destination(),
        )
        .await
        .unwrap()
    else {
        panic!("one original child claim");
    };
    assert_eq!(claim.session.response_locale.as_deref(), Some("zh-CN"));
    assert_eq!(
        claim.creation.response_locale,
        claim.session.response_locale
    );
}
