use super::*;
#[tokio::test]
async fn normalized_json_matches_original_lineage_but_changed_task_is_rejected() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let call = &calls[0];
    let original = serde_json::to_string_pretty(
        &serde_json::from_str::<serde_json::Value>(&call.arguments_json).unwrap(),
    )
    .unwrap();
    let caller = parent
        .conversation
        .iter_mut()
        .find(|message| {
            message
                .tool_calls
                .iter()
                .any(|reference| reference.id == call.id)
        })
        .unwrap();
    caller
        .tool_calls
        .iter_mut()
        .find(|reference| reference.id == call.id)
        .unwrap()
        .arguments_json = original.clone();
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    let request = super::creation::spawn_request();
    let receipt = store
        .execute_main_tool(
            &mut parent,
            call,
            Operation::Spawn(request.clone()),
            "formatted-result",
        )
        .await
        .unwrap();
    assert!(!receipt.payload["task_id"].as_str().unwrap().is_empty());
    assert_eq!(run_row::Entity::find().all(&db).await.unwrap().len(), 1);
    assert_eq!(
        parent
            .conversation
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|reference| reference.id == call.id)
            .unwrap()
            .arguments_json,
        original
    );
    let mut changed = request.clone();
    changed.task = "Different delegated work".into();
    let changed_call = ToolCall {
        arguments_json: serde_json::to_string(&changed).unwrap(),
        ..call.clone()
    };
    assert!(
        store
            .execute_main_tool(
                &mut parent,
                &changed_call,
                Operation::Spawn(changed),
                "changed-result"
            )
            .await
            .is_err()
    );
    assert_eq!(run_row::Entity::find().all(&db).await.unwrap().len(), 1);
    let repeated = store
        .execute_main_tool(
            &mut parent,
            call,
            Operation::Spawn(request),
            "unused-repeat",
        )
        .await
        .unwrap();
    assert_eq!(repeated.result_message_id, "formatted-result");
}
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall, ToolCallRef},
    session::TurnState,
    subagent::{
        tools::{self, Operation},
        wait::WaitMode,
    },
};

pub(super) async fn committed_call(
    db: &DatabaseConnection,
    parent: &mut PersistedAgentSession,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> ToolCall {
    let call = ToolCall {
        id: id.into(),
        name: name.into(),
        arguments_json: arguments.to_string(),
    };
    let mut message = ChatMessage::assistant_tool_calls(
        format!("caller-{id}"),
        "Read task state",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    )
    .with_turn_id(parent.current_turn_id.clone().unwrap());
    let labels = parent
        .conversation
        .iter()
        .filter_map(|message| message.data_envelope.clone())
        .collect::<Vec<_>>();
    message.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            &message.message_id,
            &message.text,
            "model-output",
            &labels,
        )
        .unwrap(),
    );
    parent.conversation.push(message);
    save_main_delegation_session(db, parent).await.unwrap();
    call
}

pub(super) async fn complete_child(db: &DatabaseConnection, task_id: &str) {
    let run = load(db, task_id).await;
    let store = SubAgentStore::new(db.clone());
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&run),
            task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("child claim");
    };
    let mut child = claimed.session;
    let report = super::lifecycle::report();
    super::lifecycle::append_report(db, &mut child, &report).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    store
        .settle_answer_for_turn(&mut child, run.fence(), report.summary)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn spawn_effect_and_tool_history_commit_once_or_roll_back_together() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let original = parent.clone();
    let request = super::creation::spawn_request();
    // The mutation is already staged in the transaction when this ID collision
    // fails. No orphan child, group allocation or state notification may survive.
    assert!(
        store
            .execute_main_tool(
                &mut parent,
                &calls[0],
                Operation::Spawn(request.clone()),
                "source-message"
            )
            .await
            .is_err()
    );
    assert_eq!(parent, original);
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 0);
    let receipt = store
        .execute_main_tool(
            &mut parent,
            &calls[0],
            Operation::Spawn(request.clone()),
            "spawn-result",
        )
        .await
        .unwrap();
    let summary: AiAssistantSubAgentSummary = serde_json::from_value(receipt.payload).unwrap();
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 1);
    let stored = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&stored.state_json).unwrap(),
        parent
    );
    let previous_version = parent.version;
    let repeated = store
        .execute_main_tool(
            &mut parent,
            &calls[0],
            Operation::Spawn(request),
            "unused-repeat-id",
        )
        .await
        .unwrap();
    assert_eq!(repeated.result_message_id, "spawn-result");
    assert_eq!(parent.version, previous_version);
    assert_eq!(
        parent
            .conversation
            .iter()
            .filter(|message| message.tool_call_id.as_deref() == Some(calls[0].id.as_str()))
            .count(),
        1
    );
    let label = parent
        .conversation
        .iter()
        .find(|message| message.message_id == "spawn-result")
        .unwrap()
        .data_envelope
        .as_ref()
        .unwrap();
    assert_eq!(
        label.sensitivity,
        desk_agent_protocol::data_lineage::Sensitivity::Secret
    );
    assert_eq!(
        load(&db, &summary.task_id).await.state,
        SubAgentState::Queued
    );
}

#[tokio::test]
async fn wait_registration_persists_result_and_fences_without_consuming_attention() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let call = committed_call(
        &db,
        &mut parent,
        "wait-1",
        tools::WAIT,
        serde_json::json!({"task_ids": [&task.task_id], "mode": "all_terminal"}),
    )
    .await;
    let receipt = store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::Wait {
                task_ids: vec![task.task_id.clone()],
                mode: WaitMode::AllTerminal,
            },
            "wait-result",
        )
        .await
        .unwrap();
    assert_eq!(receipt.payload["status"], "waiting");
    let wait = parent.subagent_wait.as_ref().unwrap();
    assert_eq!(wait.result_message_id, "wait-result");
    assert_eq!(wait.tasks[0].task_id, task.task_id);
    assert_eq!(wait.tasks[0].input_revision, 1);
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let stored = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(stored.subagent_wait, parent.subagent_wait);
    assert!(
        stored
            .conversation
            .iter()
            .any(|message| message.message_id == "wait-result")
    );
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.lease_deadline.is_none());
    let events = crate::entity::agent_subagent_inbox::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.model_observed_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
}

#[tokio::test]
async fn already_completed_dependency_returns_immediately_instead_of_registering_a_wait() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    complete_child(&db, &task.task_id).await;
    let call = committed_call(
        &db,
        &mut parent,
        "wait-ended",
        tools::WAIT,
        serde_json::json!({"task_ids": [&task.task_id], "mode": "any_terminal"}),
    )
    .await;
    let receipt = store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::Wait {
                task_ids: vec![task.task_id],
                mode: WaitMode::AnyTerminal,
            },
            "wait-ended-result",
        )
        .await
        .unwrap();
    assert_eq!(receipt.payload["status"], "ready");
    assert!(parent.subagent_wait.is_none());
}

#[tokio::test]
async fn explicit_result_reads_preserve_labels_and_stage_only_exact_report_revisions() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    complete_child(&db, &task.task_id).await;
    let call = committed_call(
        &db,
        &mut parent,
        "read-1",
        tools::RESULT,
        serde_json::json!({"task_id": &task.task_id}),
    )
    .await;
    let receipt = store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::ReadResult {
                task_id: task.task_id.clone(),
                include_task: false,
            },
            "read-result",
        )
        .await
        .unwrap();
    assert!(receipt.payload.get("task").is_none());
    assert!(receipt.payload.get("objective").is_none());
    let result: desk_diagnose_core::subagent::result::SubAgentModelResult =
        serde_json::from_value(receipt.payload).unwrap();
    assert_eq!(result.state, SubAgentState::Completed);
    assert_eq!(parent.observed_subagent_results.len(), 1);
    assert_eq!(
        parent.observed_subagent_results[0].state_revision,
        result.state_revision
    );
    assert_eq!(
        parent.observed_subagent_results[0].result_message_id,
        "read-result"
    );
    assert!(parent.accepted_subagent_observations.is_empty());
    let label = parent
        .conversation
        .iter()
        .find(|message| message.message_id == "read-result")
        .unwrap()
        .data_envelope
        .as_ref()
        .unwrap();
    assert_eq!(
        label.sensitivity,
        desk_agent_protocol::data_lineage::Sensitivity::Secret
    );
    let full = store
        .result_for_owner("root", "1", "1", &task.task_id)
        .await
        .unwrap();
    assert_eq!(
        result.answer.as_deref(),
        full.report.as_ref().map(|report| report.summary.as_str())
    );
    assert_eq!(result.acceptance_criteria, full.acceptance_criteria);
    assert!(!full.objective.is_empty());
    assert_eq!(
        result.receipt_refs,
        full.report.as_ref().unwrap().receipt_refs
    );
    let call = committed_call(
        &db,
        &mut parent,
        "read-task",
        tools::RESULT,
        serde_json::json!({"task_id": &task.task_id, "include_task":true}),
    )
    .await;
    let detailed = store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::ReadResult {
                task_id: task.task_id.clone(),
                include_task: true,
            },
            "read-task-result",
        )
        .await
        .unwrap();
    assert_eq!(detailed.payload["objective"], full.objective);
    assert_eq!(detailed.payload["answer"], result.answer.unwrap());
    let events = crate::entity::agent_subagent_inbox::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.model_observed_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
}

#[tokio::test]
async fn projection_after_new_input_keeps_old_tasks_opaque_and_active_tasks_visible() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    parent.begin_focus_epoch(2, Vec::new()).unwrap();
    parent.input_revision = 2;
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "new-source".into(),
        "A separate question".into(),
        super::creation::destination(),
    )
    .unwrap();
    parent.conversation = vec![owner.clone()];
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
    parent.version = write_child_session_on(&txn, &parent, chrono::Utc::now())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let projection = store.main_projection_for_turn(&parent).await.unwrap();
    let payload: serde_json::Value = serde_json::from_str(&projection.text).unwrap();
    let payload = &payload["runtime_delegation_state"];
    assert_eq!(payload["unfinished"], 1);
    assert_eq!(payload["tasks"][0]["task_id"], task.task_id);
    assert!(payload["tasks"][0].get("task").is_none());
    assert!(payload["tasks"][0].get("name").is_none());
    assert!(payload["tasks"][0].get("objective").is_none());
    assert_eq!(
        projection.data_envelope.unwrap().sensitivity,
        desk_agent_protocol::data_lineage::Sensitivity::UserContent
    );
}
