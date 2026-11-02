use super::*;
use output_archive::gc::schedule;

#[tokio::test]
async fn competing_metadata_claims_preserve_new_native_charges_and_never_grant_source_permission() {
    let (store, mut session, _, parent, _) = admitted_plain("gc-shared-claims").await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let candidate = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let (_, child) = admit_followup(&store, &mut session, &parent, PARENT_TEXT).await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("followup-native", &child)
        .await
        .unwrap();
    let charged = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert!(charged.metadata_bytes > candidate.metadata_bytes);
    let runtime = runtime_on(&store.db, "1").await.unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let first = store.db.clone();
    let second = store.db.clone();
    let (a, b) = tokio::join!(
        schedule::claim_on(
            &first,
            candidate.id,
            &candidate.actor_id,
            candidate.output_gc_next_scan_at_ms,
            now
        ),
        schedule::claim_on(
            &second,
            candidate.id,
            &candidate.actor_id,
            candidate.output_gc_next_scan_at_ms,
            now
        ),
    );
    assert_eq!(u8::from(a.unwrap()) + u8::from(b.unwrap()), 1);
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(after.metadata_bytes, charged.metadata_bytes);
    assert_eq!(after.revision, charged.revision + 1);
    assert_eq!(after.history_body_bytes, charged.history_body_bytes);
    assert_eq!(after.memory_body_bytes, charged.memory_body_bytes);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), runtime);
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
    let next = after.output_gc_next_scan_at_ms.unwrap();
    assert!(
        !schedule::claim_on(&second, after.id, &after.actor_id, Some(next), next - 1)
            .await
            .unwrap()
    );
    assert!(
        !schedule::claim_on(&second, after.id, "another-owner", Some(next), next)
            .await
            .unwrap()
    );
    assert!(
        schedule::claim_on(&second, after.id, &after.actor_id, Some(next), next)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn scheduled_cleanup_uses_actual_proofs_and_refunds_only_once_across_independent_handles() {
    let (store, _, _) = chain("gc-scheduled-retention").await;
    detach_execution(&store).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let runtime = runtime_on(&store.db, "1").await.unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    let outcome = schedule::run_at_with_limit(&store.db, now, 1)
        .await
        .unwrap();
    assert_eq!(
        (
            outcome.claimed_actors,
            outcome.failed_actors,
            outcome.removed
        ),
        (1, 0, 2)
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 0);
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(
        before.metadata_bytes - after.metadata_bytes,
        outcome.refunded_metadata_bytes
    );
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), runtime);
    let independent = store.db.clone();
    assert_eq!(
        schedule::run_at_with_limit(&independent, now, 1)
            .await
            .unwrap()
            .claimed_actors,
        0
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        after
    );
    let retry = schedule::run_at_with_limit(
        &independent,
        after.output_gc_next_scan_at_ms.unwrap() as u64,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        (
            retry.claimed_actors,
            retry.removed,
            retry.refunded_metadata_bytes
        ),
        (1, 0, 0)
    );
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .metadata_bytes,
        after.metadata_bytes
    );
}

#[tokio::test]
async fn failed_collection_keeps_its_committed_retry_time_but_rolls_back_all_source_deletions_and_refunds()
 {
    let (store, _, _) = chain("gc-scheduled-failure").await;
    detach_execution(&store).await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let runtime = runtime_on(&store.db, "1").await.unwrap();
    store.db.execute_unprepared("CREATE TRIGGER reject_scheduled_gc BEFORE DELETE ON agent_assistant_model_output BEGIN SELECT RAISE(ABORT, 'injected archive cleanup failure'); END").await.unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    let outcome = schedule::run_at_with_limit(&store.db, now, 1)
        .await
        .unwrap();
    assert_eq!(
        (
            outcome.claimed_actors,
            outcome.failed_actors,
            outcome.removed,
            outcome.refunded_metadata_bytes
        ),
        (1, 1, 0, 0)
    );
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(
        after.metadata_bytes + outcome.refunded_metadata_bytes,
        before.metadata_bytes
    );
    assert_eq!(
        after.output_gc_completed_at_ms,
        before.output_gc_completed_at_ms
    );
    assert_eq!(after.output_gc_archive_id, before.output_gc_archive_id);
    assert!(after.output_gc_next_scan_at_ms.unwrap() > now as i64);
    assert_eq!(after.revision, before.revision + 1);
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), runtime);
    let independent = store.db.clone();
    assert_eq!(
        schedule::run_at_with_limit(&independent, now, 1)
            .await
            .unwrap()
            .claimed_actors,
        0
    );
    store
        .db
        .execute_unprepared("DROP TRIGGER reject_scheduled_gc")
        .await
        .unwrap();
    let retry = schedule::run_at_with_limit(
        &independent,
        after.output_gc_next_scan_at_ms.unwrap() as u64,
        1,
    )
    .await
    .unwrap();
    assert_eq!((retry.failed_actors, retry.removed), (0, 2));
    assert_eq!(
        before.metadata_bytes
            - quota::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap()
                .metadata_bytes,
        retry.refunded_metadata_bytes
    );
}

#[tokio::test]
async fn corrupt_future_schedule_is_repaired_without_weakening_original_lifetime_dependencies() {
    let (store, _, _) = chain("gc-scheduled-bad-hint").await;
    quota::Entity::update_many()
        .col_expr(
            quota::Column::OutputGcNextScanAtMs,
            Expr::value(Some(i64::MAX)),
        )
        .filter(quota::Column::ActorId.eq("1"))
        .exec(&store.db)
        .await
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let runtime = runtime_on(&store.db, "1").await.unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    let outcome = schedule::run_at_with_limit(&store.db, now, 1)
        .await
        .unwrap();
    assert_eq!(
        (
            outcome.claimed_actors,
            outcome.scanned,
            outcome.held,
            outcome.removed
        ),
        (1, 2, 2, 0)
    );
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(
        after.metadata_bytes + outcome.refunded_metadata_bytes,
        before.metadata_bytes
    );
    assert!(after.output_gc_next_scan_at_ms.unwrap() > now as i64);
    assert!(after.output_gc_next_scan_at_ms.unwrap() < i64::MAX);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), runtime);
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
    assert!(
        schedule::run_at_with_limit(&store.db, u64::MAX, 1)
            .await
            .is_err()
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        after
    );
}

#[tokio::test]
async fn the_production_clock_entry_point_schedules_without_expiring_fresh_source_proofs() {
    let (store, _, _) = chain("gc-production-clock").await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let result = schedule::run_due_once(&store.db).await.unwrap();
    assert_eq!(
        (
            result.claimed_actors,
            result.failed_actors,
            result.scanned,
            result.removed
        ),
        (1, 0, 0, 0)
    );
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(before.metadata_bytes, after.metadata_bytes);
    assert!(after.output_gc_next_scan_at_ms.is_some());
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 2);
}

#[tokio::test]
async fn configured_gc_interval_and_record_batch_apply_to_real_proofs() {
    let (store, _, _) = chain("gc-configured-batch").await;
    detach_execution(&store).await;
    let mut policy = crate::assistant_data_config::policy(&store.db).await;
    let revision = policy.revision;
    policy.limits.processing.output_gc_interval_ms = 17;
    policy.limits.processing.output_gc_batch_size = 1;
    crate::assistant_data_config::update_policy(
        &store.db,
        &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicyUpdate {
            expected_revision: revision,
            policy,
        },
    )
    .await
    .unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64 + 100 * 86_400_000;
    let first = schedule::run_at_with_limit(&store.db, now, 1)
        .await
        .unwrap();
    assert_eq!((first.claimed_actors, first.scanned), (1, 1));
    let row = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(row.output_gc_next_scan_at_ms, Some((now + 17) as i64));
    assert_eq!(
        schedule::run_at_with_limit(&store.db, now + 16, 1)
            .await
            .unwrap()
            .claimed_actors,
        0
    );
    let next = schedule::run_at_with_limit(&store.db, now + 17, 1)
        .await
        .unwrap();
    assert_eq!((next.claimed_actors, next.scanned), (1, 1));
}
