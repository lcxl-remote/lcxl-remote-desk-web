use super::*;
use sea_orm::{ActiveModelTrait, ConnectionTrait, PaginatorTrait, Schema};

#[tokio::test]
async fn rehearsal_reservation_is_fresh_idempotent_and_owner_bound() {
    let store = super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(rehearsal::Entity))
        .await
        .unwrap();
    let draft = super::super::tests::draft();
    let task = store.create_draft(1, &draft, 1000).await.unwrap();
    assert!(matches!(
        store
            .reserve_rehearsal(2, &task.schedule_id, task.revision, "run")
            .await,
        Err(ScheduleStoreError::NotFound)
    ));
    let first = store
        .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
        .await
        .unwrap();
    assert_eq!(first.status, "pending");
    assert_eq!(first.task_revision, task.task_revision);
    assert_eq!(first.prompt, task.prompt);
    assert_eq!(first.prompt_sha256, digest(&task.prompt));
    assert!(
        desk_diagnose_core::conversation_key::is_valid_client_conversation_id(
            &first.client_conversation_id
        )
    );
    assert_eq!(
        first.conversation_id,
        derive_conversation_key(
            "1",
            &task.target_device_id,
            Some(&first.client_conversation_id),
            ""
        )
    );
    assert!(matches!(
        store.read_rehearsal(2, &first.rehearsal_id).await,
        Err(ScheduleStoreError::NotFound)
    ));
    assert_eq!(
        store
            .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
            .await
            .unwrap(),
        first
    );
    let changed = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(changed.revision, task.revision + 1);
    assert_eq!(changed.status, "rehearsing");
    assert!(changed.next_run_at.is_none());
    assert!(changed.authorization_revision.is_none());
    assert!(
        store
            .reserve_rehearsal(1, &task.schedule_id, changed.revision, "run")
            .await
            .is_err()
    );
    assert!(
        store
            .reserve_rehearsal(1, &task.schedule_id, changed.revision, "other")
            .await
            .is_err()
    );
    assert_eq!(
        store
            .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
            .await
            .unwrap(),
        first
    );
    assert_eq!(rehearsal::Entity::find().count(&store.db).await.unwrap(), 1);
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), changed);
    assert!(
        store
            .cancel_pending_rehearsal(1, &first.rehearsal_id, task.revision)
            .await
            .is_err()
    );
    assert_eq!(
        store.read_rehearsal(1, &first.rehearsal_id).await.unwrap(),
        first
    );
    let cancelled = store
        .cancel_pending_rehearsal(1, &first.rehearsal_id, changed.revision)
        .await
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(
        store
            .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
            .await
            .unwrap(),
        cancelled
    );
    assert_eq!(
        store
            .cancel_pending_rehearsal(1, &first.rehearsal_id, changed.revision)
            .await
            .unwrap(),
        cancelled
    );
    let current = store.read(1, &task.schedule_id).await.unwrap();
    assert_eq!(current.status, "draft");
    let next = store
        .reserve_rehearsal(1, &task.schedule_id, current.revision, "new-run")
        .await
        .unwrap();
    assert_ne!(next.conversation_id, first.conversation_id);
    assert_ne!(next.client_conversation_id, first.client_conversation_id);
    assert_eq!(next.status, "pending");
}

#[tokio::test]
async fn rehearsal_reservation_rejects_wrong_kind_and_rolls_back_overflow() {
    let store = super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(rehearsal::Entity))
        .await
        .unwrap();
    let task = store
        .create_draft(1, &super::super::tests::draft(), 1000)
        .await
        .unwrap();
    let mut changed: entity::ActiveModel = task.clone().into();
    changed.kind = Set("conversation_resume".into());
    changed.update(&store.db).await.unwrap();
    assert!(
        store
            .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
            .await
            .is_err()
    );
    let mut overflow: entity::ActiveModel = task.clone().into();
    overflow.kind = Set("fresh_task".into());
    overflow.revision = Set(i64::MAX);
    overflow.update(&store.db).await.unwrap();
    let before = store.read(1, &task.schedule_id).await.unwrap();
    assert!(
        store
            .reserve_rehearsal(1, &task.schedule_id, i64::MAX, "run")
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
    assert_eq!(rehearsal::Entity::find().count(&store.db).await.unwrap(), 0);
}

#[tokio::test]
async fn unknown_side_effect_blocks_a_new_rehearsal_without_changing_the_task() {
    let store = super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(rehearsal::Entity))
        .await
        .unwrap();
    let task = store
        .create_draft(1, &super::super::tests::draft(), 1000)
        .await
        .unwrap();
    let mut failures = desk_diagnose_core::schedule::lifecycle::FailureState::default();
    failures
        .pause_reasons
        .insert(desk_agent_protocol::schedule::SchedulePauseReason::UnknownSideEffect);
    let mut changed: entity::ActiveModel = task.clone().into();
    changed.status = Set("paused".into());
    changed.failure_state_json = Set(serde_json::to_string(&failures).unwrap());
    changed.update(&store.db).await.unwrap();
    let before = store.read(1, &task.schedule_id).await.unwrap();
    assert!(
        store
            .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
    assert_eq!(rehearsal::Entity::find().count(&store.db).await.unwrap(), 0);
}
