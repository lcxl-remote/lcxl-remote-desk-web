use super::*;
use desk_diagnose_core::test_timing::{MODEL_IO_TIMEOUT, SCENARIO_TIMEOUT};
use desk_diagnose_core::{
    chat::{ChatRole, ModelTurn},
    model_egress::ModelEgressPolicy,
    session::{ActionIdentity, ExecutionState, TurnState, WorkKind},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn capture(listener: TcpListener) -> String {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut raw = Vec::new();
    let mut buffer = [0; 8192];
    let (header, length) = loop {
        let n = socket.read(&mut buffer).await.unwrap();
        assert!(n > 0);
        raw.extend_from_slice(&buffer[..n]);
        if let Some(index) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&raw[..index])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (index + 4, length);
        }
    };
    while raw.len() < header + length {
        let n = socket.read(&mut buffer).await.unwrap();
        assert!(n > 0);
        raw.extend_from_slice(&buffer[..n]);
    }
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Completion explained without tools.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.shutdown().await.unwrap();
    String::from_utf8(raw[header..header + length].to_vec()).unwrap()
}

async fn set_session(f: &Fixture, session: &PersistedAgentSession) {
    let row = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut row: agent_session::ActiveModel = row.into();
    row.state_json = Set(session.encode_json_for_storage().unwrap());
    row.version = Set(session.version);
    row.lease_token = Set(session.lease_token as i64);
    row.update(&f.store.db).await.unwrap();
}

async fn fixture(
    config: &crate::model_provider::ModelProviderConfig,
    secret: bool,
    database_url: &str,
) -> (Fixture, ModelEgressPolicy) {
    let db = crate::config::test_support::Database::connect(database_url)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(&db, config.clone())
        .await
        .unwrap();
    let config = crate::model_provider::load(&db).await.unwrap();
    let mut user = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "input-1".into(),
        "explain the original background result".into(),
        config.destination_identity().unwrap(),
    )
    .unwrap();
    user.turn_id = Some("turn-1".into());
    if secret {
        user.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret;
    }
    let mut f =
        Fixture::new_with_source(db, "1", false, false, false, None, Some(user.clone())).await;
    let policy = ModelEgressPolicy {
        destination: config.destination_identity().unwrap(),
        selected_source_tools: [f.call.name.clone()].into_iter().collect(),
        export_authorization_id: "original-owner-selected-context".into(),
        now_unix_ms: Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };

    let mut proposal = f.session.conversation[1].clone();
    use desk_diagnose_core::seam::ModelSeam;
    let context = crate::model_dial::SignalModelSeam::from_config(&config)
        .unwrap()
        .with_context_db(f.store.db.clone())
        .context_policy(desk_diagnose_core::model_capability::ModelRequirements::TEXT_ONLY)
        .await
        .unwrap();
    proposal.replay_disposition =
        Some(desk_diagnose_core::replay::ReplayDisposition::NotRequired {
            source_context_key: context.source_context_key,
        });
    let turn = ModelTurn {
        text: proposal.text.clone(),
        tool_calls: vec![f.call.clone()],
        ..Default::default()
    };
    proposal.data_envelope = Some(
        policy
            .derive_model_output_envelope(&turn, &[user.data_envelope.clone().unwrap()])
            .unwrap(),
    );
    if secret {
        user.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret;
        proposal.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret;
    }
    f.session.conversation = vec![user, proposal];
    use desk_diagnose_core::context_attachment::*;
    let now = Utc::now().timestamp_millis() as u64;
    f.session.context_attachments.push(ContextAttachment {
        schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: "selected-context".into(),
        client_request_id: "select-context".into(),
        actor_id: "1".into(),
        device_id: "device-1".into(),
        surface: AgentSessionSurface::AiAssistant,
        kind: ContextAttachmentKind::InteractiveSession,
        object_ref: AttachmentObjectRef {
            opaque_token: "context-token".into(),
            object_incarnation: "browser-session".into(),
            source_provider_id: "browser.page.open".into(),
            source_capability_id: "browser.page.open".into(),
        },
        bounds: AttachmentBounds {
            max_bytes: 1024,
            max_objects: 1,
        },
        display_summary: "synthetic selected context".into(),
        created_at_unix_ms: now,
        expires_at_unix_ms: now + 240_000,
        envelope: f.session.conversation[0].data_envelope.clone().unwrap(),
        state: AttachmentState::Active,
    });
    set_session(&f, &f.session).await;
    if !secret {
        // The tool proposal must have the same original receipt and accepted
        // output provenance as a real model call, rather than only a derived label.
        use crate::assistant_data_store::model_gate;
        use crate::model_egress_store::SignalModelEgressStore;
        use desk_diagnose_core::{
            prompt::ResponseFormatSpec,
            seam::{ModelRequest, SessionSeam},
        };
        use sea_orm::TransactionTrait;
        let mut request = ModelRequest::text_only(
            vec![f.session.conversation[0].clone()],
            ResponseFormatSpec::None,
        );
        request.context_conversation_id = Some(f.session.conversation_id.clone());
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(f.store.db.clone())
            .with_client_metadata(None, AgentSessionSurface::AiAssistant);
        request.assistant_data = sessions
            .assistant_data_request_manifest(&f.session, &request, now)
            .await
            .unwrap()
            .map(Box::new);
        let authorized = policy.authorize_request(request).unwrap();
        let txn = f.store.db.begin().await.unwrap();
        let prepared = model_gate::prepare_on(&txn, &authorized.request)
            .await
            .unwrap()
            .unwrap();
        SignalModelEgressStore::record_snapshot_intent_on(
            &txn,
            "original-proposal".into(),
            policy.export_authorization_id.clone(),
            1,
            &authorized.input_snapshot().unwrap(),
        )
        .await
        .unwrap();
        SignalModelEgressStore::link_context_dispatch_on(
            &txn,
            "original-proposal",
            &f.session.conversation_id,
            false,
        )
        .await
        .unwrap();
        model_gate::authorize_on(
            &txn,
            Some(&prepared),
            &authorized.request,
            "original-proposal",
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
        SignalModelEgressStore::new(f.store.db.clone())
            .mark_succeeded(
                "original-proposal",
                f.session.conversation[1].data_envelope.as_ref().unwrap(),
            )
            .await
            .unwrap();
    }
    (f, policy)
}

async fn publish_as_runtime_task(db: crate::config::connection::DatabaseConnection) {
    actix_web::rt::spawn(async move {
        let store = SignalCapabilityGrantStore::new(db);
        let execution = store.publish_computer_results_once();
        assert!(std::mem::size_of_val(&execution) <= 16 * 1024);
        execution.await.unwrap();
    })
    .await
    .unwrap();
}

#[actix_web::test]
async fn background_result_publication_fits_production_thread_stack() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        exercise(Some(bytes)).await;
    }
}

#[actix_web::test]
async fn production_publisher_uses_original_export_and_strict_model_before_network() {
    exercise(None).await;
}

async fn exercise(stack_bytes: Option<usize>) {
    crate::ai_assistant_gate::global_ai_assistant_gate().replace(
        desk_agent_protocol::ai_assistant::AiAssistantSettings {
            revision: 1,
            enabled: true,
        },
    );
    for case in [
        "running",
        "crash",
        "secret",
        "unselected",
        "changed-model",
        "new-input",
        "missing-export",
        "detached",
        "narrowed-policy",
        "expired-label",
        "legacy-replay",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("synthetic-completion".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-never-a-real-key".into()),
            max_context_bytes: Some(131072),
            ..Default::default()
        };
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("completion.sqlite").display()
        );
        let (f, mut policy) = fixture(&config, case == "secret", &url).await;
        if case == "unselected" {
            policy.selected_source_tools.clear();
        }
        let selection = (case != "missing-export").then_some(&policy);
        f.store
            .bind_computer_transport("host-original", &f.plan, &f.session, &f.call, selection)
            .await
            .unwrap();
        let first_binding = f.outbox().await;
        f.store
            .bind_computer_transport("host-original", &f.plan, &f.session, &f.call, selection)
            .await
            .unwrap();
        assert_eq!(
            first_binding,
            f.outbox().await,
            "selection binding must be idempotent"
        );
        f.accept().await.unwrap();
        age(&f, 9).await;
        assert!(promote(&f).await);
        let mut session = f.session.clone();
        if case != "crash" {
            let mut running = ChatMessage::background_task_running(
                "running",
                &f.call.id,
                &f.plan.action_request_id,
            );
            running.turn_id = Some("turn-1".into());
            running.data_envelope =
                desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                    session.conversation[1].data_envelope.as_ref(),
                    &f.call.id,
                    &running.text,
                    "provider_execution_status",
                )
                .unwrap();
            session.conversation.push(running);
            session.execution_state = ExecutionState::Executing {
                action: ActionIdentity::new(
                    f.plan.work_id.parse().unwrap(),
                    &f.plan.action_request_id,
                    &f.plan.execution_generation,
                    WorkKind::ComputerAction,
                ),
            };
            session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
        }
        if case == "new-input" {
            session.input_revision += 1;
            session
                .begin_focus_epoch(session.input_revision, std::iter::empty::<String>())
                .unwrap();
        }
        if case == "detached" {
            assert!(session.detach_context("selected-context"));
        }
        if case == "narrowed-policy" {
            session.policy_revision += 1;
        }
        if case == "expired-label" {
            session.conversation[0]
                .data_envelope
                .as_mut()
                .unwrap()
                .retention
                .expires_at_unix_ms = Some(1);
        }
        if case == "legacy-replay" {
            session.conversation[1].replay_disposition =
                Some(desk_diagnose_core::replay::ReplayDisposition::legacy_unknown());
            // Keep the original result inside this legacy tool group so a
            // replay filter cannot leave an independently readable tail.
            session.execution_state = ExecutionState::OutcomeUnknown {
                action: ActionIdentity::new(
                    f.plan.work_id.parse().unwrap(),
                    &f.plan.action_request_id,
                    &f.plan.execution_generation,
                    WorkKind::ComputerAction,
                ),
                placeholder_message_id: "running".into(),
                since: Utc::now().to_rfc3339(),
            };
        }
        set_session(&f, &session).await;
        if case == "crash" {
            // A real owner claim has a live lease. Process loss becomes
            // recoverable only after that original planner lease expires.
            agent_session::Entity::update_many()
                .set(agent_session::ActiveModel {
                    lease_deadline: Set(Some(Utc::now() - chrono::Duration::seconds(1))),
                    ..Default::default()
                })
                .filter(agent_session::Column::ConversationId.eq(&f.session.conversation_id))
                .filter(agent_session::Column::ActorId.eq(&f.session.actor_id))
                .exec(&f.store.db)
                .await
                .unwrap();
        }
        if case == "changed-model" {
            let mut changed = config.clone();
            changed.profile_revision += 1;
            crate::model_provider::save(&f.store.db, changed)
                .await
                .unwrap();
        }
        f.store
            .accept_computer_completion(
                "host-original",
                "device-1",
                &f.plan.execution_generation,
                &completion::verified(&f.plan),
            )
            .await
            .unwrap();
        let original = f
            .store
            .read_computer_result(&f.plan.execution_generation, "run-1", "1", "device-1")
            .await
            .unwrap()
            .unwrap();
        let original_receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&f.store.db)
            .await
            .unwrap();
        let allowed = matches!(case, "running" | "crash" | "expired-label");
        let capture = actix_web::rt::spawn(capture(listener));
        tokio::time::timeout(
            // This path performs real loopback model I/O for every fixture.
            // Leave enough headroom when the full crate runs in parallel on
            // a loaded macOS builder; semantic deadlines are asserted by the
            // production store rather than this test harness watchdog.
            SCENARIO_TIMEOUT,
            async {
                if let Some(bytes) = stack_bytes {
                    let db = f.store.db.clone();
                    let (sent, done) = tokio::sync::oneshot::channel();
                    let thread = std::thread::Builder::new()
                        .name("background-result-stack".into())
                        .stack_size(bytes)
                        .spawn(move || {
                            actix_web::rt::System::new().block_on(async move {
                                publish_as_runtime_task(db).await;
                                sent.send(()).unwrap();
                            })
                        })
                        .unwrap();
                    done.await.unwrap();
                    thread.join().unwrap();
                } else {
                    publish_as_runtime_task(f.store.db.clone()).await;
                }
            },
        )
        .await
        .expect(case);
        let row = agent_session::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap();
        let after = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        let all_receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&f.store.db)
            .await
            .unwrap();
        assert!(
            original_receipts
                .iter()
                .all(|row| all_receipts.contains(row))
        );
        let receipts: Vec<_> = all_receipts
            .into_iter()
            .filter(|row| {
                !original_receipts
                    .iter()
                    .any(|original| original.receipt_id == row.receipt_id)
            })
            .collect();
        if allowed {
            assert_eq!(after.turn_state, TurnState::Idle, "{case}");
            assert!(
                after.terminal_error.is_none(),
                "{case}: {:?}",
                after.terminal_error
            );
            assert!(
                capture.is_finished() || !receipts.is_empty(),
                "{case}: turn={:?} error={:?} pending={:?} tail={:?}",
                after.turn_state,
                after.terminal_error,
                after.pending_auto_triggers,
                after.conversation.last()
            );
            let body = tokio::time::timeout(MODEL_IO_TIMEOUT, capture)
                .await
                .expect(case)
                .unwrap();
            assert!(body.contains("page_opened"), "{case}: {body}");
            assert!(!body.contains("test-only-never-a-real-key"));
            assert!(!body.contains("data_envelope"));
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                json.get("tools")
                    .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
            );
            assert_eq!(receipts.len(), 1, "{case}");
            assert_eq!(
                receipts[0].state,
                crate::model_egress_store::STATE_SUCCEEDED
            );
            let answer = after
                .conversation
                .iter()
                .find(|message| {
                    message.role == ChatRole::Assistant
                        && message.text == "Completion explained without tools."
                })
                .expect(case);
            assert!(answer.data_envelope.is_some());
        } else {
            assert!(receipts.is_empty(), "{case}: unexpected network audit");
            assert!(!capture.is_finished(), "{case}: unexpected network request");
            capture.abort();
            let _ = capture.await;
        }
        let delivered = after
            .conversation
            .iter()
            .find(|message| message.message_id == original.work.completion_event_id)
            .expect(case);
        assert_eq!(delivered.text, original.output.content);
        assert_eq!(
            delivered.data_envelope.as_ref(),
            Some(&original.receipt.envelope)
        );
        assert_eq!(row.version, after.version);
        assert!(
            after.pending_auto_triggers.is_empty(),
            "{case}: turn={:?}, pending={:?}",
            after.turn_state,
            after.pending_auto_triggers
        );
        assert_eq!(
            agent_action_item::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap()
                .unwrap()
                .completion_delivery_state,
            "consumed",
            "{case}"
        );
        publish_as_runtime_task(f.store.db.clone()).await;
        assert_eq!(
            row,
            agent_session::Entity::find()
                .one(&f.store.db)
                .await
                .unwrap()
                .unwrap(),
            "{case}: duplicate changed session"
        );
    }
}
