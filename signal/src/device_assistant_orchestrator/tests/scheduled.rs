//! Actual model composition with durable input and an atomically claimed timer.
use super::*;
use crate::schedule_store::{ContinuationClaim, ScheduleStore};
use desk_agent_protocol::schedule::*;
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

const ANSWER: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"captured-ok\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
    "data: [DONE]\n\n"
);

#[actix_web::test]
async fn scheduled_composition_reuses_claim_and_original_input_without_a_new_user_event() {
    exercise(false, false, false, false, false).await;
}

#[actix_web::test]
async fn scheduled_composition_rejects_cancelled_claim_before_second_model_request() {
    exercise(true, false, false, false, false).await;
}

#[actix_web::test]
async fn scheduled_composition_persisted_model_failure_settles_once() {
    exercise(false, true, false, false, false).await;
}

#[actix_web::test]
async fn scheduled_composition_after_owner_decision_preserves_run_and_uses_decision_bridge() {
    exercise(false, false, true, false, false).await;
}

#[actix_web::test]
async fn scheduled_failed_permission_turn_settles_original_decision() {
    exercise(false, true, true, false, false).await;
}

#[actix_web::test]
async fn scheduled_second_permission_wait_settles_previous_decision_atomically() {
    exercise(false, false, true, true, false).await;
}

async fn exercise(
    cancel_before_resume: bool,
    fail_model: bool,
    permission: bool,
    next_permission: bool,
    dispatch: bool,
) {
    exercise_with_restart(
        cancel_before_resume,
        fail_model,
        permission,
        next_permission,
        dispatch,
        false,
        false,
    )
    .await;
}

async fn exercise_with_restart(
    cancel_before_resume: bool,
    fail_model: bool,
    permission: bool,
    next_permission: bool,
    dispatch: bool,
    restart: bool,
    newer_input: bool,
) {
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let gateway = actix_web::rt::spawn(async move {
        let first = capture_one_openai_request_with_sse(listener.clone(), ANSWER).await;
        if newer_input {
            let chat = capture_one_openai_request_with_sse(listener.clone(), ANSWER).await;
            assert!(String::from_utf8_lossy(&chat).contains("SYNTHETIC_REPLACEMENT_REQUIREMENT"));
        }
        let second = capture_one_openai_request_with_sse(
            listener,
            if fail_model {
                "data: {invalid-json}\n\ndata: [DONE]\n\n"
            } else {
                ANSWER
            },
        )
        .await;
        (first, second)
    });
    let durable = restart.then(|| tempfile::tempdir().unwrap());
    let database_url = durable.as_ref().map_or_else(
        || "sqlite::memory:".into(),
        |directory| {
            format!(
                "sqlite://{}?mode=rwc",
                directory.path().join("schedule.sqlite").display()
            )
        },
    );
    let mut db = Database::connect(&database_url).await.unwrap();
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
    let original = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let store = ScheduleStore::new(db.clone());
    let draft = ScheduleDraft {
        time_confirmation: None,
        client_create_key: "schedule-original".into(),
        kind: ScheduledTaskKind::ConversationResume,
        target_device_id: "device".into(),
        title: "Continue".into(),
        prompt: "APPROVED_TASK_SEND_HELLO".into(),
        locale: None,
        model_id: None,
        spec: ScheduleSpec {
            schema_version: 1,
            rule: ScheduleRule::Once {
                at: (chrono::Utc::now() + chrono::Duration::seconds(3))
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
    assert_eq!(draft_task.status, ScheduledTaskStatus::Active);
    let active = draft_task;
    let task = store.read(1, &active.schedule_id).await.unwrap();
    drop(store);
    if restart {
        db.close().await.unwrap();
        db = Database::connect(&database_url).await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let restored = crate::entity::agent_session::Entity::find()
            .filter(crate::entity::agent_session::Column::ConversationId.eq(&conversation))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let restored_session = PersistedAgentSession::decode_json(&restored.state_json).unwrap();
        assert_eq!(restored_session.input_revision, original.input_revision);
        assert_eq!(
            &restored_session.conversation[..original.conversation.len()],
            &original.conversation
        );
        assert!(
            restored_session
                .conversation
                .last()
                .unwrap()
                .text
                .contains("scheduled_task_activated")
        );
    }
    let store = ScheduleStore::new(db.clone());
    assert_eq!(store.read(1, &active.schedule_id).await.unwrap(), task);
    let wait = task.next_run_at.unwrap() - store.database_time().await.unwrap() + 20;
    tokio::time::sleep(std::time::Duration::from_millis(wait.max(0) as u64)).await;
    let run = store
        .materialize_due(&task.schedule_id, task.revision)
        .await
        .unwrap()
        .unwrap();
    if newer_input {
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "newer-input".into(),
            "controller".into(),
            "offline-host".into(),
            1,
            "device".into(),
            DeviceAssistantAsk {
                question: "SYNTHETIC_REPLACEMENT_REQUIREMENT".into(),
                client_message_id: "replacement-message".into(),
                conversation_id: Some(client_id.into()),
                ..Default::default()
            },
            None,
        )
        .await;
    }
    if dispatch {
        let host = format!("schedule-dispatch-{}", uuid::Uuid::new_v4());
        let request = actix_web::test::TestRequest::get()
            .insert_header(("upgrade", "websocket"))
            .insert_header(("connection", "upgrade"))
            .insert_header(("sec-websocket-version", "13"))
            .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
            .to_http_request();
        let payload = <actix_web::web::Payload as actix_web::FromRequest>::from_request(
            &request,
            &mut actix_web::dev::Payload::None,
        )
        .await
        .unwrap();
        let (_response, socket, _stream) = actix_ws::handle(&request, payload).unwrap();
        use desk_signal_facade::model::{
            auth_context::AuthContext, connection::ConnectionModel, signal::RemoteDeskTypeEnum,
            version::VersionInfo,
        };
        let peer = ConnectionState {
            model: ConnectionModel {
                connection_id: host.clone(),
                ip: None,
                version_info: VersionInfo::new(
                    1,
                    1,
                    "fixture".into(),
                    RemoteDeskTypeEnum::Server,
                    None,
                    Some("device".into()),
                ),
                device_id: None,
                owner_node_id: None,
            },
            session: std::sync::Arc::new(tokio::sync::RwLock::new(socket)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: None,
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        };
        let gate = std::sync::Arc::new(crate::device_assistant_gate::DeviceAssistantGate::new(
            desk_agent_protocol::device_assistant::DeviceAssistantSettings {
                enabled: true,
                revision: 1,
            },
        ));
        let executor = crate::schedule_executor::SignalScheduleExecutor::new(
            db.clone(),
            connections.clone(),
            gate.clone(),
        );
        gate.replace(
            desk_agent_protocol::device_assistant::DeviceAssistantSettings {
                enabled: false,
                revision: 2,
            },
        );
        assert_eq!(executor.scan_once(0).await.unwrap().deferred, 1);
        gate.replace(
            desk_agent_protocol::device_assistant::DeviceAssistantSettings {
                enabled: true,
                revision: 3,
            },
        );
        assert_eq!(executor.scan_once(0).await.unwrap().deferred, 1);
        let waiting = crate::entity::agent_schedule_run::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(waiting.status, "waiting_device");
        assert_eq!(waiting.attempt, 0);
        connections.write().await.insert(host.clone(), peer);
        assert_eq!(executor.scan_once(0).await.unwrap().deferred, 1);
        let now = chrono::Utc::now();
        let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
        cache
            .update(
                &host,
                desk_agent_protocol::computer_use::ComputerUseReadiness {
                    schema_version: desk_agent_protocol::computer_use::COMPUTER_USE_SCHEMA_VERSION,
                    revision: 1,
                    observed_at: now.to_rfc3339(),
                    expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                    server_api_version: 1,
                    os: "fixture".into(),
                    interactive_session_incarnation: "worker".into(),
                    local_ceiling_revision: 1,
                    capabilities: vec![],
                    context_references: vec![],
                },
                now,
            )
            .unwrap();
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            tokio::join!(executor.scan_once(0), executor.scan_once(0))
        })
        .await
        .unwrap();
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.settled + b.settled, 1);
        assert_eq!(a.needs_reconciliation + b.needs_reconciliation, 0);
        assert_eq!(executor.scan_once(0).await.unwrap().scanned, 0);
        let work = crate::entity::agent_schedule_run::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.status, if fail_model { "failed" } else { "succeeded" });
        assert_eq!(work.attempt, 1);
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let after = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert_eq!(
            after.input_revision,
            original.input_revision + u64::from(newer_input)
        );
        assert_eq!(
            after.latest_input_seq,
            original.latest_input_seq + u64::from(newer_input)
        );
        let (_, body) = tokio::time::timeout(std::time::Duration::from_secs(5), gateway)
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("SYNTHETIC_SCHEDULED_ORIGINAL"));
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "user"
                    && message["content"].as_str().is_some_and(|text| {
                        text.contains("APPROVED_TASK_SEND_HELLO")
                            && text.contains("AUTOMATIC SERVER CONTROL EVENT")
                    })
            }),
            "messages={}",
            body["messages"]
        );
        cache.remove_connection(&host);
        connections.write().await.clear();
        return;
    }
    let mut claimed = store
        .claim_conversation_resume(ContinuationClaim {
            owner: 1,
            run_id: &run.run_id,
            node_id: "node",
            lease_seconds: 90,
            policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
            scope: original.scope_snapshot,
        })
        .await
        .unwrap();
    if cancel_before_resume {
        let current = store.read(1, &run.schedule_id).await.unwrap();
        crate::schedule_management::manage(
            &db,
            1,
            Request::Delete {
                schedule_id: current.schedule_id,
                expected_revision: current.revision,
            },
        )
        .await
        .unwrap();
        assert!(
            super::super::scheduled::prepare(&db, claimed, 90)
                .await
                .is_err()
        );
        assert!(
            !gateway.is_finished(),
            "second model request must not complete"
        );
        gateway.abort();
        assert!(gateway.await.unwrap_err().is_cancelled());
        let work = crate::entity::agent_schedule_run::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.status, "running");
        assert!(work.cancel_requested_at.is_some());
        assert!(!work.failure_accounted);
        return;
    }
    if permission {
        claimed = owner_decision(&db, &store, claimed).await;
        use crate::entity::agent_permission_resume as receipt;
        use sea_orm::sea_query::Expr;
        receipt::Entity::update_many()
            .col_expr(receipt::Column::State, Expr::value("settled"))
            .exec(&db)
            .await
            .unwrap();
        assert!(
            super::super::scheduled::prepare(
                &db,
                crate::schedule_store::ClaimedContinuation {
                    run: claimed.run.clone(),
                    session: claimed.session.clone(),
                },
                90
            )
            .await
            .is_err()
        );
        assert!(!gateway.is_finished());
        receipt::Entity::update_many()
            .col_expr(receipt::Column::State, Expr::value("started"))
            .exec(&db)
            .await
            .unwrap();
    }
    let claimed_epoch = claimed.run.lease_epoch;
    let token = claimed.session.lease_token;
    let mut prepared = super::super::scheduled::prepare(&db, claimed, 90)
        .await
        .unwrap();
    // The durable source wins even if an in-memory copy's transcript was changed.
    prepared.claimed.session.conversation.clear();
    let prepared = super::super::scheduled::prepare(&db, prepared.claimed, 90)
        .await
        .unwrap();
    assert!(!prepared.claimed.session.conversation.is_empty());
    assert_eq!(
        prepared.permission_request_id.as_deref(),
        permission.then_some("scheduled-permission")
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        compose_turn(
            connections,
            db.clone(),
            run.run_id.clone(),
            String::new(),
            "offline-host".into(),
            1,
            "device".into(),
            DeviceAssistantAsk {
                question: "MUST_NOT_REPLACE_INPUT".into(),
                client_message_id: "MUST_NOT_APPEND".into(),
                ..Default::default()
            },
            None,
            Some(prepared),
        ),
    )
    .await
    .unwrap();
    if fail_model {
        let error = result.unwrap_err();
        let held = || crate::schedule_store::ContinuationLease {
            owner: 1,
            run_id: &run.run_id,
            node_id: "node",
            run_epoch: claimed_epoch,
            session_token: token,
        };
        let mut wrong = error.clone();
        wrong.message.push_str("different error");
        assert!(
            store
                .finish_failed_continuation(held(), &wrong)
                .await
                .is_err()
        );
        assert!(
            store
                .finish_answered_continuation(held(), "captured-ok")
                .await
                .is_err()
        );
        let settled = store
            .finish_failed_continuation(held(), &error)
            .await
            .unwrap();
        assert_eq!(settled.status, "failed");
        if permission {
            assert_eq!(permission_receipt(&db).await.state, "settled");
        }
        assert!(settled.failure_accounted);
        assert_eq!(
            store
                .finish_failed_continuation(held(), &error)
                .await
                .unwrap(),
            settled
        );
        let task = crate::entity::agent_schedule::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json).unwrap();
        assert_eq!(failures.consecutive_failures, 1);
        assert!(failures.pause_reasons.is_empty());
        assert!(task.active_run_id.is_none());
        tokio::time::timeout(std::time::Duration::from_secs(5), gateway)
            .await
            .unwrap()
            .unwrap();
        return;
    }
    let result = result.unwrap().unwrap();
    assert!(matches!(result, LoopOutcome::Answered(_)), "{result:?}");
    let (_, second) = tokio::time::timeout(std::time::Duration::from_secs(5), gateway)
        .await
        .unwrap()
        .unwrap();
    let second = String::from_utf8(second).unwrap();
    assert!(second.contains("SYNTHETIC_SCHEDULED_ORIGINAL"));
    if permission {
        assert!(second.contains("PERMISSION DECISION RESUME"));
        assert!(!second.contains("continuation time has arrived"));
    }
    assert!(!second.contains("MUST_NOT_REPLACE_INPUT"));
    let row = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let after = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(after.input_revision, 1);
    assert_eq!(after.latest_input_seq, 1);
    assert_eq!(after.lease_token, token);
    assert_eq!(
        after
            .conversation
            .iter()
            .filter(
                |message| desk_diagnose_core::permission_resume::is_resume_control_message(message)
            )
            .count(),
        1
    );
    let events = crate::entity::agent_run_event::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "user_followup")
            .count(),
        1
    );
    // The dispatcher finalizes only after checking this exact durable answer.
    let lease = || crate::schedule_store::ContinuationLease {
        owner: 1,
        run_id: &run.run_id,
        node_id: "node",
        run_epoch: claimed_epoch,
        session_token: token,
    };
    assert!(
        store
            .finish_answered_continuation(lease(), "uncommitted answer")
            .await
            .is_err()
    );
    let LoopOutcome::Answered(answer) = result else {
        unreachable!()
    };
    let mut stale = lease();
    stale.session_token += 1;
    assert!(
        store
            .finish_answered_continuation(stale, &answer)
            .await
            .is_err()
    );
    let now = chrono::Utc::now();
    let action = crate::entity::agent_exec_task::ActiveModel {
        exec_request_id: Set("unresolved-execution".into()),
        execution_generation: Set("generation".into()),
        conversation_id: Set(conversation.clone()),
        tool_call_id: Set("call".into()),
        target_connection_id: Set("offline-host".into()),
        status: Set("running".into()),
        disposition_json: Set(None),
        result_text: Set(None),
        event_id: Set("completion".into()),
        delivery_state: Set("pending".into()),
        deadline: Set(now + chrono::Duration::minutes(1)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    assert!(
        store
            .finish_answered_continuation(lease(), &answer)
            .await
            .is_err()
    );
    let mut action: crate::entity::agent_exec_task::ActiveModel = action.into();
    action.status = Set("unknown".into());
    let action = action.update(&db).await.unwrap();
    assert!(
        store
            .finish_answered_continuation(lease(), &answer)
            .await
            .is_err()
    );
    assert!(
        crate::entity::agent_schedule::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .active_run_id
            .is_some()
    );
    let mut action: crate::entity::agent_exec_task::ActiveModel = action.into();
    action.status = Set("done".into());
    action.result_text = Set(Some("synthetic terminal receipt".into()));
    action.update(&db).await.unwrap();
    if next_permission {
        use desk_diagnose_core::{dynamic_run::*, seam::SessionSeam};
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        let mut request = session.permission_requests[0].clone();
        request.request_id = "next-permission".into();
        request.state = PermissionRequestState::Pending;
        session.add_permission_request(request.clone()).unwrap();
        session.last_event_seq += 1;
        let event = PermissionRequestedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "next-permission-requested".into(),
                run_id: session.conversation_id.clone(),
                event_seq: session.last_event_seq,
                input_revision: session.input_revision,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec![],
                result_envelope_ids: vec![],
                created_at: chrono::Utc::now().to_rfc3339(),
            },
            request,
        };
        crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
            .save_permission_request(&mut session, &event)
            .await
            .unwrap();
    }
    let record = || async {
        if next_permission {
            store
                .await_continuation_permission(lease(), "next-permission")
                .await
        } else {
            store.finish_answered_continuation(lease(), &answer).await
        }
    };
    if permission {
        use sea_orm::sea_query::Expr;
        let before = permission_receipt(&db).await;
        assert_eq!(before.state, "started");
        let task = crate::entity::agent_schedule::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        crate::entity::agent_schedule::Entity::update_many()
            .col_expr(
                crate::entity::agent_schedule::Column::Revision,
                Expr::value(i64::MAX),
            )
            .exec(&db)
            .await
            .unwrap();
        assert!(record().await.is_err());
        assert_eq!(permission_receipt(&db).await, before);
        assert_eq!(
            crate::entity::agent_schedule_run::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        crate::entity::agent_schedule::Entity::update_many()
            .col_expr(
                crate::entity::agent_schedule::Column::Revision,
                Expr::value(task.revision),
            )
            .exec(&db)
            .await
            .unwrap();
    }
    let settled = record().await.unwrap();
    assert_eq!(
        settled.status,
        if next_permission {
            "awaiting_permission"
        } else {
            "succeeded"
        }
    );
    assert_eq!(settled.failure_accounted, !next_permission);
    assert_eq!(
        store
            .resolve_run_session(1, &settled.schedule_id, &settled.run_id)
            .await
            .unwrap(),
        (settled.conversation_id.clone(), "device".into())
    );
    if permission {
        assert_ne!(settled.turn_id, format!("{}-turn", settled.run_id));
    }

    if permission {
        let receipt = permission_receipt(&db).await;
        assert_eq!(receipt.state, "settled");
        assert_eq!(record().await.unwrap(), settled);
        assert_eq!(permission_receipt(&db).await, receipt);
    }
    if next_permission {
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert_eq!(
            session
                .permission_requests
                .iter()
                .find(|request| request.request_id == "next-permission")
                .unwrap()
                .state,
            desk_diagnose_core::dynamic_run::PermissionRequestState::Pending
        );
        assert_eq!(
            settled.result_ref.as_deref(),
            Some("permission:next-permission")
        );
        assert!(settled.finished_at.is_none());
        return;
    }
    assert!(
        settled
            .result_ref
            .as_deref()
            .unwrap()
            .starts_with("message:")
    );
    assert_eq!(
        store
            .finish_answered_continuation(lease(), &answer)
            .await
            .unwrap(),
        settled
    );
    assert!(
        crate::entity::agent_schedule::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .active_run_id
            .is_none()
    );
}

async fn permission_receipt(
    db: &DatabaseConnection,
) -> crate::entity::agent_permission_resume::Model {
    crate::entity::agent_permission_resume::Entity::find()
        .filter(
            crate::entity::agent_permission_resume::Column::RequestId.eq("scheduled-permission"),
        )
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

async fn owner_decision(
    db: &DatabaseConnection,
    schedules: &ScheduleStore,
    mut claimed: crate::schedule_store::ClaimedContinuation,
) -> crate::schedule_store::ClaimedContinuation {
    use desk_diagnose_core::{dynamic_run::*, seam::SessionSeam, session::TurnState};
    let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            claimed.session.client_conversation_id.clone(),
            claimed.session.surface,
        );
    let request: PermissionRequest = serde_json::from_value(serde_json::json!({
        "schema_version":1, "request_id":"scheduled-permission", "input_revision":1,
        "state":"pending", "created_at":chrono::Utc::now().to_rfc3339(), "items":[{
            "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
            "expected_effect":"read_device", "resource_scope":["target:current_device"],
            "operation_scope":["observe"], "suggested_ttl_seconds":120,
            "suggested_max_uses":1, "reason":"Inspect current device"
        }]
    })).unwrap();
    claimed
        .session
        .add_permission_request(request.clone())
        .unwrap();
    claimed.session.last_event_seq += 1;
    let event = PermissionRequestedEvent {
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: "scheduled-permission-requested".into(),
            run_id: claimed.session.conversation_id.clone(),
            event_seq: claimed.session.last_event_seq,
            input_revision: 1,
            kind: AgentRunEventKind::PermissionRequested,
            correlation_id: Some(request.request_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at: request.created_at.clone(),
        },
        request,
    };
    store
        .save_permission_request(&mut claimed.session, &event)
        .await
        .unwrap();
    claimed
        .session
        .finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    claimed.session.handled_input_seq = claimed.session.latest_input_seq;
    store.save(&mut claimed.session).await.unwrap();
    schedules
        .await_continuation_permission(
            crate::schedule_store::ContinuationLease {
                owner: 1,
                run_id: &claimed.run.run_id,
                node_id: "node",
                run_epoch: claimed.run.lease_epoch,
                session_token: claimed.session.lease_token,
            },
            "scheduled-permission",
        )
        .await
        .unwrap();
    let registry = device_assistant_provider_registry();
    store
        .decide_permission_request(
            &claimed.session.conversation_id,
            "1",
            "device",
            "scheduled-permission",
            vec![PermissionDecisionItem {
                item_id: "read".into(),
                decision: PermissionItemDecision::Deny,
            }],
            crate::agent_session_store::PermissionGrantIssuanceContext {
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &[],
                readiness_revision: 1,
                now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
                implicit_fresh_object_refs: &[],
            },
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    let row = crate::entity::agent_session::Entity::find()
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let current = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    schedules
        .claim_continuation_permission(crate::schedule_store::ContinuationPermissionClaim {
            continuation: ContinuationClaim {
                owner: 1,
                run_id: &claimed.run.run_id,
                node_id: "node",
                lease_seconds: 90,
                policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
                scope: current.scope_snapshot,
            },
            request_id: "scheduled-permission",
            expected_session_version: current.version,
            expected_run_epoch: claimed.run.lease_epoch,
            grants: &[],
        })
        .await
        .unwrap()
}

#[actix_web::test]
async fn scheduled_executor_dispatches_original_input_once_after_device_returns() {
    exercise(false, false, false, false, true).await;
}

#[actix_web::test]
async fn scheduled_executor_accounts_persisted_model_failure_once() {
    exercise(false, true, false, false, true).await;
}

#[actix_web::test]
async fn scheduled_executor_reopens_durable_schedule_and_conversation_after_restart() {
    exercise_with_restart(false, false, false, false, true, true, false).await;
}

#[actix_web::test]
async fn scheduled_executor_preserves_approved_timer_after_real_new_user_input() {
    exercise_with_restart(false, false, false, false, true, false, true).await;
}

mod process_restart;
