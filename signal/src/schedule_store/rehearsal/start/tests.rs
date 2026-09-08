use super::*;
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

async fn fixture() -> (ScheduleStore, rehearsal::Model) {
    let store = super::super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(rehearsal::Entity))
        .await
        .unwrap();
    store
        .db
        .execute(&schema.create_table_from_entity(agent_session::Entity))
        .await
        .unwrap();
    let task = store
        .create_draft(1, &super::super::super::tests::draft(), 1000)
        .await
        .unwrap();
    let row = store
        .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
        .await
        .unwrap();
    (store, row)
}

#[tokio::test]
async fn claim_is_once_only_and_cannot_be_cancelled_as_unstarted() {
    let (store, row) = fixture().await;
    assert!(matches!(
        store.claim_rehearsal(2, &row.rehearsal_id).await,
        Err(ScheduleStoreError::NotFound)
    ));
    let claimed = store.claim_rehearsal(1, &row.rehearsal_id).await.unwrap();
    assert_eq!(claimed.status, "running");
    assert!(claimed.started_at.is_some());
    assert_eq!(claimed.conversation_id, row.conversation_id);
    assert_eq!(claimed.prompt, row.prompt);
    let task = store.read(1, &row.schedule_id).await.unwrap();
    assert!(store.claim_rehearsal(1, &row.rehearsal_id).await.is_err());
    assert!(
        store
            .cancel_pending_rehearsal(1, &row.rehearsal_id, task.revision)
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &row.schedule_id).await.unwrap(), task);
    assert_eq!(
        store.read_rehearsal(1, &row.rehearsal_id).await.unwrap(),
        claimed
    );
    assert!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn changed_requirement_overflow_and_existing_session_reject_without_claim() {
    for reason in ["requirement", "overflow", "existing"] {
        let (store, row) = fixture().await;
        let task = store.read(1, &row.schedule_id).await.unwrap();
        match reason {
            "requirement" => {
                let mut changed: entity::ActiveModel = task.into();
                changed.prompt = Set("Different task".into());
                changed.update(&store.db).await.unwrap();
            }
            "overflow" => {
                let mut changed: entity::ActiveModel = task.into();
                changed.revision = Set(i64::MAX);
                changed.update(&store.db).await.unwrap();
            }
            _ => {
                // Any existing session blocks admission; no history is adopted.
                agent_session::ActiveModel {
                    conversation_id: Set(row.conversation_id.clone()),
                    actor_id: Set("1".into()),
                    device_id: Set(row.target_device_id.clone()),
                    state_json: Set("{}".into()),
                    version: Set(0),
                    lease_token: Set(0),
                    created_at: Set(chrono::Utc::now()),
                    updated_at: Set(chrono::Utc::now()),
                    ..Default::default()
                }
                .insert(&store.db)
                .await
                .unwrap();
            }
        }
        let before = store.read(1, &row.schedule_id).await.unwrap();
        assert!(
            store.claim_rehearsal(1, &row.rehearsal_id).await.is_err(),
            "{reason}"
        );
        assert_eq!(
            store.read(1, &row.schedule_id).await.unwrap(),
            before,
            "{reason}"
        );
        assert_eq!(
            store.read_rehearsal(1, &row.rehearsal_id).await.unwrap(),
            row,
            "{reason}"
        );
    }
}

#[tokio::test]
async fn reserved_input_rejects_preemption_and_parameter_changes() {
    let (store, row) = fixture().await;
    let message = desk_diagnose_core::chat::ChatMessage::text(
        format!("rehearsal:{}:input", row.rehearsal_id),
        desk_diagnose_core::chat::ChatRole::User,
        &row.prompt,
    );
    let check = |message: desk_diagnose_core::chat::ChatMessage| {
        let store = store.clone();
        let row = row.clone();
        async move {
            let txn = store.db.begin().await.unwrap();
            let result = super::super::validate_rehearsal_input_on(
                &txn,
                "1",
                &row.target_device_id,
                &row.conversation_id,
                Some(&row.client_conversation_id),
                &message,
            )
            .await;
            txn.rollback().await.unwrap();
            result
        }
    };
    assert!(check(message.clone()).await.is_err());
    store.claim_rehearsal(1, &row.rehearsal_id).await.unwrap();
    check(message.clone()).await.unwrap();
    let mut changed = message.clone();
    changed.text = "Different request".into();
    assert!(check(changed).await.is_err());
    let mut changed = message.clone();
    changed.message_id = "another-input".into();
    assert!(check(changed).await.is_err());
    let task = store.read(1, &row.schedule_id).await.unwrap();
    let mut changed: entity::ActiveModel = task.into();
    changed.status = Set("deleted".into());
    changed.update(&store.db).await.unwrap();
    assert!(check(message).await.is_err());
}
