//! Actual OSS entry, SQLite and synthetic HTTP model; no device UI is exercised.

use super::*;
use crate::agent_run_event_store::{InputSubject, SignalAgentRunEventStore};
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};
use desk_agent_protocol::device_assistant::DeviceAssistantObjectContextOperation;

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
        ..Default::default()
    };
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(&db, config).await.unwrap();
    let client_id = "input-entry";
    let run_id = derive_conversation_key("7", "device", Some(client_id), "unused");
    let update = DeviceAssistantObjectContextUpdate {
        conversation_id: client_id.into(),
        client_request_id: "original-object".into(),
        operation: DeviceAssistantObjectContextOperation::AttachFile {
            object_ref: ObjectRef {
                token: "opaque-original-file".into(),
                snapshot_id: "worker".into(),
                object_kind: ObjectKind::File,
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
            },
            display_summary: "not-file-type-authority".into(),
        },
    };
    apply_object_context_update(db.clone(), 7, "device".into(), &update)
        .await
        .unwrap();
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client_id.into()), AgentSessionSurface::DeviceAssistant);
    let before = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    let selected = before.context_attachments[0].clone();
    let ask = DeviceAssistantAsk {
        question: "original requirement".into(),
        client_message_id: "original-message".into(),
        conversation_id: Some(client_id.into()),
        selected_attachment_ids: vec![selected.attachment_id.clone()],
        ..Default::default()
    };
    let connections = web::Data::new(SharedConnectionMap::new());
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "transport".into(),
            "controller".into(),
            "offline-host".into(),
            7,
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
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), capture)
        .await
        .unwrap()
        .unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("original requirement"));
    assert!(!body.contains("opaque-original-file"));
    let events = SignalAgentRunEventStore::new(db.clone());
    let frozen = events
        .original_read_context(
            InputSubject {
                run_id: &run_id,
                actor_id: "7",
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
                actor_id: "7",
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
            std::time::Duration::from_secs(5),
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("retry-{mutation}"),
                "controller".into(),
                "offline-host".into(),
                7,
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
    assert_eq!(receipts.len(), 1);
    db.close().await.unwrap();
}
