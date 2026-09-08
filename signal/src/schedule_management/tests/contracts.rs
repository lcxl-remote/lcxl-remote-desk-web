use super::*;
use desk_agent_protocol::schedule::contract::{TaskBudget, TaskContract, TaskExceptionMode};

#[tokio::test]
async fn contract_management_preserves_public_target_and_requires_current_owner_revision() {
    let db = fixture().await;
    let schema = Schema::new(db.get_database_backend());
    db.execute(&schema.create_table_from_entity(crate::entity::agent_task_contract::Entity))
        .await
        .unwrap();
    let Response::Task { task } = manage(&db, 1, Request::CreateDraft { draft: draft() })
        .await
        .unwrap()
    else {
        panic!("task response")
    };
    let get = || Request::GetTaskContract {
        schedule_id: task.schedule_id.clone(),
    };
    assert!(matches!(
        manage(&db, 2, get()).await,
        Err(ScheduleStoreError::NotFound)
    ));
    let Response::TaskContract {
        task_revision,
        prompt_sha256,
        contract,
        ..
    } = manage(&db, 1, get()).await.unwrap()
    else {
        panic!("contract response")
    };
    assert!(contract.is_none());
    let definition = TaskContract {
        schema_version: 1,
        schedule_id: task.schedule_id.clone(),
        task_revision,
        contract_revision: 0,
        target_device_id: task.target_device_id.clone().unwrap(),
        prompt_sha256,
        permissions: vec![],
        steps: vec![],
        exception_mode: TaskExceptionMode::Deny,
        budget: TaskBudget {
            max_runs_per_utc_day: 24,
            max_calls_per_run: 10,
            max_model_tokens_per_run: 10000,
            max_runtime_seconds: 300,
        },
    };
    let save = |contract| Request::SaveTaskContract {
        expected_revision: task.revision,
        contract: Box::new(contract),
    };
    let mut replaced = definition.clone();
    replaced.target_device_id = "another-device".into();
    assert!(matches!(
        manage(&db, 1, save(replaced)).await,
        Err(ScheduleStoreError::NotFound)
    ));
    assert!(matches!(
        manage(&db, 2, save(definition.clone())).await,
        Err(ScheduleStoreError::NotFound)
    ));
    let Response::TaskContract {
        task: saved,
        contract: Some(contract),
        contract_sha256: Some(digest),
        ..
    } = manage(&db, 1, save(definition.clone())).await.unwrap()
    else {
        panic!("saved contract response")
    };
    assert_eq!(saved.status, ScheduledTaskStatus::Draft);
    assert!(saved.next_run_at.is_none());
    assert_eq!(contract.target_device_id, definition.target_device_id);
    assert_eq!(contract.contract_revision, 1);
    assert_eq!(digest.len(), 64);
    assert!(matches!(
        manage(&db, 1, save(definition)).await,
        Err(ScheduleStoreError::Conflict)
    ));
    let Response::TaskContract {
        contract: Some(read),
        contract_sha256: Some(read_digest),
        ..
    } = manage(&db, 1, get()).await.unwrap()
    else {
        panic!("read contract response")
    };
    assert_eq!(read, contract);
    assert_eq!(read_digest, digest);
    let stored = ScheduleStore::new(db.clone())
        .read(1, &task.schedule_id)
        .await
        .unwrap();
    assert!(stored.authorization_revision.is_none());
    db.execute(&schema.create_table_from_entity(crate::entity::agent_task_authorization::Entity))
        .await
        .unwrap();
    let input = crate::schedule_store::PublishTask {
        schedule_id: task.schedule_id.clone(),
        expected_revision: saved.revision,
        contract_revision: read.contract_revision as i64,
        contract_sha256: read_digest,
        rehearsal_run_id: "guided-run".into(),
        expires_at: None,
        client_publish_key: "owner-confirmation".into(),
    };
    // This unit fixture verifies the management-to-transaction boundary only.
    // Its stub evidence does not prove runtime authorization or real delivery.
    assert!(
        publish(
            &db,
            2,
            input.clone(),
            &crate::schedule_store::TestPublicationVerifier(true)
        )
        .await
        .is_err()
    );
    assert!(
        publish(
            &db,
            1,
            input.clone(),
            &crate::schedule_store::TestPublicationVerifier(false)
        )
        .await
        .is_err()
    );
    assert!(
        ScheduleStore::new(db.clone())
            .read(1, &task.schedule_id)
            .await
            .unwrap()
            .authorization_revision
            .is_none()
    );
    let Response::Task { task: published } = publish(
        &db,
        1,
        input.clone(),
        &crate::schedule_store::TestPublicationVerifier(true),
    )
    .await
    .unwrap() else {
        panic!("published task")
    };
    assert_eq!(published.status, ScheduledTaskStatus::Active);
    assert!(published.next_run_at.is_some());
    let Response::Task { task: repeated } = publish(
        &db,
        1,
        input.clone(),
        &crate::schedule_store::TestPublicationVerifier(true),
    )
    .await
    .unwrap() else {
        panic!("idempotent task")
    };
    assert_eq!(published.revision, repeated.revision);
    let mut changed = input;
    changed.contract_sha256 = "0".repeat(64);
    assert!(
        publish(
            &db,
            1,
            changed,
            &crate::schedule_store::TestPublicationVerifier(true)
        )
        .await
        .is_err()
    );
}
