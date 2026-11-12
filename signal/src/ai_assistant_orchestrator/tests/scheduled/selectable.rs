use super::super::original_input::{register_request_device, respond_to_system_info};
use super::*;
use desk_diagnose_core::conversation_key::{
    AssistantConversationScope, derive_assistant_conversation_key,
};
use desk_signal_facade::model::os::OperationSystemEnum;
use serde_json::json;

#[actix_web::test]
async fn selectable_timer_runs_on_original_device_after_owner_clears_selection() {
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("timer-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let peer = register_request_device(
        &connections,
        "timer-a",
        "Original Mac",
        OperationSystemEnum::Mac,
    )
    .await;
    for device in ["timer-a", "timer-b"] {
        crate::device_code_store::get_or_create(&db, device)
            .await
            .unwrap();
    }
    use desk_agent_protocol::computer_use::*;
    let now = chrono::Utc::now();
    crate::computer_use_readiness::global_computer_use_readiness_cache()
        .update(
            "live-timer-a",
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                server_api_version: 1,
                os: "macos".into(),
                interactive_user_home: None,
                interactive_session_incarnation: "timer-worker".into(),
                local_ceiling_revision: 1,
                unattended_command_execution: false,
                capabilities: vec![ComputerUseCapabilityReadiness {
                    capability: desk_agent_protocol::Capability::SystemInfo,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::SystemDiagnostics,
                        version: "1".into(),
                    },
                    supported: true,
                    ready: true,
                    reason: None,
                }],
                context_references: vec![],
            },
            now,
        )
        .unwrap();
    let gate = crate::ai_assistant_gate::AiAssistantGate::new(
        desk_agent_protocol::ai_assistant::AiAssistantSettings {
            revision: 1,
            enabled: true,
        },
    );
    let client = "selectable-timer";
    let conversation =
        derive_assistant_conversation_key("1", AssistantConversationScope::Personal, client);
    for (index, targets) in [vec!["timer-a".into(), "timer-b".into()], vec![]]
        .into_iter()
        .enumerate()
    {
        let captured = listener.clone();
        let gateway = actix_web::rt::spawn(async move {
            if index == 0 {
                capture_openai_answer_with_title(captured.as_ref(), ANSWER).await;
            } else {
                capture_one_openai_request_with_sse(captured.as_ref(), ANSWER).await;
            }
        });
        tokio::time::timeout(
            MODEL_IO_TIMEOUT,
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("input-{index}"),
                "browser".into(),
                String::new(),
                1,
                String::new(),
                AiAssistantAsk {
                    question: format!("Owner requirement {index}"),
                    client_message_id: format!("message-{index}"),
                    conversation_id: Some(client.into()),
                    client_timezone: "UTC".into(),
                    target_refs: targets,
                    ..Default::default()
                },
                None,
            ),
        )
        .await
        .unwrap();
        tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
            .await
            .unwrap()
            .unwrap();
        if index == 0 {
            let result = crate::schedule_management::manage(
                &db,
                1,
                desk_agent_protocol::schedule::management::ScheduleManagementRequest::CreateTask {
                    task: ScheduleDraft {
                        time_confirmation: None,
                        client_create_key: "timer-create".into(),
                        kind: ScheduledTaskKind::ConversationResume,
                        target_device_id: "timer-a".into(),
                        title: "Check original Mac".into(),
                        prompt: "Read the original Mac system version".into(),
                        locale: None,
                        model_id: None,
                        spec: ScheduleSpec {
                            schema_version: 1,
                            rule: ScheduleRule::Once {
                                at: (chrono::Utc::now() + chrono::Duration::seconds(2))
                                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                            },
                        },
                        source_conversation_id: Some(conversation.clone()),
                        requirement_revision: Some(1),
                        creation_source: ScheduleCreationSource::Manual,
                    },
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                result,
                desk_agent_protocol::schedule::management::ScheduleManagementResponse::Task { .. }
            ));
        }
    }
    let store = ScheduleStore::new(db.clone());
    let task = crate::entity::agent_schedule::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let wait = task.next_run_at.unwrap() - store.database_time().await.unwrap() + 20;
    tokio::time::sleep(std::time::Duration::from_millis(wait.max(0) as u64)).await;
    let run = store
        .materialize_due(&task.schedule_id, task.revision)
        .await
        .unwrap()
        .unwrap();
    let original = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&conversation, "1", "")
        .await
        .unwrap()
        .unwrap();
    let claimed = store
        .claim_conversation_resume(ContinuationClaim {
            owner: 1,
            run_id: &run.run_id,
            node_id: "timer-test",
            lease_seconds: 90,
            policy_revision:
                desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
            scope: original.scope_snapshot,
        })
        .await
        .unwrap();
    assert!(
        claimed
            .session
            .admitted_assistant_request_targets()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        claimed
            .session
            .single_assistant_request_target()
            .unwrap()
            .device_id,
        "timer-a"
    );
    let owner_group = claimed.session.scheduled_owner_group_id.clone();
    assert_ne!(claimed.session.delegation_group_id, owner_group);
    let group = crate::entity::agent_delegation_group::Entity::find()
        .filter(
            crate::entity::agent_delegation_group::Column::GroupId.eq(claimed
                .session
                .delegation_group_id
                .clone()
                .unwrap()),
        )
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let creation: desk_diagnose_core::subagent::creation::CreationEnvelope =
        serde_json::from_str(&group.creation_envelope_json).unwrap();
    assert_eq!(
        creation.targets,
        claimed.session.assistant_request_targets().unwrap()
    );
    assert!(matches!(
        creation.source,
        desk_diagnose_core::subagent::DelegationSource::ConversationResume { .. }
    ));
    let token = claimed.session.lease_token;
    let epoch = claimed.run.lease_epoch;
    let capture = listener.clone();
    let gateway = actix_web::rt::spawn(async move {
        let read = super::super::original_input::memory_sse(
            json!({"tool_calls":[{
                "index":0,"id":"timer-read","type":"function","function":{"name":"read_system_info","arguments":"{}"}
            }]}),
            "tool_calls",
        );
        capture_one_openai_request_with_sse(capture.as_ref(), &read).await;
        capture_one_openai_request_with_sse(capture.as_ref(), ANSWER).await
    });
    let device_connections = connections.clone();
    let edge = actix_web::rt::spawn(async move {
        respond_to_system_info(&device_connections, "timer-a", peer, "MACOS_TIMER_RECEIPT").await;
    });
    let outcome = tokio::time::timeout(
        MODEL_IO_TIMEOUT,
        resume_scheduled_turn(
            connections.clone(),
            db.clone(),
            &gate,
            "live-timer-a".into(),
            claimed,
            90,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::Answered(_)), "{outcome:?}");
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&conversation))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let saved = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(
        saved
            .conversation
            .iter()
            .any(
                |message| message.tool_call_id.as_deref() == Some("timer-read")
                    && message.tool_ok == Some(true)
            ),
        "timer results: {:?}",
        saved
            .conversation
            .iter()
            .filter(|message| message.tool_call_id.as_deref() == Some("timer-read"))
            .map(|message| &message.text)
            .collect::<Vec<_>>()
    );
    tokio::time::timeout(MODEL_IO_TIMEOUT, edge)
        .await
        .unwrap()
        .unwrap();
    let body = tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("MACOS_TIMER_RECEIPT"));
    store
        .finish_answered_continuation(
            crate::schedule_store::ContinuationLease {
                owner: 1,
                run_id: &run.run_id,
                node_id: "timer-test",
                run_epoch: epoch,
                session_token: token,
            },
            "captured-ok",
        )
        .await
        .unwrap();
    let restored = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&conversation))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let restored = PersistedAgentSession::decode_json(&restored.state_json).unwrap();
    assert_eq!(restored.delegation_group_id, owner_group);
    assert!(restored.scheduled_request.is_none());
    assert!(restored.assistant_request_targets().unwrap().is_empty());
    assert_eq!(
        store
            .resolve_run_session(1, &task.schedule_id, &run.run_id)
            .await
            .unwrap(),
        (conversation, String::new())
    );
}

/// An AI-proposed conversation timer is enabled inside the proposing turn;
/// its occurrence must still run on the device the owner selected.
#[actix_web::test]
async fn ai_proposed_timer_runs_on_the_selected_device() {
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("timer-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let peer = register_request_device(
        &connections,
        "timer-a",
        "Original Mac",
        OperationSystemEnum::Mac,
    )
    .await;
    for device in ["timer-a", "timer-b"] {
        crate::device_code_store::get_or_create(&db, device)
            .await
            .unwrap();
    }
    use desk_agent_protocol::computer_use::*;
    let now = chrono::Utc::now();
    crate::computer_use_readiness::global_computer_use_readiness_cache()
        .update(
            "live-timer-a",
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                server_api_version: 1,
                os: "macos".into(),
                interactive_user_home: None,
                interactive_session_incarnation: "timer-worker".into(),
                local_ceiling_revision: 1,
                unattended_command_execution: false,
                capabilities: vec![ComputerUseCapabilityReadiness {
                    capability: desk_agent_protocol::Capability::SystemInfo,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::SystemDiagnostics,
                        version: "1".into(),
                    },
                    supported: true,
                    ready: true,
                    reason: None,
                }],
                context_references: vec![],
            },
            now,
        )
        .unwrap();
    let gate = crate::ai_assistant_gate::AiAssistantGate::new(
        desk_agent_protocol::ai_assistant::AiAssistantSettings {
            revision: 1,
            enabled: true,
        },
    );
    let client = "ai-timer";
    let conversation =
        derive_assistant_conversation_key("1", AssistantConversationScope::Personal, client);
    let at = (chrono::Utc::now() + chrono::Duration::seconds(3))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let arguments = json!({
        "kind": "conversation_resume",
        "title": "Read Mac time",
        "prompt": "Read the selected Mac system information",
        "rule": {"kind": "once", "at": at},
    })
    .to_string();
    let captured = listener.clone();
    let gateway = actix_web::rt::spawn(async move {
        let propose = super::super::original_input::memory_sse(
            json!({"tool_calls":[{
                "index":0,"id":"timer-propose","type":"function",
                "function":{"name":"request_scheduled_task","arguments":arguments}
            }]}),
            "tool_calls",
        );
        capture_one_openai_request_with_sse(captured.as_ref(), &propose).await;
        capture_openai_answer_with_title(captured.as_ref(), ANSWER).await;
    });
    tokio::time::timeout(
        MODEL_IO_TIMEOUT,
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "ai-timer-input".into(),
            "browser".into(),
            String::new(),
            1,
            String::new(),
            AiAssistantAsk {
                question: "Read the Mac time a few seconds from now".into(),
                client_message_id: "ai-timer-message".into(),
                conversation_id: Some(client.into()),
                client_timezone: "UTC".into(),
                target_refs: vec!["timer-a".into()],
                ..Default::default()
            },
            None,
        ),
    )
    .await
    .unwrap();
    tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    let store = ScheduleStore::new(db.clone());
    let task = crate::entity::agent_schedule::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status, "active");
    assert_eq!(task.target_device_id, "timer-a");
    let wait = task.next_run_at.unwrap() - store.database_time().await.unwrap() + 20;
    tokio::time::sleep(std::time::Duration::from_millis(wait.max(0) as u64)).await;
    let run = store
        .materialize_due(&task.schedule_id, task.revision)
        .await
        .unwrap()
        .unwrap();
    let original = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&conversation, "1", "")
        .await
        .unwrap()
        .unwrap();
    let claimed = store
        .claim_conversation_resume(ContinuationClaim {
            owner: 1,
            run_id: &run.run_id,
            node_id: "timer-test",
            lease_seconds: 90,
            policy_revision:
                desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
            scope: original.scope_snapshot,
        })
        .await
        .unwrap();
    let capture = listener.clone();
    let gateway = actix_web::rt::spawn(async move {
        let read = super::super::original_input::memory_sse(
            json!({"tool_calls":[{
                "index":0,"id":"timer-read","type":"function","function":{"name":"read_system_info","arguments":"{}"}
            }]}),
            "tool_calls",
        );
        capture_one_openai_request_with_sse(capture.as_ref(), &read).await;
        capture_one_openai_request_with_sse(capture.as_ref(), ANSWER).await
    });
    let device_connections = connections.clone();
    let edge = actix_web::rt::spawn(async move {
        respond_to_system_info(&device_connections, "timer-a", peer, "AI_TIMER_RECEIPT").await;
    });
    let outcome = tokio::time::timeout(
        MODEL_IO_TIMEOUT,
        resume_scheduled_turn(
            connections.clone(),
            db.clone(),
            &gate,
            "live-timer-a".into(),
            claimed,
            90,
        ),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, Ok(LoopOutcome::Answered(_))),
        "{outcome:?}"
    );
    tokio::time::timeout(MODEL_IO_TIMEOUT, edge)
        .await
        .unwrap()
        .unwrap();
    let body = tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("AI_TIMER_RECEIPT"));
}
