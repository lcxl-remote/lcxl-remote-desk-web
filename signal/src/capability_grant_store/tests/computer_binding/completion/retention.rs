use super::*;
use crate::entity::{agent_exec_task, agent_run_event, ai_usage, turn_usage, usage_retention};
use desk_diagnose_core::session::TurnState;

async fn add_cleanup_tables(db: &DatabaseConnection) {
    let schema = Schema::new(db.get_database_backend());
    for table in [
        schema.create_table_from_entity(usage_retention::Entity),
        schema.create_table_from_entity(turn_usage::Entity),
        schema.create_table_from_entity(ai_usage::Entity),
        schema.create_table_from_entity(agent_exec_task::Entity),
        schema.create_table_from_entity(agent_run_event::Entity),
        schema.create_table_from_entity(crate::entity::agent_permission_resume::Entity),
    ] {
        db.execute(&table).await.unwrap();
    }
}

async fn counts(db: &DatabaseConnection) -> Vec<u64> {
    vec![
        agent_session::Entity::find().count(db).await.unwrap(),
        agent_action_item::Entity::find().count(db).await.unwrap(),
        agent_capability_dispatch_outbox::Entity::find()
            .count(db)
            .await
            .unwrap(),
        agent_capability_grant::Entity::find()
            .count(db)
            .await
            .unwrap(),
        agent_grant_reservation::Entity::find()
            .count(db)
            .await
            .unwrap(),
        agent_exec_task::Entity::find().count(db).await.unwrap(),
        agent_run_event::Entity::find().count(db).await.unwrap(),
        crate::entity::agent_permission_resume::Entity::find()
            .count(db)
            .await
            .unwrap(),
    ]
}

#[tokio::test]
async fn expiry_deletes_original_results_with_run_or_rolls_back_every_related_record() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("retention.db")).await).await;
    let db = &f.store.db;
    add_cleanup_tables(db).await;
    f.bind().await;
    f.accept().await.unwrap();
    let native = verified(&f.plan);
    assert_eq!(
        observe(&f, &native).await.unwrap(),
        CompletionObservation::Stored
    );
    let original = work(&f).await;
    let outbox = f.outbox().await;
    let now = Utc::now();
    let old = now - chrono::Duration::days(40);
    let row = agent_session::Entity::find()
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.finish_turn(TurnState::Idle, old.to_rfc3339());
    let mut row: agent_session::ActiveModel = row.into();
    row.state_json = Set(session.encode_json_for_storage().unwrap());
    row.lease_deadline = Set(None);
    row.updated_at = Set(old);
    row.update(db).await.unwrap();
    agent_exec_task::ActiveModel {
        exec_request_id: Set("legacy-exec".into()),
        execution_generation: Set("legacy-generation".into()),
        conversation_id: Set("run-1".into()),
        tool_call_id: Set("legacy-call".into()),
        target_connection_id: Set("host-original".into()),
        status: Set("completed".into()),
        disposition_json: Set(None),
        result_text: Set(Some("synthetic legacy output".into())),
        event_id: Set("legacy-event".into()),
        delivery_state: Set("consumed".into()),
        deadline: Set(old),
        created_at: Set(old),
        updated_at: Set(old),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    agent_run_event::ActiveModel {
        event_id: Set("run-event".into()),
        run_id: Set("run-1".into()),
        event_seq: Set(1),
        input_revision: Set(1),
        kind: Set("synthetic".into()),
        correlation_id: Set(None),
        input_seq: Set(None),
        actor_id: Set(Some("actor-1".into())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set("{}".into()),
        payload_schema_version: Set(1),
        created_at: Set(old),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    crate::entity::agent_permission_resume::ActiveModel {
        permission_id: Set("synthetic-permission".into()),
        decision_event_id: Set("run-event".into()),
        run_id: Set("run-1".into()),
        request_id: Set("permission-1".into()),
        actor_id: Set("actor-1".into()),
        device_id: Set("device-1".into()),
        input_revision: Set(1),
        state: Set("settled".into()),
        turn_id: Set(Some("synthetic-permission".into())),
        version: Set(2),
        created_at: Set(old),
        updated_at: Set(old),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    assert_eq!(counts(db).await, vec![1; 8]);

    // Fail the last deletion, after every child row has been removed in the transaction.
    db.execute_raw(Statement::from_string(db.get_database_backend(),
        "CREATE TRIGGER refuse_retention BEFORE DELETE ON agent_session BEGIN SELECT RAISE(ABORT, 'synthetic retention failure'); END".to_owned(),
    )).await.unwrap();
    assert!(crate::usage_retention::cleanup_once(db, now).await.is_err());
    assert_eq!(counts(db).await, vec![1; 8]);
    assert_eq!(work(&f).await, original);
    assert_eq!(f.outbox().await, outbox);
    assert!(
        f.store
            .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
            .await
            .unwrap()
            .is_some()
    );
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        "DROP TRIGGER refuse_retention".to_owned(),
    ))
    .await
    .unwrap();

    assert_eq!(
        crate::usage_retention::cleanup_once(db, now).await.unwrap(),
        (0, 0, 0, 1)
    );
    assert_eq!(counts(db).await, vec![0; 8]);
    assert!(
        f.store
            .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
            .await
            .is_err()
    );
    assert!(observe(&f, &native).await.is_err());
    assert_eq!(counts(db).await, vec![0; 8]);
    assert_eq!(
        crate::usage_retention::cleanup_once(db, now).await.unwrap(),
        (0, 0, 0, 0)
    );
}
