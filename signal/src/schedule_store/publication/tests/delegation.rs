//! Metadata-only occurrence tests; these do not represent a device or provider dispatch.
use super::*;
use crate::entity::{agent_schedule_run as run, agent_task_budget_reservation as ledger};
use desk_diagnose_core::subagent::creation::ScheduledCreationSource;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

async fn frozen_source(
    store: &ScheduleStore,
    task: &entity::Model,
    work: &run::Model,
) -> ScheduledCreationSource {
    let txn = store.db.begin().await.unwrap();
    let current = ScheduleStore::lock_run_authority(
        &txn,
        1,
        &task.target_device_id,
        &work.run_id,
        "authority-node",
        work.lease_epoch,
    )
    .await
    .unwrap();
    let source = current.delegation_source().unwrap();
    txn.rollback().await.unwrap();
    source
}

async fn admits_source(
    store: &ScheduleStore,
    owner: i32,
    device: &str,
    source: &ScheduledCreationSource,
) -> bool {
    let txn = store.db.begin().await.unwrap();
    let valid = ScheduleStore::lock_delegation_source_authority(&txn, owner, device, source)
        .await
        .is_ok();
    txn.rollback().await.unwrap();
    valid
}

#[tokio::test]
async fn source_outlives_parent_planner_holder_but_never_supplies_that_holder() {
    for state in ["running", "awaiting_permission", "awaiting_children"] {
        let (store, task, _, work) = running_fixture().await;
        let source = frozen_source(&store, &task, &work).await;
        let now = database_now(&store.db).await.unwrap();
        run::Entity::update_many()
            .set(run::ActiveModel {
                status: Set(state.into()),
                lease_owner: Set(None),
                lease_epoch: Set(work.lease_epoch + 1),
                lease_deadline: Set(None),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .exec(&store.db)
            .await
            .unwrap();
        assert!(admits_source(&store, 1, &task.target_device_id, &source).await);
        let txn = store.db.begin().await.unwrap();
        assert!(
            ScheduleStore::lock_run_authority(
                &txn,
                1,
                &task.target_device_id,
                &work.run_id,
                "authority-node",
                work.lease_epoch
            )
            .await
            .is_err()
        );
        txn.rollback().await.unwrap();

        let txn = store.db.begin().await.unwrap();
        let authority = ScheduleStore::lock_delegation_source_authority(
            &txn,
            1,
            &task.target_device_id,
            &source,
        )
        .await
        .unwrap();
        assert_eq!(authority.deadline_ms(), source.deadline_ms().unwrap());
        assert!(authority.deadline_ms() > now);
        let call = ScheduleStore::reserve_delegation_model_budget_on(
            &txn,
            &authority,
            "source-model-one",
            &"a".repeat(64),
            100,
        )
        .await
        .unwrap();
        assert_eq!(call.run_id, work.run_id);
        assert_eq!(call.owner_user_id, 1);
        assert_eq!(call.charged_units, 100);
        txn.commit().await.unwrap();
        let rows = ledger::Entity::find().all(&store.db).await.unwrap();
        assert_eq!(rows.iter().filter(|row| row.kind == "run").count(), 1);
        assert_eq!(
            rows.iter().filter(|row| row.kind == "model_tokens").count(),
            1
        );
        assert!(rows.iter().all(|row| row.run_id == work.run_id));
    }
}

#[tokio::test]
async fn source_fence_rejects_cancel_finish_replacement_and_expired_original_scope() {
    for case in 0..8 {
        let (store, task, parent, work) = running_fixture().await;
        let mut source = frozen_source(&store, &task, &work).await;
        let now = database_now(&store.db).await.unwrap();
        match case {
            0 => {
                run::Entity::update_many()
                    .set(run::ActiveModel {
                        cancel_requested_at: Set(Some(now)),
                        ..Default::default()
                    })
                    .filter(run::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            1 => {
                run::Entity::update_many()
                    .set(run::ActiveModel {
                        finished_at: Set(Some(now)),
                        status: Set("succeeded".into()),
                        ..Default::default()
                    })
                    .filter(run::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            2 => {
                authorization::Entity::update_many()
                    .set(authorization::ActiveModel {
                        revoked_at: Set(Some(now)),
                        ..Default::default()
                    })
                    .filter(authorization::Column::Id.eq(parent.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            3 => {
                entity::Entity::update_many()
                    .set(entity::ActiveModel {
                        task_revision: Set(task.task_revision + 1),
                        ..Default::default()
                    })
                    .filter(entity::Column::Id.eq(task.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            4 => source.provenance.authorization_id.push_str("-other"),
            5 => source.started_at_ms += 1,
            6 => {
                authorization::Entity::update_many()
                    .set(authorization::ActiveModel {
                        expires_at: Set(Some(now - 1)),
                        ..Default::default()
                    })
                    .filter(authorization::Column::Id.eq(parent.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            7 => {
                let duration = i64::from(
                    source
                        .validate()
                        .unwrap()
                        .contract()
                        .budget
                        .max_runtime_seconds,
                ) * 1_000;
                source.started_at_ms = now - duration - 1;
                run::Entity::update_many()
                    .set(run::ActiveModel {
                        started_at: Set(Some(source.started_at_ms)),
                        ..Default::default()
                    })
                    .filter(run::Column::Id.eq(work.id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !admits_source(&store, 1, &task.target_device_id, &source).await,
            "case {case}"
        );
        assert!(
            ledger::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let (store, task, _, work) = running_fixture().await;
    let source = frozen_source(&store, &task, &work).await;
    assert!(!admits_source(&store, 2, &task.target_device_id, &source).await);
    assert!(!admits_source(&store, 1, "other-device", &source).await);
}

#[tokio::test]
async fn user_schedule_pause_preserves_current_occurrence_without_extending_its_deadline() {
    let (store, task, _, work) = running_fixture().await;
    let source = frozen_source(&store, &task, &work).await;
    let before = store.read(1, &task.schedule_id).await.unwrap();
    store
        .pause(
            1,
            &task.schedule_id,
            before.revision,
            database_now(&store.db).await.unwrap(),
        )
        .await
        .unwrap();
    assert!(admits_source(&store, 1, &task.target_device_id, &source).await);
    let txn = store.db.begin().await.unwrap();
    let authority =
        ScheduleStore::lock_delegation_source_authority(&txn, 1, &task.target_device_id, &source)
            .await
            .unwrap();
    assert_eq!(authority.deadline_ms(), source.deadline_ms().unwrap());
    txn.rollback().await.unwrap();
    let persisted = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.started_at, work.started_at);
    assert!(persisted.cancel_requested_at.is_none());
}

#[tokio::test]
async fn main_and_delegated_model_allocations_share_one_occurrence_budget() {
    use super::super::super::{TaskBudgetKind, TaskBudgetRequest};
    let (store, task, _, work) = running_fixture().await;
    let source = frozen_source(&store, &task, &work).await;
    let maximum = source
        .validate()
        .unwrap()
        .contract()
        .budget
        .max_model_tokens_per_run;
    assert!(maximum > 100);
    let txn = store.db.begin().await.unwrap();
    ScheduleStore::reserve_task_budget(
        &txn,
        &TaskBudgetRequest {
            owner: 1,
            device: &task.target_device_id,
            run_id: &work.run_id,
            node: "authority-node",
            lease_epoch: work.lease_epoch,
            kind: TaskBudgetKind::ModelTokens,
            rule_id: None,
            logical_key: "parent-model",
            input_sha256: &"b".repeat(64),
            units: maximum - 100,
        },
    )
    .await
    .unwrap();
    let authority =
        ScheduleStore::lock_delegation_source_authority(&txn, 1, &task.target_device_id, &source)
            .await
            .unwrap();
    let child = ScheduleStore::reserve_delegation_model_budget_on(
        &txn,
        &authority,
        "child-model",
        &"c".repeat(64),
        100,
    )
    .await
    .unwrap();
    let replay = ScheduleStore::reserve_delegation_model_budget_on(
        &txn,
        &authority,
        "child-model",
        &"c".repeat(64),
        100,
    )
    .await
    .unwrap();
    assert_eq!(replay, child);
    assert!(
        ScheduleStore::reserve_delegation_model_budget_on(
            &txn,
            &authority,
            "child-model",
            &"d".repeat(64),
            100
        )
        .await
        .is_err()
    );
    assert!(matches!(
        ScheduleStore::reserve_delegation_model_budget_on(
            &txn,
            &authority,
            "safety-model",
            &"e".repeat(64),
            1
        )
        .await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    txn.commit().await.unwrap();
    let rows = ledger::Entity::find().all(&store.db).await.unwrap();
    assert_eq!(rows.iter().filter(|row| row.kind == "run").count(), 1);
    assert_eq!(
        rows.iter().filter(|row| row.kind == "model_tokens").count(),
        2
    );

    let txn = store.db.begin().await.unwrap();
    ScheduleStore::settle_task_model_budget(
        &txn,
        1,
        &work.run_id,
        &child.reservation_id,
        30,
        &"f".repeat(64),
    )
    .await
    .unwrap();
    let authority =
        ScheduleStore::lock_delegation_source_authority(&txn, 1, &task.target_device_id, &source)
            .await
            .unwrap();
    ScheduleStore::reserve_delegation_model_budget_on(
        &txn,
        &authority,
        "safety-model",
        &"e".repeat(64),
        70,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let total: i64 = ledger::Entity::find()
        .filter(ledger::Column::Kind.eq("model_tokens"))
        .all(&store.db)
        .await
        .unwrap()
        .iter()
        .map(|row| row.charged_units)
        .sum();
    assert_eq!(u64::try_from(total).unwrap(), maximum);
}

#[tokio::test]
async fn late_source_usage_settles_once_after_cancellation_without_reopening_work() {
    let (store, task, _, work) = running_fixture().await;
    let source = frozen_source(&store, &task, &work).await;
    let txn = store.db.begin().await.unwrap();
    let authority =
        ScheduleStore::lock_delegation_source_authority(&txn, 1, &task.target_device_id, &source)
            .await
            .unwrap();
    let allocated = ScheduleStore::reserve_delegation_model_budget_on(
        &txn,
        &authority,
        "child-unknown",
        &"a".repeat(64),
        100,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let now = database_now(&store.db).await.unwrap();
    run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("cancelled".into()),
            cancel_requested_at: Set(Some(now)),
            finished_at: Set(Some(now)),
            lease_owner: Set(None),
            lease_deadline: Set(None),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .exec(&store.db)
        .await
        .unwrap();
    let cancelled = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(!admits_source(&store, 1, &task.target_device_id, &source).await);
    let txn = store.db.begin().await.unwrap();
    let settled = ScheduleStore::settle_task_model_budget(
        &txn,
        1,
        &work.run_id,
        &allocated.reservation_id,
        19,
        &"b".repeat(64),
    )
    .await
    .unwrap();
    let replay = ScheduleStore::settle_task_model_budget(
        &txn,
        1,
        &work.run_id,
        &allocated.reservation_id,
        19,
        &"b".repeat(64),
    )
    .await
    .unwrap();
    assert_eq!(settled, replay);
    assert_eq!(settled.charged_units, 19);
    assert_eq!(settled.state, "settled");
    txn.commit().await.unwrap();
    assert_eq!(
        run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        cancelled
    );
    assert!(!admits_source(&store, 1, &task.target_device_id, &source).await);
}
