use super::*;
use crate::{
    entity::agent_assistant_quota as quota,
    memory_quota::{FactQuotaDelta, apply_on},
};

#[tokio::test]
async fn record_limit_is_per_account_and_lowering_it_keeps_existing_records_updatable() {
    let store = setup().await;
    for actor in ["owner", "other"] {
        ensure_quota_on(&store.db, actor).await.unwrap();
    }
    let policy = AssistantDataPolicy {
        max_memory_records: 2,
        ..Default::default()
    };
    let txn = store.db.begin().await.unwrap();
    apply_on(
        &txn,
        "owner",
        FactQuotaDelta {
            memory_records: 2,
            ..Default::default()
        },
        &policy,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let lowered = AssistantDataPolicy {
        max_memory_records: 1,
        ..policy
    };
    let txn = store.db.begin().await.unwrap();
    apply_on(
        &txn,
        "owner",
        FactQuotaDelta {
            memory_body_bytes: 1,
            ..Default::default()
        },
        &lowered,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let before = quota::Entity::find()
        .filter(quota::Column::ActorId.eq("owner"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.memory_records, 2);
    assert_eq!(before.memory_body_bytes, 1);
    let txn = store.db.begin().await.unwrap();
    assert!(matches!(
        apply_on(
            &txn,
            "owner",
            FactQuotaDelta {
                memory_records: 1,
                ..Default::default()
            },
            &lowered
        )
        .await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("owner"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    let txn = store.db.begin().await.unwrap();
    apply_on(
        &txn,
        "other",
        FactQuotaDelta {
            memory_records: 1,
            ..Default::default()
        },
        &lowered,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("other"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .memory_records,
        1
    );
}
