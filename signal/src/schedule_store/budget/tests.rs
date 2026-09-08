use super::super::publication::tests::{Verifier, fixture_on as publication_fixture};
use super::*;
use crate::entity::{agent_schedule_run as run, agent_task_authorization as authorization};
use desk_agent_protocol::schedule::{ScheduledRunStatus, contract::TaskContract};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, TransactionTrait};

pub(crate) async fn fixture_on(
    db: DatabaseConnection,
) -> (
    ScheduleStore,
    entity::Model,
    authorization::Model,
    run::Model,
) {
    fixture_with_rule_limit(db, 2).await
}

async fn fixture_with_rule_limit(
    db: DatabaseConnection,
    rule_limit: u32,
) -> (
    ScheduleStore,
    entity::Model,
    authorization::Model,
    run::Model,
) {
    use desk_agent_protocol::capability_grant::{CapabilityGrantLimits, CapabilityRiskTier};
    use desk_agent_protocol::capability_provider::CapabilityEffect;
    use desk_agent_protocol::schedule::contract::{
        TaskInputConstraint, TaskPermissionRule, TaskPermissionScope,
    };
    let (store, task, original, mut input) = publication_fixture(db).await;
    let mut definition: TaskContract = serde_json::from_str(&original.canonical_json).unwrap();
    let scope = TaskPermissionScope {
        resources: vec!["device:1".into()],
        operations: vec!["observe".into()],
        export_destinations: vec![],
        limits: CapabilityGrantLimits {
            max_calls: rule_limit,
            max_bytes_per_call: 100,
            max_items_per_call: 2,
        },
    };
    let mut ceiling = scope.clone();
    ceiling.limits.max_calls = 2;
    definition.permissions = ["read-a", "read-b"]
        .into_iter()
        .map(|name| TaskPermissionRule {
            rule_id: name.into(),
            provider_id: "device".into(),
            capability_id: name.into(),
            tool_name: name.into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ReadDevice,
            risk_tier: CapabilityRiskTier::R0,
            input: TaskInputConstraint::ScopedRead,
            automatic: scope.clone(),
            approval_ceiling: ceiling.clone(),
        })
        .collect();
    definition.budget.max_runs_per_utc_day = 1;
    definition.budget.max_calls_per_run = 2;
    definition.budget.max_model_tokens_per_run = 1000;
    let contract = store
        .save_contract(1, task.revision, &definition)
        .await
        .unwrap();
    input.expected_revision = store.read(1, &task.schedule_id).await.unwrap().revision;
    input.contract_revision = contract.contract_revision;
    input.contract_sha256 = contract.digest_sha256;
    let parent = store
        .publish_task(1, &input, &Verifier(true))
        .await
        .unwrap();
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "budget-run")
        .await
        .unwrap();
    let work = store
        .claim_queued(&queued.run_id, "budget-node", 90)
        .await
        .unwrap();
    (store, task, parent, work)
}
async fn fixture() -> (
    ScheduleStore,
    entity::Model,
    authorization::Model,
    run::Model,
) {
    fixture_on(Database::connect("sqlite::memory:").await.unwrap()).await
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
            rule_id: (kind == TaskBudgetKind::ToolCall).then_some("read-a"),
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
    let (store, task, _, work) = fixture().await;
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
            rule_id: Some("read-a"),
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
                rule_id: Some("read-a"),
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
    let (store, task, _, work) = fixture().await;
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
    let (store, task, _, work) = fixture().await;
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
async fn late_usage_settlement_never_restores_revoked_authority() {
    let (store, task, parent, work) = fixture().await;
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
    store
        .revoke_task_authorization(1, &parent.authorization_id, "owner revoked")
        .await
        .unwrap();
    settle(&store, &work, &first, 400, &"c".repeat(64))
        .await
        .unwrap();
    assert_eq!(
        store.read(1, &task.schedule_id).await.unwrap().status,
        "paused"
    );
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
async fn utc_day_run_quota_survives_a_new_authorization_revision() {
    let (store, task, _, work) = fixture().await;
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
    let old = store
        .read_contract(1, &task.schedule_id, paused.contract_revision.unwrap())
        .await
        .unwrap();
    let definition = serde_json::from_str(&old.canonical_json).unwrap();
    let contract = store
        .save_contract(1, paused.revision, &definition)
        .await
        .unwrap();
    let current = store.read(1, &task.schedule_id).await.unwrap();
    let parent = store
        .publish_task(
            1,
            &super::super::PublishTask {
                schedule_id: task.schedule_id.clone(),
                expected_revision: current.revision,
                contract_revision: contract.contract_revision,
                contract_sha256: contract.digest_sha256,
                rehearsal_run_id: "rehearsal-2".into(),
                expires_at: None,
                client_publish_key: "publish-2".into(),
            },
            &Verifier(true),
        )
        .await
        .unwrap();
    assert_eq!(parent.authorization_revision, 2);
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
        let (store, task, _, work) = fixture().await;
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

async fn reserve_rule(
    store: &ScheduleStore,
    task: &entity::Model,
    work: &run::Model,
    call_key: &str,
    rule_id: Option<&str>,
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
            kind: TaskBudgetKind::ToolCall,
            rule_id,
            logical_key: call_key,
            input_sha256: &"a".repeat(64),
            units: 1,
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

#[tokio::test]
async fn per_rule_quota_uses_the_automatic_limit_and_cannot_be_rebound_on_replay() {
    let (store, task, _, work) =
        fixture_with_rule_limit(Database::connect("sqlite::memory:").await.unwrap(), 1).await;
    assert!(matches!(
        reserve_rule(&store, &task, &work, "missing", None).await,
        Err(ScheduleStoreError::Invalid)
    ));
    assert!(matches!(
        reserve_rule(&store, &task, &work, "unknown", Some("not-approved")).await,
        Err(ScheduleStoreError::Invalid)
    ));
    assert_eq!(ledger::Entity::find().count(&store.db).await.unwrap(), 0);
    let first = reserve_rule(&store, &task, &work, "call-a", Some("read-a"))
        .await
        .unwrap();
    assert_eq!(first.rule_id.as_deref(), Some("read-a"));
    assert_eq!(
        reserve_rule(&store, &task, &work, "call-a", Some("read-a"))
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        reserve_rule(&store, &task, &work, "call-a", Some("read-b")).await,
        Err(ScheduleStoreError::Conflict)
    ));
    // The approval ceiling permits two calls, but it is not automatic authority.
    assert!(matches!(
        reserve_rule(&store, &task, &work, "second-a", Some("read-a")).await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    reserve_rule(&store, &task, &work, "call-b", Some("read-b"))
        .await
        .unwrap();
    assert_eq!(ledger::Entity::find().count(&store.db).await.unwrap(), 3);
}

#[tokio::test]
async fn total_call_quota_still_applies_when_another_rule_has_capacity() {
    let (store, task, _, work) = fixture().await;
    reserve_rule(&store, &task, &work, "a-1", Some("read-a"))
        .await
        .unwrap();
    reserve_rule(&store, &task, &work, "a-2", Some("read-a"))
        .await
        .unwrap();
    assert!(matches!(
        reserve_rule(&store, &task, &work, "b-1", Some("read-b")).await,
        Err(ScheduleStoreError::BudgetExceeded)
    ));
    assert_eq!(ledger::Entity::find().count(&store.db).await.unwrap(), 3);
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
        let (store, task, _, work) = fixture().await;
        store
            .db
            .execute(
                &sea_orm::Schema::new(store.db.get_database_backend())
                    .create_table_from_entity(grants::Entity),
            )
            .await
            .unwrap();
        let txn = store.db.begin().await.unwrap();
        let canonical = digest("{}");
        let budget = ScheduleStore::reserve_task_budget(
            &txn,
            &TaskBudgetRequest {
                owner: 1,
                device: &task.target_device_id,
                run_id: &work.run_id,
                node: "budget-node",
                lease_epoch: work.lease_epoch,
                kind: TaskBudgetKind::ToolCall,
                rule_id: Some("read-a"),
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
            allowed_content_digests_sha256: vec![],
            use_policy: CapabilityGrantUsePolicy::OneShotExact,
            canonical_input_digest_sha256: Some(canonical),
            issued_by: CapabilityGrantIssuer::TaskAuthorization(authority.provenance().clone()),
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
