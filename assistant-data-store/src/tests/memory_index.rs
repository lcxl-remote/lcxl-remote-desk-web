use super::*;
use crate::{
    entity::{
        agent_assistant_origin as origin, agent_assistant_search_document as document,
        agent_assistant_search_term as term, agent_memory as memory,
    },
    memory_index::{self, MemoryIndexInput},
};
use sea_orm::PaginatorTrait;
use std::collections::BTreeSet;

#[tokio::test]
async fn complete_memory_index_replacement_and_removal_preserve_all_terms_and_refunds() {
    let store = setup().await;
    let body = format!(
        "{}{} tail_index_confirmed",
        "background ".repeat(5000),
        (0x4e00..0x4e00 + 9000)
            .map(|value| char::from_u32(value).unwrap())
            .collect::<String>()
    );
    let mut fact: memory::Model = serde_json::from_value(serde_json::json!({
        "id":1, "memory_id":"memory", "actor_id":"owner", "scope_key":"personal",
        "device_ref":"", "body":body, "context":null, "content_sha256":"a".repeat(64),
        "revision":1, "state_generation":1, "status":"active",
        "accepted_input_seq":1, "observed_at_ms":1, "updated_at_ms":1,
        "valid_from_ms":1, "valid_until_ms":null
    }))
    .unwrap();
    let origin: origin::Model = serde_json::from_value(serde_json::json!({
        "id":1, "origin_id":"origin", "actor_id":"owner", "device_ref":"device",
        "conversation_id":"conversation", "root_conversation_id":"conversation", "native_id":"message",
        "accepted_input_seq":1, "kind":"input", "original_sha256":"a".repeat(64),
        "history_admitted":true, "deletion_generation":1, "deleted_at_ms":null,
        "policy_json":"{}", "created_at_ms":1, "statement_timezone":"UTC"
    })).unwrap();
    let policy = AssistantDataPolicy::default();
    let txn = store.db.begin().await.unwrap();
    let initial = memory_index::prepare_on(
        &txn,
        MemoryIndexInput {
            next: &fact,
            removed: BTreeSet::new(),
            text: &fact.body,
            origin: &origin,
            policy: &policy,
            body_expiry: None,
        },
    )
    .await
    .unwrap();
    let initial_delta = initial.delta;
    assert!(initial_delta.term_records > 16384);
    let image = memory_index::apply_on(&txn, initial).await.unwrap();
    image.validate_on(&txn).await.unwrap();
    assert!(document::Entity::find().count(&txn).await.unwrap() > 8);
    assert_eq!(
        term::Entity::find()
            .filter(term::Column::Term.eq("tail_index_confirmed"))
            .count(&txn)
            .await
            .unwrap(),
        1
    );
    assert!(
        document::Entity::find()
            .filter(document::Column::Partial.eq(true))
            .one(&txn)
            .await
            .unwrap()
            .is_none()
    );
    txn.commit().await.unwrap();

    fact.body = "replacement_only".into();
    fact.revision += 1;
    let txn = store.db.begin().await.unwrap();
    let replacement = memory_index::prepare_on(
        &txn,
        MemoryIndexInput {
            next: &fact,
            removed: BTreeSet::from([fact.memory_id.clone()]),
            text: &fact.body,
            origin: &origin,
            policy: &policy,
            body_expiry: None,
        },
    )
    .await
    .unwrap();
    let replacement_delta = replacement.delta;
    memory_index::validate_original_on(&txn, &replacement)
        .await
        .unwrap();
    memory_index::apply_on(&txn, replacement)
        .await
        .unwrap()
        .validate_on(&txn)
        .await
        .unwrap();
    assert_eq!(document::Entity::find().count(&txn).await.unwrap(), 1);
    assert_eq!(term::Entity::find().count(&txn).await.unwrap(), 1);
    assert_eq!(
        term::Entity::find().one(&txn).await.unwrap().unwrap().term,
        "replacement_only"
    );
    txn.rollback().await.unwrap();
    assert!(document::Entity::find().count(&store.db).await.unwrap() > 8);
    assert_eq!(
        term::Entity::find().count(&store.db).await.unwrap(),
        initial_delta.term_records as u64
    );

    let txn = store.db.begin().await.unwrap();
    let removal = memory_index::prepare_removal_on(&txn, "owner", BTreeSet::from([fact.memory_id]))
        .await
        .unwrap();
    assert_eq!(removal.delta.term_records + initial_delta.term_records, 0);
    assert_eq!(
        removal.delta.metadata_bytes + initial_delta.metadata_bytes,
        0
    );
    assert!(replacement_delta.term_records < 0);
    memory_index::validate_original_on(&txn, &removal)
        .await
        .unwrap();
    memory_index::apply_on(&txn, removal)
        .await
        .unwrap()
        .validate_on(&txn)
        .await
        .unwrap();
    assert_eq!(document::Entity::find().count(&txn).await.unwrap(), 0);
    assert_eq!(term::Entity::find().count(&txn).await.unwrap(), 0);
    txn.commit().await.unwrap();
}
