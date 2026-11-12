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
        worker_id: None,
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
    let now = chrono::Utc::now();
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "source-message".into(),
        "Investigate these independent symptoms".into(),
        model_destination,
    )
    .unwrap();
    let mut parent = super::input_sources::first_input(db, owner.clone(), now).await;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
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
    initialize_input_group_on(
        &txn,
        &mut parent,
        owner,
        None,
        None,
        now.timestamp_millis(),
        Default::default(),
    )
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
async fn a_reused_worker_continues_in_its_session_with_the_full_history() {
    use desk_diagnose_core::{session::TurnState, subagent::runtime::RuntimeTurn};

    let db = database().await;
    let (mut parent, mut calls) =
        runnable_parent_with_source(&db, destination(), Sensitivity::UserContent).await;
    let store = SubAgentStore::new(db.clone());
    let first = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let original = load(&db, &first.task_id).await;

    let mut next = spawn_request();
    next.worker_id = Some(first.worker_id.clone());
    next.task = "Check a distinct issue".into();
    calls[1].arguments_json = serde_json::to_string(&next).unwrap();
    parent.conversation.last_mut().unwrap().tool_calls[1].arguments_json =
        calls[1].arguments_json.clone();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let second = store
        .spawn_for_turn(&parent, &calls[1], &next)
        .await
        .unwrap();
    assert_eq!(second.child_session_id, first.child_session_id);
    assert_eq!(
        session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&first.child_session_id))
            .count(&db)
            .await
            .unwrap(),
        1
    );
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &claim_params(&original),
            &first.task_id,
            original.fence(),
            &destination(),
        )
        .await
        .unwrap()
    else {
        panic!("original worker request");
    };
    // The session works on one request at a time.
    assert!(
        store
            .child_runtime_candidate(&second.task_id)
            .await
            .unwrap()
            .is_none()
    );
    let mut session = claimed.session;
    let answer = super::lifecycle::report();
    super::lifecycle::append_report(&db, &mut session, &answer).await;
    session.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    store
        .settle_answer_for_turn(&mut session, original.fence(), answer.summary.clone())
        .await
        .unwrap()
        .unwrap();

    let Some(RuntimeTurn::Child { session, run, .. }) = store
        .child_runtime_candidate(&second.task_id)
        .await
        .unwrap()
    else {
        panic!("the next request is admitted to the Worker session");
    };
    assert_eq!(run.binding.task_id, second.task_id);
    assert_eq!(session.conversation_id, first.child_session_id);
    assert!(
        session
            .conversation
            .iter()
            .any(|message| message.text.contains(&answer.summary))
    );
    let instruction = format!("{}-instruction", first.task_id);
    let source = format!("{}-source", second.task_id);
    let first_at = session
        .conversation
        .iter()
        .position(|message| message.message_id == instruction)
        .unwrap();
    let second_at = session
        .conversation
        .iter()
        .position(|message| message.message_id == source)
        .unwrap();
    assert!(first_at < second_at);
    let next_run = load(&db, &second.task_id).await;
    assert_eq!(next_run.fence(), run.fence());
    let SubAgentClaimOutcome::Claimed(next_claim) = store
        .claim_child(
            &claim_params(&next_run),
            &second.task_id,
            next_run.fence(),
            &destination(),
        )
        .await
        .unwrap()
    else {
        panic!("follow-up worker request");
    };
    // Actions of the admitted request pass the delegated action check although
    // the shared session counts revisions across requests.
    let held = &next_claim.session;
    let binding = held.agent_role.binding().unwrap();
    assert_ne!(held.input_revision, binding.input_revision);
    let fence = desk_diagnose_core::action_turn_fence::AssistantTurnFence {
        schema_version: 1,
        conversation_id: held.conversation_id.clone(),
        turn_id: held.current_turn_id.clone().unwrap(),
        actor_id: held.actor_id.clone(),
        device_id: held.device_id.clone(),
        input_revision: held.input_revision,
        lease_token: held.lease_token,
        control_revision: held.control_revision,
        delegation: Some(
            desk_diagnose_core::action_turn_fence::DelegatedActionFence {
                root_conversation_id: binding.root_conversation_id.clone(),
                group_id: binding.group_id.clone(),
                task_id: binding.task_id.clone(),
                source_epoch: binding.source_epoch,
            },
        ),
    };
    assert!(
        crate::agent_subagent_store::check_child_action_on(
            &db,
            held,
            &fence,
            chrono::Utc::now().timestamp_millis()
        )
        .await
        .unwrap()
    );
    let projection = store
        .child_projection_for_turn(&next_claim.session)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&projection.text).unwrap();
    assert_eq!(value["runtime_delegation_state"]["objective"], next.task);
    assert!(!projection.text.contains("previous_worker_report"));

    let different = store
        .spawn_for_turn(&parent, &calls[2], &spawn_request())
        .await
        .unwrap();
    assert_ne!(different.child_session_id, first.child_session_id);
}

#[tokio::test]
async fn a_worker_claims_requests_in_order_and_keeps_execution_bindings_separate() {
    use desk_agent_protocol::ai_assistant::subagent::{
        AiAssistantSubAgentControl, SubAgentControlAction,
    };

    let db = database().await;
    let (mut parent, mut calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let first = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let first_run = load(&db, &first.task_id).await;

    let mut follow_up = spawn_request();
    follow_up.worker_id = Some(first.worker_id.clone());
    follow_up.task = "Verify a separate symptom".into();
    calls[1].arguments_json = serde_json::to_string(&follow_up).unwrap();
    let caller = parent.conversation.last_mut().unwrap();
    caller.tool_calls[1].arguments_json = calls[1].arguments_json.clone();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();

    let second = store
        .spawn_for_turn(&parent, &calls[1], &follow_up)
        .await
        .unwrap();
    assert_eq!(second.worker_id, first.worker_id);
    assert_ne!(second.task_id, first.task_id);
    assert_eq!(second.child_session_id, first.child_session_id);
    assert_eq!(
        store
            .spawn_for_turn(&parent, &calls[1], &follow_up)
            .await
            .unwrap(),
        second
    );
    use desk_assistant_data_store::worker_request::{in_scope, worker_page_on, worker_status_on};
    let scope = || {
        in_scope(
            &parent.actor_id,
            &parent.device_id,
            &parent.conversation_id,
            None,
        )
    };
    let query_worker = || worker_status_on(&db, scope(), &first.worker_id);
    let page = worker_page_on(&db, scope(), None, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.items, vec![first.worker_id.clone()]);
    assert_eq!(page.total, 1);
    assert!(!page.has_more);
    let queued = query_worker().await.unwrap().unwrap();
    assert_eq!(queued.current_request.unwrap().request_id, first.task_id);
    assert_eq!(queued.queued_requests.len(), 1);
    assert_eq!(queued.queued_requests[0].request_id, second.task_id);
    assert!(queued.latest_finished_request.is_none());
    assert!(
        worker_status_on(
            &db,
            in_scope(&parent.actor_id, &parent.device_id, "other-root", None),
            &first.worker_id
        )
        .await
        .unwrap()
        .is_none()
    );
    let second_run = load(&db, &second.task_id).await;
    let params = claim_params(&second_run);
    assert!(
        store
            .child_runtime_candidate(&second.task_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store
            .claim_child(&params, &second.task_id, second_run.fence(), &destination())
            .await
            .unwrap(),
        SubAgentClaimOutcome::Blocked(SubAgentClaimBlock::NotReady)
    ));
    assert!(matches!(
        store
            .claim_child(
                &claim_params(&first_run),
                &first.task_id,
                first_run.fence(),
                &destination()
            )
            .await
            .unwrap(),
        SubAgentClaimOutcome::Claimed(_)
    ));

    let running = query_worker().await.unwrap().unwrap();
    assert_eq!(
        running.current_request.unwrap().state,
        SubAgentState::Running
    );
    assert_eq!(running.queued_requests[0].state, SubAgentState::Queued);
    let txn = crate::db::begin_write(&db, session_row::Entity)
        .await
        .unwrap();
    super::super::task_control::apply_task_control_on(
        &txn,
        &parent,
        &AiAssistantSubAgentControl {
            client_request_id: "cancel-first".into(),
            task_id: first.task_id.clone(),
            expected_input_revision: first.input_revision,
            expected_control_revision: first.control_revision,
            action: SubAgentControlAction::Cancel,
        },
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();

    assert_eq!(
        load(&db, &first.task_id).await.state,
        SubAgentState::Cancelled
    );
    let promoted = query_worker().await.unwrap().unwrap();
    assert_eq!(promoted.current_request.unwrap().request_id, second.task_id);
    assert!(promoted.queued_requests.is_empty());
    let finished = promoted.latest_finished_request.unwrap();
    assert_eq!(finished.request_id, first.task_id);
    assert_eq!(finished.state, SubAgentState::Cancelled);

    let other = store
        .spawn_for_turn(&parent, &calls[2], &spawn_request())
        .await
        .unwrap();
    let first_page = worker_page_on(&db, scope(), None, 1)
        .await
        .unwrap()
        .unwrap();
    assert!(first_page.has_more);
    let second_page = worker_page_on(&db, scope(), first_page.next_cursor.as_deref(), 1)
        .await
        .unwrap()
        .unwrap();
    assert!(!second_page.has_more);
    assert_eq!(second_page.total, 2);
    let mut workers = vec![first.worker_id.clone(), other.worker_id];
    workers.sort();
    assert_eq!([first_page.items, second_page.items].concat(), workers);

    assert!(
        store
            .child_runtime_candidate(&second.task_id)
            .await
            .unwrap()
            .is_some()
    );
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(&params, &second.task_id, second_run.fence(), &destination())
        .await
        .unwrap()
    else {
        panic!("the next request can claim its own execution session");
    };
    assert_eq!(
        claimed.session.agent_role.binding(),
        Some(&second_run.binding)
    );
    assert!(claimed.session.permission_requests.is_empty());
    assert!(claimed.session.execution_state.states().is_empty());
    assert_eq!(
        load(&db, &second.task_id).await.state,
        SubAgentState::Running
    );
}

#[tokio::test]
async fn cancelling_a_queued_worker_request_leaves_the_shared_session_alone() {
    use desk_agent_protocol::ai_assistant::subagent::{
        AiAssistantSubAgentControl, SubAgentControlAction,
    };

    let db = database().await;
    let (mut parent, mut calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let first = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let mut follow_up = spawn_request();
    follow_up.worker_id = Some(first.worker_id.clone());
    follow_up.task = "Verify a separate symptom".into();
    calls[1].arguments_json = serde_json::to_string(&follow_up).unwrap();
    parent.conversation.last_mut().unwrap().tool_calls[1].arguments_json =
        calls[1].arguments_json.clone();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let second = store
        .spawn_for_turn(&parent, &calls[1], &follow_up)
        .await
        .unwrap();
    let shared = || async {
        session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&first.child_session_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap()
    };
    let before = shared().await;
    let txn = crate::db::begin_write(&db, session_row::Entity)
        .await
        .unwrap();
    super::super::task_control::apply_task_control_on(
        &txn,
        &parent,
        &AiAssistantSubAgentControl {
            client_request_id: "cancel-queued".into(),
            task_id: second.task_id.clone(),
            expected_input_revision: second.input_revision,
            expected_control_revision: second.control_revision,
            action: SubAgentControlAction::Cancel,
        },
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        load(&db, &second.task_id).await.state,
        SubAgentState::Cancelled
    );
    let after = shared().await;
    assert_eq!(after.state_json, before.state_json);
    assert_eq!(after.version, before.version);
    let first_run = load(&db, &first.task_id).await;
    assert!(matches!(
        store
            .claim_child(
                &claim_params(&first_run),
                &first.task_id,
                first_run.fence(),
                &destination()
            )
            .await
            .unwrap(),
        SubAgentClaimOutcome::Claimed(_)
    ));
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
async fn a_child_follows_the_owner_model_within_the_same_gateway() {
    let db = database().await;
    let (parent, calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let run = load(&db, &task.task_id).await;
    let current = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "another-model".into(),
        profile_revision: 9,
    };
    assert!(matches!(
        store
            .claim_child(&claim_params(&run), &task.task_id, run.fence(), &current)
            .await
            .unwrap(),
        SubAgentClaimOutcome::Claimed(_)
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
        Default::default(),
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

#[tokio::test]
async fn child_read_binding_is_narrowed_and_parent_completion_keeps_its_original_binding() {
    let db = database().await;
    let (parent, calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let mut runtime = store
        .child_runtime_candidate(&task.task_id)
        .await
        .unwrap()
        .unwrap();
    if let desk_diagnose_core::subagent::runtime::RuntimeTurn::Child {
        creation, session, ..
    } = &mut runtime
    {
        session.input_revision = 7;
        creation.source.original_read_context = Some(
            desk_diagnose_core::input_read_context::ReadContextSelection {
                external_issues: Vec::new(),
                external_issue_sources: Vec::new(),
                tool_names: vec!["read_system_info".into()],
                expires_at: None,
                object_attachments: vec![],
                live_targets: vec![],
            },
        );
    } else {
        panic!("child runtime required")
    }
    let (revision, context) = runtime.read_context_binding().unwrap();
    assert_eq!(revision, 7);
    assert_eq!(context.tool_names, vec!["read_system_info"]);
    assert!(context.object_attachments.is_empty() && context.live_targets.is_empty());
    let source = runtime.source().clone();
    let completion = desk_diagnose_core::subagent::runtime::RuntimeTurn::ParentCompletion {
        session: parent,
        source,
    };
    let (parent_revision, original) = completion.read_context_binding().unwrap();
    assert_eq!(parent_revision, 1);
    assert_eq!(original.tool_names, vec!["read_system_info"]);
    if let desk_diagnose_core::subagent::runtime::RuntimeTurn::Child { creation, .. } = &mut runtime
    {
        creation.source.original_read_context = None;
    }
    let (_, absent) = runtime.read_context_binding().unwrap();
    assert!(absent.tool_names.is_empty());
    assert!(absent.object_attachments.is_empty() && absent.live_targets.is_empty());
}

/// A child uses the directories its owner approved in the main conversation:
/// its turn starts with them, and dispatch re-reads the owner's current consent.
#[tokio::test]
async fn a_child_uses_the_main_conversations_approved_directories_until_revoked() {
    use desk_diagnose_core::file_scope::{DirectoryConsentSource, DirectoryProposal};
    let db = database().await;
    let (mut parent, calls) = runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &spawn_request())
        .await
        .unwrap();
    let subject = parent
        .file_scope_subject(&parent.actor_id, &parent.device_id, &parent.conversation_id)
        .unwrap();
    let directory = desk_agent_protocol::computer_use::ObjectRef {
        token: "device-reference".into(),
        snapshot_id: "device-generation".into(),
        object_kind: desk_agent_protocol::computer_use::ObjectKind::Directory,
        expires_at: String::new(),
    };
    parent
        .file_scope
        .propose(
            &subject,
            parent.file_scope.revision(),
            DirectoryProposal {
                request_id: "directory-request".into(),
                requested_path: "/srv/reports".into(),
                canonical_path: "/srv/reports".into(),
                directory: directory.clone(),
                purpose: "Write the report".into(),
                source: DirectoryConsentSource::ModelProposal,
            },
            1,
        )
        .unwrap();
    parent
        .file_scope
        .decide(
            &subject,
            parent.file_scope.revision(),
            "directory-request",
            true,
            1,
        )
        .unwrap();
    crate::agent_subagent_store::save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let run = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &claim_params(&run),
            &task.task_id,
            run.fence(),
            &destination(),
        )
        .await
        .unwrap()
    else {
        panic!("child claim");
    };
    let child = claimed.session;
    assert_eq!(child.delegated_directories.len(), 1);
    assert_eq!(child.delegated_directories[0].directory, directory);
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let txn = db.begin().await.unwrap();
    let view = crate::agent_subagent_store::current_directory_view_on(&txn, &child, now)
        .await
        .unwrap();
    assert_eq!(view.delegated_directories, child.delegated_directories);
    txn.rollback().await.unwrap();
    // The owner revokes it in the main conversation: the next dispatch check
    // no longer sees it, although the child's turn snapshot still lists it.
    parent
        .file_scope
        .revoke(&subject, parent.file_scope.revision(), "directory-request")
        .unwrap();
    crate::agent_subagent_store::save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    let view = crate::agent_subagent_store::current_directory_view_on(&txn, &child, now)
        .await
        .unwrap();
    assert!(view.delegated_directories.is_empty());
    assert!(!child.delegated_directories.is_empty());
}
