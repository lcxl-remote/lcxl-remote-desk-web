use super::*;
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
) -> (Fixture, ModelEgressPolicy) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(&db, config.clone())
        .await
        .unwrap();
    let mut f = Fixture::new(db).await;
    let policy = ModelEgressPolicy {
        destination: config.destination_identity().unwrap(),
        selected_source_tools: [f.call.name.clone()].into_iter().collect(),
        export_authorization_id: "original-owner-selected-context".into(),
        now_unix_ms: Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        omit_finite_retention_historical_turns: false,
    };
    let mut user = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "input-1".into(),
        "explain the original background result".into(),
        policy.destination.clone(),
    )
    .unwrap();
    user.turn_id = Some("turn-1".into());
    let mut proposal = f.session.conversation[1].clone();
    use desk_diagnose_core::seam::ModelSeam;
    let context = crate::model_dial::SignalModelSeam::from_config(config)
        .unwrap()
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
        actor_id: "actor-1".into(),
        device_id: "device-1".into(),
        surface: AgentSessionSurface::DeviceAssistant,
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
    (f, policy)
}

#[actix_web::test]
async fn production_publisher_uses_original_export_and_strict_model_before_network() {
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
        "model-claim-race",
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
        let (f, mut policy) = fixture(&config, case == "secret").await;
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
            .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .unwrap();
        let allowed = matches!(case, "running" | "crash");
        if case == "model-claim-race" {
            f.store.db.execute_unprepared(
                "CREATE TRIGGER rotate_model_on_automatic_claim AFTER UPDATE ON agent_session
                 WHEN json_extract(NEW.state_json, '$.automation_turns_used') > json_extract(OLD.state_json, '$.automation_turns_used')
                 BEGIN UPDATE model_provider SET profile_revision = profile_revision + 1; END;"
            ).await.unwrap();
        }
        let capture = actix_web::rt::spawn(capture(listener));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.store.publish_computer_results_once(),
        )
        .await
        .expect(case)
        .unwrap();
        let row = agent_session::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap();
        let after = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        if case == "model-claim-race" {
            assert_eq!(after.automation_turns_used, 1);
            assert_eq!(
                crate::model_provider::load(&f.store.db)
                    .await
                    .unwrap()
                    .profile_revision,
                config.profile_revision + 1
            );
        }
        let receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(&f.store.db)
            .await
            .unwrap();
        if allowed {
            assert!(
                capture.is_finished() || !receipts.is_empty(),
                "{case}: turn={:?} pending={:?} tail={:?}",
                after.turn_state,
                after.pending_auto_triggers,
                after.conversation.last()
            );
            let body = tokio::time::timeout(std::time::Duration::from_secs(2), capture)
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
        assert!(after.pending_auto_triggers.is_empty(), "{case}");
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
        f.store.publish_computer_results_once().await.unwrap();
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
