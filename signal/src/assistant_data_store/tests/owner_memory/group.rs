//! Storage-only owner-control groups grant no model or source authority.
use super::super::super::memory_control::{
    self as control, ControlPreparation, ForgetGroupRequest,
};
use super::*;
use crate::entity::{
    agent_assistant_control_receipt as receipt, agent_assistant_source_suppression as suppression,
};
use desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget;
use sea_orm::QueryOrder;

async fn begin(store: &TestStore) -> crate::config::connection::DatabaseTransaction {
    crate::db::begin_write(&store.db, fact::Entity)
        .await
        .unwrap()
}
async fn request(store: &TestStore, ids: &[String], label: &str) -> ForgetGroupRequest {
    ForgetGroupRequest {
        operation_id: format!("group-control-{label}"),
        producer_message_id: "original-assistant-message".into(),
        call_message_sha256: "b".repeat(64),
        owner_input_seq: unsigned(runtime_on(&store.db, "1").await.unwrap().input_seq).unwrap(),
        targets: ids
            .iter()
            .map(|id| MemoryForgetTarget {
                memory_id: id.clone(),
                expected_revision: 1,
            })
            .collect(),
    }
}
async fn state(store: &TestStore) -> String {
    serde_json::to_string(&(
        fact::Entity::find()
            .order_by_asc(fact::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        evidence::Entity::find()
            .order_by_asc(evidence::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        fact_revision::Entity::find()
            .order_by_asc(fact_revision::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        tombstone::Entity::find()
            .order_by_asc(tombstone::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        suppression::Entity::find()
            .order_by_asc(suppression::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        receipt::Entity::find()
            .order_by_asc(receipt::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        runtime::Entity::find()
            .order_by_asc(runtime::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
        quota::Entity::find()
            .order_by_asc(quota::Column::Id)
            .all(&store.db)
            .await
            .unwrap(),
    ))
    .unwrap()
}

#[tokio::test]
async fn memory_control_group_stages_all_targets_and_charges_once_only_after_exact_group_closure() {
    let store = setup().await;
    let first = seed(&store, 1, "group-first").await.0;
    let second = seed(&store, 1, "group-second").await.0;
    let request = request(&store, &[first.clone(), second.clone()], "valid").await;
    super::super::super::ensure_quota_on(&store.db, "1")
        .await
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let runtime_before = runtime_on(&store.db, "1").await.unwrap();
    let txn = begin(&store).await;
    let ControlPreparation::Prepared(mut pending) =
        control::prepare_forget_group_on(&txn, "1", &request, 100)
            .await
            .unwrap()
    else {
        panic!("expected complete preparation");
    };
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        before
    );
    assert_eq!(pending.receipt().unwrap().affected_count, 2);
    pending
        .bind_tool_group_on(&txn, &request.producer_message_id, &"c".repeat(64))
        .await
        .unwrap();
    let result = pending.finalize_on(&txn).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(result.outcome, DataControlOutcome::Applied);
    assert_eq!(result.affected_count, 2);
    let changed = runtime_on(&store.db, "1").await.unwrap();
    assert_eq!(
        changed.control_generation,
        runtime_before.control_generation + 1
    );
    assert_eq!(
        changed.memory_state_generation,
        runtime_before.memory_state_generation + 1
    );
    assert_eq!(changed.input_seq, runtime_before.input_seq);
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .revision,
        before.revision + 1
    );
    let saved = state(&store).await;
    let txn = begin(&store).await;
    let ControlPreparation::Unchanged(replayed) =
        control::prepare_forget_group_on(&txn, "1", &request, 101)
            .await
            .unwrap()
    else {
        panic!("expected original receipt");
    };
    assert_eq!(replayed.outcome, DataControlOutcome::AlreadyApplied);
    assert_eq!(replayed.affected_count, 2);
    txn.commit().await.unwrap();
    assert_eq!(state(&store).await, saved);
    let mut changed_request = request;
    changed_request.targets.reverse();
    let txn = begin(&store).await;
    assert!(
        control::prepare_forget_group_on(&txn, "1", &changed_request, 102)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn memory_control_group_stale_missing_foreign_or_duplicate_targets_have_no_partial_effect() {
    let store = setup().await;
    let first = seed(&store, 1, "group-good").await.0;
    let other = seed(&store, 1, "group-foreign").await.0;
    // Corrupt cross-account rows must remain unreachable in the singleton store.
    fact::Entity::update_many()
        .col_expr(fact::Column::ActorId, sea_orm::sea_query::Expr::value("2"))
        .filter(fact::Column::MemoryId.eq(&other))
        .exec(&store.db)
        .await
        .unwrap();
    for variant in 0..4 {
        let mut request = request(
            &store,
            &[first.clone(), other.clone()],
            &format!("bad-{variant}"),
        )
        .await;
        match variant {
            0 => request.targets[1].memory_id = "missing-memory".into(),
            1 => {}
            2 => request.targets[0].expected_revision = 2,
            3 => request.targets[1] = request.targets[0].clone(),
            _ => unreachable!(),
        }
        let before = state(&store).await;
        let txn = begin(&store).await;
        let outcome = control::prepare_forget_group_on(&txn, "1", &request, 100).await;
        if variant == 3 {
            assert!(outcome.is_err());
        } else {
            let ControlPreparation::Unchanged(receipt) = outcome.unwrap() else {
                panic!("stale batch cannot write");
            };
            assert_eq!(receipt.outcome, DataControlOutcome::StaleTarget);
            assert_eq!(receipt.affected_count, 0);
        }
        txn.commit().await.unwrap();
        assert_eq!(state(&store).await, before);
    }
}

#[tokio::test]
async fn memory_control_group_cannot_commit_without_a_matching_closed_group_or_bind_it_twice() {
    let store = setup().await;
    let first = seed(&store, 1, "group-unclosed").await.0;
    let request = request(&store, &[first], "unclosed").await;
    let before = state(&store).await;
    let txn = begin(&store).await;
    let ControlPreparation::Prepared(pending) =
        control::prepare_forget_group_on(&txn, "1", &request, 100)
            .await
            .unwrap()
    else {
        panic!();
    };
    assert!(pending.finalize_on(&txn).await.is_err());
    txn.rollback().await.unwrap();
    assert_eq!(state(&store).await, before);
    let txn = begin(&store).await;
    let ControlPreparation::Prepared(mut pending) =
        control::prepare_forget_group_on(&txn, "1", &request, 100)
            .await
            .unwrap()
    else {
        panic!();
    };
    assert!(
        pending
            .bind_tool_group_on(&txn, "other-producer", &"c".repeat(64))
            .await
            .is_err()
    );
    assert!(
        pending
            .bind_tool_group_on(&txn, &request.producer_message_id, "invalid-digest")
            .await
            .is_err()
    );
    pending
        .bind_tool_group_on(&txn, &request.producer_message_id, &"c".repeat(64))
        .await
        .unwrap();
    assert!(
        pending
            .bind_tool_group_on(&txn, &request.producer_message_id, &"c".repeat(64))
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(state(&store).await, before);
}

#[tokio::test]
async fn memory_control_group_rolls_back_all_members_and_metadata_on_late_changes_or_final_quota_failure()
 {
    for change in [
        "head",
        "source",
        "revision",
        "runtime",
        "receipt",
        "quota",
        "tombstone",
        "suppression",
    ] {
        let store = setup().await;
        let first = seed(&store, 1, "rollback-first").await.0;
        let second = seed(&store, 1, "rollback-second").await.0;
        let request = request(&store, &[first, second], change).await;
        let before = state(&store).await;
        let sql = match change {
            "head" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_memory SET context = 'late'; END"
            }
            "source" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_memory_source SET body = body || ' '; END"
            }
            "revision" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_memory_revision SET body = body || ' '; END"
            }
            "runtime" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_assistant_runtime SET revision = revision + 1; END"
            }
            "receipt" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_assistant_control_receipt SET affected_count = 1; END"
            }
            "tombstone" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_memory_tombstone SET cutoff_input_seq = cutoff_input_seq + 1; END"
            }
            "suppression" => {
                "CREATE TRIGGER late_control AFTER INSERT ON agent_assistant_control_receipt BEGIN UPDATE agent_assistant_source_suppression SET created_at_ms = created_at_ms + 1; END"
            }
            "quota" => {
                "CREATE TRIGGER late_control BEFORE UPDATE ON agent_assistant_quota BEGIN SELECT RAISE(ABORT, 'injected control quota failure'); END"
            }
            _ => unreachable!(),
        };
        store.db.execute_unprepared(sql).await.unwrap();
        let txn = begin(&store).await;
        let result = match control::prepare_forget_group_on(&txn, "1", &request, 100).await {
            Ok(ControlPreparation::Prepared(mut pending)) => match pending
                .bind_tool_group_on(&txn, &request.producer_message_id, &"c".repeat(64))
                .await
            {
                Ok(()) => pending.finalize_on(&txn).await.map(|_| ()),
                Err(error) => Err(error),
            },
            Ok(ControlPreparation::Unchanged(_)) => panic!("expected actual original targets"),
            Err(error) => Err(error),
        };
        assert!(result.is_err(), "late {change} must reject the whole group");
        txn.rollback().await.unwrap();
        assert_eq!(state(&store).await, before);
    }
}

#[tokio::test]
async fn a_delayed_forget_keeps_its_original_input_cutoff_without_rewinding_the_actor_sequence() {
    let store = setup().await;
    let first = seed(&store, 1, "intent-old-fact").await.0;
    let request = request(&store, &[first], "original-intent").await;
    let later = seed(&store, 1, "independent-later-fact").await.0;
    let later_before = fact::Entity::find()
        .filter(fact::Column::MemoryId.eq(&later))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let runtime_before = runtime_on(&store.db, "1").await.unwrap();
    assert!(later_before.accepted_input_seq > request.owner_input_seq as i64);
    let txn = begin(&store).await;
    let ControlPreparation::Prepared(mut pending) =
        control::prepare_forget_group_on(&txn, "1", &request, 100)
            .await
            .unwrap()
    else {
        panic!("original target expected")
    };
    assert_eq!(
        receipt::Entity::find()
            .filter(receipt::Column::OperationId.eq(&request.operation_id))
            .one(&txn)
            .await
            .unwrap()
            .unwrap()
            .accepted_input_seq as u64,
        request.owner_input_seq
    );
    pending
        .bind_tool_group_on(&txn, &request.producer_message_id, &"c".repeat(64))
        .await
        .unwrap();
    pending.finalize_on(&txn).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        runtime_on(&store.db, "1").await.unwrap().input_seq,
        runtime_before.input_seq
    );
    assert_eq!(
        fact::Entity::find()
            .filter(fact::Column::MemoryId.eq(&later))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        later_before
    );
    assert!(
        tombstone::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .iter()
            .all(|row| row.cutoff_input_seq == request.owner_input_seq as i64)
    );
    let saved = state(&store).await;
    let txn = begin(&store).await;
    let ControlPreparation::Unchanged(receipt) =
        control::prepare_forget_group_on(&txn, "1", &request, 101)
            .await
            .unwrap()
    else {
        panic!("original replay expected")
    };
    assert_eq!(receipt.outcome, DataControlOutcome::AlreadyApplied);
    txn.commit().await.unwrap();
    assert_eq!(state(&store).await, saved);
    let mut changed = request;
    changed.owner_input_seq += 1;
    let txn = begin(&store).await;
    assert!(
        control::prepare_forget_group_on(&txn, "1", &changed, 102)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(state(&store).await, saved);
}

#[tokio::test]
async fn an_old_intent_cannot_forget_a_newer_fact_even_when_its_exact_version_is_supplied() {
    let store = setup().await;
    let first = seed(&store, 1, "old-intent-target").await.0;
    let mut request = request(&store, &[first], "cannot-cover-new-fact").await;
    let later = seed(&store, 1, "newer-exact-target").await.0;
    request.targets.push(MemoryForgetTarget {
        memory_id: later,
        expected_revision: 1,
    });
    let saved = state(&store).await;
    let txn = begin(&store).await;
    let ControlPreparation::Unchanged(receipt) =
        control::prepare_forget_group_on(&txn, "1", &request, 100)
            .await
            .unwrap()
    else {
        panic!("entire group must remain unchanged")
    };
    assert_eq!(receipt.outcome, DataControlOutcome::StaleTarget);
    txn.commit().await.unwrap();
    assert_eq!(state(&store).await, saved);
    request.owner_input_seq = runtime_on(&store.db, "1").await.unwrap().input_seq as u64 + 1;
    let txn = begin(&store).await;
    assert!(
        control::prepare_forget_group_on(&txn, "1", &request, 101)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(state(&store).await, saved);
}
