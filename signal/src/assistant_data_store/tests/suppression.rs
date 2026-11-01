use super::super::suppression::{suppress_on, suppressed_on};
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization,
    agent_assistant_source_suppression as suppression, agent_memory as memory,
};
use desk_agent_protocol::ai_assistant::{memory::MemoryDeleteRequest, source::HistoryDependency};
use desk_diagnose_core::{
    ai_assistant::memory_control::KnownMemorySource,
    native_ids::{self, Kind},
};
use sea_orm::PaginatorTrait;

fn known(label: &str) -> KnownMemorySource {
    KnownMemorySource {
        source_id: native_ids::stable(Kind::AssistantSource, &label),
        revision: 1,
        origin_id: native_ids::stable(Kind::AssistantOrigin, &"shared-origin"),
        content_sha256: "a".repeat(64),
    }
}

fn dependency(source: &KnownMemorySource) -> HistoryDependency {
    HistoryDependency {
        source_id: source.source_id.clone(),
        source_revision: source.revision,
        content_sha256: source.content_sha256.clone(),
        origin_id: source.origin_id.clone(),
        origin_deletion_generation: 1,
        policy_revision: 1,
        expires_at_unix_ms: None,
    }
}

#[tokio::test]
async fn suppression_matches_the_exact_fragment_version_origin_and_hash_for_the_same_actor() {
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    let source = known("preference");
    let txn = store.db.begin().await.unwrap();
    let delta = suppress_on(&txn, "1", std::slice::from_ref(&source), 11, 2, 100)
        .await
        .unwrap();
    assert!(delta > 0);
    assert!(
        suppressed_on(&txn, "1", &dependency(&source))
            .await
            .unwrap()
    );
    assert!(
        !suppressed_on(&txn, "2", &dependency(&source))
            .await
            .unwrap()
    );
    let mut changed = dependency(&source);
    changed.source_id = known("unrelated-in-same-message").source_id;
    assert!(!suppressed_on(&txn, "1", &changed).await.unwrap());
    let mut changed = dependency(&source);
    changed.source_revision += 1;
    assert!(!suppressed_on(&txn, "1", &changed).await.unwrap());
    let mut changed = dependency(&source);
    changed.content_sha256 = "b".repeat(64);
    assert!(!suppressed_on(&txn, "1", &changed).await.unwrap());
    let mut changed = dependency(&source);
    changed.origin_id = native_ids::stable(Kind::AssistantOrigin, &"another-origin");
    assert!(!suppressed_on(&txn, "1", &changed).await.unwrap());
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn bulk_upsert_deduplicates_charge_and_keeps_monotonic_input_and_control_fences() {
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    let sources = (0..128)
        .map(|index| known(&format!("source-{index}")))
        .collect::<Vec<_>>();
    let txn = store.db.begin().await.unwrap();
    let initial = suppress_on(&txn, "1", &sources, 11, 2, 100).await.unwrap();
    assert!(initial > 0);
    let repeated = sources
        .iter()
        .cloned()
        .chain(sources.iter().cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        suppress_on(&txn, "1", &repeated, 11, 2, 100).await.unwrap(),
        0
    );
    // Different manager clocks do not supersede the durable input/control order.
    assert_eq!(
        suppress_on(&txn, "1", &sources, 12, 3, 99).await.unwrap(),
        0
    );
    assert_eq!(suppression::Entity::find().count(&txn).await.unwrap(), 128);
    let row = suppression::Entity::find()
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.cutoff_input_seq, 12);
    assert_eq!(row.control_generation, 3);
    assert_eq!(row.created_at_ms, 100);
    assert!(suppress_on(&txn, "1", &sources, 11, 2, 101).await.is_err());
    txn.rollback().await.unwrap();
    assert_eq!(
        suppression::Entity::find().count(&store.db).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn owner_control_accounts_once_and_stale_or_replayed_controls_do_not_repeat_suppression() {
    let store = setup().await;
    let (memory_id, source, _) = super::owner_memory::seed(&store, 1, "accounted-control").await;
    let request = MemoryDeleteRequest {
        memory_id: memory_id.clone(),
        expected_revision: 1,
        operation_id: "control-charge".into(),
    };
    let policy = AssistantDataPolicy::default();
    let first = store
        .forget_memory(1, &request, &policy, 100)
        .await
        .unwrap();
    assert_eq!(first.affected_count, 1);
    let before = quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(before.metadata_bytes > 8192);
    let count = suppression::Entity::find().count(&store.db).await.unwrap();
    assert_eq!(count, 2); // Original fact fragment and its independent evidence identity.
    store
        .forget_memory(1, &request, &policy, 101)
        .await
        .unwrap();
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("1"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        suppression::Entity::find().count(&store.db).await.unwrap(),
        count
    );
    let fact = memory::Entity::find()
        .filter(memory::Column::MemoryId.eq(memory_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fact.status, "forgotten");
    assert!(
        store
            .owner_source(1, &source, &policy, 101)
            .await
            .unwrap()
            .content
            .is_none()
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
}
