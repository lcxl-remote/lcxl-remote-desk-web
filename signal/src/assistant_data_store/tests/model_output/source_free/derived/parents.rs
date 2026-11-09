use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_assistant_model_input_item as item,
    agent_memory_revision as version,
};

const PARENT_TEXT: &str = "Independent answer without retrieved memory";

mod schedule;

async fn chain(label: &str) -> (TestStore, DataEnvelope, DataEnvelope) {
    let (store, mut session, _, parent, _) = admitted_plain(label).await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let (_, child) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &child)
        .await
        .unwrap();
    (store, parent, child)
}

async fn detach_execution(store: &TestStore) {
    // Simulate already completed native/session retention. This test only
    // exercises independent provenance/edge GC, not native usage refunds.
    native::Entity::delete_many().exec(&store.db).await.unwrap();
    agent_session::Entity::delete_many()
        .filter(agent_session::Column::ActorId.eq("1"))
        .exec(&store.db)
        .await
        .unwrap();
    history::Entity::delete_many()
        .filter(history::Column::ActorId.eq("1"))
        .filter(history::Column::ConversationId.eq("root"))
        .exec(&store.db)
        .await
        .unwrap();
}

async fn gc(
    store: &TestStore,
    now: u64,
    maximum: u64,
) -> Result<output_archive::gc::OutputGcOutcome, AssistantStoreError> {
    output_archive::gc::collect_actor(&store.db, now, maximum).await
}

async fn item_bytes(store: &TestStore) -> i64 {
    item::Entity::find()
        .all(&store.db)
        .await
        .unwrap()
        .iter()
        .map(|row| desk_assistant_data_store::output_store::item_metadata_bytes(row).unwrap())
        .sum()
}

#[tokio::test]
async fn native_acceptance_charges_its_direct_pin_once_and_parent_references_contain_no_ancestor_snapshots()
 {
    let (store, _, _, parent, _) = admitted_plain("pin-accounting").await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
    let (_, child) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let items_before = item_bytes(&store).await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &child)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &child)
        .await
        .unwrap()
        .unwrap();
    let new_items = item_bytes(&store).await - items_before;
    assert!(new_items > 0);
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .metadata_bytes
            - before.metadata_bytes,
        output_archive::metadata_bytes(&stored.row).unwrap() + new_items
    );
    let reference = &stored.proof.input_policy_graph.parents[0];
    let original = output_archive::find_on(&store.db, "root", &parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        desk_assistant_data_store::output_store::find_head_on(&store.db, "root", &parent)
            .await
            .unwrap()
            .unwrap()
            .reference()
            .unwrap(),
        *reference
    );
    reference.validate_archive(&original.proof).unwrap();
    let raw = serde_json::to_value(reference).unwrap();
    assert!(
        raw.get("sources").is_none()
            && raw.get("input_policies").is_none()
            && raw.get("source_set").is_none()
    );
    let charged = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &child)
        .await
        .unwrap();
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        charged
    );
}

#[tokio::test]
async fn late_pin_insert_failure_rolls_back_native_acceptance_archive_and_quota_then_metadata_retry_succeeds()
 {
    let (store, _, _, parent, _) = admitted_plain("pin-insert-failure").await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
    let (_, child) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_parent_pin BEFORE INSERT ON agent_assistant_model_input_item BEGIN SELECT RAISE(ABORT, 'injected pin failure'); END").await.unwrap();
    assert!(
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("followup-native", &child)
            .await
            .is_err()
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 1);
    let row = native::Entity::find_by_id("followup-native")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.model_output_envelope_id.is_none());
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(13), Some(5))
    );
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_parent_pin")
        .await
        .unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &child)
        .await
        .unwrap();
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
}

#[tokio::test]
async fn missing_shared_items_fail_complete_reads_but_not_parent_heads() {
    let (store, parent, _) = chain("shared-item-change").await;
    let stored = item::Entity::find()
        .filter(item::Column::Kind.eq(2))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    // The accepted cost of linear parent checks: heads never load item bodies.
    assert!(
        desk_assistant_data_store::output_store::find_head_on(&store.db, "root", &parent)
            .await
            .unwrap()
            .is_some()
    );
    item::Entity::delete_by_id(stored.id)
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        output_archive::find_on(&store.db, "root", &parent)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn changed_parent_summary_fails_complete_reads() {
    let (store, _, child) = chain("shared-summary-change").await;
    archived::Entity::update_many()
        .col_expr(
            archived::Column::ParentSummaryJson,
            Expr::value(r#"{"source_contract_revision":1,"policy_revision":1,"retained_policy":null,"retained_roots":[]}"#),
        )
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        output_archive::find_on(&store.db, "root", &child)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn repeated_items_are_stored_once_across_calls() {
    let (store, parent, child) = chain("shared-item-once").await;
    let first = output_archive::find_on(&store.db, "root", &parent)
        .await
        .unwrap()
        .unwrap();
    let second = output_archive::find_on(&store.db, "root", &child)
        .await
        .unwrap()
        .unwrap();
    let distinct = first
        .proof
        .sources
        .inputs
        .iter()
        .chain(&second.proof.sources.inputs)
        .map(|input| serde_json::to_string(input).unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        item::Entity::find()
            .filter(item::Column::Kind.eq(1))
            .count(&store.db)
            .await
            .unwrap() as usize,
        distinct.len()
    );
    assert!(
        !second
            .row
            .proof_json
            .contains(&first.proof.sources.inputs[0].envelope_id)
    );
}

#[tokio::test]
async fn model_calls_check_parents_without_reading_their_proof_bodies() {
    let (store, _, _, parent, _) = admitted_plain("pin-head-only").await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
    let (_, child) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    archived::Entity::update_many()
        .col_expr(archived::Column::ProofJson, Expr::value("unreadable"))
        .filter(archived::Column::OutputEnvelopeId.eq(&parent.envelope_id))
        .exec(&store.db)
        .await
        .unwrap();
    item::Entity::delete_many().exec(&store.db).await.unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &child)
        .await
        .unwrap();
    assert!(
        output_archive::find_on(&store.db, "root", &child)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        output_archive::find_on(&store.db, "root", &parent)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn acceptance_leaves_the_archive_as_the_only_copy_of_the_input_lists() {
    let (store, parent, _) = chain("single-list-copy").await;
    let receipt = native::Entity::find_by_id("plain-native")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(receipt.input_snapshot_json.is_empty());
    assert!(receipt.input_lineage_json.is_empty());
    assert!(receipt.envelope_ids_json.is_empty());
    let sources = SignalModelEgressStore::read_context_output_on(&store.db, "root", &parent)
        .await
        .unwrap();
    assert!(!sources.input_snapshot.envelopes.is_empty());
}

#[tokio::test]
async fn gc_keeps_items_until_the_last_archive_of_the_conversation_is_removed() {
    let (store, parent, child) = chain("item-gc-order").await;
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    detach_execution(&store).await;
    assert_eq!(gc(&store, now, 1).await.unwrap().removed, 1);
    assert!(
        output_archive::find_on(&store.db, "root", &child)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        output_archive::find_on(&store.db, "root", &parent)
            .await
            .unwrap()
            .is_some()
    );
    assert!(item::Entity::find().count(&store.db).await.unwrap() > 0);
    assert_eq!(gc(&store, now, 1).await.unwrap().removed, 1);
    assert_eq!(item::Entity::find().count(&store.db).await.unwrap(), 0);
}

#[tokio::test]
async fn cursor_passes_held_rows_then_unpins_newest_children_and_refunds_each_row_once() {
    let (store, parent, child) = chain("pin-gc-order").await;
    let parent_row = output_archive::find_on(&store.db, "root", &parent)
        .await
        .unwrap()
        .unwrap()
        .row;
    let child_row = output_archive::find_on(&store.db, "root", &child)
        .await
        .unwrap()
        .unwrap()
        .row;
    let all_items = item_bytes(&store).await;
    let now = parent_row.completed_at_ms.max(child_row.completed_at_ms) as u64
        + u64::from(AssistantDataPolicy::default().history_retention_days) * 86_400_000
        + 1;
    let floor = parent_row.completed_at_ms.min(child_row.completed_at_ms) as u64
        + u64::from(AssistantDataPolicy::default().history_retention_days) * 86_400_000
        - 1;
    let before_floor = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let result = gc(&store, floor, 1).await.unwrap();
    assert_eq!(result.removed, 0);
    let after_floor = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let mut expected = before_floor.clone();
    expected.metadata_bytes -= 0;

    if expected != before_floor {
        expected.revision += 1;
    }
    assert_eq!(after_floor, expected);
    let runtime = runtime_on(&store.db, "1").await.unwrap();
    for _ in 0..2 {
        let held = gc(&store, now, 1).await.unwrap();
        assert_eq!((held.scanned, held.held, held.removed), (1, 1, 0));
    }
    assert!(gc(&store, now, 1).await.unwrap().cycle_finished);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), runtime);
    detach_execution(&store).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let mut refunded = 0;
    let mut removed = 0;
    for _ in 0..8 {
        let result = gc(&store, now, 1).await.unwrap();
        removed += result.removed;
        refunded += result.refunded_metadata_bytes;
        if archived::Entity::find().count(&store.db).await.unwrap() == 0 {
            break;
        }
        if output_archive::find_on(&store.db, "root", &child)
            .await
            .unwrap()
            .is_some()
        {
            assert!(
                output_archive::find_on(&store.db, "root", &parent)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
    assert_eq!(removed, 2);
    assert_eq!(
        refunded,
        output_archive::metadata_bytes(&child_row).unwrap()
            + output_archive::metadata_bytes(&parent_row).unwrap()
            + all_items
    );
    assert_eq!(item::Entity::find().count(&store.db).await.unwrap(), 0);
    assert!(gc(&store, now, 1).await.unwrap().cycle_finished);
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(before.metadata_bytes - after.metadata_bytes, refunded);
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(gc(&store, now, 1).await.unwrap().refunded_metadata_bytes, 0);
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        after
    );
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), runtime);
}

#[tokio::test]
async fn independent_memories_do_not_pin_original_model_archives_or_their_ancestors() {
    let (store, _, child) = chain("pin-gc-fact-producer").await;
    let child = output_archive::find_on(&store.db, "root", &child)
        .await
        .unwrap()
        .unwrap();
    let original = version::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    // This fixture changes only a lifetime reference, not fact-write authority.
    version::Entity::update_many()
        .col_expr(
            version::Column::ProducerCallId,
            Expr::value(child.row.physical_call_id.clone()),
        )
        .filter(version::Column::Id.eq(original.id))
        .exec(&store.db)
        .await
        .unwrap();
    detach_execution(&store).await;
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    let versions_before = version::Entity::find().all(&store.db).await.unwrap();
    let collected = gc(&store, now, 64).await.unwrap();
    assert_eq!((collected.removed, collected.held), (2, 0));
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        version::Entity::find().all(&store.db).await.unwrap(),
        versions_before
    );
    assert_eq!(gc(&store, now, 64).await.unwrap().removed, 0);
}

#[tokio::test]
async fn malformed_scan_hint_does_not_change_source_permissions_or_block_a_new_scan_cycle() {
    let (store, _, _) = chain("pin-gc-cursor-repair").await;
    quota::Entity::update_many()
        .col_expr(
            quota::Column::OutputGcCompletedAtMs,
            Expr::value(Some(-1_i64)),
        )
        .col_expr(
            quota::Column::OutputGcArchiveId,
            Expr::value(Option::<i64>::None),
        )
        .exec(&store.db)
        .await
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    let result = gc(&store, now, 64).await.unwrap();
    assert_eq!((result.scanned, result.held, result.removed), (2, 2, 0));
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(after.metadata_bytes, before.metadata_bytes);
    assert_eq!(
        (after.output_gc_completed_at_ms, after.output_gc_archive_id),
        (None, None)
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
}

#[tokio::test]
async fn an_untrusted_record_timestamp_is_not_a_deletion_authority_or_a_cursor_that_stalls_valid_records()
 {
    let (store, parent, _) = chain("pin-gc-corrupt-date").await;
    detach_execution(&store).await;
    archived::Entity::update_many()
        .col_expr(archived::Column::CompletedAtMs, Expr::value(-1_i64))
        .filter(archived::Column::OutputEnvelopeId.eq(parent.envelope_id))
        .exec(&store.db)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    assert_eq!(gc(&store, now, 64).await.unwrap().removed, 1);
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 1);
    assert_eq!(gc(&store, now, 64).await.unwrap().scanned, 0);
    assert_eq!(
        archived::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .completed_at_ms,
        -1
    );
}

#[tokio::test]
async fn final_quota_failure_restores_gc_rows_pins_and_cursor() {
    let (store, _, _) = chain("pin-gc-rollback").await;
    detach_execution(&store).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    store.db.execute_unprepared("CREATE TRIGGER reject_gc_refund BEFORE UPDATE ON agent_assistant_quota BEGIN SELECT RAISE(ABORT, 'injected refund failure'); END").await.unwrap();
    assert!(gc(&store, now, 64).await.is_err());
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_gc_refund")
        .await
        .unwrap();
    assert_eq!(gc(&store, now, 64).await.unwrap().removed, 2);
}

#[tokio::test]
async fn transaction_archive_reuse_rechecks_the_complete_original_row() {
    let (store, _, _, parent, _) = admitted_plain("archive-reuse-fence").await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    let known = output_archive::find_on(&txn, "root", &parent)
        .await
        .unwrap()
        .unwrap();
    let repeated = output_archive::find_reusing_on(&txn, "root", &parent, Some(&known))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.row, known.row);
    assert_eq!(repeated.proof, known.proof);
    archived::Entity::update_many()
        .col_expr(
            archived::Column::ProofJson,
            Expr::value("invalid archive encoding"),
        )
        .filter(archived::Column::Id.eq(known.row.id))
        .exec(&txn)
        .await
        .unwrap();
    assert!(
        output_archive::find_reusing_on(&txn, "root", &parent, Some(&known))
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        output_archive::find_on(&store.db, "root", &parent)
            .await
            .unwrap()
            .unwrap()
            .row,
        known.row
    );
}
