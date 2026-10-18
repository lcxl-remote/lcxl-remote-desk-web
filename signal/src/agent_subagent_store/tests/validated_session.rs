use super::super::validated_session::{CallSession, SessionRead, TxnValidatedSession};
use super::*;

#[tokio::test]
async fn proof_rechecks_the_same_row_and_preserves_raw_version() {
    let db = database().await;
    seed_parent(&db).await;
    let txn = db.begin().await.unwrap();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    // Isolate the thread probe from other concurrently seeded "root" fixtures.
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.conversation_id = uuid::Uuid::new_v4().to_string();
    let mut active: session_row::ActiveModel = row.into();
    active.conversation_id = Set(session.conversation_id.clone());
    active.state_json = Set(session.encode_json_for_storage().unwrap());
    let row = active.update(&txn).await.unwrap();
    let decode_probe = crate::stack_probe::start(&row.conversation_id);
    let proof = TxnValidatedSession::from_row(&txn, row.clone()).unwrap();
    let live = proof.recheck().await.unwrap();
    for (run, actor, device) in [
        ("other", row.actor_id.as_str(), row.device_id.as_str()),
        (
            row.conversation_id.as_str(),
            "other",
            row.device_id.as_str(),
        ),
        (row.conversation_id.as_str(), row.actor_id.as_str(), "other"),
    ] {
        assert!(proof.recheck_subject(run, actor, device).await.is_err());
    }
    assert!(matches!(
        SessionRead::Reuse(&proof).decode(&live).unwrap(),
        super::super::validated_session::DecodedSession::Borrowed(_)
    ));
    proof.validate_parent().await.unwrap();
    assert_eq!(
        decode_probe.count("decode"),
        1,
        "rechecks and the same root must reuse the full decode"
    );
    for axis in 0..7 {
        let mut changed = row.clone();
        match axis {
            0 => changed.id += 1,
            1 => changed.conversation_id.push_str("-other"),
            2 => changed.actor_id.push_str("-other"),
            3 => changed.device_id.push_str("-other"),
            4 => changed.version += 1,
            5 => changed.lease_token += 1,
            _ => changed.state_json.push(' '),
        }
        assert!(proof.reuse(&changed).is_err(), "changed axis {axis}");
    }
    let mut renewed = row.clone();
    renewed.lease_deadline = None;
    assert!(
        proof.reuse(&renewed).is_ok(),
        "renewal is checked from the live row"
    );
    let mut mismatched = row;
    mismatched.version += 1;
    let original_version = proof.session().version;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            version: Set(mismatched.version),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(mismatched.id))
        .exec(&txn)
        .await
        .unwrap();
    let proof = TxnValidatedSession::from_row(&txn, mismatched.clone()).unwrap();
    proof.recheck().await.unwrap();
    assert!(
        proof.validate_parent().await.is_err(),
        "root validation must compare the original JSON version"
    );
    assert_eq!(proof.session().version, original_version);
    assert_eq!(CallSession::Validated(&proof).version(), mismatched.version);
    assert!(proof.session().version != proof.version());
    drop(proof);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn proof_cannot_reuse_json_changed_inside_the_same_transaction() {
    let db = database().await;
    seed_parent(&db).await;
    let txn = db.begin().await.unwrap();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let proof = TxnValidatedSession::from_row(&txn, row.clone()).unwrap();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(format!("{} ", row.state_json)),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .exec(proof.txn())
        .await
        .unwrap();
    assert!(proof.recheck().await.is_err());
    assert!(proof.validate_parent().await.is_err());
    drop(proof);
    txn.rollback().await.unwrap();
}
