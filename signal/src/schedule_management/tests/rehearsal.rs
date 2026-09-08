use super::*;
use desk_agent_protocol::{
    device_assistant::DeviceAssistantAsk, schedule::management::RehearsalStatus,
};

#[tokio::test]
async fn rehearsal_management_reserves_public_intent_without_starting_or_granting() {
    let db = fixture().await;
    let schema = Schema::new(db.get_database_backend());
    for table in [
        schema.create_table_from_entity(crate::entity::agent_task_rehearsal::Entity),
        schema.create_table_from_entity(crate::entity::agent_session::Entity),
    ] {
        db.execute(&table).await.unwrap();
    }
    let created = task(
        manage(&db, 1, Request::CreateDraft { draft: draft() })
            .await
            .unwrap(),
    );
    let lookup = Request::GetTaskRehearsal {
        schedule_id: created.schedule_id.clone(),
    };
    let empty = serde_json::to_value(manage(&db, 1, lookup.clone()).await.unwrap()).unwrap();
    assert_eq!(empty["result"], "task_rehearsal");
    assert!(empty["rehearsal"].is_null());
    assert!(manage(&db, 2, lookup.clone()).await.is_err());
    let request = Request::ReserveRehearsal {
        schedule_id: created.schedule_id.clone(),
        expected_revision: created.revision,
        client_request_key: "interactive-rehearsal-1".into(),
    };
    let first = manage(&db, 1, request.clone()).await.unwrap();
    let Response::Rehearsal {
        task: rehearsing,
        rehearsal,
    } = first.clone()
    else {
        panic!("expected rehearsal view")
    };
    assert_eq!(rehearsal.status, RehearsalStatus::Pending);
    let latest = serde_json::to_value(manage(&db, 1, lookup.clone()).await.unwrap()).unwrap();
    assert_eq!(latest["rehearsal"]["rehearsal_id"], rehearsal.rehearsal_id);

    assert!(
        manage(
            &db,
            1,
            Request::GetRehearsalPermissions {
                rehearsal_id: rehearsal.rehearsal_id.clone()
            }
        )
        .await
        .is_err()
    );

    assert_eq!(
        rehearsal.target_device_id.as_deref(),
        Some("public-device-1")
    );
    assert!(rehearsal.client_conversation_id.starts_with("rehearsal_"));
    assert_eq!(rehearsal.prompt, created.prompt);
    assert!(rehearsal.started_at.is_none());
    assert!(rehearsal.finished_at.is_none());
    assert_eq!(rehearsing.status, ScheduledTaskStatus::Rehearsing);
    assert!(rehearsing.next_run_at.is_none());
    let store = ScheduleStore::new(db.clone());
    let stored = store
        .read_rehearsal(1, &rehearsal.rehearsal_id)
        .await
        .unwrap();
    let json = serde_json::to_value(&first).unwrap();
    assert!(json["rehearsal"].get("conversation_id").is_none());
    assert!(json["rehearsal"].get("owner_user_id").is_none());
    assert!(json["rehearsal"].get("completed_session_sha256").is_none());
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains(&stored.conversation_id)
    );
    assert_eq!(
        serde_json::to_value(manage(&db, 1, request).await.unwrap()).unwrap(),
        json
    );
    assert_eq!(
        serde_json::to_value(
            manage(
                &db,
                1,
                Request::GetRehearsal {
                    rehearsal_id: rehearsal.rehearsal_id.clone()
                }
            )
            .await
            .unwrap()
        )
        .unwrap(),
        json
    );
    assert!(
        manage(
            &db,
            2,
            Request::GetRehearsal {
                rehearsal_id: rehearsal.rehearsal_id.clone()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        crate::entity::agent_session::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        crate::entity::agent_schedule_run::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert!(
        store
            .read(1, &created.schedule_id)
            .await
            .unwrap()
            .authorization_revision
            .is_none()
    );
    let ask = DeviceAssistantAsk {
        question: rehearsal.prompt.clone(),
        conversation_id: Some(rehearsal.client_conversation_id.clone()),
        client_message_id: rehearsal.initial_message_id.clone(),
        locale: rehearsal.locale.clone(),
        ..Default::default()
    };
    assert_eq!(
        store
            .rehearsal_for_input(1, &stored.target_device_id, &ask)
            .await
            .unwrap()
            .unwrap()
            .rehearsal_id,
        rehearsal.rehearsal_id
    );
    let mut changed = ask.clone();
    changed.question.push('!');
    assert!(
        store
            .rehearsal_for_input(1, &stored.target_device_id, &changed)
            .await
            .is_err()
    );
    assert!(
        store
            .rehearsal_for_input(2, &stored.target_device_id, &ask)
            .await
            .is_err()
    );
    assert!(
        manage(
            &db,
            1,
            Request::CancelPendingRehearsal {
                rehearsal_id: rehearsal.rehearsal_id.clone(),
                expected_revision: created.revision
            }
        )
        .await
        .is_err()
    );
    let Response::Rehearsal {
        task: cancelled_task,
        rehearsal: cancelled,
    } = manage(
        &db,
        1,
        Request::CancelPendingRehearsal {
            rehearsal_id: rehearsal.rehearsal_id.clone(),
            expected_revision: rehearsing.revision,
        },
    )
    .await
    .unwrap()
    else {
        panic!("expected cancelled reservation")
    };
    assert_eq!(cancelled.status, RehearsalStatus::Cancelled);
    let latest = serde_json::to_value(manage(&db, 1, lookup.clone()).await.unwrap()).unwrap();
    assert_eq!(latest["rehearsal"]["status"], "cancelled");

    assert_eq!(cancelled_task.status, ScheduledTaskStatus::Draft);
    assert!(
        store
            .rehearsal_for_input(1, &stored.target_device_id, &ask)
            .await
            .is_err()
    );
    let Response::Rehearsal {
        task: next_task,
        rehearsal: next,
    } = manage(
        &db,
        1,
        Request::ReserveRehearsal {
            schedule_id: created.schedule_id,
            expected_revision: cancelled_task.revision,
            client_request_key: "interactive-rehearsal-2".into(),
        },
    )
    .await
    .unwrap()
    else {
        panic!("expected fresh reservation")
    };
    assert_ne!(
        next.client_conversation_id,
        rehearsal.client_conversation_id
    );
    store.claim_rehearsal(1, &next.rehearsal_id).await.unwrap();
    let latest = serde_json::to_value(manage(&db, 1, lookup).await.unwrap()).unwrap();
    assert_eq!(latest["rehearsal"]["rehearsal_id"], next.rehearsal_id);
    assert_eq!(latest["rehearsal"]["status"], "running");

    assert!(
        manage(
            &db,
            1,
            Request::CancelPendingRehearsal {
                rehearsal_id: next.rehearsal_id.clone(),
                expected_revision: next_task.revision
            }
        )
        .await
        .is_err()
    );
    let Response::Rehearsal {
        rehearsal: running, ..
    } = manage(
        &db,
        1,
        Request::GetRehearsal {
            rehearsal_id: next.rehearsal_id,
        },
    )
    .await
    .unwrap()
    else {
        panic!("expected running rehearsal")
    };
    assert_eq!(running.status, RehearsalStatus::Running);
    assert!(running.started_at.is_some());
}
