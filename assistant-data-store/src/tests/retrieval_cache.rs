use super::super::retrieval_cache::{
    self as cache, load_cursor_on, load_logical_on, prepare_on, prune_expired_on, update_on,
};
use super::*;
use crate::entity::{
    agent_assistant_quota as quota, agent_assistant_read_projection as projection,
    agent_assistant_retrieval_query as query,
};
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope, user_instructions::AssistantDataSettings,
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::owner_history_scope,
        history::{RetrievalCallKind, RetrievalReservation},
        retrieval_query::{PageAnchor, QuerySourceVersion},
        retrieval_state::{
            FrozenRetrievalQuery, FrozenRetrievalRequest, RetrievalQueryBinding,
            RetrievalSelection, RetrievalWatermark,
        },
    },
    native_ids::{self, Kind},
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

// These rows store query metadata only. They do not authorize any source read.
fn source(label: &str) -> QuerySourceVersion {
    QuerySourceVersion {
        source_id: native_ids::stable(Kind::AssistantSource, &label),
        revision: 1,
    }
}
fn fixture(logical: &str, captured: u64) -> FrozenRetrievalQuery {
    let binding = RetrievalQueryBinding {
        actor_id: "1".into(),
        root_conversation_id: "root".into(),
        retrieval_episode_id: "episode".into(),
        source_device_ref: "device".into(),
        task_id: None,
        scope_sha256: "a".repeat(64),
        history_scope: owner_history_scope(
            "1",
            "root",
            "device",
            1,
            100,
            "episode",
            1,
            &AssistantDataSettings::default(),
            "UTC",
            false,
        )
        .unwrap(),
        memory_device_rule: Some(AssistantDeviceScope::AllMyDevices),
        request_memory_device_refs: Vec::new(),
        destination: super::destination("model"),
        authority_expires_at_unix_ms: None,
    };
    let request = FrozenRetrievalRequest::Conversations {
        request: serde_json::from_value(serde_json::json!({
            "queries": ["REPORT 邮件"], "time_range": null, "cursor": null,
        }))
        .unwrap(),
    };
    let reservation = RetrievalReservation {
        logical_call_id: logical.into(),
        request_sha256: "b".repeat(64),
        kind: RetrievalCallKind::Relevance,
        reserved_at_unix_ms: captured,
        reserved_bytes: 16384,
        actual_bytes: None,
    };
    FrozenRetrievalQuery::prepare(
        binding,
        &reservation,
        request,
        RetrievalSelection {
            devices: vec!["device".into()],
            watermark: RetrievalWatermark {
                corpus_generation: 10,
                memory_generation: 1,
                control_generation: 1,
                policy_revision: 1,
            },
            candidates: vec![source("one"), source("two")],
            reasons: vec!["candidate_statistics".into()],
        },
        captured + 1,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap()
}
async fn quota_row(store: &TestStore) -> quota::Model {
    quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn original_query_survives_logical_retries_without_requerying_recharging_or_renewing() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let proof = fixture("query-one", 1000);
    let txn = store.db.begin().await.unwrap();
    let original = prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    assert!(original.row.proof_json.len() > original.row.proof_json.chars().count());
    let before = quota_row(&store).await;
    assert_eq!(
        before.metadata_bytes,
        8192 + cache::metadata_bytes(&original.row).unwrap()
    );
    let mut rescanned = proof.clone();
    rescanned.snapshot.candidates = vec![source("later")];
    rescanned.refreshed_at_unix_ms = 2000;
    let txn = store.db.begin().await.unwrap();
    let replay = prepare_on(&txn, &rescanned, &policy, 2100).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(replay.row, original.row);
    assert_eq!(replay.proof, original.proof);
    assert_eq!(quota_row(&store).await, before);
    let loaded = load_logical_on(
        &store.db,
        &proof.binding,
        &proof.initial_logical_call_id,
        &proof.request,
        2200,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(loaded.row, original.row);
    assert!(
        load_logical_on(
            &store.db,
            &proof.binding,
            &proof.initial_logical_call_id,
            &proof.request,
            proof.snapshot.expires_at_unix_ms
        )
        .await
        .is_err()
    );
    assert_eq!(query::Entity::find().count(&store.db).await.unwrap(), 1);
}

#[tokio::test]
async fn cache_lookup_binds_actor_recipient_semantic_arguments_and_original_page_identity() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let mut proof = fixture("query-one", 1000);
    let cursor = proof
        .add_page(
            native_ids::stable(Kind::AssistantPageCursor, &"page"),
            PageAnchor::Relevance { next_offset: 1 },
        )
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    let mut equivalent = proof.request.clone();
    if let FrozenRetrievalRequest::Conversations { request } = &mut equivalent {
        request.queries = vec!["邮件 report".into()];
        request.cursor = Some(cursor.clone());
    }
    assert!(
        load_cursor_on(&store.db, &proof.binding, &equivalent, &cursor, 1200)
            .await
            .is_ok()
    );
    let mut other = proof.binding.clone();
    other.actor_id = "2".into();
    other.history_scope.as_mut().unwrap().actor_id = "2".into();
    assert!(
        load_cursor_on(&store.db, &other, &proof.request, &cursor, 1200)
            .await
            .is_err()
    );
    assert!(
        load_logical_on(
            &store.db,
            &other,
            &proof.initial_logical_call_id,
            &proof.request,
            1200
        )
        .await
        .unwrap()
        .is_none()
    );
    let mut other = proof.binding.clone();
    other.destination = super::destination("another-model");
    assert!(
        load_logical_on(
            &store.db,
            &other,
            &proof.initial_logical_call_id,
            &proof.request,
            1200
        )
        .await
        .is_err()
    );
    let mut changed = proof.request.clone();
    if let FrozenRetrievalRequest::Conversations { request } = &mut changed {
        request.queries = vec!["unrelated".into()];
    }
    assert!(
        load_cursor_on(&store.db, &proof.binding, &changed, &cursor, 1200)
            .await
            .is_err()
    );
    let forged = desk_diagnose_core::ai_assistant::retrieval_state::QueryCursor::new(
        &proof.snapshot.query_id,
        &native_ids::stable(Kind::AssistantPageCursor, &"unknown-page"),
    )
    .unwrap()
    .encode();
    assert!(
        load_cursor_on(&store.db, &proof.binding, &proof.request, &forged, 1200)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn query_update_uses_a_fresh_raw_row_cas_and_accounts_only_its_exact_utf8_growth() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let proof = fixture("query-one", 1000);
    let txn = store.db.begin().await.unwrap();
    let old = prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    let before = quota_row(&store).await;
    let mut next = proof.clone();
    next.remember(&[source("one")]).unwrap();
    let cursor = next
        .add_page(
            native_ids::stable(Kind::AssistantPageCursor, &"page"),
            PageAnchor::Relevance { next_offset: 1 },
        )
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    let saved = update_on(&txn, &old, &next, &policy, 1200).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(saved.row.revision, old.row.revision + 1);
    assert_eq!(
        quota_row(&store).await.metadata_bytes - before.metadata_bytes,
        cache::metadata_bytes(&saved.row).unwrap() - cache::metadata_bytes(&old.row).unwrap()
    );
    assert!(
        load_cursor_on(&store.db, &proof.binding, &proof.request, &cursor, 1200)
            .await
            .is_ok()
    );
    let before = quota_row(&store).await;
    let txn = store.db.begin().await.unwrap();
    let same = update_on(&txn, &saved, &next, &policy, 1300).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(same.row, saved.row);
    assert_eq!(quota_row(&store).await, before);
    let txn = store.db.begin().await.unwrap();
    assert!(update_on(&txn, &old, &next, &policy, 1300).await.is_err());
    txn.rollback().await.unwrap();
    // Even a raw change under the same revision invalidates a previously decoded row.
    query::Entity::update_many()
        .col_expr(
            query::Column::ProofJson,
            Expr::value(format!("{} ", saved.row.proof_json)),
        )
        .filter(query::Column::Id.eq(saved.row.id))
        .exec(&store.db)
        .await
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(update_on(&txn, &saved, &next, &policy, 1400).await.is_err());
    txn.rollback().await.unwrap();
    assert!(
        load_logical_on(
            &store.db,
            &proof.binding,
            &proof.initial_logical_call_id,
            &proof.request,
            1400
        )
        .await
        .is_err()
    );
    assert_eq!(quota_row(&store).await, before);
}

#[tokio::test]
async fn capacity_failures_roll_back_new_queries_and_page_metadata_without_a_partial_charge() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    super::super::ensure_quota_on(&store.db, "1").await.unwrap();
    let before = quota_row(&store).await;
    let proof = fixture("query-one", 1000);
    policy.max_metadata_bytes = 8192;
    let txn = store.db.begin().await.unwrap();
    assert!(matches!(
        prepare_on(&txn, &proof, &policy, 1100).await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(quota_row(&store).await, before);
    assert_eq!(query::Entity::find().count(&store.db).await.unwrap(), 0);
    policy = AssistantDataPolicy::default();
    let txn = store.db.begin().await.unwrap();
    let old = prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    let before = quota_row(&store).await;
    policy.max_metadata_bytes = before.metadata_bytes as u64;
    let mut next = proof;
    next.remember(&[source("one")]).unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(matches!(
        update_on(&txn, &old, &next, &policy, 1200).await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(quota_row(&store).await, before);
    assert_eq!(
        query::Entity::find_by_id(old.row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        old.row
    );
}

#[tokio::test]
async fn a_scope_refresh_can_release_metadata_when_a_new_platform_limit_is_already_exceeded() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    let proof = fixture("query-one", 1000);
    let txn = store.db.begin().await.unwrap();
    let old = prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    let before = quota_row(&store).await;
    let mut next = proof;
    let mut watermark = next.watermark();
    watermark.control_generation += 1;
    next.refresh(
        RetrievalSelection {
            devices: vec!["device".into()],
            watermark,
            candidates: Vec::new(),
            reasons: Vec::new(),
        },
        1200,
    )
    .unwrap();
    policy.max_metadata_bytes = 8192;
    let txn = store.db.begin().await.unwrap();
    let saved = update_on(&txn, &old, &next, &policy, 1200).await.unwrap();
    txn.commit().await.unwrap();
    assert!(cache::metadata_bytes(&saved.row).unwrap() < cache::metadata_bytes(&old.row).unwrap());
    assert_eq!(
        quota_row(&store).await.metadata_bytes,
        before.metadata_bytes + cache::metadata_bytes(&saved.row).unwrap()
            - cache::metadata_bytes(&old.row).unwrap()
    );
}

#[tokio::test]
async fn bounded_cleanup_refunds_only_expired_cache_rows_and_keeps_independent_read_proofs() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let txn = store.db.begin().await.unwrap();
    let proof = super::projection::proof("{\"memories\":[]}", "independent");
    let independent =
        super::super::projection::prepare_on(&txn, &proof, "{\"memories\":[]}", &policy, 1100)
            .await
            .unwrap();
    txn.commit().await.unwrap();
    let baseline = quota_row(&store).await;
    let mut expected = Vec::new();
    for index in 0..5 {
        let proof = fixture(&format!("query-{index}"), 1000);
        let txn = store.db.begin().await.unwrap();
        let stored = prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
        txn.commit().await.unwrap();
        expected.push(stored.row);
    }
    let future = fixture("query-future", 10_000);
    let txn = store.db.begin().await.unwrap();
    let future = prepare_on(&txn, &future, &policy, 10_001).await.unwrap();
    txn.commit().await.unwrap();
    let charged_before_cleanup = quota_row(&store).await.metadata_bytes;
    let now = 1000 + desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS;
    let mut deleted = 0;
    loop {
        let txn = store.db.begin().await.unwrap();
        let count = prune_expired_on(&txn, "1", now, 2).await.unwrap();
        txn.commit().await.unwrap();
        deleted += count;
        if count == 0 {
            break;
        }
    }
    assert_eq!(deleted, 5);
    assert_eq!(query::Entity::find().count(&store.db).await.unwrap(), 1);
    assert_eq!(
        query::Entity::find().one(&store.db).await.unwrap().unwrap(),
        future.row
    );
    assert_eq!(
        quota_row(&store).await.metadata_bytes,
        baseline.metadata_bytes + cache::metadata_bytes(&future.row).unwrap()
    );
    assert_eq!(
        projection::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        independent.row
    );
    assert!(prune_expired_on(&store.db, "1", now, 129).await.is_err());
    assert_eq!(prune_expired_on(&store.db, "2", now, 2).await.unwrap(), 0);
    assert_eq!(
        charged_before_cleanup - quota_row(&store).await.metadata_bytes,
        expected
            .iter()
            .map(|row| cache::metadata_bytes(row).unwrap())
            .sum::<i64>()
    );
}

#[tokio::test]
async fn failed_final_refund_rolls_back_cache_deletion_and_never_recreates_a_removed_partition() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let proof = fixture("query-one", 1000);
    let txn = store.db.begin().await.unwrap();
    let old = prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    let before = quota_row(&store).await;
    store.db.execute_unprepared("CREATE TRIGGER reject_query_refund BEFORE UPDATE ON agent_assistant_quota BEGIN SELECT RAISE(ABORT, 'injected quota failure'); END").await.unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(
        prune_expired_on(&txn, "1", proof.snapshot.expires_at_unix_ms, 2)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        query::Entity::find().one(&store.db).await.unwrap().unwrap(),
        old.row
    );
    assert_eq!(quota_row(&store).await, before);
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_query_refund")
        .await
        .unwrap();
    quota::Entity::delete_many()
        .filter(quota::Column::ActorId.eq("1"))
        .exec(&store.db)
        .await
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    assert_eq!(
        prune_expired_on(&txn, "1", proof.snapshot.expires_at_unix_ms, 2)
            .await
            .unwrap(),
        1
    );
    txn.commit().await.unwrap();
    assert_eq!(quota::Entity::find().count(&store.db).await.unwrap(), 0);
}

#[test]
fn corrupted_denormalized_query_bindings_are_rejected_even_with_a_valid_json_digest() {
    let proof = fixture("query-one", 1000);
    // Construct the canonical row through its public domain fields, then alter one column.
    let raw = proof.encode().unwrap();
    let row = query::Model {
        id: 1,
        query_id: proof.snapshot.query_id.clone(),
        actor_id: "1".into(),
        root_conversation_id: "root".into(),
        retrieval_episode_id: "episode".into(),
        purpose: "model_history".into(),
        kind: proof.request.kind().into(),
        request_sha256: proof.snapshot.request_sha256.clone(),
        scope_sha256: proof.binding.fingerprint().unwrap(),
        proof_json: raw,
        captured_at_ms: proof.snapshot.captured_at_unix_ms as i64,
        expires_at_ms: proof.snapshot.expires_at_unix_ms as i64,
        revision: 1,
    };
    assert!(cache::decode_row(row.clone()).is_ok());
    let mut changed = row.clone();
    changed.scope_sha256 = "f".repeat(64);
    assert!(cache::decode_row(changed).is_err());
    let mut changed = row.clone();
    changed.actor_id = "2".into();
    assert!(cache::decode_row(changed).is_err());
    let mut changed = row;
    changed.expires_at_ms += 1;
    assert!(cache::decode_row(changed).is_err());
}

#[tokio::test]
async fn window_lookup_uses_its_frozen_anchor_and_rejects_changed_bounds_purpose_or_recipient() {
    use desk_agent_protocol::ai_assistant::history::{
        ConversationReadRequest, ConversationReadView,
    };
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let original = fixture("window-original", 1000);
    let anchor = source("original-anchor");
    let request = ConversationReadRequest {
        source_id: anchor.source_id.clone(),
        view: ConversationReadView::Latest,
        limit: 10,
        max_bytes: 32768,
        cursor: None,
    };
    let reservation = RetrievalReservation {
        logical_call_id: "window-original".into(),
        request_sha256: "b".repeat(64),
        kind: RetrievalCallKind::Read,
        reserved_at_unix_ms: 1000,
        reserved_bytes: 32768,
        actual_bytes: None,
    };
    let mut proof = FrozenRetrievalQuery::prepare(
        original.binding.clone(),
        &reservation,
        FrozenRetrievalRequest::Window {
            request: request.clone(),
            anchor: anchor.clone(),
            conversation_id: "past-root".into(),
        },
        RetrievalSelection {
            devices: vec!["device".into()],
            watermark: original.watermark(),
            candidates: Vec::new(),
            reasons: Vec::new(),
        },
        1100,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap();
    let cursor = proof
        .add_page(
            native_ids::stable(Kind::AssistantPageCursor, &"original-window-page"),
            PageAnchor::Window {
                admitted_generation: 5,
                fragment_index: 0,
                source_id: anchor.source_id.clone(),
                forward: true,
            },
        )
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, &policy, 1101).await.unwrap();
    txn.commit().await.unwrap();
    let before = quota_row(&store).await;
    let retry =
        cache::load_window_logical_on(&store.db, &proof.binding, "window-original", &request, 1200)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(retry.proof.request, proof.request);
    let mut continuation = request.clone();
    continuation.cursor = Some(cursor);
    let resumed = cache::load_window_cursor_on(&store.db, &proof.binding, &continuation, 1200)
        .await
        .unwrap();
    assert_eq!(resumed.proof.request, proof.request);
    assert_eq!(resumed.proof.source_observed_at_unix_ms, 1100);
    if let FrozenRetrievalRequest::Window {
        anchor: retained, ..
    } = &resumed.proof.request
    {
        assert_eq!(retained, &anchor);
    } else {
        panic!("window cache changed its purpose");
    }
    // No latest-source lookup is part of this metadata API. A new observed
    // version cannot replace the version admitted by the original window.
    let mut newer = anchor.clone();
    newer.revision = 2;
    let changed_anchor = FrozenRetrievalRequest::Window {
        request: request.clone(),
        anchor: newer,
        conversation_id: "past-root".into(),
    };
    assert!(
        load_logical_on(
            &store.db,
            &proof.binding,
            "window-original",
            &changed_anchor,
            1200
        )
        .await
        .is_err()
    );
    let mut variations = Vec::new();
    let mut changed = continuation.clone();
    changed.source_id = source("other-anchor").source_id;
    variations.push(changed);
    let mut changed = continuation.clone();
    changed.view = ConversationReadView::Context;
    variations.push(changed);
    let mut changed = continuation.clone();
    changed.limit -= 1;
    variations.push(changed);
    let mut changed = continuation.clone();
    changed.max_bytes -= 1;
    variations.push(changed);
    for changed in variations {
        assert!(
            cache::load_window_cursor_on(&store.db, &proof.binding, &changed, 1200)
                .await
                .is_err()
        );
    }
    let mut other = proof.binding.clone();
    other.destination = super::destination("another-model");
    assert!(
        cache::load_window_cursor_on(&store.db, &other, &continuation, 1200)
            .await
            .is_err()
    );
    let mut other = proof.binding.clone();
    other.retrieval_episode_id = "other-episode".into();
    other.history_scope.as_mut().unwrap().retrieval_episode_id = "other-episode".into();
    assert!(
        cache::load_window_cursor_on(&store.db, &other, &continuation, 1200)
            .await
            .is_err()
    );
    assert!(
        cache::load_window_cursor_on(
            &store.db,
            &proof.binding,
            &continuation,
            proof.snapshot.expires_at_unix_ms
        )
        .await
        .is_err()
    );
    let search = fixture("search-only", 1000);
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &search, &policy, 1101).await.unwrap();
    txn.commit().await.unwrap();
    assert!(
        cache::load_window_logical_on(&store.db, &search.binding, "search-only", &request, 1200)
            .await
            .is_err()
    );
    // Metadata reads never charge or mutate the original window cache.
    let final_quota = quota_row(&store).await;
    let search_row = query::Entity::find()
        .filter(query::Column::QueryId.eq(&search.snapshot.query_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_quota.metadata_bytes - before.metadata_bytes,
        cache::metadata_bytes(&search_row).unwrap()
    );
}

#[tokio::test]
async fn configured_longer_query_round_trips_its_original_absolute_deadline() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    let mut proof = fixture("longer-lifetime", 1000);
    proof.snapshot.expires_at_unix_ms = 3_601_000;
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, &policy, 1100).await.unwrap();
    txn.commit().await.unwrap();
    let loaded = load_logical_on(
        &store.db,
        &proof.binding,
        &proof.initial_logical_call_id,
        &proof.request,
        1_801_001,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(loaded.proof.snapshot.expires_at_unix_ms, 3_601_000);
}
