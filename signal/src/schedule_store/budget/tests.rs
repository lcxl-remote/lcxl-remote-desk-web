use super::*;
use crate::config::connection::DatabaseConnection;
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::{ScheduledRunStatus, policy::TaskBudget};
use sea_orm::{ConnectionTrait, TransactionTrait};

/// Small platform limits pinned by the claimed occurrence: one run a day,
/// two calls and 1000 model tokens per run.
pub(crate) async fn fixture_on(
    db: DatabaseConnection,
) -> (ScheduleStore, entity::Model, run::Model) {
    crate::schedule_budget_policy::update(
        &db,
        &desk_agent_protocol::schedule::policy::UpdateScheduleBudgetPolicy {
            expected_revision: crate::schedule_budget_policy::read(&db)
                .await
                .unwrap()
                .revision,
            maximum: TaskBudget {
                max_runs_per_utc_day: 1,
                max_calls_per_run: 2,
                max_model_tokens_per_run: 1000,
                max_runtime_seconds: 300,
            },
        },
    )
    .await
    .unwrap();
    let (store, task) = super::super::tests::enabled_fresh_task_on(db, None, 1, None).await;
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "budget-run")
        .await
        .unwrap();
    let work = store
        .claim_queued(&queued.run_id, "budget-node", 90)
        .await
        .unwrap();
    (store, task, work)
}
async fn fixture() -> (ScheduleStore, entity::Model, run::Model) {
    fixture_on(
        crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap(),
    )
    .await
}

pub(crate) async fn reserve(
    store: &ScheduleStore,
    task: &entity::Model,
    work: &run::Model,
    kind: TaskBudgetKind,
    logical_key: &str,
    units: u64,
) -> Result<ledger::Model, ScheduleStoreError> {
    let txn = store.db.begin().await.unwrap();
    let result = ScheduleStore::reserve_task_budget(
        &txn,
        &TaskBudgetRequest {
            owner: 1,
            device: &task.target_device_id,
            run_id: &work.run_id,
            node: "budget-node",
            lease_epoch: work.lease_epoch,
            kind,
            logical_key,
            input_sha256: &"a".repeat(64),
            units,
        },
    )
    .await;
    if result.is_ok() {
        txn.commit().await.unwrap();
    } else {
        txn.rollback().await.unwrap();
    }
    result
}
async fn settle(
    store: &ScheduleStore,
    work: &run::Model,
    row: &ledger::Model,
    actual: u64,
    receipt: &str,
) -> Result<ledger::Model, ScheduleStoreError> {
    let txn = store.db.begin().await.unwrap();
    let result = ScheduleStore::settle_task_model_budget(
        &txn,
        1,
        &work.run_id,
        &row.reservation_id,
        actual,
        receipt,
    )
    .await;
    if result.is_ok() {
        txn.commit().await.unwrap();
    } else {
        txn.rollback().await.unwrap();
    }
    result
}

#[tokio::test]
async fn tool_budget_is_atomic_idempotent_and_never_refilled_by_a_retry() {
    let (store, task, work) = fixture().await;
    let before = store.read(1, &task.schedule_id).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    ScheduleStore::reserve_task_budget(
        &txn,
        &TaskBudgetRequest {
            owner: 1,
            device: &task.target_device_id,
            run_id: &work.run_id,
            node: "budget-node",
            lease_epoch: work.lease_epoch,
            kind: TaskBudgetKind::ToolCall,
            logical_key: "rolled-back",
            input_sha256: &"a".repeat(64),
            units: 1,
        },
    )
    .await
    .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(ledger::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(store.read(1, &task.schedule_id).await.unwrap(), before);
    let first = reserve(&store, &task, &work, TaskBudgetKind::ToolCall, "call-1", 1)
        .await
        .unwrap();
    assert_eq!(
        reserve(&store, &task, &work, TaskBudgetKind::ToolCall, "call-1", 1)
            .await
            .unwrap(),
        first
    );
    reserve(&store, &task, &work, TaskBudgetKind::ToolCall, "call-2", 1)
        .await
        .unwrap();
    assert!(matches!(
        reserve(&store, &task, &work, TaskBudgetKind::ToolCall, "call-3", 1).await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    assert_eq!(ledger::Entity::find().count(&store.db).await.unwrap(), 3);
    let txn = store.db.begin().await.unwrap();
    assert!(matches!(
        ScheduleStore::reserve_task_budget(
            &txn,
            &TaskBudgetRequest {
                owner: 1,
                device: &task.target_device_id,
                run_id: &work.run_id,
                node: "budget-node",
                lease_epoch: work.lease_epoch,
                kind: TaskBudgetKind::ToolCall,
                logical_key: "call-1",
                input_sha256: &"b".repeat(64),
                units: 1,
            }
        )
        .await,
        Err(ScheduleStoreError::Conflict)
    ));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn model_upper_bounds_stay_reserved_until_an_exact_usage_receipt_settles_once() {
    let (store, task, work) = fixture().await;
    assert!(matches!(
        reserve(
            &store,
            &task,
            &work,
            TaskBudgetKind::ModelTokens,
            "too-large",
            1001
        )
        .await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    assert_eq!(ledger::Entity::find().count(&store.db).await.unwrap(), 0);
    let first = reserve(
        &store,
        &task,
        &work,
        TaskBudgetKind::ModelTokens,
        "model-1",
        800,
    )
    .await
    .unwrap();
    assert!(matches!(
        reserve(
            &store,
            &task,
            &work,
            TaskBudgetKind::ModelTokens,
            "model-2",
            300
        )
        .await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    assert!(matches!(
        reserve(
            &store,
            &task,
            &work,
            TaskBudgetKind::ModelTokens,
            "model-1",
            700
        )
        .await,
        Err(ScheduleStoreError::Conflict)
    ));
    let receipt = "c".repeat(64);
    let settled = settle(&store, &work, &first, 400, &receipt).await.unwrap();
    assert_eq!(settled.state, "settled");
    assert_eq!(settled.reserved_units, 800);
    assert_eq!(settled.charged_units, 400);
    assert_eq!(
        settle(&store, &work, &first, 400, &receipt).await.unwrap(),
        settled
    );
    assert!(matches!(
        settle(&store, &work, &first, 399, &receipt).await,
        Err(ScheduleStoreError::Conflict)
    ));
    // A different receipt cannot reuse the settled reservation.
    assert!(matches!(
        settle(&store, &work, &first, 400, "another-receipt").await,
        Err(ScheduleStoreError::Conflict)
    ));
    reserve(
        &store,
        &task,
        &work,
        TaskBudgetKind::ModelTokens,
        "model-2",
        600,
    )
    .await
    .unwrap();
    assert!(matches!(
        reserve(
            &store,
            &task,
            &work,
            TaskBudgetKind::ModelTokens,
            "model-3",
            1
        )
        .await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
}

#[tokio::test]
async fn real_overrun_is_recorded_and_stops_further_budget_allocation() {
    let (store, task, work) = fixture().await;
    let first = reserve(
        &store,
        &task,
        &work,
        TaskBudgetKind::ModelTokens,
        "model",
        200,
    )
    .await
    .unwrap();
    let settled = settle(&store, &work, &first, 201, &"d".repeat(64))
        .await
        .unwrap();
    assert_eq!(settled.state, "overrun");
    assert_eq!(settled.charged_units, 201);
    assert!(matches!(
        reserve(&store, &task, &work, TaskBudgetKind::ToolCall, "tool", 1).await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
}

#[tokio::test]
async fn late_usage_settlement_never_restores_a_cancelled_run() {
    let (store, task, work) = fixture().await;
    let first = reserve(
        &store,
        &task,
        &work,
        TaskBudgetKind::ModelTokens,
        "model",
        800,
    )
    .await
    .unwrap();
    store.cancel_run(1, &work.run_id).await.unwrap();
    settle(&store, &work, &first, 400, &"c".repeat(64))
        .await
        .unwrap();
    assert!(
        reserve(
            &store,
            &task,
            &work,
            TaskBudgetKind::ModelTokens,
            "model",
            800
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn utc_day_run_quota_survives_a_pause_and_resume() {
    let (store, task, work) = fixture().await;
    reserve(&store, &task, &work, TaskBudgetKind::ToolCall, "tool", 1)
        .await
        .unwrap();
    store
        .finish_run(
            &work.run_id,
            "budget-node",
            work.lease_epoch,
            ScheduledRunStatus::Succeeded,
            None,
            None,
        )
        .await
        .unwrap();
    let current = store.read(1, &task.schedule_id).await.unwrap();
    let paused = store
        .pause(
            1,
            &task.schedule_id,
            current.revision,
            super::super::queue::database_now(&store.db).await.unwrap(),
        )
        .await
        .unwrap();
    store
        .resume_task(
            1,
            &task.schedule_id,
            paused.revision,
            &super::super::tests::AcceptSubject,
        )
        .await
        .unwrap();
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "another-run")
        .await
        .unwrap();
    let next = store
        .claim_queued(&queued.run_id, "budget-node", 90)
        .await
        .unwrap();
    assert!(matches!(
        reserve(&store, &task, &next, TaskBudgetKind::ToolCall, "tool", 1).await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    let rows = ledger::Entity::find()
        .filter(ledger::Column::Kind.eq("run"))
        .all(&store.db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].utc_day, rows[0].created_at.div_euclid(86_400_000));
}

#[tokio::test]
async fn missing_admission_or_undercharged_pending_rows_cannot_create_more_budget() {
    for remove_admission in [false, true] {
        let (store, task, work) = fixture().await;
        let first = reserve(
            &store,
            &task,
            &work,
            TaskBudgetKind::ModelTokens,
            "model-1",
            800,
        )
        .await
        .unwrap();
        if remove_admission {
            ledger::Entity::delete_many()
                .filter(ledger::Column::Kind.eq("run"))
                .exec(&store.db)
                .await
                .unwrap();
            assert!(matches!(
                reserve(
                    &store,
                    &task,
                    &work,
                    TaskBudgetKind::ModelTokens,
                    "model-1",
                    800
                )
                .await,
                Err(ScheduleStoreError::Invalid)
            ));
        } else {
            ledger::Entity::update_many()
                .set(ledger::ActiveModel {
                    charged_units: Set(0),
                    ..Default::default()
                })
                .filter(ledger::Column::Id.eq(first.id))
                .exec(&store.db)
                .await
                .unwrap();
            assert!(matches!(
                reserve(
                    &store,
                    &task,
                    &work,
                    TaskBudgetKind::ModelTokens,
                    "model-2",
                    800
                )
                .await,
                Err(ScheduleStoreError::Invalid)
            ));
        }
    }
}

#[tokio::test]
async fn budget_and_exact_task_grant_commit_or_rollback_together() {
    use crate::capability_grant_store::SignalCapabilityGrantStore;
    use crate::entity::agent_capability_grant as grants;
    use desk_agent_protocol::capability_grant::{
        CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer,
        CapabilityGrantLimits, CapabilityGrantUsePolicy, CapabilityRiskTier,
    };
    use desk_agent_protocol::capability_provider::{CapabilityEffect, ProductSurface};
    for commit in [false, true] {
        let (store, task, work) = fixture().await;
        store
            .db
            .execute(
                &sea_orm::Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(grants::Entity),
            )
            .await
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        let canonical = super::super::digest("{}");
        let budget = ScheduleStore::reserve_task_budget(
            &txn,
            &TaskBudgetRequest {
                owner: 1,
                device: &task.target_device_id,
                run_id: &work.run_id,
                node: "budget-node",
                lease_epoch: work.lease_epoch,
                kind: TaskBudgetKind::ToolCall,
                logical_key: "exact-call",
                input_sha256: &canonical,
                units: 1,
            },
        )
        .await
        .unwrap();
        let authority = ScheduleStore::lock_run_authority(
            &txn,
            1,
            &task.target_device_id,
            &work.run_id,
            "budget-node",
            work.lease_epoch,
        )
        .await
        .unwrap();
        let grant = CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: format!("task-grant-{}", budget.reservation_id),
            actor_id: "actor-1".into(),
            run_id: work.conversation_id.clone(),
            input_revision: 1,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: task.target_device_id.clone(),
            target_session_id: None,
            provider_id: "device".into(),
            capability_id: "read-a".into(),
            tool_name: "read-a".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            risk_tier: CapabilityRiskTier::R0,
            resource_scope: vec!["device:1".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: vec![],
            allowed_envelope_ids: vec![],
            use_policy: CapabilityGrantUsePolicy::OneShotExact,
            canonical_input_digest_sha256: Some(canonical),
            issued_by: CapabilityGrantIssuer::ScheduledRun(authority.provenance().clone()),
            issued_at_unix_ms: authority.verified_at() as u64,
            expires_at_unix_ms: authority.valid_until() as u64,
            remaining_uses: 1,
            limits: CapabilityGrantLimits {
                max_calls: 1,
                max_bytes_per_call: 100,
                max_items_per_call: 2,
            },
            policy_revision: 1,
            readiness_revision: 1,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        };
        let original = SignalCapabilityGrantStore::issue_on(&txn, &grant)
            .await
            .unwrap();
        assert_eq!(
            SignalCapabilityGrantStore::issue_on(&txn, &grant)
                .await
                .unwrap(),
            original
        );
        if commit {
            txn.commit().await.unwrap();
        } else {
            txn.rollback().await.unwrap();
        }
        assert_eq!(
            grants::Entity::find().count(&store.db).await.unwrap(),
            u64::from(commit)
        );
        assert_eq!(
            ledger::Entity::find().count(&store.db).await.unwrap(),
            2 * u64::from(commit)
        );
    }
}
