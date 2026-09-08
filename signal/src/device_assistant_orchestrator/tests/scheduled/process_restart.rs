//! Real signaling process recovery using a durable management-created timer.
use super::*;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start(executable: &str, config: &Path, port: u16, log: &Path) -> Server {
    let output = std::fs::File::create(log).unwrap();
    let mut server = Server(
        Command::new(executable)
            .args(["--startup-mode", "signaling", "--config-file-path"])
            .arg(config)
            .env("TMPDIR", config.parent().unwrap())
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited: {}",
            std::fs::read_to_string(log).unwrap()
        );
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return server;
        }
        assert!(
            Instant::now() < deadline,
            "startup timeout; see {}",
            log.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[actix_web::test]
#[ignore = "requires LRD_TEST_SERVER_BINARY pointing to the current built OSS server"]
async fn real_process_restart_materializes_timer_and_preserves_offline_wait() {
    exercise_process(false).await;
}

#[actix_web::test]
#[ignore = "requires current LRD_TEST_SERVER_BINARY and loopback networking"]
async fn real_process_restart_resumes_same_conversation_after_host_reconnects() {
    exercise_process(true).await;
}

async fn exercise_process(reconnect: bool) {
    let executable = std::env::var("LRD_TEST_SERVER_BINARY").expect("current OSS server binary");
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let gateway = actix_web::rt::spawn(async move {
        let first = capture_one_openai_request_with_sse(listener.clone(), ANSWER).await;
        let second = if reconnect {
            Some(capture_one_openai_request_with_sse(listener, ANSWER).await)
        } else {
            None
        };
        (first, second)
    });
    let durable = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        durable.path().join("desk_signal.db").display()
    );
    let db = Database::connect(&database_url).await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new());
    let client_id = "scheduled-original";
    let conversation = derive_conversation_key("1", "device", Some(client_id), "first");
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "first".into(),
            "controller".into(),
            "offline-host".into(),
            1,
            "device".into(),
            DeviceAssistantAsk {
                question: "SYNTHETIC_SCHEDULED_ORIGINAL".into(),
                client_message_id: "original-message".into(),
                conversation_id: Some(client_id.into()),
                ..Default::default()
            },
            None,
        ),
    )
    .await
    .unwrap();
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&conversation))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let draft = ScheduleDraft {
        time_confirmation: None,
        client_create_key: "schedule-original".into(),
        kind: ScheduledTaskKind::ConversationResume,
        target_device_id: "device".into(),
        title: "Continue".into(),
        prompt: "Continue".into(),
        locale: None,
        model_id: None,
        spec: ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: (chrono::Utc::now() + chrono::Duration::seconds(15))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        },
        source_conversation_id: Some(client_id.into()),
        requirement_revision: Some(1),
        creation_source: ScheduleCreationSource::Manual,
    };
    use desk_agent_protocol::schedule::management::{
        ScheduleManagementRequest as Request, ScheduleManagementResponse as Response,
    };
    let Response::Task { task: draft_task } =
        crate::schedule_management::manage(&db, 1, Request::CreateDraft { draft })
            .await
            .unwrap()
    else {
        panic!("expected draft")
    };
    assert_eq!(draft_task.status, ScheduledTaskStatus::Draft);
    let Response::Task { task: active } = crate::schedule_management::manage(
        &db,
        1,
        Request::ActivateConversationResume {
            schedule_id: draft_task.schedule_id,
            expected_revision: draft_task.revision,
        },
    )
    .await
    .unwrap() else {
        panic!("expected active task")
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config = durable.path().join("config.toml");
    std::fs::write(&config, format!(
        "[system]\nport = {port}\nlisten_addr_ipv4 = \"127.0.0.1\"\nenable_ipv6 = false\ntelemetry_consent = false\nlocal_signaling_token = \"process-test-node-token\"\n[turn]\nenable_turn = false\n[device_assistant]\nenabled = true\nrevision = 1\n"
    )).unwrap();
    let first = start(
        &executable,
        &config,
        port,
        &durable.path().join("first.log"),
    )
    .await;
    let store = ScheduleStore::new(db.clone());
    let task = store.read(1, &active.schedule_id).await.unwrap();
    assert!(store.database_time().await.unwrap() < task.next_run_at.unwrap());
    drop(first);
    let second = start(
        &executable,
        &config,
        port,
        &durable.path().join("second.log"),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(25);
    let waiting = loop {
        let runs = crate::entity::agent_schedule_run::Entity::find()
            .filter(crate::entity::agent_schedule_run::Column::ScheduleId.eq(&active.schedule_id))
            .all(&db)
            .await
            .unwrap();
        assert!(runs.len() <= 1);
        if let Some(run) = runs.into_iter().next() {
            if run.status == "waiting_device" {
                break run;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timer did not reach offline wait"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(waiting.conversation_id, conversation);
    assert_eq!(waiting.attempt, 0);
    assert!(waiting.started_at.is_none());
    drop(second);
    let third = start(
        &executable,
        &config,
        port,
        &durable.path().join("third.log"),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let runs = crate::entity::agent_schedule_run::Entity::find()
        .filter(crate::entity::agent_schedule_run::Column::ScheduleId.eq(&active.schedule_id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, waiting.run_id);
    assert_eq!(runs[0].status, "waiting_device");
    assert_eq!(runs[0].attempt, 0);
    let restored = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&conversation))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.state_json, row.state_json);
    if reconnect {
        use desk_signal_facade::model::{
            signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
            version::VersionInfo,
        };
        use futures_util::{SinkExt, StreamExt};
        let mut version = VersionInfo::new(
            1,
            1,
            "process-test".into(),
            RemoteDeskTypeEnum::Server,
            None,
            Some("device".into()),
        );
        version.token = Some("process-test-node-token".into());
        let query = serde_urlencoded::to_string(&version).unwrap();
        let (_, mut socket) = awc::Client::default()
            .ws(format!("ws://127.0.0.1:{port}/api/desk/signaling?{query}"))
            .connect()
            .await
            .unwrap();
        let now = chrono::Utc::now();
        let readiness = desk_agent_protocol::computer_use::ComputerUseReadiness {
            schema_version: desk_agent_protocol::computer_use::COMPUTER_USE_SCHEMA_VERSION,
            revision: 1,
            observed_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
            server_api_version: 1,
            os: "fixture".into(),
            interactive_session_incarnation: "reconnected-worker".into(),
            local_ceiling_revision: 1,
            capabilities: vec![],
            context_references: vec![],
        };
        let frame = SignalingModel::new(
            "reconnected-readiness",
            SignalingType::ComputerUseReadinessUpdated,
            None,
            None,
            Some(serde_json::to_value(readiness).unwrap()),
            None,
        );
        socket
            .send(awc::ws::Message::Text(
                serde_json::to_string(&frame).unwrap().into(),
            ))
            .await
            .unwrap();
        let socket_task = actix_web::rt::spawn(async move {
            while let Some(Ok(frame)) = socket.next().await {
                if let awc::ws::Frame::Ping(bytes) = frame {
                    socket.send(awc::ws::Message::Pong(bytes)).await.unwrap();
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let run = crate::entity::agent_schedule_run::Entity::find_by_id(waiting.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            if run.status == "succeeded" {
                assert_eq!(run.attempt, 1);
                assert_eq!(run.conversation_id, conversation);
                assert!(run.result_ref.is_some());
                break;
            }
            let terminal = if run.status == "failed" {
                let current = crate::entity::agent_session::Entity::find_by_id(row.id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                PersistedAgentSession::decode_json(&current.state_json)
                    .unwrap()
                    .terminal_error
            } else {
                None
            };
            assert!(
                run.status != "failed" && Instant::now() < deadline,
                "reconnected timer status: {}, error: {:?}, terminal: {:?}, process: {}",
                run.status,
                run.error_kind,
                terminal,
                std::fs::read_to_string(durable.path().join("third.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        socket_task.abort();
    }
    let (_, second_request) = tokio::time::timeout(Duration::from_secs(5), gateway)
        .await
        .unwrap()
        .unwrap();
    if reconnect {
        assert!(
            String::from_utf8_lossy(&second_request.unwrap())
                .contains("SYNTHETIC_SCHEDULED_ORIGINAL")
        );
        let final_row = crate::entity::agent_session::Entity::find_by_id(row.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let before = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        let after = PersistedAgentSession::decode_json(&final_row.state_json).unwrap();
        assert_eq!(after.input_revision, before.input_revision);
        assert_eq!(after.latest_input_seq, before.latest_input_seq);
    }
    drop(third);
    if let Ok(destination) = std::env::var("LRD_TEST_BROWSER_FIXTURE_PATH") {
        // Keep the authenticated runtime fixture for manual browser acceptance.
        // Ordinary test runs retain automatic temporary-directory cleanup.
        db.close().await.unwrap();
        let path = durable.keep();
        std::fs::write(destination, path.to_string_lossy().as_bytes()).unwrap();
    }
}
