use super::*;
use sea_orm::ConnectionTrait;

#[tokio::test]
async fn original_native_acceptance_precedes_quota_and_archive_metadata_is_the_final_stage() {
    let (store, _, proof, _) = admitted("archive-write-order").await;
    for statement in [
        "CREATE TABLE output_finish_order (phase INTEGER NOT NULL)",
        "INSERT INTO output_finish_order VALUES (0)",
        "CREATE TRIGGER observe_native_output AFTER UPDATE OF state ON model_egress_receipt WHEN NEW.model_output_envelope_id IS NOT NULL BEGIN UPDATE output_finish_order SET phase=1; END",
        "CREATE TRIGGER require_native_before_quota BEFORE UPDATE ON agent_assistant_quota WHEN (SELECT phase FROM output_finish_order) != 1 BEGIN SELECT RAISE(ABORT, 'native acceptance must precede quota'); END",
        "CREATE TRIGGER observe_final_output_quota AFTER UPDATE ON agent_assistant_quota BEGIN UPDATE output_finish_order SET phase=2; END",
        "CREATE TRIGGER reject_native_after_output_quota BEFORE UPDATE ON model_egress_receipt WHEN (SELECT phase FROM output_finish_order) >= 2 BEGIN SELECT RAISE(ABORT, 'native mutation after quota'); END",
        "CREATE TRIGGER require_quota_before_archive BEFORE INSERT ON agent_assistant_model_output WHEN (SELECT phase FROM output_finish_order) != 2 BEGIN SELECT RAISE(ABORT, 'archive must follow final quota'); END",
    ] {
        store.db.execute_unprepared(statement).await.unwrap();
    }
    let candidate = output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let row = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.model_output_envelope_id.as_deref(),
        Some(candidate.envelope_id.as_str())
    );
    output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap()
        .proof
        .validate()
        .unwrap();
}

#[tokio::test]
async fn final_archive_insert_failure_rolls_back_native_acceptance_and_quota_but_keeps_original_actual_usage()
 {
    let (store, _, proof, _) = admitted("archive-last-insert-failure").await;
    let original = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let before_quota = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_output_archive BEFORE INSERT ON agent_assistant_model_output BEGIN SELECT RAISE(ABORT, 'injected final archive insert failure'); END").await.unwrap();
    let candidate = output(&proof);
    assert!(
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .is_err()
    );
    assert_eq!(
        native::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before_quota
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 0);
    let usage: desk_diagnose_core::chat::TokenUsage =
        serde_json::from_str(original.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(12), Some(4))
    );
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_output_archive")
        .await
        .unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 1);
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 1);
}
