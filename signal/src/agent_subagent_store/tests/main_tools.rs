use super::*;

#[tokio::test]
async fn runtime_schema_defaults_preserve_result_and_list_call_authority() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::UserContent,
    )
    .await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    complete_child(&db, &task.task_id).await;
    let projection = store.main_projection_for_turn(&parent).await.unwrap();
    let runtime: serde_json::Value = serde_json::from_str(&projection.text).unwrap();
    assert_eq!(
        runtime["runtime_delegation_state"]["pending_reports"][0]["request_id"],
        task.task_id
    );
    parent
        .ui_references
        .observe(std::slice::from_ref(&projection))
        .unwrap();
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    for (id, name, arguments, field, default) in [
        (
            "read-default",
            tools::REPORTS,
            serde_json::json!({"request_id": &task.task_id}),
            "after_seq",
            serde_json::json!(0),
        ),
        (
            "list-default",
            tools::LIST,
            serde_json::json!({}),
            "limit",
            serde_json::Value::Null,
        ),
    ] {
        let original = committed_call(&db, &mut parent, id, name, arguments).await;
        let resolved = desk_diagnose_core::ui_model_ids::resolve_session_call_with_context(
            &original,
            &parent,
            chrono::Utc::now().timestamp_millis() as u64,
            None,
        )
        .unwrap();
        let mut input: serde_json::Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(input[field], default);
        let operation = tools::parse(&parent, &resolved).unwrap();
        let receipt = store
            .execute_main_tool(&mut parent, &resolved, operation, &format!("{id}-result"))
            .await
            .unwrap();
        if name == tools::REPORTS {
            assert!(
                receipt.payload["reports"][0]["content"]["summary"]
                    .as_str()
                    .is_some_and(|answer| !answer.is_empty())
            );
            assert!(receipt.payload.get("objective").is_none());
            assert_eq!(parent.observed_subagent_results.len(), 1);
        } else {
            assert_eq!(receipt.payload["total"], 1);
            assert_eq!(
                receipt.payload["items"],
                serde_json::json!([&task.worker_id])
            );
        }
        let committed = parent
            .conversation
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|call| call.id == original.id)
            .unwrap();
        assert_eq!(committed.arguments_json, original.arguments_json);
        input[field] = if name == tools::REPORTS {
            serde_json::json!(1)
        } else {
            serde_json::json!(11)
        };
        let changed = ToolCall {
            arguments_json: input.to_string(),
            ..resolved
        };
        let operation = tools::parse(&parent, &changed).unwrap();
        let unchanged = parent.clone();
        assert!(
            store
                .execute_main_tool(&mut parent, &changed, operation, "changed-default-result")
                .await
                .is_err()
        );
        assert_eq!(parent, unchanged);
    }

    let call = committed_call(
        &db,
        &mut parent,
        "worker-status",
        tools::STATUS,
        serde_json::json!({"worker_id": &task.worker_id}),
    )
    .await;
    let operation = tools::parse(&parent, &call).unwrap();
    let receipt = store
        .execute_main_tool(&mut parent, &call, operation, "worker-status-result")
        .await
        .unwrap();
    assert_eq!(receipt.payload["worker_id"], task.worker_id);
    assert!(receipt.payload["current_request"].is_null());
    assert_eq!(
        receipt.payload["latest_finished_request"]["request_id"],
        task.task_id
    );
    assert_eq!(
        receipt.payload["latest_finished_request"]["state"],
        "completed"
    );
    assert_eq!(parent.observed_subagent_results.len(), 1);
}

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
    subagent::tools::{self, Operation},
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
    if desk_diagnose_core::model_controls::is_control(name) {
        let projection = SubAgentStore::new(db.clone())
            .main_projection_for_turn(parent)
            .await
            .unwrap();
        let policy = desk_diagnose_core::model_egress::ModelEgressPolicy {
            destination: super::creation::destination(),
            selected_source_tools: Default::default(),
            export_authorization_id: format!("control-test-export-{id}"),
            now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
            byte_cap: 1024 * 1024,
            permission_resume: false,
        };
        let authorized = policy
            .authorize_request(desk_diagnose_core::seam::ModelRequest::text_only(
                vec![projection.clone()],
                desk_diagnose_core::prompt::ResponseFormatSpec::None,
            ))
            .unwrap();
        let turn = desk_diagnose_core::chat::ModelTurn {
            text: message.text.clone(),
            tool_calls: vec![call.clone()],
            stop_reason: desk_diagnose_core::chat::StopReason::ToolUse,
            ..Default::default()
        };
        let output = policy
            .derive_model_output_envelope(&turn, &authorized.input_envelopes)
            .unwrap();
        let mut binding =
            desk_diagnose_core::model_controls::FrozenModelControls::capture(parent, &[projection]);
        binding.retain_exported(Some(&output), Some(&policy.export_authorization_id));
        binding.retain_called_targets(&turn.tool_calls);
        message.model_controls = Some(Box::new(binding));
        message.data_envelope = Some(output.clone());
        super::observation::record_accepted_provider(
            db,
            parent,
            &policy,
            &authorized.input_envelopes[0],
            &output,
        )
        .await;
    }
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
        tools::WAIT_REPORTS,
        serde_json::json!({"requests":[{"request_id":&task.task_id,"after_seq":0}],"kinds":["completed","failed"],"mode":"all_terminal"}),
    )
    .await;
    let operation = tools::parse(&parent, &call).unwrap();
    let receipt = store
        .execute_main_tool(&mut parent, &call, operation, "wait-result")
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
async fn all_terminal_report_wait_keeps_native_terminal_fallback_for_protected_reports() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let mut ids = Vec::new();
    for call in &calls[..2] {
        let child = store
            .spawn_for_turn(&parent, call, &super::creation::spawn_request())
            .await
            .unwrap();
        ids.push(child.task_id);
    }
    complete_child(&db, &ids[0]).await;
    let requests = ids
        .iter()
        .map(|id| serde_json::json!({"request_id":id,"after_seq":0}))
        .collect::<Vec<_>>();
    let call = committed_call(
        &db, &mut parent, "wait-for-two", tools::WAIT_REPORTS,
        serde_json::json!({"requests":requests,"kinds":["completed","failed"],"mode":"all_terminal"}),
    ).await;
    let operation = tools::parse(&parent, &call).unwrap();
    let waiting = store
        .execute_main_tool(&mut parent, &call, operation, "wait-for-two-receipt")
        .await
        .unwrap();
    assert_eq!(waiting.payload["status"], "waiting");
    assert_eq!(waiting.payload["reports"], serde_json::json!([]));
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert_eq!(
        store.resolve_parent_wait("root", "1", "1").await.unwrap(),
        ParentWaitResolution::Pending
    );

    complete_child(&db, &ids[1]).await;
    let ready = store.resolve_parent_wait("root", "1", "1").await.unwrap();
    assert!(matches!(ready, ParentWaitResolution::Ready(_)));
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let resumed = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let receipt = resumed
        .conversation
        .iter()
        .find(|message| message.message_id == "wait-for-two-receipt")
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&receipt.text).unwrap();
    assert_eq!(payload["status"], "ready");
    assert_eq!(payload["reports"], serde_json::json!([]));
    let tasks = payload["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().all(|task| task["state"] == "completed"));
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
        tools::WAIT_REPORTS,
        serde_json::json!({"requests":[{"request_id":&task.task_id,"after_seq":0}],"kinds":["completed","failed"],"mode":"any_terminal"}),
    )
    .await;
    let operation = tools::parse(&parent, &call).unwrap();
    let receipt = store
        .execute_main_tool(&mut parent, &call, operation, "wait-ended-result")
        .await
        .unwrap();
    assert_eq!(receipt.payload["status"], "ready");
    assert!(parent.subagent_wait.is_none());
}

#[tokio::test]
async fn protected_terminal_report_is_not_returned_or_staged_for_observation() {
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
        "read-protected-report",
        tools::REPORTS,
        serde_json::json!({"request_id": &task.task_id}),
    )
    .await;
    let operation = tools::parse(&parent, &call).unwrap();
    let receipt = store
        .execute_main_tool(&mut parent, &call, operation, "read-protected-receipt")
        .await
        .unwrap();
    let page: desk_diagnose_core::subagent::report::WorkerReportPage =
        serde_json::from_value(receipt.payload).unwrap();
    assert_eq!(page.request_id, task.task_id);
    assert!(page.reports.is_empty());
    assert!(page.acceptance_criteria.is_empty());
    assert_eq!(page.next_after_seq, 0);
    assert!(!page.has_more);
    assert!(parent.observed_subagent_results.is_empty());
    assert!(parent.accepted_subagent_observations.is_empty());
    let full = store
        .result_for_owner("root", "1", "1", &task.task_id, None)
        .await
        .unwrap();
    assert_eq!(full.task.state, SubAgentState::Completed);
    assert!(!full.report.unwrap().summary.is_empty());
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
        Default::default(),
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

#[tokio::test]
async fn short_task_ids_keep_query_wait_and_control_call_authority() {
    for tool in [
        tools::STATUS,
        tools::REPORTS,
        tools::WAIT_REPORTS,
        tools::CANCEL,
        tools::ADJUST,
    ] {
        let db = database().await;
        let (mut parent, calls) = super::creation::runnable_parent_with_source(
            &db,
            super::creation::destination(),
            desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
        )
        .await;
        let store = SubAgentStore::new(db.clone());
        let receipt = store
            .execute_main_tool(
                &mut parent,
                &calls[0],
                Operation::Spawn(super::creation::spawn_request()),
                "spawn-result",
            )
            .await
            .unwrap();
        let native = receipt.payload["task_id"].as_str().unwrap().to_owned();
        parent.ui_references.observe(&parent.conversation).unwrap();
        save_main_delegation_session(&db, &mut parent)
            .await
            .unwrap();
        if matches!(tool, tools::REPORTS | tools::WAIT_REPORTS) {
            complete_child(&db, &native).await;
        }
        let arguments = match tool {
            tools::WAIT_REPORTS => {
                serde_json::json!({"requests":[{"request_id":native,"after_seq":0}],"kinds":["completed","failed"],"mode":"all_terminal"})
            }
            tools::REPORTS | tools::CANCEL => {
                serde_json::json!({"request_id":native})
            }
            tools::ADJUST => {
                serde_json::json!({"request_id":native,"message":"Check one additional symptom"})
            }
            _ => serde_json::json!({"task_id":native}),
        };
        let original = committed_call(&db, &mut parent, "short-call", tool, arguments).await;
        let resolved = desk_diagnose_core::ui_model_ids::resolve_session_call_with_context(
            &original, &parent, 1, None,
        )
        .unwrap();
        let operation = tools::parse(&parent, &resolved).unwrap();
        let receipt = store
            .execute_main_tool(&mut parent, &resolved, operation, "short-result")
            .await
            .unwrap();
        let returned_task = if tool == tools::WAIT_REPORTS {
            &receipt.payload["tasks"][0]
        } else {
            &receipt.payload
        };
        let key = if tool == tools::REPORTS {
            "request_id"
        } else {
            "task_id"
        };
        assert_eq!(returned_task[key], native);
        assert_eq!(
            parent
                .conversation
                .iter()
                .flat_map(|message| &message.tool_calls)
                .find(|call| call.id == original.id)
                .unwrap()
                .arguments_json,
            original.arguments_json
        );
        parent.ui_references.observe(&parent.conversation).unwrap();
        let mut request = desk_diagnose_core::seam::ModelRequest::text_only(
            parent.conversation.clone(),
            desk_diagnose_core::prompt::ResponseFormatSpec::None,
        );
        request.ui_references = Some(parent.ui_references.clone());
        let projected = desk_diagnose_core::ui_model_ids::project_request(&request);
        let output = projected
            .messages
            .iter()
            .find(|message| message.message_id == "short-result")
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(
            if tool == tools::WAIT_REPORTS {
                output["tasks"][0]["task_id"].as_str()
            } else {
                output[key].as_str()
            },
            Some(native.as_str())
        );
        assert_eq!(request.messages, parent.conversation);
        if matches!(tool, tools::STATUS | tools::REPORTS | tools::WAIT_REPORTS) {
            let mut changed: serde_json::Value =
                serde_json::from_str(&resolved.arguments_json).unwrap();
            if tool == tools::WAIT_REPORTS {
                changed["requests"][0]["request_id"] = serde_json::json!("other-task");
            } else {
                changed[key] = serde_json::json!("other-task");
            }
            let changed = ToolCall {
                arguments_json: changed.to_string(),
                ..resolved
            };
            let operation = tools::parse(&parent, &changed).unwrap();
            let unchanged = parent.clone();
            assert!(
                store
                    .execute_main_tool(&mut parent, &changed, operation, "changed-short-result")
                    .await
                    .is_err()
            );
            assert_eq!(parent, unchanged);
        }
    }
}

#[tokio::test]
async fn frozen_model_control_survives_session_decode_but_observation_race_keeps_cas() {
    for changed in [false, true] {
        let db = database().await;
        let (mut parent, calls) = super::creation::runnable_parent_with_source(
            &db,
            super::creation::destination(),
            desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
        )
        .await;
        let store = SubAgentStore::new(db.clone());
        let task = store
            .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
            .await
            .unwrap();
        let call = committed_call(
            &db,
            &mut parent,
            "frozen-cancel",
            tools::CANCEL,
            serde_json::json!({"request_id":task.task_id}),
        )
        .await;
        parent =
            PersistedAgentSession::decode_json(&parent.encode_json_for_storage().unwrap()).unwrap();
        if changed {
            store.control_for_owner("root", "1", "1", &desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentControl { client_request_id: "concurrent-owner-adjustment".into(), task_id: task.task_id.clone(), expected_input_revision: 1, expected_control_revision: 1, action: desk_agent_protocol::ai_assistant::subagent::SubAgentControlAction::Adjust { client_timezone: "UTC".into(), message: "Check the additional symptom".into() } }).await.unwrap();
        }
        let before = load(&db, &task.task_id).await;
        let operation = tools::parse(&parent, &call).unwrap();
        let result = SubAgentStore::new(db.clone())
            .execute_main_tool(
                &mut parent,
                &call,
                operation.clone(),
                "frozen-control-result",
            )
            .await;
        if changed {
            assert!(result.is_err());
            assert_eq!(load(&db, &task.task_id).await, before);
        } else {
            let result = result.unwrap();
            assert_eq!(
                load(&db, &task.task_id).await.state,
                SubAgentState::Cancelled
            );
            let after = load(&db, &task.task_id).await;
            let repeated = store
                .execute_main_tool(&mut parent, &call, operation, "unused-repeat")
                .await
                .unwrap();
            assert_eq!(result.result_message_id, repeated.result_message_id);
            assert_eq!(load(&db, &task.task_id).await, after);
        }
    }
}

#[tokio::test]
async fn a_child_finishing_after_observation_refuses_adjustment_with_an_actionable_race() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
    )
    .await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let call = committed_call(&db, &mut parent, "late-followup", tools::ADJUST,
        serde_json::json!({"request_id":task.task_id,"message":"Please also check the additional symptom"}),
    ).await;
    store
        .control_for_owner(
            &parent.conversation_id,
            &parent.actor_id,
            &parent.device_id,
            &desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentControl {
                client_request_id: "owner-cancel-before-followup".into(),
                task_id: task.task_id.clone(),
                expected_input_revision: 1,
                expected_control_revision: 1,
                action: desk_agent_protocol::ai_assistant::subagent::SubAgentControlAction::Cancel,
            },
        )
        .await
        .unwrap();
    let before = load(&db, &task.task_id).await;
    assert_eq!(before.state, SubAgentState::Cancelled);
    let operation = tools::parse(&parent, &call).unwrap();
    let result = store
        .execute_main_tool(&mut parent, &call, operation, "late-followup-result")
        .await;
    assert!(matches!(result, Err(DbErr::Custom(ref code))
        if code == desk_diagnose_core::subagent::CONTROL_OBSERVATION_CHANGED));
    assert_eq!(load(&db, &task.task_id).await, before);
}

#[tokio::test]
async fn worker_progress_and_terminal_reports_are_immutable_ordered_and_non_consuming() {
    use crate::entity::agent_subagent_inbox as inbox;
    use desk_diagnose_core::subagent::{report::ReportKind, state::CompletionDisposition};

    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::UserContent,
    )
    .await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let run = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&run),
            &task.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("worker request can be claimed");
    };
    let mut child = claimed.session;
    let registry = tools::registry();
    assert!(
        desk_diagnose_core::registry::lookup_for_session(&registry, tools::REPORT, &parent)
            .is_none()
    );
    assert!(
        desk_diagnose_core::registry::lookup_for_session(&registry, tools::REPORT, &child)
            .is_some()
    );
    assert!(
        desk_diagnose_core::registry::lookup_for_session(&registry, tools::REPORTS, &child)
            .is_none()
    );

    for sequence in 1..=2 {
        let call = commit_worker_report_call(
            &db,
            &mut child,
            &format!("progress-{sequence}"),
            ReportKind::Progress,
        )
        .await;
        let caller_id = format!("caller-{}", call.id);
        let Operation::Report(content) = tools::parse(&child, &call).unwrap() else {
            panic!("report tool parses on an active worker");
        };
        assert!(
            store
                .report_for_turn(&mut child, &call, content.clone(), &caller_id)
                .await
                .is_err()
        );
        let receipt = store
            .report_for_turn(
                &mut child,
                &call,
                content.clone(),
                &format!("progress-ack-{sequence}"),
            )
            .await
            .unwrap();
        assert_eq!(receipt.payload["seq"], sequence);
        let replay = store
            .report_for_turn(&mut child, &call, content, "unused-id")
            .await
            .unwrap();
        assert_eq!(replay.result_message_id, receipt.result_message_id);
        assert_eq!(replay.payload, receipt.payload);
        assert_eq!(load(&db, &task.task_id).await.state, SubAgentState::Running);
    }
    let answer = super::lifecycle::report();
    super::lifecycle::append_report(&db, &mut child, &answer).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    assert_eq!(
        store
            .settle_answer_for_turn(&mut child, run.fence(), answer.summary.clone())
            .await
            .unwrap()
            .unwrap(),
        CompletionDisposition::Complete,
    );
    for (index, (after_seq, limit, expected_sequences, more)) in [
        (0, 10, vec![1, 2, 3], false),
        (0, 1, vec![1], true),
        (1, 2, vec![2, 3], false),
        (3, 2, vec![], false),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("read-worker-reports-{index}");
        let call = committed_call(
            &db, &mut parent, &id, tools::REPORTS,
            serde_json::json!({"request_id": &task.task_id, "after_seq": after_seq, "limit": limit}),
        ).await;
        let observed_before = parent.observed_subagent_results.len();
        let operation = tools::parse(&parent, &call).unwrap();
        let receipt = store
            .execute_main_tool(&mut parent, &call, operation, &format!("{id}-receipt"))
            .await
            .unwrap();
        let reports = receipt.payload["reports"].as_array().unwrap();
        let sequences = reports
            .iter()
            .map(|report| report["seq"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(sequences, expected_sequences);
        assert_eq!(receipt.payload["has_more"], more);
        assert_eq!(
            receipt.payload["next_after_seq"],
            expected_sequences.last().copied().unwrap_or(after_seq)
        );
        if let Some(report) = reports.iter().find(|report| report["seq"] == 3) {
            assert_eq!(report["content"]["kind"], "completed");
            assert_eq!(report["content"]["summary"], answer.summary);
            assert_eq!(
                receipt.payload["acceptance_criteria"],
                serde_json::json!(run.binding.acceptance_criteria)
            );
            assert_eq!(parent.observed_subagent_results.len(), observed_before + 1);
            let observed = parent.observed_subagent_results.last().unwrap();
            assert_eq!(report["report_id"], observed.report_id);
            assert_eq!(
                observed.state_revision,
                load(&db, &task.task_id).await.state_revision
            );
        } else {
            assert_eq!(parent.observed_subagent_results.len(), observed_before);
        }
    }
    let stored = inbox::Entity::find()
        .filter(inbox::Column::ReportSeq.is_not_null())
        .order_by_asc(inbox::Column::ReportSeq)
        .all(&db)
        .await
        .unwrap();
    assert_eq!(stored.len(), 3);
    assert_ne!(stored[0].event_id, stored[1].event_id);
    assert!(
        stored
            .iter()
            .all(|report| report.interpreted_at_ms.is_none()
                && report.model_observed_at_ms.is_none())
    );
}

pub(super) async fn commit_worker_report_call(
    db: &DatabaseConnection,
    child: &mut PersistedAgentSession,
    id: &str,
    kind: desk_diagnose_core::subagent::report::ReportKind,
) -> ToolCall {
    use desk_diagnose_core::chat::ToolCallRef;
    let mut arguments = serde_json::json!({
        "kind": kind,
        "summary": "A repeatable intermediate observation",
    });
    if kind == desk_diagnose_core::subagent::report::ReportKind::NeedsInput {
        arguments["question"] = serde_json::json!("Which environment should I verify first?");
    }
    let call = ToolCall {
        id: id.into(),
        name: tools::REPORT.into(),
        arguments_json: arguments.to_string(),
    };
    let caller_id = format!("caller-{id}");
    let mut message = desk_diagnose_core::chat::ChatMessage::assistant_tool_calls(
        &caller_id,
        "Reporting progress",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    )
    .with_turn_id(child.current_turn_id.clone().unwrap());
    message.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            &caller_id,
            &message.text,
            "model-output",
            &[child.conversation[0].data_envelope.clone().unwrap()],
        )
        .unwrap(),
    );
    child.conversation.push(message);
    save_child_session(db, child).await.unwrap();
    call
}

#[tokio::test]
async fn worker_terminal_tool_commits_once_and_preserves_pending_approval() {
    use desk_diagnose_core::subagent::report::{ReportKind, TERMINAL_REPORT_BLOCKED};

    for (kind, pending, expected_state) in [
        (ReportKind::Completed, false, SubAgentState::Completed),
        (ReportKind::Failed, false, SubAgentState::Failed),
        (ReportKind::Completed, true, SubAgentState::WaitingApproval),
    ] {
        let db = database().await;
        let mut child = super::lifecycle::child(&db).await;
        let binding = child.agent_role.binding().unwrap().clone();
        if pending {
            super::paused_permission::request(&mut child);
        }
        let call = commit_worker_report_call(&db, &mut child, "terminal-report", kind).await;
        let store = SubAgentStore::new(db.clone());
        let Operation::Report(content) = tools::parse(&child, &call).unwrap() else {
            panic!("terminal report is a worker tool");
        };
        let held = child.clone();
        let result = store
            .report_for_turn(&mut child, &call, content.clone(), "terminal-receipt")
            .await;
        let run = load(&db, &binding.task_id).await;
        assert_eq!(run.state, expected_state);
        if pending {
            assert!(
                matches!(result, Err(DbErr::Custom(message)) if message == TERMINAL_REPORT_BLOCKED)
            );
            assert_eq!(child, held);
            assert!(run.terminal_report.is_none());
            continue;
        }
        assert_eq!(result.unwrap().payload["seq"], 1);
        assert_eq!(child.turn_state, TurnState::Idle);
        assert_eq!(child.handled_input_seq, child.latest_input_seq);
        assert_eq!(child.lease_token, held.lease_token);
        assert_eq!(
            run.terminal_report.as_ref().unwrap().summary,
            content.summary
        );
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&child.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(row.lease_deadline.is_none());
        assert_eq!(
            PersistedAgentSession::decode_json(&row.state_json).unwrap(),
            child
        );
        assert!(
            store
                .report_for_turn(&mut child, &call, content, "duplicate-receipt")
                .await
                .is_err()
        );
        let run = load(&db, &binding.task_id).await;
        assert_eq!(run.state, expected_state);
        let rows = crate::entity::agent_subagent_inbox::Entity::find()
            .filter(crate::entity::agent_subagent_inbox::Column::TaskId.eq(&binding.task_id))
            .filter(crate::entity::agent_subagent_inbox::Column::ReportSeq.is_not_null())
            .count(&db)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }
}

#[tokio::test]
async fn question_reply_resumes_only_the_original_request_with_its_budget() {
    use desk_diagnose_core::subagent::report::{QUESTION_NOT_OPEN, ReportKind};

    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::UserContent,
    )
    .await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let original = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&original),
            &task.task_id,
            original.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("worker request can be claimed");
    };
    let mut child = claimed.session;
    let call =
        commit_worker_report_call(&db, &mut child, "ask-original", ReportKind::NeedsInput).await;
    let Operation::Report(content) = tools::parse(&child, &call).unwrap() else {
        panic!("question tool");
    };
    let question = store
        .report_for_turn(&mut child, &call, content, "question-receipt")
        .await
        .unwrap();
    let question_id = question.payload["question_id"].as_str().unwrap();
    assert_eq!(child.turn_state, TurnState::Idle);
    let waiting = load(&db, &task.task_id).await;
    assert_eq!(waiting.state, SubAgentState::WaitingInput);
    assert_eq!(waiting.pending_question_id.as_deref(), Some(question_id));
    assert!(
        store
            .child_runtime_candidate(&task.task_id)
            .await
            .unwrap()
            .is_none()
    );
    store.reconcile_task(&task.task_id).await.unwrap();
    assert_eq!(
        load(&db, &task.task_id).await.state,
        SubAgentState::WaitingInput
    );

    let read = committed_call(
        &db,
        &mut parent,
        "read-question",
        tools::REPORTS,
        serde_json::json!({"request_id": task.task_id}),
    )
    .await;
    let operation = tools::parse(&parent, &read).unwrap();
    let read = store
        .execute_main_tool(&mut parent, &read, operation, "read-question-receipt")
        .await
        .unwrap();
    assert_eq!(read.payload["reports"][0]["question_id"], question_id);
    assert_eq!(
        read.payload["reports"][0]["content"]["question"],
        "Which environment should I verify first?"
    );

    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&original.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let arguments = serde_json::json!({
        "request_id": task.task_id, "question_id": question_id, "message": "Check the staging environment first.",
    });
    let reply = committed_call(
        &db,
        &mut parent,
        "answer-original",
        tools::REPLY,
        arguments.clone(),
    )
    .await;
    let operation = tools::parse(&parent, &reply).unwrap();
    let result = store
        .execute_main_tool(
            &mut parent,
            &reply,
            operation.clone(),
            "answer-question-receipt",
        )
        .await
        .unwrap();
    assert_eq!(result.payload["state"], "queued");
    let repeated = store
        .execute_main_tool(&mut parent, &reply, operation, "unused-receipt")
        .await
        .unwrap();
    assert_eq!(result.payload, repeated.payload);
    let resumed = load(&db, &task.task_id).await;
    assert_eq!(resumed.binding, original.binding);
    assert!(resumed.pending_question_id.is_none());
    assert_eq!(
        group_row::Entity::find_by_id(group.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        group
    );
    let mut params = super::creation::claim_params(&resumed);
    params.turn_id = "worker-after-question".into();
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &params,
            &task.task_id,
            resumed.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("answer resumes the same request");
    };
    assert_eq!(claimed.session.chain_id, child.chain_id);
    assert_eq!(claimed.session.input_revision, child.input_revision);
    let replies = claimed
        .session
        .conversation
        .iter()
        .filter(|message| message.message_id == format!("reply-{question_id}"))
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert!(
        replies[0]
            .text
            .contains("Check the staging environment first.")
    );
    assert!(
        replies[0]
            .data_envelope
            .as_ref()
            .unwrap()
            .allowed_destinations
            .contains(&super::creation::destination())
    );

    let replay = committed_call(&db, &mut parent, "answer-again", tools::REPLY, arguments).await;
    let operation = tools::parse(&parent, &replay).unwrap();
    assert!(matches!(
        store.execute_main_tool(&mut parent, &replay, operation, "replayed-question-receipt").await,
        Err(DbErr::Custom(message)) if message == QUESTION_NOT_OPEN
    ));
}

#[tokio::test]
async fn owner_reads_worker_report_pages_and_answers_only_the_open_question_once() {
    use desk_agent_protocol::ai_assistant::subagent::{
        AiAssistantSubAgentControl, SubAgentControlAction,
    };
    use desk_diagnose_core::subagent::report::ReportKind;
    let db = database().await;
    let (parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::UserContent,
    )
    .await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let original = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&original),
            &task.task_id,
            original.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("worker request can be claimed");
    };
    let mut child = claimed.session;
    for seq in 1..=12 {
        let kind = if seq == 12 {
            ReportKind::NeedsInput
        } else {
            ReportKind::Progress
        };
        let id = format!("owner-report-{seq}");
        let call = commit_worker_report_call(&db, &mut child, &id, kind).await;
        let Operation::Report(content) = tools::parse(&child, &call).unwrap() else {
            panic!("worker report");
        };
        store
            .report_for_turn(&mut child, &call, content, &format!("{id}-receipt"))
            .await
            .unwrap();
    }
    let latest = store
        .result_for_owner("root", "1", "1", &task.task_id, None)
        .await
        .unwrap();
    assert_eq!(latest.task.state, SubAgentState::WaitingInput);
    let question = latest.open_question.unwrap();
    assert_eq!(question.text, "Which environment should I verify first?");
    assert_eq!(
        latest
            .reports
            .items
            .iter()
            .map(|report| report.seq)
            .collect::<Vec<_>>(),
        (3..=12).collect::<Vec<_>>()
    );
    assert_eq!(latest.reports.items[9].report_id, question.question_id);
    assert_eq!(latest.reports.next_before_seq, Some(3));
    let earlier = store
        .result_for_owner(
            "root",
            "1",
            "1",
            &task.task_id,
            latest.reports.next_before_seq,
        )
        .await
        .unwrap();
    assert_eq!(
        earlier
            .reports
            .items
            .iter()
            .map(|report| report.seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(earlier.reports.next_before_seq, None);
    assert_eq!(earlier.open_question.as_ref(), Some(&question));
    assert!(
        store
            .result_for_owner("root", "1", "1", &task.task_id, Some(0))
            .await
            .is_err()
    );

    let group_before = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&original.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let request = AiAssistantSubAgentControl {
        client_request_id: "owner-answer".into(),
        task_id: task.task_id.clone(),
        expected_input_revision: task.input_revision,
        expected_control_revision: task.control_revision,
        action: SubAgentControlAction::Reply {
            question_id: question.question_id.clone(),
            message: "Verify staging first.".into(),
        },
    };
    let mut wrong = request.clone();
    wrong.action = SubAgentControlAction::Reply {
        question_id: "unrelated-question".into(),
        message: "This must not be sent.".into(),
    };
    assert!(
        store
            .control_for_owner("root", "1", "1", &wrong)
            .await
            .is_err()
    );
    assert!(
        store
            .control_for_owner("root", "2", "1", &request)
            .await
            .is_err()
    );
    let answer = store
        .control_for_owner("root", "1", "1", &request)
        .await
        .unwrap();
    assert_eq!(answer.task.state, SubAgentState::Queued);
    assert_eq!(answer.task.input_revision, task.input_revision);
    assert_eq!(answer.task.control_revision, task.control_revision);
    assert_eq!(
        store
            .control_for_owner("root", "1", "1", &request)
            .await
            .unwrap(),
        answer
    );
    assert!(
        store
            .control_for_owner("root", "1", "1", &wrong)
            .await
            .is_err()
    );
    let late = AiAssistantSubAgentControl {
        client_request_id: "late-answer".into(),
        ..request
    };
    assert!(
        store
            .control_for_owner("root", "1", "1", &late)
            .await
            .is_err()
    );
    let after = store
        .result_for_owner("root", "1", "1", &task.task_id, None)
        .await
        .unwrap();
    assert!(after.open_question.is_none());
    assert_eq!(after.reports, latest.reports);
    let run = load(&db, &task.task_id).await;
    assert_eq!(run.binding, original.binding);
    assert_eq!(
        group_row::Entity::find_by_id(group_before.id)
            .one(&db)
            .await
            .unwrap(),
        Some(group_before),
    );
    let resume = store
        .child_runtime_candidate(&task.task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resume
            .session()
            .conversation
            .iter()
            .filter(|message| message.text.contains("Verify staging first."))
            .count(),
        1,
    );
}
