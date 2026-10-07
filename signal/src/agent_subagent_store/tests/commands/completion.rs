//! Exercise original command export after a real delegated approval and dispatch.
mod live;
use super::*;
use crate::agent_exec_store::SignalAgentExecStore;
use desk_diagnose_core::{
    command_completion::CommandCompletionContext, model_egress::ModelEgressPolicy,
    prompt::ResponseFormatSpec, seam::ModelRequest, session::WorkKind,
};

#[actix_web::test]
async fn approved_child_original_completion_reaches_authorized_model_projection() {
    assert_completion_publisher_interprets_original(false).await;
}

#[actix_web::test]
async fn recovered_child_command_completion_is_still_published_and_interpreted() {
    assert_completion_publisher_interprets_original(true).await;
}

async fn assert_completion_publisher_interprets_original(recover_first: bool) {
    struct CompletionDiagnostics;
    impl log::Log for CompletionDiagnostics {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Warn && metadata.target().contains("agent_runtime")
        }
        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!("COMPLETION_CHECK {}", record.args());
            }
        }
        fn flush(&self) {}
    }
    let _ = log::set_logger(&CompletionDiagnostics);
    log::set_max_level(log::LevelFilter::Warn);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("child-completion".into()),
        base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
        api_key: Some("test-only".into()),
        max_context_bytes: Some(131072),
        ..Default::default()
    };
    let (db, child, _) = completed_fixture_with_delivery(&config, None, recover_first).await;
    let exec = SignalAgentExecStore::new(db.clone());
    let before = deliverable_commands(&db).await;
    assert_eq!(
        before.len(),
        1,
        "an uninterpreted child result must stay deliverable"
    );
    let capture = actix_web::rt::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let request = loop {
            let mut buf = [0; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buf[..n]);
            if let Some(head) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..head]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= head + 4 + length {
                    break serde_json::from_slice::<serde_json::Value>(
                        &bytes[head + 4..head + 4 + length],
                    )
                    .unwrap();
                }
            }
        };
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Original result interpreted.\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        request
    });
    exec.publish_once().await.unwrap();
    let wire = tokio::time::timeout(std::time::Duration::from_secs(5), capture)
        .await
        .unwrap()
        .unwrap();
    assert!(
        wire.get("tools")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty))
    );
    let stored = super::super::paused_permission::session(&db, &child.conversation_id).await;
    assert_eq!(stored.turn_state, TurnState::Idle);
    assert!(stored.pending_auto_triggers.is_empty());
    assert!(deliverable_commands(&db).await.is_empty());
    let tasks = SubAgentStore::new(db.clone());
    tasks
        .reconcile_task(&child.agent_role.binding().unwrap().task_id)
        .await
        .unwrap();
    exec.publish_once().await.unwrap();
    assert!(deliverable_commands(&db).await.is_empty());
    assert_eq!(
        super::super::paused_permission::session(&db, &child.conversation_id)
            .await
            .automation_turns_used,
        1,
        "recovery and publication must not interpret the same receipt twice"
    );
    assert_ne!(
        load(&db, &child.agent_role.binding().unwrap().task_id)
            .await
            .state,
        SubAgentState::Failed
    );
}

async fn deliverable_commands(db: &DatabaseConnection) -> Vec<agent_exec_task::Model> {
    agent_exec_task::Entity::find()
        .filter(
            agent_exec_task::Column::DeliveryState.eq(crate::agent_exec_store::DELIVERY_PENDING),
        )
        .all(db)
        .await
        .unwrap()
}

#[actix_web::test]
async fn child_recovery_rearms_only_the_original_uninterpreted_command_delivery() {
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("child-completion".into()),
        base_url: Some("http://127.0.0.1:1".into()),
        api_key: Some("test-only".into()),
        max_context_bytes: Some(131072),
        ..Default::default()
    };
    let (db, child, _) = completed_fixture(&config, None).await;
    let pending = child.pending_auto_triggers[0].clone();
    let tasks = SubAgentStore::new(db.clone());
    let task_id = child.agent_role.binding().unwrap().task_id.clone();
    tasks.reconcile_task(&task_id).await.unwrap();
    let before_rearm = super::super::paused_permission::session(&db, &child.conversation_id).await;
    let exec = SignalAgentExecStore::new(db.clone());
    exec.consume_event(&pending.event_id).await.unwrap();
    let original = exec
        .find_by_generation(&pending.execution_id)
        .await
        .unwrap()
        .unwrap();

    agent_exec_task::Entity::update_many()
        .set(agent_exec_task::ActiveModel {
            tool_call_id: Set("different-call".into()),
            ..Default::default()
        })
        .filter(agent_exec_task::Column::Id.eq(original.id))
        .exec(&db)
        .await
        .unwrap();
    assert!(tasks.reconcile_task(&task_id).await.is_err());
    assert!(deliverable_commands(&db).await.is_empty());

    agent_exec_task::Entity::update_many()
        .set(agent_exec_task::ActiveModel {
            tool_call_id: Set(pending.tool_call_id),
            ..Default::default()
        })
        .filter(agent_exec_task::Column::Id.eq(original.id))
        .exec(&db)
        .await
        .unwrap();
    assert!(tasks.reconcile_task(&task_id).await.unwrap());
    assert!(!tasks.reconcile_task(&task_id).await.unwrap());
    let rearmed = deliverable_commands(&db).await;
    assert_eq!(rearmed.len(), 1);
    assert_eq!(
        rearmed[0].execution_generation,
        original.execution_generation
    );
    assert_eq!(rearmed[0].result_text, original.result_text);
    assert_eq!(rearmed[0].disposition_json, original.disposition_json);
    let stored = super::super::paused_permission::session(&db, &child.conversation_id).await;
    assert_eq!(
        stored.version, before_rearm.version,
        "repairing only a delivery hint must preserve the session fence"
    );
    assert_eq!(stored.automation_turns_used, 0);
    assert_eq!(
        stored
            .conversation
            .iter()
            .filter(|m| m.message_id == original.event_id)
            .count(),
        1
    );
}

async fn completed_fixture(
    config: &crate::model_provider::ModelProviderConfig,
    first: Option<&desk_diagnose_core::chat::ModelTurn>,
) -> (DatabaseConnection, PersistedAgentSession, ModelEgressPolicy) {
    completed_fixture_with_delivery(config, first, false).await
}

async fn completed_fixture_with_delivery(
    config: &crate::model_provider::ModelProviderConfig,
    first: Option<&desk_diagnose_core::chat::ModelTurn>,
    recover_first: bool,
) -> (DatabaseConnection, PersistedAgentSession, ModelEgressPolicy) {
    let destination = config.destination_identity().unwrap();
    let (db, _, mut child, call, grant) = approved_child_with_source(
        first.and_then(|turn| turn.tool_calls.first()).cloned(),
        destination.clone(),
        desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
    )
    .await;
    let schema = Schema::new(db.get_database_backend());
    for mut statement in [
        schema.create_table_from_entity(crate::entity::model_provider::Entity),
        schema.create_table_from_entity(crate::entity::model_probe_observation::Entity),
        schema.create_table_from_entity(crate::entity::ai_usage::Entity),
        schema.create_table_from_entity(crate::entity::context_management_config::Entity),
    ] {
        db.execute(statement.if_not_exists()).await.unwrap();
    }
    crate::model_provider::save(&db, config.clone())
        .await
        .unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let policy = ModelEgressPolicy {
        destination: destination.clone(),
        selected_source_tools: [call.name.clone()].into_iter().collect(),
        export_authorization_id: "original-child-export".into(),
        now_unix_ms: now,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    // The approved continuation owns its own model proposal, as in production.
    let proposal = child
        .conversation
        .iter_mut()
        .find(|message| {
            message
                .tool_calls
                .iter()
                .any(|candidate| candidate.id == call.id)
        })
        .unwrap();
    proposal.turn_id = child.current_turn_id.clone();
    save_child_session(&db, &mut child).await.unwrap();
    if let Some(first) = first {
        let proposal = child
            .conversation
            .iter_mut()
            .find(|message| {
                message
                    .tool_calls
                    .iter()
                    .any(|candidate| candidate.id == call.id)
            })
            .unwrap();
        let inputs = [proposal.data_envelope.clone().unwrap()];
        proposal.text = first.text.clone();
        proposal.tool_calls = first.tool_calls.iter().map(ToolCall::to_ref).collect();
        proposal.replay_disposition = first.provider_meta.replay.clone();
        proposal.data_envelope = Some(policy.derive_model_output_envelope(first, &inputs).unwrap());
        save_child_session(&db, &mut child).await.unwrap();
    }
    let store = SignalCapabilityGrantStore::new(db.clone());
    store
        .prepare(prepared(&child, &call, &grant))
        .await
        .unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } = store
        .record_dispatch_intent(prepared(&child, &call, &grant))
        .await
        .unwrap()
    else {
        panic!("one original dispatch required")
    };
    let mut origin = ActionResultOrigin::capture_confirmed_command(
        &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
        &child,
        &call,
        now,
    )
    .unwrap();
    origin.command_completion =
        Some(CommandCompletionContext::capture(&child, destination.clone(), now, 60000).unwrap());
    let export =
        crate::capability_grant_store::computer_export::ComputerExportContext::capture_command(
            &policy,
            &child,
            origin.command_completion.as_ref().unwrap(),
        )
        .unwrap();
    store
        .claim_command_dispatch(
            &dispatch_id,
            now,
            "host",
            &child.conversation_id,
            &call.id,
            60000,
            &origin,
            Some(&export),
        )
        .await
        .unwrap();
    let exec = SignalAgentExecStore::new(db.clone());
    exec.finalize(
        "host",
        &dispatch_id,
        &desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed {
            outcome: desk_agent_protocol::AgentOutcome::Ok(
                desk_agent_protocol::OperationOutput::Exec(desk_agent_protocol::ExecOutput {
                    started: true,
                    exit_code: Some(0),
                    termination_signal: None,
                    failure: None,
                    diagnostics: vec![],
                    streams: desk_agent_protocol::ExecOutputStreams::Split {
                        stdout: "SYNTHETIC-COMPLETION-OK".into(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                    },
                    duration_ms: 1,
                    redactions: vec![],
                }),
            ),
        },
    )
    .await
    .unwrap();
    let task = exec
        .find_by_generation(&dispatch_id)
        .await
        .unwrap()
        .unwrap();
    let (output, receipt) = exec.command_result(&task).await.unwrap().unwrap();
    child.execution_state = desk_diagnose_core::session::ExecutionState::Executing {
        action: receipt.action.clone(),
    };
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    let sessions = SignalAgentSessionStore::new(db.clone());
    if recover_first {
        let tasks = SubAgentStore::new(db.clone());
        tasks
            .reconcile_task(&child.agent_role.binding().unwrap().task_id)
            .await
            .unwrap();
        tasks
            .reconcile_task(&child.agent_role.binding().unwrap().task_id)
            .await
            .unwrap();
    } else {
        sessions
            .deliver_work_completion_with_envelope(
                &child.conversation_id,
                task.id,
                WorkKind::AgentExec,
                &task.event_id,
                &dispatch_id,
                &call.id,
                &task.exec_request_id,
                &output.content,
                Some(receipt.envelope),
                output.format,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await
            .unwrap();
    }
    child = super::super::paused_permission::session(&db, &child.conversation_id).await;
    store
        .completion_export(&child, &task.event_id, &destination)
        .await
        .unwrap();
    let tasks = SubAgentStore::new(db.clone());
    assert!(tasks.child_resume_available(&child).await.unwrap());
    store
        .completion_export(&child, &task.event_id, &destination)
        .await
        .unwrap();
    let creation = tasks.child_creation_context(&child).await.unwrap().unwrap();
    let projected = desk_diagnose_core::command_completion::project_child_request(
        ModelRequest::text_only(
            vec![ChatMessage::text(
                "system",
                desk_diagnose_core::chat::ChatRole::System,
                desk_diagnose_core::command_completion::INTERPRETATION_INSTRUCTION,
            )],
            ResponseFormatSpec::None,
        ),
        &child,
        &task.event_id,
        &creation,
    )
    .unwrap();
    let authorized = policy.authorize_request(projected).unwrap();
    assert!(
        authorized
            .request
            .messages
            .iter()
            .any(|m| m.message_id == task.event_id)
    );
    assert!(authorized.request.tools.is_empty());
    (db, child, policy)
}
