use super::super::projection::{load_on, metadata_bytes, prepare_on};
use super::*;
use desk_agent_protocol::{
    ai_assistant::source::SourceReadPurpose, data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    ai_assistant::{
        read_projection::{ReadProjectionKind, ReadProjectionProof},
        seam::AssistantDataReadUse,
    },
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
};
use sea_orm::PaginatorTrait;

// These fixtures exercise storage only. No prepared row is a physical send grant.
pub(super) fn proof(content: &str, revision: &str) -> ReadProjectionProof {
    let envelope = model_bound_user_message(
        format!("source-{revision}"),
        content.into(),
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope
    .unwrap();
    ReadProjectionProof {
        kind: ReadProjectionKind::CurrentMemories,
        actor_id: "1".into(),
        root_conversation_id: "root".into(),
        source_device_ref: "device".into(),
        input_revision: 1,
        actor_input_seq: 1,
        task_id: None,
        producer_message_id: None,
        producer_output_envelope_id: None,
        tool_call_id: None,
        tool_name: "current_memories".into(),
        logical_call_id: "preparation-1".into(),
        request_sha256: "a".repeat(64),
        usage: AssistantDataReadUse {
            decision_roots: Vec::new(),
            projection_id: native_ids::stable(Kind::AssistantReadProjection, &revision),
            source_envelope_id: envelope.envelope_id.clone(),
            purpose: SourceReadPurpose::ModelMemory,
            retrieval_episode_id: "episode".into(),
            scope_sha256: "b".repeat(64),
            policy_revision: 1,
            control_generation: 1,
            memory_state_generation: 1,
            roots: Vec::new(),
            history_dependencies: Vec::new(),
            memories: Vec::new(),
            expires_at_unix_ms: None,
        },
        source_envelope: envelope,
        snapshot_policy: None,
        created_at_unix_ms: 1000,
        unused_until_unix_ms: 1000
            + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS,
    }
}

#[tokio::test]
async fn frozen_result_replay_does_not_charge_again_or_replace_content_after_requery() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let original = proof("{\"memories\":[]}", "first");
    let txn = store.db.begin().await.unwrap();
    let stored = prepare_on(&txn, &original, "{\"memories\":[]}", &policy, 1001)
        .await
        .unwrap();
    let expected = metadata_bytes(&stored.row).unwrap();
    txn.commit().await.unwrap();
    let before = crate::entity::agent_assistant_quota::Entity::find()
        .filter(crate::entity::agent_assistant_quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.metadata_bytes, 8192 + expected);
    let mut later = proof("{\"memories\":[\"new result\"]}", "later");
    later.created_at_unix_ms = 1010;
    later.unused_until_unix_ms =
        1010 + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS;
    let txn = store.db.begin().await.unwrap();
    let frozen = prepare_on(
        &txn,
        &later,
        "{\"memories\":[\"new result\"]}",
        &policy,
        1011,
    )
    .await
    .unwrap();
    assert_eq!(frozen.row.result_json.as_deref(), Some("{\"memories\":[]}"));
    assert_eq!(frozen.proof, Box::new(original));
    txn.commit().await.unwrap();
    assert_eq!(
        crate::entity::agent_assistant_quota::Entity::find()
            .filter(crate::entity::agent_assistant_quota::Column::ActorId.eq("1"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        crate::entity::agent_assistant_read_projection::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn preparation_capacity_rejection_rolls_back_without_a_partial_proof_or_metadata_charge() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    ensure_quota_on(&store.db, "1").await.unwrap();
    policy.max_metadata_bytes = 8192;
    let content = serde_json::json!({"memories":["x".repeat(64 * 1024)]}).to_string();
    let original = proof(&content, "capacity");
    let txn = store.db.begin().await.unwrap();
    assert!(matches!(
        prepare_on(&txn, &original, &content, &policy, 1001).await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(
        crate::entity::agent_assistant_read_projection::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        crate::entity::agent_assistant_quota::Entity::find()
            .filter(crate::entity::agent_assistant_quota::Column::ActorId.eq("1"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .metadata_bytes,
        8192
    );
}

#[tokio::test]
async fn scoped_projection_lookup_rejects_modified_bindings_and_cannot_read_another_actor() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let original = proof("{}", "first");
    let txn = store.db.begin().await.unwrap();
    let stored = prepare_on(&txn, &original, "{}", &policy, 1001)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        load_on(&store.db, "2", &stored.row.projection_id)
            .await
            .unwrap()
            .is_none()
    );
    let expired = load_on(&store.db, "1", &stored.row.projection_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !expired
            .proof
            .preparation_available(expired.proof.unused_until_unix_ms, None)
    );
    crate::entity::agent_assistant_read_projection::Entity::update_many()
        .col_expr(
            crate::entity::agent_assistant_read_projection::Column::SourceEnvelopeId,
            sea_orm::sea_query::Expr::value("changed-envelope"),
        )
        .filter(
            crate::entity::agent_assistant_read_projection::Column::ProjectionId
                .eq(&stored.row.projection_id),
        )
        .exec(&store.db)
        .await
        .unwrap();
    assert!(matches!(
        load_on(&store.db, "1", &stored.row.projection_id).await,
        Err(AssistantStoreError::Invalid(_))
    ));
}

#[tokio::test]
async fn configured_longer_preparation_round_trips_its_original_absolute_deadline() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let mut original = proof("{}", "longer-lifetime");
    original.unused_until_unix_ms = original.created_at_unix_ms + 3_600_000;
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &original, "{}", &policy, 1001)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let loaded = load_on(&store.db, "1", &original.usage.projection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*loaded.proof, original);
    assert!(loaded.proof.preparation_available(1_801_001, None));
    assert!(!loaded.proof.preparation_available(3_601_000, None));
    assert!(loaded.proof.preparation_available(3_601_000, Some(1100)));
}

#[tokio::test]
async fn large_projection_round_trips_and_replays_with_exact_actual_metadata_charge() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let content = serde_json::json!({"memories":["x".repeat(64 * 1024)]}).to_string();
    let original = proof(&content, "large");
    let txn = store.db.begin().await.unwrap();
    let stored = prepare_on(&txn, &original, &content, &policy, 1001)
        .await
        .unwrap();
    let charge = metadata_bytes(&stored.row).unwrap();
    assert!(charge > content.len() as i64);
    txn.commit().await.unwrap();
    let loaded = load_on(&store.db, "1", &original.usage.projection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.row.result_json.as_deref(), Some(content.as_str()));
    assert_eq!(*loaded.proof, original);
    let txn = store.db.begin().await.unwrap();
    let replay = prepare_on(&txn, &original, &content, &policy, 1002)
        .await
        .unwrap();
    assert_eq!(replay.row, loaded.row);
    txn.commit().await.unwrap();
    let row = crate::entity::agent_assistant_quota::Entity::find()
        .filter(crate::entity::agent_assistant_quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.metadata_bytes, 8192 + charge);
}

#[tokio::test]
async fn decoded_projection_reuse_preserves_every_raw_field_fence_and_first_read_validation() {
    use super::super::projection::validate_unchanged_on;
    let store = setup().await;
    let content = "{\"memories\":[]}";
    let proof = proof(content, "reuse");
    let stored = prepare_on(
        &store.db,
        &proof,
        content,
        &AssistantDataPolicy::default(),
        1001,
    )
    .await
    .unwrap();
    assert!(validate_unchanged_on(&store.db, &stored).await.is_ok());
    use crate::entity::agent_assistant_read_projection as projection;
    for mutation in 0..3 {
        let txn = store.db.begin().await.unwrap();
        let update =
            projection::Entity::update_many().filter(projection::Column::Id.eq(stored.row.id));
        let update = match mutation {
            0 => update.col_expr(
                projection::Column::RevokedAtMs,
                sea_orm::sea_query::Expr::value(Some(2000_i64)),
            ),
            1 => update.col_expr(
                projection::Column::ResultJson,
                sea_orm::sea_query::Expr::value(Some("{corrupt".to_string())),
            ),
            _ => update.col_expr(
                projection::Column::Revision,
                sea_orm::sea_query::Expr::value(2_i64),
            ),
        };
        update.exec(&txn).await.unwrap();
        assert!(validate_unchanged_on(&txn, &stored).await.is_err());
        if mutation == 1 {
            assert!(load_on(&txn, "1", &stored.row.projection_id).await.is_err());
        }
        txn.rollback().await.unwrap();
    }
}
