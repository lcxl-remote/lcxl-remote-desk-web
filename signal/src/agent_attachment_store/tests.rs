use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::{
    conversation_attachment::batch::{DeliveryIdentity, OutputPart, PartContent, prepare_delivery},
    session::AgentSessionSurface,
};
use sea_orm::{Database, Schema};

#[tokio::test]
async fn local_attachments_are_durable_scoped_atomic_and_do_not_resurrect() {
    let directory =
        std::env::temp_dir().join(format!("attachment-store-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let url = format!("sqlite://{}?mode=rwc", directory.join("test.db").display());
    let db = Database::connect(&url).await.unwrap();
    let schema = Schema::new(db.get_database_backend());
    db.execute(&schema.create_table_from_entity(agent_session::Entity))
        .await
        .unwrap();
    db.execute(&schema.create_table_from_entity(attachment::Entity))
        .await
        .unwrap();
    let scope = AgentScope {
        granted: vec![],
        mode: ExecutionMode::SuggestOnly,
        expires_at: None,
        policy_name: None,
    };
    let mut session =
        PersistedAgentSession::new("conversation", "owner", "device", 1, scope, "now");
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::DeviceAssistant);
    agent_session::ActiveModel {
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(session.lease_token as i64),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let prepared = prepare_delivery(
        &DeliveryIdentity {
            conversation_id: "conversation",
            actor_id: "owner",
            device_id: "device",
            message_id: "message",
            tool_call_id: "call",
        },
        vec![
            OutputPart {
                name: "stdout".into(),
                content: PartContent::Text("x".repeat(5000)),
                source_truncated: false,
            },
            OutputPart {
                name: "stderr".into(),
                content: PartContent::Text("e".repeat(6000)),
                source_truncated: false,
            },
        ],
        1,
    )
    .unwrap();
    let saved = store_batch(&db, &session, &prepared.attachments)
        .await
        .unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(usage(&db, "conversation", "owner").await.unwrap(), 11000);
    let id = &saved[0].attachment_id;
    let original_time = saved[0].last_accessed_at_unix_ms;
    assert_eq!(
        read(&db, "conversation", "owner", id, false)
            .await
            .unwrap()
            .content
            .len(),
        5000
    );
    assert_eq!(
        list(&db, "conversation", "owner", None)
            .await
            .unwrap()
            .iter()
            .find(|m| &m.attachment_id == id)
            .unwrap()
            .last_accessed_at_unix_ms,
        original_time
    );
    let retry = store_batch(&db, &session, &prepared.attachments)
        .await
        .unwrap();
    assert_eq!(retry, saved);
    let mut stale = session.clone();
    stale.version += 1;
    assert!(
        store_batch(&db, &stale, &prepared.attachments)
            .await
            .is_err()
    );
    assert!(
        read(&db, "conversation", "another_owner", id, true)
            .await
            .is_err()
    );
    assert!(
        read(&db, "another_conversation", "owner", id, true)
            .await
            .is_err()
    );
    assert!(
        delete(
            &db,
            "conversation",
            "owner",
            &[id.clone(), "missing".into()]
        )
        .await
        .is_err()
    );
    assert!(read(&db, "conversation", "owner", id, false).await.is_ok());
    db.close().await.unwrap();
    let db = Database::connect(&url).await.unwrap();
    assert_eq!(
        read(&db, "conversation", "owner", id, true)
            .await
            .unwrap()
            .content,
        vec![b'x'; 5000]
    );
    delete(&db, "conversation", "owner", &[id.clone()])
        .await
        .unwrap();
    assert_eq!(usage(&db, "conversation", "owner").await.unwrap(), 6000);
    assert!(
        read(&db, "conversation", "owner", id, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("deleted")
    );
    cleanup(&db).await.unwrap();
    assert!(
        store_batch(&db, &session, &prepared.attachments)
            .await
            .is_err()
    );
    let tombstone = list(&db, "conversation", "owner", None)
        .await
        .unwrap()
        .into_iter()
        .find(|m| &m.attachment_id == id)
        .unwrap();
    assert!(matches!(
        tombstone.availability,
        Availability::Deleted { .. }
    ));
    let root = content_root(&db).await.unwrap();
    for index in 0..1100 {
        std::fs::write(root.join(format!("unmanaged-{index}")), b"keep").unwrap();
    }
    let orphan_key = write_content(&root, b"uncommitted content").await.unwrap();
    let temporary = root.join(format!(".{}.42.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temporary, b"interrupted durable write").unwrap();
    for _ in 0..4 {
        cleanup(&db).await.unwrap();
    }
    assert!(!content_path(&root, &orphan_key).unwrap().exists());
    assert!(!temporary.exists());
    assert!(root.join("unmanaged-1099").exists());
    assert_eq!(
        read(&db, "conversation", "owner", &saved[1].attachment_id, false)
            .await
            .unwrap()
            .content
            .len(),
        6000
    );

    let image = prepare_delivery(
        &DeliveryIdentity {
            conversation_id: "conversation",
            actor_id: "owner",
            device_id: "device",
            message_id: "image-message",
            tool_call_id: "image-call",
        },
        vec![OutputPart {
            name: "image".into(),
            content: PartContent::ImageDataUrl("data:image/png;base64,AQID".into()),
            source_truncated: false,
        }],
        2,
    )
    .unwrap();
    let before_image = usage(&db, "conversation", "owner").await.unwrap();
    let image_saved = store_batch(&db, &session, &image.attachments)
        .await
        .unwrap();
    assert_eq!(
        usage(&db, "conversation", "owner").await.unwrap(),
        before_image + 3
    );
    let images = list_images(&db, "conversation", "owner", None)
        .await
        .unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].attachment_id, image_saved[0].attachment_id);
    assert_eq!(
        image_id_by_call(&db, "conversation", "owner", "image-call")
            .await
            .unwrap(),
        images[0].attachment_id
    );
    assert!(
        image_id_by_call(&db, "conversation", "other", "image-call")
            .await
            .is_err()
    );
    delete(
        &db,
        "conversation",
        "owner",
        &[images[0].attachment_id.clone()],
    )
    .await
    .unwrap();
    assert!(
        list_images(&db, "conversation", "owner", None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        usage(&db, "conversation", "owner").await.unwrap(),
        before_image
    );
    assert!(
        read(&db, "conversation", "owner", &images[0].attachment_id, true)
            .await
            .is_err()
    );
    db.close().await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
