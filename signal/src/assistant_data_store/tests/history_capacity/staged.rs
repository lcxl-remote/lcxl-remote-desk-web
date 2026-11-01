use super::*;
use crate::assistant_data_store::history as capture_store;
use crate::entity::{
    agent_assistant_history_conversation as conversations,
    agent_assistant_search_document as documents,
};
use desk_diagnose_core::{ai_assistant::source::prepare_capture, chat::ChatMessage};
use sea_orm::ConnectionTrait;
use sea_orm::ExprTrait;

#[tokio::test]
async fn a_native_control_change_after_preparation_is_not_overwritten_by_the_staged_runtime_snapshot()
 {
    let fixture = Fixture::new().await;
    let captures = fixture.captures();
    let refs = captures.iter().collect::<Vec<_>>();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let txn = fixture.begin().await;
    let prepared = capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap();
    runtime::Entity::update_many()
        .col_expr(
            runtime::Column::ControlGeneration,
            sea_orm::sea_query::Expr::col(runtime::Column::ControlGeneration).add(1_i64),
        )
        .col_expr(
            runtime::Column::Revision,
            sea_orm::sea_query::Expr::col(runtime::Column::Revision).add(1_i64),
        )
        .filter(runtime::Column::ActorId.eq("1"))
        .exec(&txn)
        .await
        .unwrap();
    let current = runtime_on(&txn, "1").await.unwrap();
    let outcomes = prepared.finalize_on().await.unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.admission == HistoryAdmission::OmittedPolicy)
    );
    txn.commit().await.unwrap();
    assert_eq!(runtime_on(&fixture.store.db, "1").await.unwrap(), current);
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    let gaps = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_policy"))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gaps.source_count, 3);
}

#[tokio::test]
async fn a_conversation_deleted_by_native_work_after_inspection_is_not_restored_by_the_history_writer()
 {
    let fixture = Fixture::new().await;
    let captures = fixture.captures();
    let txn = fixture.begin().await;
    capture_store::capture_on(&txn, &captures[0], &fixture.policy)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let runtime = runtime_on(&fixture.store.db, "1").await.unwrap();
    let txn = fixture.begin().await;
    let refs = captures[1..].iter().collect::<Vec<_>>();
    let prepared = capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap();
    conversations::Entity::update_many()
        .col_expr(
            conversations::Column::Status,
            sea_orm::sea_query::Expr::value("user_deleted"),
        )
        .col_expr(
            conversations::Column::Revision,
            sea_orm::sea_query::Expr::col(conversations::Column::Revision).add(1_i64),
        )
        .filter(conversations::Column::ConversationId.eq("staged-root"))
        .exec(&txn)
        .await
        .unwrap();
    let deleted = conversations::Entity::find()
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let outcomes = prepared.finalize_on().await.unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.admission == HistoryAdmission::OmittedPolicy)
    );
    txn.commit().await.unwrap();
    assert_eq!(
        conversations::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        deleted
    );
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(runtime_on(&fixture.store.db, "1").await.unwrap(), runtime);
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        1
    );
}

struct Fixture {
    store: TestStore,
    device_ref: String,
    policy: AssistantDataPolicy,
    source_policy: desk_agent_protocol::ai_assistant::source::SourceUsePolicy,
    messages: Vec<ChatMessage>,
    instance: String,
}

impl Fixture {
    async fn new() -> Self {
        let store = setup().await;
        store
            .settings(1, &AssistantDataPolicy::default())
            .await
            .unwrap();
        ensure_quota_on(&store.db, "1").await.unwrap();
        let instance = store.db.config_read().await.metadata.instance.clone();
        let device_ref =
            native_ids::stable(Kind::AssistantDeviceBinding, &(&instance, "staged-device"));
        let messages = (0..3)
            .map(|index| {
                desk_diagnose_core::model_message_labels::model_bound_user_message(
                    native_ids::stable(Kind::Message, &("staged", index)),
                    format!("Research database 中文 record {index}"),
                    DestinationIdentity::Model {
                        connection_id: "model-connection".into(),
                        connection_revision: 1,
                        model_id: "model".into(),
                        profile_revision: 1,
                    },
                )
                .unwrap()
            })
            .collect();
        runtime::Entity::update_many()
            .col_expr(
                runtime::Column::InputSeq,
                sea_orm::sea_query::Expr::value(3_i64),
            )
            .filter(runtime::Column::ActorId.eq("1"))
            .exec(&store.db)
            .await
            .unwrap();
        Self {
            store,
            device_ref,
            policy: AssistantDataPolicy::default(),
            source_policy: owner_text_policy(1, false),
            messages,
            instance,
        }
    }

    fn captures(&self) -> Vec<HistoryCapture<'_>> {
        self.messages
            .iter()
            .enumerate()
            .map(|(index, message)| HistoryCapture {
                native: None,
                actor_id: "1",
                device_ref: &self.device_ref,
                directory: Some(HistorySourceDirectory {
                    instance_id: &self.instance,
                    audience: "staged-device",
                    device_name: Some("Staged device"),
                }),
                conversation_id: "staged-root",
                root_conversation_id: "staged-root",
                native_id: &message.message_id,
                kind: AssistantSourceKind::Input,
                role: "user",
                content: &message.text,
                envelope: message.data_envelope.as_ref().unwrap(),
                source_policy: &self.source_policy,
                dependencies: &[],
                accepted_input_seq: index as u64 + 1,
                recorded_at_unix_ms: 100 + index as u64 * 10,
                statement_timezone: "UTC",
                revision: 1,
            })
            .collect()
    }

    async fn begin(&self) -> crate::config::connection::DatabaseTransaction {
        let txn = crate::db::begin_write(&self.store.db, archive::Entity)
            .await
            .unwrap();
        ensure_runtime_on(&txn, "1").await.unwrap();
        txn
    }
}

#[tokio::test]
async fn staged_batch_prepares_all_sources_before_native_work_and_advances_only_its_actual_writes()
{
    exercise_staged_batch(Fixture::new().await).await;
}

async fn exercise_staged_batch(fixture: Fixture) {
    if std::env::var_os("LRD_TEST_HISTORY_WRITE_STACK_BYTES").is_some() {
        assert_eq!(std::thread::current().name(), Some("history-write-stack"));
    }
    let captures = fixture.captures();
    let refs = captures.iter().collect::<Vec<_>>();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let baseline = runtime_on(&fixture.store.db, "1").await.unwrap();
    let txn = fixture.begin().await;
    let prepared = capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap();
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        before
    );
    // Native work may finish after inspection and before the final quota phase.
    for statement in [
        "CREATE TABLE staged_native_write (phase INTEGER NOT NULL)",
        "INSERT INTO staged_native_write VALUES (0)",
        "CREATE TRIGGER staged_quota_phase AFTER UPDATE ON agent_assistant_quota BEGIN UPDATE staged_native_write SET phase=2; END",
        "CREATE TRIGGER no_native_after_staged_quota BEFORE UPDATE ON staged_native_write WHEN OLD.phase=2 AND NEW.phase!=2 BEGIN SELECT RAISE(ABORT, 'native work after history quota'); END",
        "UPDATE staged_native_write SET phase=1",
    ] {
        txn.execute_unprepared(statement).await.unwrap();
    }
    let outcomes = prepared.finalize_on().await.unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.admission == HistoryAdmission::Indexed)
    );
    txn.commit().await.unwrap();
    let after = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.history_records - before.history_records, 3);
    assert_eq!(
        after.history_body_bytes - before.history_body_bytes,
        fixture
            .messages
            .iter()
            .map(|message| message.text.len() as i64)
            .sum::<i64>()
    );
    let runtime = runtime_on(&fixture.store.db, "1").await.unwrap();
    assert_eq!(
        runtime.history_data_generation,
        baseline.history_data_generation + 3
    );
    assert_eq!(runtime.revision, baseline.revision + 3);
    let conversation = conversations::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            conversation.first_recorded_at_ms,
            conversation.last_recorded_at_ms,
            conversation.revision
        ),
        (100, 120, 3)
    );
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        3
    );
    let txn = fixture.begin().await;
    let replay = capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap()
        .finalize_on()
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        replay
            .iter()
            .map(|outcome| &outcome.source_ids)
            .collect::<Vec<_>>(),
        outcomes
            .iter()
            .map(|outcome| &outcome.source_ids)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        after
    );
    assert_eq!(runtime_on(&fixture.store.db, "1").await.unwrap(), runtime);
    assert_eq!(
        crate::entity::agent_assistant_device_binding::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn a_failed_first_or_middle_unit_does_not_advance_generation_or_poison_the_remaining_units() {
    for failure in [0, 1] {
        exercise_failed_unit(Fixture::new().await, failure).await;
    }
}

async fn exercise_failed_unit(fixture: Fixture, failure: usize) {
    if std::env::var_os("LRD_TEST_HISTORY_WRITE_STACK_BYTES").is_some() {
        assert_eq!(std::thread::current().name(), Some("history-write-stack"));
    }
    let captures = fixture.captures();
    let refs = captures.iter().collect::<Vec<_>>();
    let bad = prepare_capture(&captures[failure], &fixture.policy)
        .unwrap()
        .unwrap();
    let baseline = runtime_on(&fixture.store.db, "1").await.unwrap();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    fixture.store.db.execute_unprepared(&format!(
        "CREATE TRIGGER reject_staged_index BEFORE INSERT ON agent_assistant_search_document WHEN NEW.source_id='{}' BEGIN SELECT RAISE(ABORT, 'injected late history-index failure'); END",
        bad.source_ids[0])).await.unwrap();
    let txn = fixture.begin().await;
    let outcomes = capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap()
        .finalize_on()
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(outcomes[failure].admission, HistoryAdmission::OmittedError);
    assert!(
        outcomes
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != failure)
            .all(|(_, outcome)| outcome.admission == HistoryAdmission::Indexed)
    );
    let after = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.history_records - before.history_records, 2);
    let expected_bytes = fixture
        .messages
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != failure)
        .map(|(_, message)| message.text.len() as i64)
        .sum::<i64>();
    assert_eq!(
        after.history_body_bytes - before.history_body_bytes,
        expected_bytes
    );
    let runtime = runtime_on(&fixture.store.db, "1").await.unwrap();
    assert_eq!(
        runtime.history_data_generation,
        baseline.history_data_generation + 2
    );
    assert_eq!(runtime.revision, baseline.revision + 2);
    let conversation = conversations::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            conversation.first_recorded_at_ms,
            conversation.last_recorded_at_ms,
            conversation.revision
        ),
        (if failure == 0 { 110 } else { 100 }, 120, 2)
    );
    assert_eq!(
        origin::Entity::find()
            .filter(origin::Column::OriginId.eq(bad.origin_id))
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        documents::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        2
    );
    let gaps = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_error"))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gaps.source_count, 1);
    assert_eq!(
        crate::entity::agent_assistant_device_binding::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn capacity_is_decided_per_native_unit_and_does_not_discard_an_admitted_prefix() {
    let mut fixture = Fixture::new().await;
    fixture.policy.max_history_records = 2;
    let captures = fixture.captures();
    let refs = captures.iter().collect::<Vec<_>>();
    let baseline = runtime_on(&fixture.store.db, "1").await.unwrap();
    let txn = fixture.begin().await;
    let outcomes = capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap()
        .finalize_on()
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.admission)
            .collect::<Vec<_>>(),
        vec![
            HistoryAdmission::Indexed,
            HistoryAdmission::Indexed,
            HistoryAdmission::OmittedCapacity
        ]
    );
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap()
            .history_records,
        2
    );
    assert_eq!(
        runtime_on(&fixture.store.db, "1")
            .await
            .unwrap()
            .history_data_generation,
        baseline.history_data_generation + 2
    );
    let conversation = conversations::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (conversation.last_recorded_at_ms, conversation.revision),
        (110, 2)
    );
    let gaps = coverage::Entity::find()
        .filter(coverage::Column::Reason.eq("omitted_capacity"))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gaps.source_count, 1);
}

#[tokio::test]
async fn a_duplicate_unit_is_rejected_before_native_or_quota_writes_and_outer_rollback_erases_a_finished_batch()
 {
    exercise_outer_rollback(Fixture::new().await).await;
}

async fn exercise_outer_rollback(fixture: Fixture) {
    if std::env::var_os("LRD_TEST_HISTORY_WRITE_STACK_BYTES").is_some() {
        assert_eq!(std::thread::current().name(), Some("history-write-stack"));
    }
    let captures = fixture.captures();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let runtime = runtime_on(&fixture.store.db, "1").await.unwrap();
    let txn = fixture.begin().await;
    assert!(
        capture_store::staged::prepare_many_on(
            &txn,
            &[&captures[0], &captures[0]],
            &fixture.policy
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    let txn = fixture.begin().await;
    let refs = captures.iter().collect::<Vec<_>>();
    capture_store::staged::prepare_many_on(&txn, &refs, &fixture.policy)
        .await
        .unwrap()
        .finalize_on()
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(runtime_on(&fixture.store.db, "1").await.unwrap(), runtime);
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        origin::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        conversations::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        crate::entity::agent_assistant_device_binding::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
}

#[test]
fn staged_history_batch_failures_and_outer_rollback_fit_small_stacks() {
    const STACK: &str = "LRD_TEST_HISTORY_WRITE_STACK_BYTES";
    if let Ok(value) = std::env::var(STACK) {
        let bytes = value.parse::<usize>().unwrap();
        assert!([1_048_576, 2_097_152].contains(&bytes));
        actix_web::rt::System::new().block_on(async {
            let batch = Box::new(Fixture::new().await);
            let first_failure = Box::new(Fixture::new().await);
            let middle_failure = Box::new(Fixture::new().await);
            let rollback = Box::new(Fixture::new().await);
            let (done, ready) = tokio::sync::oneshot::channel();
            let thread = std::thread::Builder::new()
                .name("history-write-stack".into())
                .stack_size(bytes)
                .spawn(move || {
                    let runtime = actix_web::rt::System::new();
                    eprintln!(
                        "history batch, failed units and rollback running on {bytes} byte thread"
                    );
                    runtime.block_on(desk_diagnose_core::future::boxed(|| {
                        exercise_staged_batch(*batch)
                    }));
                    runtime.block_on(desk_diagnose_core::future::boxed(|| {
                        exercise_failed_unit(*first_failure, 0)
                    }));
                    runtime.block_on(desk_diagnose_core::future::boxed(|| {
                        exercise_failed_unit(*middle_failure, 1)
                    }));
                    runtime.block_on(desk_diagnose_core::future::boxed(|| {
                        exercise_outer_rollback(*rollback)
                    }));
                    let _ = done.send(());
                })
                .unwrap();
            ready.await.unwrap();
            thread.join().unwrap();
        });
        return;
    }
    for bytes in [1_048_576, 2_097_152] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "staged_history_batch_failures_and_outer_rollback_fit_small_stacks",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_remove("RUST_MIN_STACK")
            .env(STACK, bytes.to_string())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "history write failed on {bytes} bytes: {status}"
        );
    }
}

#[tokio::test]
async fn configured_history_capture_batch_rejects_oversize_without_partial_writes() {
    let fixture = Fixture::new().await;
    let mut policy = fixture.policy.clone();
    policy.limits.processing.history_capture_batch_size = 2;
    let captures = fixture.captures();
    let refs = captures.iter().collect::<Vec<_>>();
    let txn = fixture.begin().await;
    assert!(matches!(
        capture_store::staged::prepare_many_on(&txn, &refs, &policy).await,
        Err(AssistantStoreError::Invalid(
            "invalid history capture batch"
        ))
    ));
    txn.rollback().await.unwrap();
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        0
    );
    policy.limits.processing.history_capture_batch_size = 3;
    let txn = fixture.begin().await;
    let prepared = capture_store::staged::prepare_many_on(&txn, &refs, &policy)
        .await
        .unwrap();
    let outcomes = prepared.finalize_on().await.unwrap();
    assert_eq!(outcomes.len(), 3);
    txn.commit().await.unwrap();
    assert_eq!(
        archive::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        3
    );
}
