//! Exercise the public assistant entry with a reserved rehearsal and a local model.
use super::*;
use crate::schedule_store::ScheduleStore;
use desk_agent_protocol::schedule::*;
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter};

#[actix_web::test]
async fn rehearsal_entry_commits_answer_without_publishing_or_replaying_input() {
    run_case(false).await;
}

#[actix_web::test]
async fn rehearsal_entry_settles_invalid_model_response_without_replaying_input() {
    run_case(true).await;
}

async fn run_case(failed: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = actix_web::rt::spawn(capture_one_openai_request_with_sse(
        listener,
        if failed {
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"
        } else {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"rehearsal-complete\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            )
        },
    ));
    let db = Database::connect("sqlite::memory:").await.unwrap();
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
    let store = ScheduleStore::new(db.clone());
    let task = store
        .create_draft(
            1,
            &ScheduleDraft {
                time_confirmation: None,
                client_create_key: "rehearsal-entry".into(),
                kind: ScheduledTaskKind::FreshTask,
                target_device_id: "device".into(),
                title: "Rehearse".into(),
                prompt: "SYNTHETIC_REHEARSAL_REQUIREMENT".into(),
                locale: None,
                model_id: None,
                spec: ScheduleSpec {
                    schema_version: 1,
                    rule: ScheduleRule::Once {
                        at: (chrono::Utc::now() + chrono::Duration::hours(1))
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    },
                },
                source_conversation_id: None,
                requirement_revision: None,
                creation_source: ScheduleCreationSource::Manual,
            },
            store.database_time().await.unwrap(),
        )
        .await
        .unwrap();
    let reserved = store
        .reserve_rehearsal(1, &task.schedule_id, task.revision, "first")
        .await
        .unwrap();
    let ask = DeviceAssistantAsk {
        question: reserved.prompt.clone(),
        locale: reserved.locale.clone(),
        conversation_id: Some(reserved.client_conversation_id.clone()),
        client_message_id: format!("rehearsal:{}:input", reserved.rehearsal_id),
        ..Default::default()
    };
    let connections = web::Data::new(SharedConnectionMap::new());
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        run_turn(
            connections.clone(),
            db.clone(),
            "browser-request".into(),
            "controller".into(),
            "offline-host".into(),
            1,
            "device".into(),
            ask.clone(),
        ),
    )
    .await
    .unwrap();
    let completed = store
        .read_rehearsal(1, &reserved.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(
        completed.status,
        if failed { "failed" } else { "completed" }
    );
    assert!(completed.completed_session_sha256.is_some());
    let task = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(
        task.status,
        if failed {
            "draft"
        } else {
            "awaiting_authorization"
        }
    );
    assert!(task.authorization_revision.is_none());
    assert!(task.next_run_at.is_none());
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(&reserved.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let snapshot = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(snapshot.input_revision, 1);
    if failed {
        assert_eq!(
            snapshot.turn_state,
            desk_diagnose_core::session::TurnState::Failed
        );
        assert!(completed.answer_message_id.is_none());
    } else {
        assert_eq!(
            snapshot.conversation.last().unwrap().text,
            "rehearsal-complete"
        );
    }
    let request = tokio::time::timeout(std::time::Duration::from_secs(2), gateway)
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(request)
            .unwrap()
            .contains("SYNTHETIC_REHEARSAL_REQUIREMENT")
    );
    // A transport retry cannot append another input or start another model call.
    run_turn(
        connections,
        db.clone(),
        "retry".into(),
        "controller".into(),
        "offline-host".into(),
        1,
        "device".into(),
        ask,
    )
    .await;
    assert_eq!(
        store
            .read_rehearsal(1, &reserved.rehearsal_id)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(
        crate::entity::agent_session::Entity::find_by_id(row.id)
            .one(&db)
            .await
            .unwrap(),
        Some(row)
    );
    assert_eq!(
        crate::entity::agent_schedule_run::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        crate::entity::agent_task_authorization::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
}
