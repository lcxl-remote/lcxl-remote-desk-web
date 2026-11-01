use super::super::super::native_export::{load_call_set_on, record_call_on};
use super::*;
use desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet;

async fn prepare_pair(store: &TestStore, policy: &AssistantDataPolicy) -> [ReadProjectionProof; 2] {
    let mut proofs = [source_proof("batch-first"), source_proof("batch-second")];
    // Both projections belong to one frozen owner input, independent of how
    // much wall time the fixture spends constructing their proof metadata.
    let created = proofs[0].created_at_unix_ms;
    let unused_until = proofs
        .iter()
        .map(|proof| proof.unused_until_unix_ms)
        .min()
        .unwrap();
    for proof in &mut proofs {
        proof.created_at_unix_ms = created;
        proof.unused_until_unix_ms = unused_until;
    }
    let txn = store.db.begin().await.unwrap();
    for (index, proof) in proofs.iter_mut().enumerate() {
        proof.logical_call_id = format!("batch-preparation-{index}");
        prepare_on(&txn, proof, "{}", policy, proof.created_at_unix_ms + 1)
            .await
            .unwrap();
    }
    txn.commit().await.unwrap();
    proofs
}

#[tokio::test]
async fn a_physical_call_seals_and_consumes_its_whole_batch_before_one_exact_quota_charge() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proofs = prepare_pair(&store, &policy).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    let grants = seed_native_batch_on(&txn, &proofs, "physical-batch").await;
    // A second quota update or any native/projection mutation after that update
    // aborts the transaction, including for the second source in the batch.
    for sql in [
        "CREATE TABLE batch_write_order (phase INTEGER NOT NULL)",
        "INSERT INTO batch_write_order VALUES (0)",
        "CREATE TRIGGER reject_second_batch_quota BEFORE UPDATE ON agent_assistant_quota WHEN (SELECT phase FROM batch_write_order) > 0 BEGIN SELECT RAISE(ABORT, 'second quota update'); END",
        "CREATE TRIGGER observe_batch_quota AFTER UPDATE ON agent_assistant_quota BEGIN UPDATE batch_write_order SET phase = phase + 1; END",
        "CREATE TRIGGER reject_late_batch_projection BEFORE UPDATE ON agent_assistant_read_projection WHEN (SELECT phase FROM batch_write_order) > 0 BEGIN SELECT RAISE(ABORT, 'projection after quota'); END",
        "CREATE TRIGGER reject_late_batch_native BEFORE UPDATE ON model_egress_receipt WHEN (SELECT phase FROM batch_write_order) > 0 BEGIN SELECT RAISE(ABORT, 'native seal after quota'); END",
    ] {
        txn.execute_unprepared(sql).await.unwrap();
    }
    let accepted = record_call_on(&txn, &grants, &policy).await.unwrap();
    assert_eq!(accepted.len(), 2);
    let set = AssistantNativeSourceSet::from_grants(&grants).unwrap();
    let native = native::Entity::find().one(&txn).await.unwrap().unwrap();
    assert_eq!(
        native.assistant_source_set_json.as_deref(),
        Some(serde_json::to_string(&set).unwrap().as_str())
    );
    assert_eq!(load_call_set_on(&txn, &set).await.unwrap().len(), 2);
    let mut expected = serde_json::to_string(&set).unwrap().len() as i64;
    for value in &accepted {
        expected += metadata_bytes(&value.row).unwrap();
    }
    let sources = source_use::Entity::find().all(&txn).await.unwrap();
    let memories = memory_use::Entity::find().all(&txn).await.unwrap();
    assert_eq!(sources.len(), 2);
    assert_eq!(memories.len(), 2);
    for row in &sources {
        expected += source_use_bytes(row).unwrap();
    }
    for row in &memories {
        expected += memory_use_bytes(row).unwrap();
    }
    let charged = quota::Entity::find().one(&txn).await.unwrap().unwrap();
    assert_eq!(charged.metadata_bytes - before.metadata_bytes, expected);
    let phase = txn
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT phase FROM batch_write_order".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(phase.try_get::<i64>("", "phase").unwrap(), 1);
    for proof in &proofs {
        let row = load_projection_on(&txn, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row;
        assert_eq!(
            row.first_consumed_at_ms,
            Some(grants[0].authorized_at_unix_ms as i64)
        );
        assert_eq!(row.last_consumed_at_ms, row.first_consumed_at_ms);
    }
    let mut reversed = grants.clone();
    reversed.reverse();
    let replay = record_call_on(&txn, &reversed, &policy).await.unwrap();
    assert_eq!(replay[0].row, accepted[1].row);
    assert_eq!(replay[1].row, accepted[0].row);
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        charged
    );
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 2);
    txn.commit().await.unwrap();
    // A later retry cannot silently turn a sealed two-source call into one.
    let txn = store.db.begin().await.unwrap();
    assert!(record_call_on(&txn, &grants[..1], &policy).await.is_err());
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        charged
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn a_bad_second_grant_fails_before_consuming_or_sealing_the_valid_first_grant() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proofs = prepare_pair(&store, &policy).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    let mut grants = seed_native_batch_on(&txn, &proofs, "physical-invalid-second").await;
    grants[1].projection_sha256 = "d".repeat(64);
    assert!(record_call_on(&txn, &grants, &policy).await.is_err());
    assert!(
        native::Entity::find()
            .one(&txn)
            .await
            .unwrap()
            .unwrap()
            .assistant_source_set_json
            .is_none()
    );
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 0);
    assert_eq!(source_use::Entity::find().count(&txn).await.unwrap(), 0);
    for proof in &proofs {
        let row = load_projection_on(&txn, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row;
        assert!(row.first_consumed_at_ms.is_none() && row.last_consumed_at_ms.is_none());
    }
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        before
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn a_late_second_insert_failure_rolls_back_the_native_receipt_and_entire_batch() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proofs = prepare_pair(&store, &policy).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    let grants = seed_native_batch_on(&txn, &proofs, "physical-late-failure").await;
    let reject = format!(
        "CREATE TRIGGER reject_second_batch_authorization BEFORE INSERT ON agent_assistant_data_authorization WHEN NEW.authorization_id = '{}' BEGIN SELECT RAISE(ABORT, 'second authorization failure'); END",
        grants[1].authorization_id
    );
    txn.execute_unprepared(&reject).await.unwrap();
    assert!(record_call_on(&txn, &grants, &policy).await.is_err());
    txn.rollback().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        source_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert_eq!(
        memory_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    for proof in &proofs {
        let row = load_projection_on(&store.db, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row;
        assert!(row.first_consumed_at_ms.is_none() && row.last_consumed_at_ms.is_none());
    }
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
}

#[tokio::test]
async fn a_missing_member_of_a_sealed_batch_cannot_be_recreated_as_an_unrelated_new_use() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proofs = prepare_pair(&store, &policy).await;
    let txn = store.db.begin().await.unwrap();
    let grants = seed_native_batch_on(&txn, &proofs, "physical-missing-member").await;
    record_call_on(&txn, &grants, &policy).await.unwrap();
    txn.commit().await.unwrap();
    authorization::Entity::delete_many()
        .filter(authorization::Column::AuthorizationId.eq(&grants[1].authorization_id))
        .exec(&store.db)
        .await
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(record_call_on(&txn, &grants, &policy).await.is_err());
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 1);
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        before
    );
    txn.rollback().await.unwrap();
}
