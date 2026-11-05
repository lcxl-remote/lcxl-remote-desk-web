//! Actual OSS entry, SQLite and synthetic HTTP model; no device UI is exercised.

use super::*;
use crate::agent_run_event_store::{InputSubject, SignalAgentRunEventStore};
use desk_agent_protocol::ai_assistant::AiAssistantObjectContextOperation;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};

#[actix_web::test]
async fn targetless_owner_turn_uses_audited_provider_and_keeps_its_answer() {
    personal_input_roundtrip(Vec::new()).await;
}

#[actix_web::test]
async fn personal_input_with_selected_targets_runs_under_the_same_account() {
    personal_input_roundtrip(vec!["selected-mac".into(), "selected-linux".into()]).await;
}

#[actix_web::test]
async fn queued_owner_input_runs_after_reopen_without_the_submitting_connection() {
    desk_diagnose_core::future::boxed(|| queued_owner_input_roundtrip(false, false, false)).await;
}

#[actix_web::test]
async fn workbench_queue_scans_into_independent_native_conversations() {
    desk_diagnose_core::future::boxed(|| queued_owner_input_roundtrip(true, false, false)).await;
}

#[actix_web::test]
async fn deleting_workbench_closes_native_work_and_preserves_dispatched_actions() {
    desk_diagnose_core::future::boxed(|| queued_owner_input_roundtrip(true, true, false)).await;
}

#[actix_web::test]
async fn workbench_batch_reads_selected_issues_before_dispatch() {
    desk_diagnose_core::future::boxed(|| queued_owner_input_roundtrip(true, false, true)).await;
}

async fn queued_owner_input_roundtrip(workbench: bool, delete_workbench: bool, with_issues: bool) {
    use sea_orm::{ColumnTrait, QueryFilter};
    crate::ai_assistant_gate::enable_test_host();
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("queued.db").display()
    );
    let connect = || crate::config::test_support::Database::connect(&url);
    let db = connect().await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let issue_fixture = if with_issues {
        Some(desk_signal_facade::mcp_client::test_support::IssueFixture::start().await)
    } else {
        None
    };
    let issue_binding = if let Some(fixture) = &issue_fixture {
        crate::issue_connections::update(&db, &fixture.update("1"))
            .await
            .unwrap();
        Some(
            crate::issue_connections::select_issues(&db, "1", &[fixture.selection()])
                .await
                .unwrap()[0]
                .clone(),
        )
    } else {
        None
    };
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("queue-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let ask = AiAssistantAsk {
        question: "Resume the queued work".into(),
        client_message_id: "durable-queued-message".into(),
        conversation_id: Some("durable-queued-conversation".into()),
        client_timezone: "UTC".into(),
        ..Default::default()
    };
    if workbench {
        use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
        use sea_orm::{ActiveModelTrait, Set};
        let actor = owner.to_string();
        let continuation = ask.conversation_id.as_deref().unwrap();
        let root = derive_assistant_conversation_key(
            &actor,
            AssistantConversationScope::Personal,
            continuation,
        );
        let now = chrono::Utc::now();
        let mut session = PersistedAgentSession::new_workbench_coordinator(
            &actor,
            &root,
            None,
            PERSONAL_ASSISTANT_POLICY_REVISION,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            &now.to_rfc3339(),
        )
        .unwrap();
        session.adopt_client_metadata(Some(continuation), AgentSessionSurface::AiAssistant);
        crate::entity::agent_session::ActiveModel {
            conversation_id: Set(root),
            actor_id: Set(actor),
            device_id: Set(String::new()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            conversation_schema_version: Set(Some(i32::from(
                desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
            ))),
            version: Set(session.version),
            lease_token: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }
    submit_owner_turn(
        web::Data::new(SharedConnectionMap::new_oss()),
        db.clone(),
        "submission-frame".into(),
        "closed-browser".into(),
        String::new(),
        owner,
        String::new(),
        ask.clone(),
    )
    .await;
    assert_eq!(
        crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .is_some(),
        workbench,
    );
    let [row]: [_; 1] = main_request::recovery_heads(&db, 0, 2)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(row.client_message_id, ask.client_message_id);
    drop(db);

    let db = connect().await.unwrap();
    let gate = std::sync::Arc::new(crate::ai_assistant_gate::AiAssistantGate::new(
        desk_agent_protocol::ai_assistant::AiAssistantSettings {
            revision: 1,
            enabled: true,
        },
    ));
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let mut peers = Vec::new();
    if workbench {
        use desk_signal_facade::model::os::OperationSystemEnum;
        for (id, platform) in [
            ("queue-selected-mac", OperationSystemEnum::Mac),
            ("queue-unselected-linux", OperationSystemEnum::Linux),
        ] {
            crate::device_code_store::get_or_create(&db, id)
                .await
                .unwrap();
            peers.push(register_request_device(&connections, id, id, platform).await);
        }
        desk_assistant_data_store::device_roster::replace(
            &db,
            &row.actor_id,
            &row.conversation_id,
            0,
            vec!["queue-selected-mac".into(), "queue-unselected-linux".into()],
        )
        .await
        .unwrap();
    }
    let executor = main_requests::SignalMainRequestExecutor::new(db.clone(), connections, gate);
    let answer_gateway = listener.clone();
    let answer = actix_web::rt::spawn(async move {
        let sse = memory_sse(serde_json::json!({"content":"Resumed by the host"}), "stop");
        if workbench {
            let spawn = report_test_call(
                "spawn-workbench-worker",
                "send_subagent_request",
                serde_json::json!({
                    "name":"diagnose",
                    "task":"Investigate the original work",
                    "acceptance_criteria":["Return the result"],
                    "required_for_completion":true
                }),
            );
            let first = capture_one_openai_request_with_sse(
                answer_gateway.as_ref(),
                &memory_sse(serde_json::json!({"tool_calls":[spawn]}), "tool_calls"),
            )
            .await;
            let (_, title) = capture_openai_answer_with_title(answer_gateway.as_ref(), &sse).await;
            (first, title)
        } else {
            capture_openai_answer_with_title(answer_gateway.as_ref(), &sse).await
        }
    });
    tokio::time::timeout(SCENARIO_TIMEOUT, executor.scan_once(0))
        .await
        .unwrap()
        .unwrap();
    let (body, _) = tokio::time::timeout(MODEL_IO_TIMEOUT, answer)
        .await
        .unwrap()
        .unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains(&ask.question));
    assert_eq!(body.contains("WORKBENCH COORDINATION"), workbench);
    let execution = |request_id: &str| {
        if workbench {
            desk_diagnose_core::native_ids::workbench_work_conversation(
                &row.actor_id,
                &row.conversation_id,
                request_id,
            )
        } else {
            row.conversation_id.clone()
        }
    };
    let snapshot = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&execution(&row.request_id), &row.actor_id, "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.input_revision, 1);
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("Resumed by the host")
    );
    assert!(
        main_request::recovery_heads(&db, 0, 2)
            .await
            .unwrap()
            .is_empty()
    );
    let native_state = || async {
        let row = crate::entity::agent_session::Entity::find()
            .filter(crate::entity::agent_session::Column::ConversationId.eq(&row.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        desk_diagnose_core::session::PersistedAgentSession::decode_boxed_json(&row.state_json)
            .unwrap()
    };
    let before = native_state().await;
    let mut pending_inputs = vec![
        ("queued-b", "Coordinate request B"),
        ("queued-c", "Coordinate request C"),
    ];
    if workbench {
        pending_inputs.push(("queued-d", "Coordinate request D"));
    }
    for (id, question) in pending_inputs {
        submit_owner_turn(
            web::Data::new(SharedConnectionMap::new_oss()),
            db.clone(),
            id.to_owned(),
            "closed-browser".into(),
            String::new(),
            owner,
            String::new(),
            AiAssistantAsk {
                target_refs: if workbench && id == "queued-b" {
                    vec!["queue-selected-mac".into()]
                } else {
                    vec![]
                },
                selected_issues: issue_fixture
                    .iter()
                    .filter(|_| id == "queued-b")
                    .map(|fixture| fixture.selection())
                    .collect(),
                question: question.into(),
                client_message_id: id.into(),
                ..ask.clone()
            },
        )
        .await;
    }
    let b = main_request::first_pending(&db, &row.actor_id, &row.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let c = crate::entity::agent_main_request::Entity::find()
        .filter(crate::entity::agent_main_request::Column::ClientMessageId.eq("queued-c"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let d = if workbench {
        Some(
            crate::entity::agent_main_request::Entity::find()
                .filter(crate::entity::agent_main_request::Column::ClientMessageId.eq("queued-d"))
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
        )
    } else {
        None
    };
    let wake = if workbench {
        let work = execution(&row.request_id);
        Some(
            desk_diagnose_core::future::boxed(|| {
                prepare_workbench_mixed_reports(&db, &listener, &work, &row.actor_id)
            })
            .await,
        )
    } else {
        None
    };
    let pending_delivery = if let Some(fixture) = &issue_fixture {
        let destination = desk_assistant_data_store::main_request::FrozenOwnerInput::decode(&b)
            .unwrap()
            .destination;
        let (admitted, artifact) = fixture
            .publication_for(&row.actor_id, &row.conversation_id, destination)
            .await;
        desk_assistant_data_store::issue_authority::put(&db, admitted.subscription.clone(), 0, 0)
            .await
            .unwrap();
        desk_assistant_data_store::issue_authority::admit(&db, &admitted)
            .await
            .unwrap();
        let saved = desk_assistant_data_store::issue_reply::freeze(
            &db,
            &artifact,
            desk_assistant_data_store::issue_reply::DeliveryMode::Automatic,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
        fixture.enable_replies(false);
        fixture.pause_next_reply();
        let delivery_db = db.clone();
        let delivery = actix_web::rt::spawn(async move {
            crate::issue_replies::deliver_next(&delivery_db, "node-delivery", "pending-a").await
        });
        fixture.wait_for_read().await;
        assert_eq!(fixture.writes(), 1);
        Some((saved, delivery))
    } else {
        None
    };
    let mut decisions = vec![
        serde_json::json!({"request_id":b.request_id,"route":"native"}),
        if workbench {
            serde_json::json!({"request_id":c.request_id,"route":"worker","dispatch":{}})
        } else {
            serde_json::json!({"request_id":c.request_id,"route":"native"})
        },
    ];
    if let Some((_, reports)) = &wake {
        decisions.push(serde_json::json!({"request_id":reports[1].event_id,"route":"native"}));
    }
    if let Some(d) = &d {
        decisions.push(serde_json::json!({"request_id":d.request_id,"route":"native"}));
    }
    let read_report = wake.as_ref().map(|(_, reports)| {
        let report: desk_diagnose_core::subagent::report::StoredReport =
            serde_json::from_str(&reports[1].event_json).unwrap();
        (
            report.report.report_id,
            report.report.content.evidence_refs[0].clone(),
        )
    });
    let pending_request_id = b.request_id.clone();
    let reply = serde_json::json!({"decisions":decisions}).to_string();
    let batch = actix_web::rt::spawn({
        let listener = listener.clone();
        async move {
            let decision = memory_sse(serde_json::json!({"content":reply}), "stop");
            let Some((report_id, evidence_ref)) = read_report else {
                return capture_one_openai_request_with_sse(listener.as_ref(), &decision).await;
            };
            let read = report_test_call(
                "read-in-batch",
                "read_coordination_report",
                serde_json::json!({"report_id":report_id}),
            );
            let mut foreign = report_test_call(
                "read-outside-batch",
                "read_coordination_report",
                serde_json::json!({"report_id":"unknown-report"}),
            );
            foreign["index"] = 1.into();
            let mut write = report_test_call(
                "forged-write",
                "exec_command",
                serde_json::json!({"command":"unapproved command"}),
            );
            write["index"] = 2.into();
            let mut history = report_test_call(
                "read-request-history",
                "read_coordination_report",
                serde_json::json!({"report_id":report_id,"history_after_seq":0,"limit":1}),
            );
            history["index"] = 3.into();
            let mut evidence = report_test_call(
                "read-exact-evidence",
                "read_coordination_report",
                serde_json::json!({"report_id":report_id,"evidence_ref":evidence_ref}),
            );
            evidence["index"] = 4.into();
            let mut uncited = report_test_call(
                "read-uncited-evidence",
                "read_coordination_report",
                serde_json::json!({"report_id":report_id,"evidence_ref":"foreign-evidence"}),
            );
            uncited["index"] = 5.into();
            let mut pending = report_test_call(
                "read-pending-devices",
                "read_coordination_request",
                serde_json::json!({"request_id":pending_request_id}),
            );
            pending["index"] = 6.into();
            let mut foreign_request = report_test_call(
                "read-foreign-request",
                "read_coordination_request",
                serde_json::json!({"request_id":"unleased-owner-request"}),
            );
            foreign_request["index"] = 7.into();
            if let Some(issue) = &issue_binding {
                write = report_test_call(
                    "batch-issue-read",
                    "call_external_tool",
                    serde_json::json!({"request_id":pending_request_id,"tool_ref":issue.tool_ref(),"args":{"method":"get"}}),
                );
                write["index"] = 2.into();
                uncited = report_test_call(
                    "batch-forged-issue-write",
                    "call_external_tool",
                    serde_json::json!({"request_id":pending_request_id,"tool_ref":issue.tool_ref(),"args":{"method":"add_comment"}}),
                );
                uncited["index"] = 5.into();
            }
            let original = capture_one_openai_request_with_sse(
                listener.as_ref(),
                &memory_sse(
                    serde_json::json!({"tool_calls":[read, foreign, write, history, evidence, uncited, pending, foreign_request]}),
                    "tool_calls",
                ),
            )
            .await;
            let decided = capture_one_openai_request_with_sse(listener.as_ref(), &decision).await;
            let sent: serde_json::Value = serde_json::from_slice(&decided).unwrap();
            assert!(sent["tools"].as_array().is_none_or(Vec::is_empty));
            let results: Vec<_> = sent["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "tool")
                .map(|message| {
                    let result: serde_json::Value =
                        serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
                    result
                })
                .collect();
            assert_eq!(results.len(), 8);
            assert_eq!(
                results[0]["report"]["content"]["question"],
                "Which version should I use?"
            );
            assert_eq!(results[1], serde_json::json!({"status":"unavailable"}));
            if issue_binding.is_some() {
                assert!(
                    results[2]
                        .to_string()
                        .contains("Reproduce the macOS problem")
                );
            } else {
                assert_eq!(results[2], results[1]);
            }
            assert_eq!(results[0]["worker"]["state"], "waiting_input");
            assert_eq!(results[0]["worker"]["wait_reason"], "main_agent_reply");
            assert_eq!(results[3]["request_id"], results[0]["request_id"]);
            assert_eq!(results[3]["worker"], results[0]["worker"]);
            let page = &results[3]["history"];
            assert_eq!(page["reports"].as_array().unwrap().len(), 1);
            assert_eq!(
                page["reports"][0]["content"]["summary"],
                "Checked the first part"
            );
            assert_eq!(page["next_after_seq"], 1);
            assert_eq!(page["has_more"], true);
            assert_eq!(results[4]["evidence"]["role"], "tool");
            let progress_receipt: serde_json::Value =
                serde_json::from_str(results[4]["evidence"]["text"].as_str().unwrap()).unwrap();
            assert_eq!(progress_receipt["seq"], 1);
            assert_eq!(results[5]["status"], "unavailable");
            assert!(results[5].get("evidence").is_none());
            assert_eq!(results[6]["request_id"], pending_request_id);
            assert_eq!(results[6]["devices"].as_array().unwrap().len(), 1);
            assert_eq!(results[6]["devices"][0]["target_ref"], "queue-selected-mac");
            assert_eq!(results[6]["devices"][0]["platform"], "macOS");
            assert_eq!(results[6]["devices"][0]["online"], true);
            assert_eq!(results[7], serde_json::json!({"status":"unavailable"}));
            original
        }
    });
    tokio::time::timeout(SCENARIO_TIMEOUT, executor.scan_once(0))
        .await
        .unwrap()
        .unwrap();
    let captured = tokio::time::timeout(MODEL_IO_TIMEOUT, batch)
        .await
        .unwrap()
        .unwrap();
    if let Some((saved, delivery)) = pending_delivery {
        let live =
            desk_assistant_data_store::issue_reply::load(&db, &row.actor_id, &saved.artifact_id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            live.state,
            desk_assistant_data_store::issue_reply::DISPATCHED
        );
        for pending in [&b, &c] {
            let routed = desk_assistant_data_store::main_request::load(
                &db,
                &row.actor_id,
                &row.conversation_id,
                &pending.request_id,
            )
            .await
            .unwrap()
            .unwrap();
            assert!(routed.coordinated || routed.first_dispatch_json.is_some());
        }
        let before_release = crate::entity::agent_session::Entity::find()
            .filter(crate::entity::agent_session::Column::ConversationId.eq(&row.conversation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        issue_fixture.as_ref().unwrap().release_read();
        assert!(delivery.await.unwrap().unwrap());
        let completed =
            desk_assistant_data_store::issue_reply::load(&db, &row.actor_id, &saved.artifact_id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            completed.state,
            desk_assistant_data_store::issue_reply::DELIVERED
        );
        assert_eq!(
            crate::entity::agent_session::Entity::find_by_id(before_release.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            before_release
        );
    }
    let batch_body: serde_json::Value = serde_json::from_slice(&captured).unwrap();
    let projected = batch_body["messages"].to_string();
    assert!(projected.contains("Coordinate request B"));
    assert!(projected.contains("Coordinate request C"));
    assert!(!projected.contains(&ask.question));
    if let Some((task, reports)) = &wake {
        let tools = batch_body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), if with_issues { 6 } else { 5 });
        assert_eq!(tools[0]["function"]["name"], "read_coordination_report");
        assert_eq!(tools[1]["function"]["name"], "read_coordination_request");
        assert_eq!(tools[2]["function"]["name"], "load_skill");
        assert_eq!(tools[3]["function"]["name"], "list_subagents");
        assert_eq!(tools[4]["function"]["name"], "get_subagent_status");
        if with_issues {
            assert_eq!(tools[5]["function"]["name"], "call_external_tool");
        }
        assert!(projected.contains(&reports[0].event_id));
        assert!(projected.contains(&reports[1].event_id));
        assert!(!projected.contains("Which version should I use?"));
        let routed = workbench_report_rows(&db, &task.task_id).await;
        assert_eq!(routed[0].coordination_attempts, Some(1));
        assert!(routed[0].coordinated_at_ms.is_none());
        assert!(routed[0].coordination_retry_at_ms.is_some());
        assert!(routed[1].coordinated_at_ms.is_some());
        assert!(
            routed
                .iter()
                .all(|report| report.interpreted_at_ms.is_none())
        );
    } else {
        assert!(batch_body["tools"].as_array().is_none_or(Vec::is_empty));
    }
    let decided = native_state().await;
    assert_eq!(decided.input_revision, before.input_revision);
    assert_eq!(decided.focus_epoch, before.focus_epoch);
    assert_eq!(decided.conversation, before.conversation);
    assert!(decided.main_coordination.is_none());
    for pending in [&b, &c] {
        let saved = main_request::load(
            &db,
            &row.actor_id,
            &row.conversation_id,
            &pending.request_id,
        )
        .await
        .unwrap()
        .unwrap();
        if workbench && pending.id == c.id {
            assert!(!saved.coordinated);
            assert!(saved.first_dispatch_json.is_some());
            assert!(saved.first_worker_request_id.is_none());
        } else {
            assert!(saved.coordinated);
        }
    }
    let activate_without_claim = |row: crate::entity::agent_main_request::Model| {
        let db = &db;
        let execution = execution(&row.request_id);
        async move {
            let frozen = main_request::FrozenOwnerInput::decode(&row).unwrap();
            let providers = ai_assistant_provider_registry();
            let scope = AgentScope {
                granted: frozen
                    .reads
                    .tool_names
                    .iter()
                    .map(|name| {
                        providers
                            .capability_for_tool(name)
                            .unwrap()
                            .required_capability
                    })
                    .collect(),
                mode: ExecutionMode::ReadOnly,
                expires_at: frozen.reads.expires_at.clone(),
                policy_name: None,
            };
            let message = desk_diagnose_core::model_message_labels::model_bound_user_message(
                frozen.ask.client_message_id.clone(),
                frozen.ask.question.clone(),
                frozen.destination,
            )
            .unwrap();
            let input = crate::agent_run_event_store::AppendUserFollowupParams {
                client_timezone: frozen.ask.client_timezone,
                temporary_conversation: None,
                event_id: frozen.ask.client_message_id,
                run_id: execution,
                client_conversation_id: frozen.ask.conversation_id,
                actor_id: row.actor_id,
                device_id: String::new(),
                target_refs: frozen.ask.target_refs,
                surface: AgentSessionSurface::AiAssistant,
                policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
                current_scope: scope,
                read_context: Some(frozen.reads),
                message,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            SignalAgentRunEventStore::new(db.clone())
                .admit_user_input(
                    &input,
                    None,
                    OwnerAdmission::Activate {
                        request_id: &row.request_id,
                    },
                )
                .await
                .unwrap()
        }
    };
    if workbench {
        let ack = activate_without_claim(
            main_request::first_pending(&db, &row.actor_id, &row.conversation_id)
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .native()
        .unwrap();
        assert!(ack.newly_appended);
    }
    let next_gateway = listener.clone();
    let worker = wake.as_ref().map(|(task, _)| task.worker_id.clone());
    if workbench {
        use desk_agent_protocol::ai_assistant::subagent_policy::{
            SubAgentLimits, UpdateSubAgentPolicy,
        };
        let policy = crate::subagent_policy::read(&db).await.unwrap();
        crate::subagent_policy::update(
            &db,
            &UpdateSubAgentPolicy {
                expected_revision: policy.revision,
                limits: SubAgentLimits {
                    max_unfinished_per_root: 1,
                    ..policy.limits
                },
            },
        )
        .await
        .unwrap();
    }
    let gateway = actix_web::rt::spawn(async move {
        let listener = next_gateway;
        let reply = memory_sse(serde_json::json!({"content":"Request B handled"}), "stop");
        if let Some(worker) = worker {
            let dispatch = report_test_call(
                "reuse-workbench-worker",
                "send_subagent_request",
                serde_json::json!({
                    "worker_id":worker,
                    "name":"diagnose",
                    "task":"Investigate the independent second work",
                    "acceptance_criteria":["Return the second result"],
                    "required_for_completion":false
                }),
            );
            let mut independent = report_test_call(
                "dispatch-independent-workbench-worker",
                "send_subagent_request",
                serde_json::json!({
                    "name":"verify",
                    "task":"Verify one independent detail",
                    "acceptance_criteria":["Return a short finding"],
                    "required_for_completion":false
                }),
            );
            independent["index"] = 1.into();
            let first = capture_one_openai_request_with_sse(
                listener.as_ref(),
                &memory_sse(
                    serde_json::json!({"tool_calls":[dispatch, independent]}),
                    "tool_calls",
                ),
            )
            .await;
            capture_openai_answer_with_title(listener.as_ref(), &reply).await;
            first
        } else {
            capture_one_openai_request_with_sse(listener.as_ref(), &reply).await
        }
    });
    tokio::time::timeout(SCENARIO_TIMEOUT, executor.scan_once(0))
        .await
        .unwrap()
        .unwrap();
    let native = tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    let native = String::from_utf8(native).unwrap();
    assert!(native.contains("Coordinate request B"));
    assert!(!native.contains("Coordinate request C"));
    assert_eq!(
        native_state().await.input_revision,
        if workbench { 0 } else { 2 }
    );
    assert_eq!(
        main_request::recovery_heads(&db, 0, 2).await.unwrap()[0].request_id,
        c.request_id
    );
    if workbench {
        let second = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .read_snapshot_for_subject(&execution(&b.request_id), &row.actor_id, "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.input_revision, 1);
        assert_eq!(
            latest_committed_answer(&second).as_deref(),
            Some("Request B handled")
        );
        assert_eq!(
            native_state().await.coordinating_work.as_deref(),
            Some(b.request_id.as_str())
        );
    }
    let followup_for_history = if let Some((task, _)) = wake {
        use crate::agent_subagent_store::SubAgentStore;
        use crate::entity::agent_subagent_run as run;
        assert!(native.contains(&task.worker_id));
        assert!(!native.contains("Need the version"));
        let followup = run::Entity::find()
            .filter(run::Column::RootConversationId.eq(execution(&b.request_id)))
            .filter(run::Column::WorkerId.eq(&task.worker_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(followup.worker_id, task.worker_id);
        assert_eq!(
            followup.workbench_id.as_deref(),
            Some(row.conversation_id.as_str())
        );
        assert_ne!(followup.child_conversation_id, task.child_conversation_id);
        assert_ne!(followup.group_id, task.group_id);
        let store = SubAgentStore::new(db.clone());
        let independent = run::Entity::find()
            .filter(run::Column::RootConversationId.eq(execution(&b.request_id)))
            .filter(run::Column::WorkerId.ne(&task.worker_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        desk_diagnose_core::future::boxed(|| {
            verify_workbench_worker_slot(&db, &listener, &independent, false)
        })
        .await;
        assert!(
            store
                .child_runtime_candidate(&followup.task_id)
                .await
                .unwrap()
                .is_none()
        );
        let work = execution(&row.request_id);
        desk_diagnose_core::future::boxed(|| {
            exercise_workbench_report_wake(&db, &listener, &work, &row.actor_id, task)
        })
        .await;
        let ready = store
            .child_runtime_candidate(&followup.task_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ready.source().owner_requirement.text,
            "Coordinate request B"
        );
        desk_diagnose_core::future::boxed(|| {
            verify_workbench_worker_slot(&db, &listener, &independent, true)
        })
        .await;
        Some(ready)
    } else {
        None
    };
    let c_work = execution(&c.request_id);
    let destination_change = if let Some(d) = d {
        tokio::time::timeout(MODEL_IO_TIMEOUT, executor.scan_once(0))
            .await
            .unwrap()
            .unwrap();
        let committed = main_request::load(&db, &row.actor_id, &row.conversation_id, &c.request_id)
            .await
            .unwrap()
            .unwrap();
        assert!(committed.coordinated && committed.pending_input_json.is_none());
        assert!(committed.first_dispatch_json.is_none());
        assert!(matches!(
            activate_without_claim(c.clone()).await,
            OwnerAdmissionResult::Dispatched
        ));
        let work = execution(&c.request_id);
        let [child]: [_; 1] = crate::entity::agent_subagent_run::Entity::find()
            .filter(crate::entity::agent_subagent_run::Column::RootConversationId.eq(&work))
            .all(&db)
            .await
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(
            committed.first_worker_request_id.as_deref(),
            Some(child.task_id.as_str())
        );
        let context: desk_diagnose_core::subagent::creation::TaskCreationEnvelope =
            serde_json::from_str(&child.creation_envelope_json).unwrap();
        assert_eq!(
            context.source.owner_requirement.text,
            "Coordinate request C"
        );
        assert!(context.instruction.text.contains("Coordinate request C"));
        assert!(!context.instruction.text.contains("Coordinate request B"));
        assert!(
            !context
                .instruction
                .text
                .contains("Which version should I use?")
        );
        desk_diagnose_core::future::boxed(|| {
            verify_workbench_worker_slot(&db, &listener, &child, true)
        })
        .await;
        let original = crate::agent_subagent_store::SubAgentStore::new(db.clone())
            .parent_runtime_candidate(&work, &row.actor_id, "")
            .await
            .unwrap()
            .unwrap();
        assert!(original.session().has_workbench_report_notification());
        assert_eq!(
            original.source().owner_requirement.text,
            "Coordinate request C"
        );
        let ack = activate_without_claim(d.clone()).await.native().unwrap();
        assert!(ack.newly_appended);
        d
    } else {
        c
    };
    if let Some(ready) = followup_for_history {
        let gate = crate::ai_assistant_gate::global_ai_assistant_gate();
        let (outcome, _) = tokio::time::timeout(SCENARIO_TIMEOUT, async {
            tokio::join!(
                desk_diagnose_core::future::boxed(|| resume_subagent_turn(
                    web::Data::new(SharedConnectionMap::new_oss()),
                    db.clone(),
                    &gate,
                    ready,
                )),
                capture_one_openai_request_with_reply(listener.as_ref(), |body| {
                    let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
                    let messages = sent["messages"].to_string();
                    assert!(messages.contains("previous_worker_report"));
                    assert!(messages.contains("Validated version 15"));
                    assert!(messages.contains("Investigate the independent second work"));
                    assert!(!messages.contains("Coordinate request C"));
                    memory_sse(
                        serde_json::json!({"tool_calls":[report_test_call(
                            "followup-question",
                            "report_subagent_status",
                            serde_json::json!({"kind":"needs_input","summary":"Confirm the next environment",
                                "question":"Which environment should I use?"})
                        )]}),
                        "tool_calls",
                    )
                }),
            )
        })
        .await
        .unwrap();
        assert!(matches!(outcome.unwrap(), LoopOutcome::SubAgentYielded));
    }
    if workbench {
        use crate::{agent_subagent_store::SubAgentStore, entity::agent_subagent_run as run};
        use desk_agent_protocol::ai_assistant::subagent::{
            AiAssistantStopControl, SubAgentStopChoice,
        };
        let child = run::Entity::find()
            .filter(run::Column::RootConversationId.eq(execution(&b.request_id)))
            .filter(run::Column::State.eq("waiting_input"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let store = SubAgentStore::new(db.clone());
        if delete_workbench {
            let native_works = [
                execution(&row.request_id),
                execution(&b.request_id),
                c_work,
                execution(&destination_change.request_id),
            ];
            desk_diagnose_core::future::boxed(|| {
                verify_workbench_deletion(
                    &db,
                    &row,
                    &child,
                    &native_works,
                    &destination_change.request_id,
                )
            })
            .await;
            executor.scan_once(0).await.unwrap();
            return;
        }
        let mut control = AiAssistantStopControl {
            client_request_id: "workbench-stop-main".into(),
            expected_input_revision: 0,
            expected_control_revision: native_state().await.control_revision,
            subagent_choice: None,
        };
        assert!(
            store
                .stop_for_owner(&row.conversation_id, &row.actor_id, "", &control)
                .await
                .is_err()
        );
        control.subagent_choice = Some(SubAgentStopChoice::MainOnly);
        let stopped = store
            .stop_for_owner(&row.conversation_id, &row.actor_id, "", &control)
            .await
            .unwrap();
        assert!(stopped.result.stopped_subagents.is_empty());
        assert!(
            run::Entity::find_by_id(child.id)
                .one(&db)
                .await
                .unwrap()
                .is_some_and(|row| row.state == "waiting_input")
        );
        assert_eq!(
            store
                .stop_for_owner(&row.conversation_id, &row.actor_id, "", &control)
                .await
                .unwrap(),
            stopped
        );
        let before = main_request::load(
            &db,
            &row.actor_id,
            &row.conversation_id,
            &destination_change.request_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(before.pending_input_json.is_some());
        executor.scan_once(0).await.unwrap();
        assert_eq!(
            main_request::load(
                &db,
                &row.actor_id,
                &row.conversation_id,
                &destination_change.request_id
            )
            .await
            .unwrap()
            .unwrap(),
            before
        );
        let all = store
            .stop_for_owner(
                &row.conversation_id,
                &row.actor_id,
                "",
                &AiAssistantStopControl {
                    client_request_id: "workbench-stop-workers".into(),
                    expected_input_revision: 0,
                    expected_control_revision: stopped.result.control_revision,
                    subagent_choice: Some(SubAgentStopChoice::IncludeSubAgents),
                },
            )
            .await
            .unwrap();
        assert_eq!(all.result.stopped_subagents.len(), 1);
        assert_eq!(all.result.stopped_subagents[0].task_id, child.task_id);
        assert!(
            store
                .child_runtime_candidate(&child.task_id)
                .await
                .unwrap()
                .is_none()
        );
        store
            .resume_main_queue_for_owner(
                &row.conversation_id,
                &row.actor_id,
                "",
                all.result.control_revision,
            )
            .await
            .unwrap();
    }
    let mut changed_model = crate::model_provider::load(&db).await.unwrap();
    changed_model.model = Some("different-destination".into());
    crate::model_provider::save(&db, changed_model)
        .await
        .unwrap();
    executor.scan_once(0).await.unwrap();
    let receipt = main_request::receipts(
        &db,
        &row.actor_id,
        &row.conversation_id,
        Some(&destination_change.request_id),
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    assert_eq!(
        receipt.state,
        desk_agent_protocol::ai_assistant::MainRequestState::Failed
    );
    assert_eq!(
        receipt.failure,
        Some(desk_agent_protocol::ai_assistant::MainRequestFailure::DestinationChanged)
    );
    if workbench {
        let closed = crate::entity::agent_session::Entity::find()
            .filter(
                crate::entity::agent_session::Column::ConversationId
                    .eq(execution(&destination_change.request_id)),
            )
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let closed =
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&closed.state_json)
                .unwrap();
        assert_eq!(
            closed.turn_state,
            desk_diagnose_core::session::TurnState::Failed
        );
    }
    assert_eq!(
        native_state().await.input_revision,
        if workbench { 0 } else { 2 }
    );
    if let Some(fixture) = issue_fixture {
        assert_eq!(fixture.writes(), 1);
        fixture.stop().await;
    }
}

async fn verify_workbench_deletion(
    db: &DatabaseConnection,
    root: &desk_assistant_data_store::entity::agent_main_request::Model,
    worker: &crate::entity::agent_subagent_run::Model,
    native_works: &[String],
    unclaimed_request: &str,
) {
    use crate::agent_session_store::SignalAgentSessionStore;
    use crate::agent_subagent_store::{
        SubAgentStore, assert_stop_only, reclaim_condition, seed_native_stop_test,
    };
    use crate::entity::{
        agent_delegation_group as group, agent_exec_task as command,
        agent_file_recovery_cleanup as removed, agent_session as session,
        agent_subagent_inbox as inbox, agent_subagent_run as run,
    };
    use sea_orm::{ColumnTrait, QueryFilter};
    let now = chrono::Utc::now();
    let action =
        seed_native_stop_test(db, &worker.child_conversation_id, "workbench-original").await;
    let [head]: [_; 1] = desk_assistant_data_store::main_request::recovery_heads(db, 0, 8)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(head.conversation_id, root.conversation_id);
    assert_eq!(head.request_id, unclaimed_request);
    assert!(head.input_revision.is_some() && head.pending_input_json.is_some());
    assert!(
        session::Entity::find()
            .filter(session::Column::ConversationId.eq(&root.conversation_id))
            .filter(reclaim_condition(now.timestamp_millis()))
            .one(db)
            .await
            .unwrap()
            .is_none(),
        "live native work holds the coordinator even without the queue filter"
    );

    let session_store = SignalAgentSessionStore::new(db.clone());
    assert!(
        session_store
            .delete_for_subject(&root.conversation_id, "2", "")
            .await
            .is_err()
    );
    session_store
        .delete_for_subject(&root.conversation_id, &root.actor_id, "")
        .await
        .unwrap();
    assert!(
        desk_assistant_data_store::main_request::receipts(
            db,
            &root.actor_id,
            &root.conversation_id,
            None
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        desk_assistant_data_store::device_roster::load(db, &root.actor_id, &root.conversation_id)
            .await
            .unwrap()
            .is_none()
    );
    for conversation in std::iter::once(&root.conversation_id).chain(native_works.iter()) {
        assert!(
            session::Entity::find()
                .filter(session::Column::ConversationId.eq(conversation))
                .one(db)
                .await
                .unwrap()
                .is_none()
        );
        let marker = removed::Entity::find_by_id(conversation)
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert!(marker.history_deleted_at_unix_ms.is_some());
    }

    let group = group::Entity::find()
        .filter(group::Column::GroupId.eq(&worker.group_id))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(group.source_admission, "closed");
    let cancelled = run::Entity::find_by_id(worker.id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.state, "cancelled");
    assert_eq!(
        cancelled.child_conversation_id,
        worker.child_conversation_id
    );
    assert_eq!(cancelled.input_revision, worker.input_revision);
    let workers = SubAgentStore::new(db.clone());
    assert!(
        workers
            .queued_task_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        workers
            .child_runtime_candidate(&worker.task_id)
            .await
            .is_err()
    );
    let settled = command::Entity::find_by_id(action.id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_stop_only(&action, &settled);
    let original_reports = inbox::Entity::find()
        .filter(inbox::Column::TaskId.eq(&worker.task_id))
        .filter(inbox::Column::ReportSeq.is_not_null())
        .all(db)
        .await
        .unwrap();
    assert_eq!(original_reports.len(), 1);
    assert_eq!(original_reports[0].event_kind, "report_needs_input");
    let child_removal = removed::Entity::find_by_id(&worker.child_conversation_id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_removal.actor_id, root.actor_id);
}

async fn verify_workbench_worker_slot(
    db: &DatabaseConnection,
    listener: &TcpListener,
    queued: &crate::entity::agent_subagent_run::Model,
    expect_claim: bool,
) {
    use crate::agent_subagent_store::SubAgentStore;
    use crate::entity::agent_subagent_run as run;
    let store = SubAgentStore::new(db.clone());
    let runtime = store
        .child_runtime_candidate(&queued.task_id)
        .await
        .unwrap()
        .unwrap();
    let gate = crate::ai_assistant_gate::global_ai_assistant_gate();
    let turn = desk_diagnose_core::future::boxed(|| {
        resume_subagent_turn(
            web::Data::new(SharedConnectionMap::new_oss()),
            db.clone(),
            &gate,
            runtime,
        )
    });
    if expect_claim {
        let (outcome, _) = tokio::time::timeout(SCENARIO_TIMEOUT, async {
            tokio::join!(
                turn,
                capture_one_openai_request_with_reply(listener, |_| {
                    memory_sse(
                        serde_json::json!({"content":"Independent check finished"}),
                        "stop",
                    )
                })
            )
        })
        .await
        .unwrap();
        assert!(matches!(outcome.unwrap(), LoopOutcome::Answered(_)));
    } else {
        assert!(matches!(
            tokio::time::timeout(MODEL_IO_TIMEOUT, turn)
                .await
                .unwrap()
                .unwrap(),
            LoopOutcome::TurnBusy
        ));
    }
    let actual = run::Entity::find_by_id(queued.id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    if expect_claim {
        assert_eq!(actual.state, "completed");
        assert!(actual.workbench_claimed_at_ms.is_some());
    } else {
        assert!(queued.workbench_claimed_at_ms.is_none());
        assert_eq!(&actual, queued);
    }
}

fn report_test_call(id: &str, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"index":0,"id":id,"type":"function","function":{
        "name":name,"arguments":arguments.to_string()
    }})
}

async fn prepare_workbench_mixed_reports(
    db: &DatabaseConnection,
    listener: &TcpListener,
    work: &str,
    actor: &str,
) -> (
    crate::entity::agent_subagent_run::Model,
    Vec<crate::entity::agent_subagent_inbox::Model>,
) {
    use crate::agent_subagent_store::SubAgentStore;
    use crate::entity::{agent_session, agent_subagent_run as run};
    use desk_diagnose_core::{session::PersistedAgentSession, subagent::report::ReportKind};
    use sea_orm::{ColumnTrait, QueryFilter};
    use serde_json::json;

    let store = SubAgentStore::new(db.clone());
    let task = run::Entity::find()
        .filter(run::Column::RootConversationId.eq(work))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let gate = crate::ai_assistant_gate::global_ai_assistant_gate();
    assert!(
        store
            .parent_runtime_candidate(work, actor, "")
            .await
            .unwrap()
            .is_none()
    );
    let worker = store
        .child_runtime_candidate(&task.task_id)
        .await
        .unwrap()
        .unwrap();
    let (outcome, ()) = tokio::time::timeout(SCENARIO_TIMEOUT, async {
        tokio::join!(
            resume_subagent_turn(connections.clone(), db.clone(), &gate, worker),
            async {
                let progress = memory_sse(
                    json!({"tool_calls":[report_test_call(
                        "progress", "report_subagent_status",
                        json!({"kind":"progress","summary":"Checked the first part"}),
                    )]}),
                    "tool_calls",
                );
                capture_one_openai_request_with_sse(listener, &progress).await;
                let (arrived, captured) = tokio::sync::oneshot::channel();
                let (release, paused) = tokio::sync::oneshot::channel();
                let evidence = std::cell::OnceCell::<String>::new();
                tokio::join!(
                    capture_one_openai_request_with_reply_paused(
                        listener,
                        |_| memory_sse(
                            json!({"tool_calls":[report_test_call(
                                "question", "report_subagent_status",
                                json!({"kind":"needs_input","summary":"Need the version",
                                    "question":"Which version should I use?",
                                    "evidence_refs":[evidence.get().unwrap()]}),
                            )]}),
                            "tool_calls",
                        ),
                        Some((arrived, paused)),
                    ),
                    async {
                        captured.await.unwrap();
                        let child = agent_session::Entity::find()
                            .filter(
                                agent_session::Column::ConversationId
                                    .eq(&task.child_conversation_id),
                            )
                            .one(db)
                            .await
                            .unwrap()
                            .unwrap();
                        let child = PersistedAgentSession::decode_json(&child.state_json).unwrap();
                        let label = child
                            .conversation
                            .iter()
                            .rev()
                            .find_map(|message| {
                                (message.role == ChatRole::Tool)
                                    .then_some(message.data_envelope.as_ref())
                                    .flatten()
                            })
                            .unwrap();
                        evidence.set(label.envelope_id.clone()).unwrap();
                        release.send(()).unwrap();
                    }
                );
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(outcome.unwrap(), LoopOutcome::SubAgentYielded));
    let first_reports = workbench_report_rows(db, &task.task_id).await;
    assert_eq!(first_reports.len(), 2);
    assert_eq!(
        first_reports[0].event_kind,
        ReportKind::Progress.inbox_kind()
    );
    assert_eq!(
        first_reports[1].event_kind,
        ReportKind::NeedsInput.inbox_kind()
    );
    assert!(
        store
            .parent_runtime_candidate(work, actor, "")
            .await
            .unwrap()
            .is_none()
    );
    (task, first_reports)
}

async fn workbench_report_rows(
    db: &DatabaseConnection,
    task: &str,
) -> Vec<crate::entity::agent_subagent_inbox::Model> {
    use crate::entity::agent_subagent_inbox as inbox;
    use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
    inbox::Entity::find()
        .filter(inbox::Column::TaskId.eq(task))
        .filter(inbox::Column::ReportSeq.is_not_null())
        .order_by_asc(inbox::Column::ReportSeq)
        .all(db)
        .await
        .unwrap()
}

async fn exercise_workbench_report_wake(
    db: &DatabaseConnection,
    listener: &TcpListener,
    work: &str,
    actor: &str,
    task: crate::entity::agent_subagent_run::Model,
) {
    use crate::agent_subagent_store::{ParentWaitResolution, SubAgentStore};
    use crate::entity::agent_subagent_run as run;
    use desk_diagnose_core::subagent::report::ReportKind;
    use serde_json::json;

    let store = SubAgentStore::new(db.clone());
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let gate = crate::ai_assistant_gate::global_ai_assistant_gate();
    let reports = || workbench_report_rows(db, &task.task_id);
    let first_reports = reports().await;
    let question = &first_reports[1];
    let (eligible, _) = store.root_wait_candidates(0, 32).await.unwrap();
    assert!(
        eligible
            .iter()
            .any(|group| group.root_conversation_id == work)
    );
    assert!(matches!(
        store.resolve_parent_wait(work, actor, "").await.unwrap(),
        ParentWaitResolution::NotWaiting
    ));
    let ready = store
        .parent_runtime_candidate(work, actor, "")
        .await
        .unwrap()
        .unwrap();
    let repeated = store
        .parent_runtime_candidate(work, actor, "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ready.session().ready_subagent_notification,
        repeated.session().ready_subagent_notification
    );
    assert!(ready.session().ready_subagent_wait.is_none());

    let (outcome, ()) = tokio::time::timeout(SCENARIO_TIMEOUT, async {
        tokio::join!(
            resume_subagent_turn(connections.clone(), db.clone(), &gate, ready),
            async {
                let mut acknowledge = report_test_call(
                    "ack-first-progress",
                    "acknowledge_subagent_reports",
                    json!({"report_ids":[first_reports[0].event_id]}),
                );
                acknowledge["index"] = json!(1);
                capture_one_openai_request_with_reply(listener, |body| {
                    let input: serde_json::Value = serde_json::from_slice(body).unwrap();
                    let messages = input["messages"].to_string();
                    assert!(messages.contains("Checked the first part"), "{messages}");
                    assert!(messages.contains(&question.event_id), "{messages}");
                    assert!(!messages.contains("Coordinate request B"), "{messages}");
                    assert!(
                        input["tools"]
                            .to_string()
                            .contains("reply_subagent_request")
                    );
                    memory_sse(
                        json!({"tool_calls":[report_test_call("reply-exact-question",
                        "reply_subagent_request", json!({"request_id": task.task_id,
                            "question_id": question.event_id, "message":"Use version 15"})),
                        acknowledge]}),
                        "tool_calls",
                    )
                })
                .await;
                let reply = memory_sse(json!({"content":"I answered the Worker"}), "stop");
                capture_one_openai_request_with_sse(listener, &reply).await;
            }
        )
    })
    .await
    .unwrap();
    assert!(matches!(outcome.unwrap(), LoopOutcome::Answered(_)));
    let latest = run::Entity::find_by_id(task.id)
        .one(db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.state, "queued");
    let delivered = reports().await;
    assert!(delivered[0].interpreted_at_ms.is_some());
    assert!(delivered[1].model_notified_at_ms.is_some());
    assert!(delivered[1].interpreted_at_ms.is_none());
    assert!(
        store
            .parent_runtime_candidate(work, actor, "")
            .await
            .unwrap()
            .is_none()
    );

    let worker = store
        .child_runtime_candidate(&task.task_id)
        .await
        .unwrap()
        .unwrap();
    let (outcome, _) = tokio::time::timeout(SCENARIO_TIMEOUT, async {
        tokio::join!(
            resume_subagent_turn(connections.clone(), db.clone(), &gate, worker),
            capture_one_openai_request_with_reply(listener, |body| {
                assert!(String::from_utf8_lossy(body).contains("Use version 15"));
                memory_sse(json!({"tool_calls":[report_test_call("finish-work",
                    "report_subagent_status", json!({"kind":"completed","summary":"Validated version 15"}))
                ]}), "tool_calls")
            })
        )
    }).await.unwrap();
    assert!(matches!(outcome.unwrap(), LoopOutcome::SubAgentYielded));
    let ready = store
        .parent_runtime_candidate(work, actor, "")
        .await
        .unwrap()
        .unwrap();
    let (outcome, _) = tokio::time::timeout(SCENARIO_TIMEOUT, async {
        tokio::join!(
            resume_subagent_turn(connections, db.clone(), &gate, ready),
            capture_one_openai_request_with_reply(listener, |body| {
                assert!(String::from_utf8_lossy(body).contains("Validated version 15"));
                memory_sse(json!({"content":"Work complete"}), "stop")
            })
        )
    })
    .await
    .unwrap();
    assert!(matches!(outcome.unwrap(), LoopOutcome::Answered(_)));
    let complete = reports().await;
    assert_eq!(complete.len(), 3);
    assert_eq!(complete[2].event_kind, ReportKind::Completed.inbox_kind());
    assert!(complete[2].model_notified_at_ms.is_some());
    assert!(
        store
            .parent_runtime_candidate(work, actor, "")
            .await
            .unwrap()
            .is_none()
    );
}

async fn personal_input_roundtrip(target_refs: Vec<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    for reference in &target_refs {
        crate::device_code_store::get_or_create(&db, reference)
            .await
            .unwrap();
    }
    if !target_refs.is_empty() {
        use desk_signal_facade::model::os::OperationSystemEnum;
        for (id, name, platform) in [
            ("selected-mac", "Mac for testing", OperationSystemEnum::Mac),
            (
                "selected-linux",
                "Linux for regression",
                OperationSystemEnum::Linux,
            ),
            (
                "unselected-windows",
                "Other device",
                OperationSystemEnum::Windows,
            ),
        ] {
            register_request_device(&connections, id, name, platform).await;
        }
    }
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("targetless-fixture".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let gateway = actix_web::rt::spawn(async move {
        let first_answer = memory_sse(
            serde_json::json!({"content":"first targetless answer"}),
            "stop",
        );
        let (first, _) = capture_openai_answer_with_title(&listener, &first_answer).await;
        let second = capture_one_openai_request_with_reply(&listener, |_| {
            memory_sse(
                serde_json::json!({"content":"second targetless answer"}),
                "stop",
            )
        })
        .await;
        [first, second]
    });
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let ask = AiAssistantAsk {
        target_refs: target_refs.clone(),
        question: "Help me draft a test case".into(),
        client_message_id: "targetless-owner-input".into(),
        conversation_id: Some("targetless-owner-conversation".into()),
        client_timezone: "UTC".into(),
        ..Default::default()
    };
    let followup = AiAssistantAsk {
        question: "Now give me the expected result".into(),
        client_message_id: "targetless-owner-followup".into(),
        ..ask.clone()
    };
    for (index, input) in [ask.clone(), followup.clone(), followup]
        .into_iter()
        .enumerate()
    {
        tokio::time::timeout(
            SCENARIO_TIMEOUT,
            run_turn(
                connections.clone(),
                db.clone(),
                format!("targetless-owner-request-{index}"),
                "owner-browser".into(),
                String::new(),
                owner,
                String::new(),
                input,
            ),
        )
        .await
        .unwrap();
    }
    let [first, second] = tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    for (wire, role, content) in [
        (first, "user", ask.question.as_str()),
        (second, "assistant", "first targetless answer"),
    ] {
        let request: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == role && message["content"] == content),
            "missing {role} message {content:?} in {:?}",
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] != "system")
                .collect::<Vec<_>>(),
        );
        assert_targetless_web_tools(&request, false);
        if !target_refs.is_empty() {
            let catalogue = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .find(|text| text.contains("\"request_devices\""))
                .expect("selected device catalogue");
            assert!(catalogue.contains("\"target_ref\":\"selected-mac\""));
            assert!(catalogue.contains("\"platform\":\"macOS\""));
            assert!(catalogue.contains("\"name\":\"Mac for testing\""));
            assert!(catalogue.contains("\"target_ref\":\"selected-linux\""));
            assert!(catalogue.contains("\"platform\":\"Linux\""));
            assert!(!catalogue.contains("unselected-windows"));
        }
    }

    let actor = owner.to_string();
    let run = derive_assistant_conversation_key(
        &actor,
        AssistantConversationScope::Personal,
        ask.conversation_id.as_deref().unwrap(),
    );
    let session = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest_committed_answer(&session).as_deref(),
        Some("second targetless answer")
    );
    assert_eq!(session.input_revision, 2);
    let stored = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let stored =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&stored.state_json)
            .unwrap();
    assert!(stored.device_id.is_empty());
    assert_eq!(
        stored.assistant_request_devices(),
        Some(target_refs.as_slice())
    );
    let receipts = crate::entity::model_egress_receipt::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(receipts.len(), 3);
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.state == crate::model_egress_store::STATE_SUCCEEDED)
    );
}

async fn register_request_device(
    connections: &SharedConnectionMap,
    id: &str,
    name: &str,
    platform: desk_signal_facade::model::os::OperationSystemEnum,
) -> actix_web::HttpResponse {
    use desk_signal_facade::model::{
        auth_context::AuthContext, connection::ConnectionModel, peer::PeerInfo,
        signal::RemoteDeskTypeEnum,
    };
    use std::sync::Arc;
    let request = actix_web::test::TestRequest::get()
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = <web::Payload as actix_web::FromRequest>::from_request(
        &request,
        &mut actix_web::dev::Payload::None,
    )
    .await
    .unwrap();
    let (response, socket, _) = actix_ws::handle(&request, payload).unwrap();
    let connection_id = format!("live-{id}");
    connections.write().await.insert(
        connection_id.clone(),
        ConnectionState {
            lifecycle: Default::default(),
            model: Arc::new(ConnectionModel {
                connection_id,
                ip: None,
                version_info: None,
                peer: PeerInfo {
                    remote_desk_type: RemoteDeskTypeEnum::Server,
                    operation_system: platform,
                    display_name: Some(name.into()),
                },
                exec_capabilities: desk_signal_facade::model::peer::HostExecCapabilities {
                    available_exec_shells: vec!["bash".into()],
                    ..Default::default()
                },
                ai_assistant_settings: Some(
                    desk_agent_protocol::ai_assistant::AiAssistantSettings {
                        revision: 1,
                        enabled: true,
                    },
                ),
                device_id: Some(id.into()),
                owner_node_id: None,
            }),
            client_id: Some(id.into()),
            session: Arc::new(tokio::sync::RwLock::new(socket)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: None,
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        },
    );
    response
}

#[actix_web::test]
async fn personal_input_seals_command_only_for_the_selected_device() {
    run_selected_command(SelectedCommandFinish::Foreground).await;
}

#[actix_web::test]
async fn personal_input_seals_command_and_continues_after_background_result() {
    run_selected_command(SelectedCommandFinish::Background).await;
}

#[actix_web::test]
async fn personal_input_seals_command_but_retires_a_late_receipt_after_supersession() {
    run_selected_command(SelectedCommandFinish::Retired).await;
}

enum SelectedCommandFinish {
    Foreground,
    Background,
    Retired,
}

async fn run_selected_command(finish: SelectedCommandFinish) {
    let background = !matches!(finish, SelectedCommandFinish::Foreground);
    let retired = matches!(finish, SelectedCommandFinish::Retired);
    static DEVICE_FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _fixture = DEVICE_FIXTURE.lock().await;
    use crate::controller::ai_assistant_session::decide_permission_on;
    use desk_agent_protocol::{Capability, computer_use::*};
    use desk_assistant_data_store::main_request::{OwnerAdmission, OwnerAdmissionResult};
    use desk_signal_facade::controller::ai_assistant_session::{
        PermissionDecisionBody, PermissionDecisionItemBody, PermissionItemDecisionBody,
    };
    use desk_signal_facade::model::os::OperationSystemEnum;
    use serde_json::{Value, json};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
            model: Some("selected-command-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    if background {
        crate::ai_assistant_gate::enable_test_host();
        crate::agent_runtime::register_continuation_connections(connections.clone());
    }
    let now = chrono::Utc::now();
    let mut peers = Vec::new();
    for id in ["command-selected", "command-outside"] {
        crate::device_code_store::get_or_create(&db, id)
            .await
            .unwrap();
        peers.push(register_request_device(&connections, id, id, OperationSystemEnum::Linux).await);
        let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
        cache.remove_connection(&format!("live-{id}"));
        cache
            .update(
                &format!("live-{id}"),
                ComputerUseReadiness {
                    schema_version: COMPUTER_USE_SCHEMA_VERSION,
                    revision: 1,
                    observed_at: now.to_rfc3339(),
                    expires_at: (now + chrono::Duration::minutes(2)).to_rfc3339(),
                    server_api_version: 1,
                    os: "linux".into(),
                    interactive_user_home: None,
                    interactive_session_incarnation: "desktop:worker".into(),
                    local_ceiling_revision: 1,
                    capabilities: vec![ComputerUseCapabilityReadiness {
                        capability: Capability::ShellExecConfirmed,
                        adapter: ComputerUseAdapterRef {
                            kind: ComputerUseAdapterKind::Terminal,
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
    }
    let model = actix_web::rt::spawn(async move {
        let reply = |target: &str| {
            memory_sse(
                json!({"tool_calls":[{"index":0,"id":format!("plan-{target}"),"type":"function","function":{
                    "name":"request_permissions","arguments":json!({
                    "target_ref":target,"items":[{"tool_name":"exec_command",
                            "exact_input":{"shell":"bash","command":"df -h","timeout_ms":1000},
                            "reason":"Check disk space on the selected machine"}]
                    }).to_string()
                }}]}),
                "tool_calls",
            )
        };
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let spec = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["function"]["name"] == "request_permissions")
                .unwrap();
            assert!(spec["function"]["parameters"]["properties"]["target_ref"].is_object());
            assert!(request["messages"].to_string().contains("command-selected"));
            assert!(!request["messages"].to_string().contains("command-outside"));
            reply("command-outside")
        })
        .await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool"
                        && message["tool_call_id"] == "plan-command-outside"
                        && message["content"].as_str().is_some_and(
                            |value| value.contains("Selected assistant device is unavailable")
                        )),
                "{request}"
            );
            reply("command-selected")
        })
        .await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let spec = request["tools"].as_array().unwrap().iter()
                .find(|tool| tool["function"]["name"] == "exec_command")
                .unwrap_or_else(|| panic!("approved command is missing: {request}"));
            assert!(spec["function"]["parameters"]["required"].as_array().unwrap().contains(&json!("target_ref")));
            memory_sse(json!({"tool_calls":[{"index":0,"id":"execute-selected","type":"function","function":{
                "name":"exec_command","arguments":json!({"target_ref":"command-selected",
                    "shell":"bash","command":"df -h","timeout_ms":1000}).to_string()
            }}]}), "tool_calls")
        }).await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            if background {
                assert!(
                    !request["messages"]
                        .to_string()
                        .contains("SELECTED_DISK_RECEIPT")
                );
                return memory_sse(json!({"content":"Waiting for the disk report."}), "stop");
            }
            assert!(
                request["messages"]
                    .to_string()
                    .contains("SELECTED_DISK_RECEIPT"),
                "{request}"
            );
            memory_sse(json!({"content":"Received the disk report."}), "stop")
        })
        .await;
        if background && !retired {
            let mut received_result = false;
            while !received_result {
                capture_one_openai_request_with_reply(&listener, |body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    let messages = request["messages"].to_string();
                    if messages.contains("Create a short conversation title") {
                        return memory_sse(
                            json!({"content":"{\"title\":\"Check Disk Space\"}"}),
                            "stop",
                        );
                    }
                    assert!(
                        messages.contains("SELECTED_DISK_RECEIPT"),
                        "{}",
                        request["messages"].as_array().unwrap().last().unwrap()
                    );
                    received_result = true;
                    memory_sse(json!({"content":"Received the disk report."}), "stop")
                })
                .await;
            }
        }
    });
    let intent = "selected-command-conversation";
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let roster = crate::controller::ai_assistant_session::update_roster(
        &db,
        &crate::controller::ai_assistant_session::AiAssistantDeviceRosterBody {
            conversation: intent.into(),
            expected_revision: 0,
            target_refs: vec!["command-selected".into()],
        },
    )
    .await
    .unwrap();
    assert_eq!(roster.revision, 1);
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn(
            connections.clone(),
            db.clone(),
            "selected-command-transport".into(),
            "owner-browser".into(),
            String::new(),
            owner,
            String::new(),
            AiAssistantAsk {
                question: "Check disk space on this device".into(),
                client_message_id: "selected-command-input".into(),
                conversation_id: Some(intent.into()),
                client_timezone: "UTC".into(),
                target_refs: vec!["command-selected".into()],
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let actor = owner.to_string();
    let run =
        derive_assistant_conversation_key(&actor, AssistantConversationScope::Personal, intent);
    let snapshot = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .unwrap();
    let [permission] = snapshot.permission_requests.as_slice() else {
        panic!(
            "expected one selected-device approval, got {:?}: {:?}",
            snapshot.permission_requests,
            snapshot.messages.iter().rev().take(3).collect::<Vec<_>>()
        );
    };
    let confirmation = permission.exact_command().unwrap();
    assert_eq!(confirmation.target_device_id, "command-selected");
    assert_eq!(
        confirmation.target_session_id,
        "live-command-selected:desktop:worker"
    );
    assert_eq!(confirmation.proposal.command, "df -h");
    assert!(
        crate::entity::agent_action_item::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty()
    );
    let item = &permission.items[0];
    let decision = decide_permission_on(
        &db,
        connections.clone(),
        web::Json(PermissionDecisionBody {
            connection: None,
            conversation: None,
            session: Some(run.clone()),
            expected_run_request_id: None,
            request_id: permission.request_id.clone(),
            carrier_id: None,
            items: vec![PermissionDecisionItemBody {
                item_id: item.item_id.clone(),
                decision: PermissionItemDecisionBody::Approve {
                    resource_scope: item.resource_scope.clone(),
                    operation_scope: item.operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: 60,
                    max_uses: 1,
                },
            }],
        }),
    )
    .await
    .unwrap();
    let decision = actix_web::body::to_bytes(decision.into_body())
        .await
        .unwrap();
    let decision: Value = serde_json::from_slice(&decision).unwrap();
    assert_eq!(decision["data"]["state"], "approved", "{decision}");
    let queued = if background {
        let ask = AiAssistantAsk {
            question: "Check the next request".into(),
            client_message_id: "queued-after-command".into(),
            conversation_id: Some(intent.into()),
            client_timezone: "UTC".into(),
            ..Default::default()
        };
        let scope = desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        };
        let destination = crate::model_provider::load(&db)
            .await
            .unwrap()
            .destination_identity()
            .unwrap();
        let next = crate::agent_run_event_store::AppendUserFollowupParams {
            client_timezone: ask.client_timezone.clone(),
            temporary_conversation: None,
            event_id: ask.client_message_id.clone(),
            run_id: run.clone(),
            client_conversation_id: ask.conversation_id.clone(),
            actor_id: actor.clone(),
            device_id: String::new(),
            target_refs: vec![],
            surface: AgentSessionSurface::AiAssistant,
            policy_revision:
                desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
            read_context: Some(
                crate::agent_run_event_store::ReadContextSelection::capture(&[], &scope).unwrap(),
            ),
            current_scope: scope,
            message: model_bound_user_message(
                ask.client_message_id.clone(),
                ask.question.clone(),
                destination,
            )
            .unwrap(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let request_id = if retired {
            None
        } else {
            let OwnerAdmissionResult::Queued(receipt) = SignalAgentRunEventStore::new(db.clone())
                .admit_user_input(
                    &next,
                    None,
                    OwnerAdmission::Enqueue {
                        ask: &ask,
                        limit: 4,
                        reply_connection_id: None,
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected a queued request")
            };
            Some(receipt.request_id.clone())
        };
        Some((next, request_id))
    } else {
        None
    };
    tokio::time::timeout(SCENARIO_TIMEOUT, async {
        tokio::join!(
            async {
                respond_to_selected_command(&connections, db.clone(), peers.remove(0), background)
                    .await;
                if let Some((next, request_id)) = &queued {
                    let events = SignalAgentRunEventStore::new(db.clone());
                    if let Some(request_id) = request_id {
                        assert!(matches!(
                            events
                                .admit_user_input(
                                    next,
                                    None,
                                    OwnerAdmission::Activate { request_id }
                                )
                                .await
                                .unwrap(),
                            OwnerAdmissionResult::Waiting
                        ));
                    } else {
                        // An explicit correction may retire A before its receipt is published.
                        assert_eq!(
                            events
                                .append_user_followup(next)
                                .await
                                .unwrap()
                                .input_revision,
                            snapshot.input_revision + 1
                        );
                    }
                    crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
                        .publish_once()
                        .await
                        .unwrap();
                }
            },
            async {
                model.await.unwrap();
            },
        );
    })
    .await
    .unwrap();
    let tasks = crate::entity::agent_exec_task::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let [task] = tasks.as_slice() else {
        panic!("expected one selected command")
    };
    assert_eq!(task.conversation_id, run);
    assert_eq!(task.target_connection_id, "live-command-selected");
    assert_eq!(task.status, crate::agent_exec_store::STATUS_DONE);
    if background {
        assert_eq!(
            task.delivery_state,
            crate::agent_exec_store::DELIVERY_CONSUMED
        );
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let session =
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                .unwrap();
        assert!(session.pending_auto_triggers.is_empty());
        if retired {
            assert_eq!(session.input_revision, snapshot.input_revision + 1);
            assert_eq!(session.automation_turns_used, 0);
            assert!(
                !session
                    .conversation
                    .iter()
                    .any(|message| message.text.contains("SELECTED_DISK_RECEIPT"))
            );
            assert!(
                task.result_text
                    .as_deref()
                    .unwrap()
                    .contains("SELECTED_DISK_RECEIPT")
            );
            return;
        }
        assert_eq!(session.input_revision, snapshot.input_revision);
        assert_eq!(session.automation_turns_used, 1);
        let (next, request_id) = queued.unwrap();
        let request_id = request_id.unwrap();
        let receipt = SignalAgentRunEventStore::new(db)
            .admit_user_input(
                &next,
                None,
                OwnerAdmission::Activate {
                    request_id: &request_id,
                },
            )
            .await
            .unwrap()
            .native()
            .unwrap();
        assert_eq!(receipt.input_revision, session.input_revision + 1);
    }
}

async fn respond_to_selected_command(
    connections: &SharedConnectionMap,
    db: crate::config::connection::DatabaseConnection,
    peer: actix_web::HttpResponse,
    background: bool,
) {
    use desk_agent_protocol::edge_exec::{
        EdgeExecDisposition, EdgeExecRequestPayload, EdgeExecResultPayload,
    };
    use desk_agent_protocol::{AgentOutcome, ExecOutput, ExecOutputStreams, OperationOutput};
    use desk_signal_facade::service::EdgeExecObserver;

    let frame = receive_device_frame(peer).await;
    assert_eq!(frame.signaling_type, SignalingType::ExecuteEdgePlan);
    assert_eq!(
        frame.to_connection_id.as_deref(),
        Some("live-command-selected")
    );
    let payload: serde_json::Value = frame.get_data().unwrap();
    let request: EdgeExecRequestPayload = serde_json::from_value(payload["inner"].clone()).unwrap();
    let EdgeExecRequestPayload::Agentic { plan, .. } = request else {
        panic!("expected exact command")
    };
    assert_eq!(plan.argv.last().map(String::as_str), Some("df -h"));
    if background {
        tokio::time::sleep(std::time::Duration::from_secs(9)).await;
    }
    let payload = EdgeExecResultPayload {
        request_id: frame.request_id.clone(),
        disposition: EdgeExecDisposition::Executed {
            outcome: AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
                started: true,
                termination_signal: None,
                failure: None,
                diagnostics: vec![],
                exit_code: Some(0),
                duration_ms: 1,
                redactions: vec![],
                streams: ExecOutputStreams::Split {
                    stdout: "SELECTED_DISK_RECEIPT".into(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
            })),
        },
    };
    let frame = SignalingModel::new(
        &frame.request_id,
        SignalingType::EdgeExecutionCompleted,
        None,
        None,
        Some(serde_json::to_value(payload).unwrap()),
        None,
    );
    let source = connections
        .read()
        .await
        .current_oss_server("command-selected")
        .unwrap()
        .clone();
    crate::agent_exec::SignalEdgeExecObserver::new(
        crate::agent_exec::global_agent_exec_pending(),
        db,
    )
    .on_fleet_exec_result(&source, &frame)
    .await;
}

#[actix_web::test]
async fn personal_input_reads_selected_devices_and_keeps_their_distinct_results() {
    use desk_agent_protocol::{Capability, computer_use::*};
    use desk_signal_facade::model::os::OperationSystemEnum;
    use serde_json::{Value, json};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
            model: Some("selected-read-model".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let now = chrono::Utc::now();
    let mut peers = Vec::new();
    for (id, platform) in [
        ("compare-mac", OperationSystemEnum::Mac),
        ("compare-linux", OperationSystemEnum::Linux),
        ("other-device", OperationSystemEnum::Windows),
    ] {
        crate::device_code_store::get_or_create(&db, id)
            .await
            .unwrap();
        let peer = register_request_device(&connections, id, id, platform).await;
        crate::computer_use_readiness::global_computer_use_readiness_cache()
            .update(
                &format!("live-{id}"),
                ComputerUseReadiness {
                    schema_version: COMPUTER_USE_SCHEMA_VERSION,
                    revision: 1,
                    observed_at: now.to_rfc3339(),
                    expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                    server_api_version: 1,
                    os: "linux".into(),
                    interactive_user_home: None,
                    interactive_session_incarnation: "selected-read-worker".into(),
                    local_ceiling_revision: 1,
                    capabilities: vec![ComputerUseCapabilityReadiness {
                        capability: Capability::SystemInfo,
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
        peers.push(peer);
    }
    let model = actix_web::rt::spawn(async move {
        let first = capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let schema = request["tools"].as_array().unwrap().iter()
                .find(|tool| tool["function"]["name"] == "read_system_info").unwrap();
            assert_eq!(schema["function"]["parameters"]["required"], json!(["target_ref"]));
            let calls = ["compare-mac", "compare-linux", "other-device"].into_iter()
                .enumerate().map(|(index, target)| json!({
                    "index": index, "id": format!("read-{target}"), "type": "function",
                    "function": { "name": "read_system_info", "arguments": json!({"target_ref":target}).to_string() }
                })).collect::<Vec<_>>();
            memory_sse(json!({"tool_calls":calls}), "tool_calls")
        }).await;
        let answer = memory_sse(json!({"content":"Compared both selected devices"}), "stop");
        let (second, _) = capture_openai_answer_with_title(&listener, &answer).await;
        [first, second]
    });
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let turn = run_turn(
        connections.clone(),
        db.clone(),
        "selected-read-request".into(),
        "owner-browser".into(),
        String::new(),
        owner,
        String::new(),
        AiAssistantAsk {
            target_refs: vec!["compare-mac".into(), "compare-linux".into()],
            question: "Compare the operating system versions of these two devices".into(),
            client_message_id: "selected-read-message".into(),
            conversation_id: Some("selected-read-conversation".into()),
            client_timezone: "UTC".into(),
            ..Default::default()
        },
    );
    let mac = respond_to_system_info(
        &connections,
        "compare-mac",
        peers.remove(0),
        "fixture-macos-version",
    );
    let linux = respond_to_system_info(
        &connections,
        "compare-linux",
        peers.remove(0),
        "fixture-linux-version",
    );
    tokio::time::timeout(SCENARIO_TIMEOUT, async { tokio::join!(turn, mac, linux) })
        .await
        .unwrap();
    let [_, response] = tokio::time::timeout(MODEL_IO_TIMEOUT, model)
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.contains("fixture-macos-version"));
    assert!(response.contains("fixture-linux-version"));
    let work = crate::entity::agent_action_item::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let mut targets = work
        .iter()
        .map(|item| item.target_device_id.as_str())
        .collect::<Vec<_>>();
    targets.sort_unstable();
    assert_eq!(targets, ["compare-linux", "compare-mac"]);
}

async fn respond_to_system_info(
    connections: &SharedConnectionMap,
    target: &str,
    peer: actix_web::HttpResponse,
    version: &str,
) {
    use desk_agent_protocol::{
        AgentOutcome, CpuInfo, MemoryInfo, OperationOutput, ReadContextOutput, SystemInfoOutput,
        remote_tool::{RemoteToolOutput, RemoteToolRequest, RemoteToolResponse},
    };
    use desk_signal_facade::service::RemoteToolObserver;

    let frame = receive_device_frame(peer).await;
    assert_eq!(frame.signaling_type, SignalingType::InvokeRemoteTool);
    let read: RemoteToolRequest = frame.get_data().unwrap();
    assert_eq!(read.envelope.target.device_id, target);
    assert!(matches!(
        read.envelope.operation.input,
        desk_agent_protocol::ReadContextInput {
            kind: desk_agent_protocol::ContextKind::SystemInfo(_),
        }
    ));
    let result = RemoteToolOutput {
        outcome: AgentOutcome::Ok(OperationOutput::ReadContext(ReadContextOutput::SystemInfo(
            SystemInfoOutput {
                hostname: target.into(),
                os: target.into(),
                os_version: version.into(),
                arch: "arm64".into(),
                uptime_seconds: 1,
                cpu: CpuInfo {
                    usage_percent: 0.0,
                    logical_cores: 4,
                },
                memory: MemoryInfo {
                    total_bytes: 2,
                    used_bytes: 1,
                },
                disks: vec![],
            },
        ))),
        image: None,
        document_preview: None,
        document_preview_page: None,
    };
    let source = connections
        .read()
        .await
        .current_oss_server(target)
        .unwrap()
        .clone();
    let observer = crate::remote_tool_edge::SignalRemoteToolObserver::new(
        crate::remote_tool_edge::global_remote_tool_pending(),
    );
    for chunk in desk_diagnose_core::chunk::chunk_bytes(
        &read.request_id,
        &serde_json::to_vec(&result).unwrap(),
        256,
    ) {
        let response = SignalingModel::new(
            "selected-read-result",
            SignalingType::RemoteToolOutputUpdated,
            None,
            None,
            Some(serde_json::to_value(RemoteToolResponse::Chunk(chunk)).unwrap()),
            None,
        );
        observer.on_remote_tool_response(&source, &response).await;
    }
}

async fn receive_device_frame(peer: actix_web::HttpResponse) -> SignalingModel {
    use actix_http::ws::{Codec, Frame};
    use actix_web::body::MessageBody;
    use tokio_util::codec::Decoder;

    let mut body = std::pin::pin!(peer.into_body());
    let mut codec = Codec::new().client_mode();
    let mut buffer = bytes::BytesMut::new();
    loop {
        let chunk = std::future::poll_fn(|cx| body.as_mut().poll_next(cx))
            .await
            .unwrap()
            .unwrap();
        buffer.extend_from_slice(&chunk);
        if let Some(Frame::Text(text)) = codec.decode(&mut buffer).unwrap() {
            return serde_json::from_slice::<SignalingModel>(&text).unwrap();
        }
    }
}

fn assert_targetless_web_tools(request: &serde_json::Value, approved: bool) {
    use desk_diagnose_core::subagent::tools;
    let names = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        names.contains(desk_diagnose_core::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME),
        "{names:?}"
    );
    assert_eq!(
        names.contains(desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME),
        approved,
        "{names:?}"
    );
    assert!(names.contains(tools::SPAWN), "{names:?}");
    assert!(!names.contains(tools::REPORT), "{names:?}");
    assert!(
        names.iter().all(|name| matches!(
            *name,
            "describe_tools"
                | "request_permissions"
                | "fetch_public_web_page"
                | "read_conversation_attachment"
                | "search_memories"
                | "read_memory_history"
                | "read_memory_source"
                | "manage_memories"
        ) || tools::effect(name).is_some()),
        "{names:?}"
    );
}

#[actix_web::test]
async fn targetless_web_decision_resumes_the_original_model_turn_without_a_host() {
    use crate::controller::ai_assistant_session::{PermissionDecisionBody, decide_permission_on};
    use actix_web::body::to_bytes;
    use serde_json::{Value, json};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
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
            model: Some("targetless-web-fixture".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let gateway = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_targetless_web_tools(&request, false);
            memory_sse(json!({"tool_calls":[{"index":0,"id":"request-web-read","type":"function","function":{
                "name":"request_permissions",
                "arguments":json!({"items":[{"tool_name":"fetch_public_web_page","exact_input":{"url":"https://docs.example.invalid/guide"},"reason":"Read the requested public guide"}]}).to_string(),
            }}]}), "tool_calls")
        }).await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_targetless_web_tools(&request, true);
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["role"] == "user" && message["content"] == "Read the public guide"
                    })
            );
            super::permission_object::tool_reply(
                "fetch_public_web_page",
                json!({"url":"https://docs.example.invalid/guide"}),
            )
        })
        .await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["role"] == "tool"
                            && message["tool_call_id"] == "call-fetch_public_web_page"
                    })
            );
            memory_sse(
                json!({"content":"The original web request resumed."}),
                "stop",
            )
        })
        .await;
    });
    let connections = web::Data::new(SharedConnectionMap::new());
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let actor = owner.to_string();
    let intent = "targetless-web-session";
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn(
            connections.clone(),
            db.clone(),
            "targetless-web-input".into(),
            "owner-browser".into(),
            String::new(),
            owner,
            String::new(),
            AiAssistantAsk {
                question: "Read the public guide".into(),
                client_message_id: "targetless-web-input".into(),
                conversation_id: Some(intent.into()),
                client_timezone: "UTC".into(),
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let run =
        derive_assistant_conversation_key(&actor, AssistantConversationScope::Personal, intent);
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let pending = sessions
        .read_snapshot_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .unwrap();
    let [request] = pending.permission_requests.as_slice() else {
        panic!(
            "expected one web permission request, got {:?}",
            pending
                .messages
                .iter()
                .filter(|message| message.role == desk_diagnose_core::chat::ChatRole::Tool)
                .map(|message| &message.text)
                .collect::<Vec<_>>()
        )
    };
    let [item] = request.items.as_slice() else {
        panic!("expected one permission item")
    };
    let body: PermissionDecisionBody = serde_json::from_value(json!({
        "conversation":intent, "requestId":request.request_id,
        "items":[{"itemId":item.item_id,"decision":"approve","resource_scope":item.resource_scope,
            "operation_scope":item.operation_scope,"ttl_seconds":60,"max_uses":1}],
    }))
    .unwrap();
    let response = decide_permission_on(&db, connections, web::Json(body))
        .await
        .unwrap();
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
    assert_eq!(response["data"]["state"], "approved", "{response}");
    match tokio::time::timeout(MODEL_IO_TIMEOUT, gateway).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            let snapshot = sessions
                .read_snapshot_for_subject(&run, &actor, "")
                .await
                .unwrap()
                .unwrap();
            let dispatches = crate::entity::agent_capability_dispatch_outbox::Entity::find()
                .all(&db)
                .await
                .unwrap();
            panic!(
                "web resume did not finish model I/O, terminal error {:?}, {} dispatches",
                snapshot.terminal_error,
                dispatches.len()
            );
        }
    }
    let final_snapshot = tokio::time::timeout(SCENARIO_TIMEOUT, async {
        loop {
            let snapshot = sessions
                .read_snapshot_for_subject(&run, &actor, "")
                .await
                .unwrap()
                .unwrap();
            if latest_committed_answer(&snapshot).is_some() {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        latest_committed_answer(&final_snapshot).as_deref(),
        Some("The original web request resumed.")
    );
    assert_eq!(final_snapshot.input_revision, pending.input_revision);
    let dispatches = crate::entity::agent_capability_dispatch_outbox::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let [dispatch] = dispatches.as_slice() else {
        panic!("expected one central web dispatch")
    };
    let payload: crate::capability_grant_store::CapabilityDispatchPayload =
        serde_json::from_str(&dispatch.payload_json).unwrap();
    assert_eq!(payload.tool_name, "fetch_public_web_page");
    assert_eq!(payload.input_revision, pending.input_revision);
    let grant = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(payload.grant_id, grant.grant_id);
    assert!(grant.target_device_id.is_empty());
}

#[actix_web::test]
async fn actual_owner_turn_commits_memory_without_hidden_model_calls_and_answers_after_the_tool() {
    owner_memory_turn(false, "device").await;
}

#[actix_web::test]
async fn personal_input_writes_memory_only_for_the_selected_device() {
    use crate::entity::{agent_assistant_origin, agent_memory, agent_memory_source};
    use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate;
    use sea_orm::{ColumnTrait, QueryFilter};
    use serde_json::{Value, json};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    for id in ["selected-a", "selected-b", "outside-c"] {
        crate::device_code_store::get_or_create(&db, id)
            .await
            .unwrap();
    }
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("selected-memory-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let settings = crate::assistant_data_config::settings(&db).await;
    crate::assistant_data_config::update_settings(
        &db,
        &AssistantDataSettingsUpdate {
            expected_revision: settings.revision,
            automatic_memory_enabled: true,
            model_history_search_enabled: settings.model_history_search_enabled,
            explanation_version: settings.explanation_version,
        },
    )
    .await
    .unwrap();
    let content = "Release commands use the slim runner";
    let gateway = actix_web::rt::spawn(async move {
        for (index, reference, expected) in [
            (0, "outside-c", "source_out_of_scope"),
            (1, "selected-b", "committed"),
        ] {
            let tool_id = format!("write-selected-{index}");
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                assert!(
                    request["tools"].as_array().is_some_and(|tools| tools
                        .iter()
                        .any(|tool| tool["function"]["name"] == "manage_memories")),
                    "selected device input {index} should expose memory writes"
                );
                memory_sse(json!({"tool_calls":[{"index":0,"id":tool_id,"type":"function","function":{
                    "name":"manage_memories","arguments":json!({
                        "operation":"remember","scope":"current_device","target_ref":reference,"content":content,
                    }).to_string()
                }}]}), "tool_calls")
            }).await;
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let tool = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["tool_call_id"] == tool_id)
                    .unwrap_or_else(|| {
                        panic!("{tool_id} should receive feedback: {}", request["messages"])
                    });
                let result: Value =
                    serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                assert_eq!(result["reason"], expected);
                memory_sse(json!({"content":"finished writing"}), "stop")
            })
            .await;
            if index == 0 {
                capture_one_openai_request_with_reply(&listener, |body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    assert!(
                        request["messages"][0]["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("Create a short conversation title"))
                    );
                    memory_sse(json!({"content":r#"{"title":"Device memories"}"#}), "stop")
                })
                .await;
            }
        }
        for expected in [true, false] {
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let found = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|message| message["role"] == "user")
                    .filter_map(|message| {
                        serde_json::from_str::<Value>(message["content"].as_str()?).ok()
                    })
                    .any(|message| {
                        message["memories"].as_array().is_some_and(|memories| {
                            memories.iter().any(|entry| entry["content"] == content)
                        })
                    });
                assert_eq!(found, expected);
                memory_sse(json!({"content":"finished recalling"}), "stop")
            })
            .await;
        }
    });
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    for (index, targets) in [
        vec!["selected-a", "selected-b"],
        vec!["selected-a", "selected-b"],
        vec!["selected-b"],
        vec!["selected-a"],
    ]
    .into_iter()
    .enumerate()
    {
        let ask = AiAssistantAsk {
            client_timezone: "UTC".into(),
            question: if index < 2 {
                "Remember this device's release configuration"
            } else {
                "Recall this device's release configuration"
            }
            .into(),
            client_message_id: format!("memory-selected-input-{index}"),
            conversation_id: Some("selected-memory-conversation".into()),
            target_refs: targets.into_iter().map(String::from).collect(),
            ..Default::default()
        };
        tokio::time::timeout(
            SCENARIO_TIMEOUT,
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("selected-memory-request-{index}"),
                "owner-browser".into(),
                String::new(),
                crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                String::new(),
                ask,
                None,
            ),
        )
        .await
        .unwrap();
    }
    gateway.await.unwrap();
    let instance = crate::config::ConfigConnection::config_read(&db)
        .await
        .metadata
        .instance
        .clone();
    let target = crate::assistant_data_store::source_device_ref(&instance, "selected-b");
    let memories = agent_memory::Entity::find().all(&db).await.unwrap();
    let [memory] = memories.as_slice() else {
        panic!("expected exactly one device memory");
    };
    assert_eq!(memory.device_ref, target);
    assert_eq!(memory.scope_key, format!("device:{target}"));
    assert_eq!(memory.body, content);
    let evidence = agent_memory_source::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(evidence.source_device_json.is_none());
    let origin = agent_assistant_origin::Entity::find()
        .filter(agent_assistant_origin::Column::OriginId.eq(evidence.origin_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(origin.device_ref.is_empty());
}

#[actix_web::test]
async fn actual_owner_turn_updates_observed_memory_and_refreshes_context_before_answering() {
    owner_memory_turn(true, "device").await;
}

#[actix_web::test]
async fn targetless_owner_turn_writes_and_recalls_personal_memory() {
    owner_memory_turn(false, "").await;
}

async fn owner_memory_turn(update: bool, device_id: &'static str) {
    use crate::entity::agent_memory;
    use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate;
    use serde_json::{Value, json};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let mut model_config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        ..Default::default()
    };
    crate::model_provider::save(&db, model_config.clone())
        .await
        .unwrap();
    let settings = crate::assistant_data_config::settings(&db).await;
    crate::assistant_data_config::update_settings(
        &db,
        &AssistantDataSettingsUpdate {
            expected_revision: settings.revision,
            automatic_memory_enabled: true,
            model_history_search_enabled: settings.model_history_search_enabled,

            explanation_version: settings.explanation_version,
        },
    )
    .await
    .unwrap();
    let target = if update {
        Some(crate::assistant_data_store::tests::seed_personal_memory(&db, "update-owner").await)
    } else {
        None
    };
    let (arrived, ready) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let gateway = actix_web::rt::spawn(async move {
        let first = capture_one_openai_request_with_reply(&listener, |body| {
            let mut arguments = json!({"operation":"remember","content":"The owner prefers concise Chinese replies"});
            if let Some(target) = &target {
                let request:Value = serde_json::from_slice(body).unwrap();
                assert!(request["messages"].to_string().contains(target), "the update target must be observed by the real model");
                arguments["operation"] = json!("update"); arguments["memory_id"] = json!(target);
                arguments["expected_revision"] = json!(1);
            }
            memory_sse(json!({"tool_calls":[{"index":0,"id":"owner-memory-write","type":"function","function":{
                "name":"manage_memories","arguments":arguments.to_string()}}]}), "tool_calls")
        }).await;
        let final_request = capture_one_openai_request_with_reply_paused(
            &listener,
            |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let tool = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "owner-memory-write"
                    })
                    .expect("ordinary tool feedback expected, no extraction call");
                let result: Value =
                    serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                assert_eq!(result["outcome"], "applied");
                assert_eq!(result["reason"], "committed");
                assert!(result["memory"]["memory_id"].is_string());
                assert_eq!(result["memory"].as_object().unwrap().len(), 2);
                memory_sse(json!({"content":"memory-answer"}), "stop")
            },
            Some((arrived, resume)),
        )
        .await;
        vec![first, final_request]
    });
    let ask = AiAssistantAsk {
        client_timezone: "UTC".into(),
        question: "Always use concise Chinese replies".into(),
        client_message_id: "memory-owner-input".into(),
        conversation_id: Some(
            if update {
                "memory-update-entry"
            } else {
                "memory-entry"
            }
            .into(),
        ),
        ..Default::default()
    };
    let host_connection = if device_id.is_empty() {
        ""
    } else {
        "offline-host"
    };
    let session_key = |client_intent| {
        if device_id.is_empty() {
            derive_assistant_conversation_key(
                "1",
                AssistantConversationScope::Personal,
                client_intent,
            )
        } else {
            derive_conversation_key("1", device_id, Some(client_intent), "unused")
        }
    };
    let connections = web::Data::new(SharedConnectionMap::new());
    let running_db = db.clone();
    let running_connections = connections.clone();
    let running_ask = ask.clone();
    let running = actix_web::rt::spawn(async move {
        run_turn_inner(
            running_connections,
            running_db,
            "memory-transport".into(),
            "controller".into(),
            host_connection.into(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            device_id.into(),
            running_ask,
            None,
        )
        .await;
    });
    tokio::time::timeout(MODEL_IO_TIMEOUT, ready)
        .await
        .unwrap()
        .unwrap();
    let memories = agent_memory::Entity::find().all(&db).await.unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].scope_key, "personal");
    assert!(memories[0].device_ref.is_empty());
    assert_eq!(
        memories[0].body,
        "The owner prefers concise Chinese replies"
    );
    let commits = crate::entity::agent_memory_commit::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(memories[0].revision, if update { 2 } else { 1 });
    assert_eq!(
        crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .len(),
        2
    );
    release.send(()).unwrap();
    tokio::time::timeout(SCENARIO_TIMEOUT, running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gateway.await.unwrap().len(), 2);
    let run_id = session_key(if update {
        "memory-update-entry"
    } else {
        "memory-entry"
    });
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert!(
        snapshot.terminal_error.is_none(),
        "{:?}",
        snapshot.terminal_error
    );
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("memory-answer")
    );
    let before = crate::entity::agent_memory_revision::Entity::find()
        .all(&db)
        .await
        .unwrap();
    run_turn_inner(
        connections.clone(),
        db.clone(),
        "memory-retry".into(),
        "controller".into(),
        host_connection.into(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        device_id.into(),
        ask,
        None,
    )
    .await;
    assert_eq!(
        crate::entity::agent_memory_revision::Entity::find()
            .all(&db)
            .await
            .unwrap(),
        before
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    model_config.base_url = Some(format!("http://{}", listener.local_addr().unwrap()));
    crate::model_provider::save(&db, model_config)
        .await
        .unwrap();
    let recall_gateway = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(request["messages"].as_array().unwrap().iter().any(|message|
                message["role"] == "user" && message["content"].as_str().is_some_and(|text|
                    text.contains("The owner prefers concise Chinese replies"))),
                "a new conversation must receive the saved memory, not just the write receipt");
            memory_sse(json!({"tool_calls":[{"index":0,"id":"recall-memory","type":"function","function":{
                "name":"search_memories","arguments":json!({"queries":["Chinese"],"limit":5}).to_string()}}]}), "tool_calls")
        }).await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let tool = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == "recall-memory"
                })
                .unwrap();
            let content = tool["content"].as_str().unwrap();
            let result: Value = serde_json::from_str(content)
                .unwrap_or_else(|error| panic!("invalid search result: {error}: {content}"));
            assert_eq!(result["memories"].as_array().unwrap().len(), 1);
            assert_eq!(
                result["memories"][0]["content"],
                "The owner prefers concise Chinese replies"
            );
            memory_sse(json!({"content":"memory-recalled"}), "stop")
        })
        .await;
    });
    run_turn_inner(
        connections,
        db.clone(),
        "recall-transport".into(),
        "controller".into(),
        host_connection.into(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        device_id.into(),
        AiAssistantAsk {
            client_timezone: "UTC".into(),
            question: "What did I ask you to remember?".into(),
            client_message_id: "recall-owner-input".into(),
            conversation_id: Some("memory-recall-entry".into()),
            ..Default::default()
        },
        None,
    )
    .await;
    tokio::time::timeout(SCENARIO_TIMEOUT, recall_gateway)
        .await
        .unwrap()
        .unwrap();
    let recalled = sessions
        .read_snapshot(&session_key("memory-recall-entry"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest_committed_answer(&recalled).as_deref(),
        Some("memory-recalled")
    );
    db.close().await.unwrap();
}

pub(super) fn memory_sse(delta: serde_json::Value, reason: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices":[{"delta":delta,"finish_reason":null}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
    )
}

#[actix_web::test]
async fn production_input_entry_freezes_objects_and_rejects_changed_retry_without_model_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let capture = actix_web::rt::spawn(capture_one_openai_request(listener));
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 128_000,
        ..Default::default()
    };
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(&db, config).await.unwrap();
    let client_id = "input-entry";
    let run_id = derive_conversation_key("1", "device", Some(client_id), "unused");
    let update = AiAssistantObjectContextUpdate {
        conversation_id: client_id.into(),
        client_request_id: "original-object".into(),
        operation: AiAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: ObjectRef {
                token: "opaque-original-file".into(),
                snapshot_id: "worker".into(),
                object_kind: ObjectKind::TerminalOutput,
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
            },
            display_summary: "not-file-type-authority".into(),
        },
    };
    apply_object_context_update(
        db.clone(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        "device".into(),
        &update,
    )
    .await
    .unwrap();
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client_id.into()), AgentSessionSurface::AiAssistant);
    let before = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    let selected = before.context_attachments[0].clone();
    let ask = AiAssistantAsk {
        client_timezone: "UTC".into(),
        temporary_conversation: None,
        question: "original requirement".into(),
        client_message_id: "original-message".into(),
        conversation_id: Some(client_id.into()),
        selected_attachment_ids: vec![selected.attachment_id.clone()],
        ..Default::default()
    };
    let connections = web::Data::new(SharedConnectionMap::new());
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "transport".into(),
            "controller".into(),
            "offline-host".into(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            "device".into(),
            ask.clone(),
            None,
        ),
    )
    .await
    .unwrap();
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert_eq!(snapshot.input_revision, 1);
    assert_eq!(snapshot.handled_input_seq, 1, "{snapshot:?}");
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("captured-ok")
    );
    let body = tokio::time::timeout(MODEL_IO_TIMEOUT, capture)
        .await
        .unwrap()
        .unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("original requirement"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["max_tokens"],
        128_000
    );
    assert!(!body.contains("opaque-original-file"));
    let events = SignalAgentRunEventStore::new(db.clone());
    let frozen = events
        .original_read_context(
            InputSubject {
                run_id: &run_id,
                actor_id: "1",
                device_id: "device",
                client_conversation_id: Some(client_id),
            },
            1,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frozen.object_attachments, [selected]);
    events
        .validate_object_read(
            InputSubject {
                run_id: &run_id,
                actor_id: "1",
                device_id: "device",
                client_conversation_id: Some(client_id),
            },
            1,
            &frozen,
            &frozen.object_attachments[0].envelope.allowed_destinations[0],
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let original_events = events.user_followups_after(&run_id, 0, 10).await.unwrap();
    let saved = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    for mutation in 0..3 {
        let mut retry = ask.clone();
        match mutation {
            0 => {} // Exact retry returns the original settled answer.
            1 => retry.question = "changed requirement".into(),
            _ => retry.selected_attachment_ids.clear(),
        }
        tokio::time::timeout(
            SCENARIO_TIMEOUT,
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("retry-{mutation}"),
                "controller".into(),
                "offline-host".into(),
                crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                "device".into(),
                retry,
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            events.user_followups_after(&run_id, 0, 10).await.unwrap(),
            original_events
        );
        assert_eq!(
            crate::entity::agent_session::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            saved
        );
    }
    let receipts = crate::entity::model_egress_receipt::Entity::find()
        .all(&db)
        .await
        .unwrap();
    // The closed synthetic gateway also records one naming attempt. Exact
    // transport replay above must not create another main or title call.
    assert_eq!(receipts.len(), 2);
    db.close().await.unwrap();
}

mod retention;

mod schedule_input;
