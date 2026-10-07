use super::*;
use desk_diagnose_core::{
    goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalRun, GoalUsage},
    subagent::reservation::{CallAdmission, DelegationCallKind},
};

pub(super) async fn funded_parent(db: &DatabaseConnection) -> (PersistedAgentSession, GoalRun) {
    let (mut parent, _) = super::creation::runnable_parent(db).await;
    let now = chrono::Utc::now().timestamp_millis();
    let goal = GoalRun::new(
        "funding-goal".into(),
        "root".into(),
        "1".into(),
        "1".into(),
        "Investigate these independent symptoms".into(),
        "source-message".into(),
        GoalOpening::OwnerRequest,
        GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        },
        1,
        now as u64,
        GoalLimits::default(),
    )
    .unwrap();
    let original_group = parent.delegation_group_id.clone();
    let txn = db.begin().await.unwrap();
    crate::agent_goal_store::insert_on(&txn, &goal)
        .await
        .unwrap();
    initialize_goal_group_on(&txn, &mut parent, &goal, now)
        .await
        .unwrap();
    assert_ne!(parent.delegation_group_id, original_group);
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    (parent, goal)
}

async fn recorded_goal<C: ConnectionTrait>(db: &C) -> GoalRun {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq("funding-goal"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    crate::agent_goal_store::decode(&row).unwrap()
}

#[tokio::test]
async fn one_call_reserves_both_constraints_and_late_usage_does_not_resume_the_source() {
    let db = database().await;
    let (parent, _) = funded_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let upper = GoalUsage {
        input_tokens: 200,
        output_tokens: 30,
        model_calls: 1,
        active_time_ms: 20,
        ..GoalUsage::default()
    };
    let now = chrono::Utc::now().timestamp_millis();
    let receipt = match store
        .reserve_runtime_call(
            &parent,
            "provider-1",
            DelegationCallKind::Model,
            &"a".repeat(64),
            upper,
            now,
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(receipt) => receipt,
        _ => panic!("one new provider call"),
    };
    assert_eq!(receipt.source_goal_upper, Some(upper));
    assert_eq!(
        recorded_goal(&db)
            .await
            .delegation_reservations
            .get(&receipt.reservation_id),
        Some(&upper)
    );
    assert!(
        store
            .reserve_runtime_call(
                &parent,
                "provider-1",
                DelegationCallKind::Model,
                &"a".repeat(64),
                upper,
                now
            )
            .await
            .is_err()
    );
    let physical_id = "paused-original-provider";
    store
        .link_model_receipt(&receipt, physical_id, now)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 1)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    let mut goal = recorded_goal(&txn).await;
    let old_version = goal.state_version;
    goal.pause_settled(GoalPauseReason::Owner, now as u64 + 2)
        .unwrap();
    assert!(
        crate::agent_goal_store::replace_on(&txn, &goal, old_version, goal.lease_epoch, None, None)
            .await
            .unwrap()
    );
    apply_goal_source_on(&txn, "root", "1", "1", "funding-goal", goal.state, now + 2)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        store
            .reserve_runtime_call(
                &parent,
                "provider-2",
                DelegationCallKind::Model,
                &"a".repeat(64),
                upper,
                now + 3
            )
            .await
            .is_err()
    );
    insert_provider_usage(&db, physical_id, now, now + 4).await;
    let actual = GoalUsage {
        input_tokens: 11,
        output_tokens: 7,
        model_calls: 1,
        active_time_ms: 4,
        ..GoalUsage::default()
    };
    store
        .settle_runtime_call(&receipt, Some(actual), now + 4)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, Some(actual), now + 5)
        .await
        .unwrap();
    let goal = recorded_goal(&db).await;
    assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Owner));
    assert_eq!(goal.used, actual);
    assert!(goal.delegation_reservations.is_empty());
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(parent.delegation_group_id.unwrap()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&row).unwrap();
    assert_eq!(group.source_admission, SourceAdmission::Paused);
    assert_eq!(group.budget.charged.tokens, 18);
    assert_eq!(group.budget.outstanding, Usage::default());
}

#[tokio::test]
async fn source_goal_exhaustion_rolls_back_the_group_reservation() {
    let db = database().await;
    let (parent, mut goal) = funded_parent(&db).await;
    let mut policy = crate::goal_budget_policy::read(&db).await.unwrap();
    let old_revision = policy.revision;
    policy.limits.model_calls = Some(1);
    crate::goal_budget_policy::update(
        &db,
        &desk_agent_protocol::ai_assistant::goal_budget::UpdateGoalBudgetPolicy {
            expected_revision: old_revision,
            limits: policy.limits,
            device_unavailable_max_ms: policy.device_unavailable_max_ms,
        },
    )
    .await
    .unwrap();
    goal.used.model_calls = 1;
    let old_version = goal.state_version;
    goal.state_version += 1;
    let txn = db.begin().await.unwrap();
    assert!(
        crate::agent_goal_store::replace_on(&txn, &goal, old_version, goal.lease_epoch, None, None)
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();
    let store = SubAgentStore::new(db.clone());
    let upper = GoalUsage {
        input_tokens: 10,
        model_calls: 1,
        ..GoalUsage::default()
    };
    assert_eq!(
        store
            .reserve_runtime_call(
                &parent,
                "cannot-admit",
                DelegationCallKind::Model,
                &"b".repeat(64),
                upper,
                chrono::Utc::now().timestamp_millis()
            )
            .await
            .unwrap(),
        CallAdmission::Exhausted
    );
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(parent.delegation_group_id.unwrap()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decode_group(&row).unwrap().budget, Default::default());
    assert_eq!(
        crate::entity::agent_delegation_reservation::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert!(recorded_goal(&db).await.delegation_reservations.is_empty());
}

#[tokio::test]
async fn linked_provider_usage_reconciles_after_crash_without_replaying_the_call() {
    let db = database().await;
    let (parent, _) = funded_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let now = chrono::Utc::now().timestamp_millis();
    let upper = GoalUsage {
        input_tokens: 200,
        output_tokens: 30,
        model_calls: 1,
        active_time_ms: 100,
        ..GoalUsage::default()
    };
    let receipt = match store
        .reserve_runtime_call(
            &parent,
            "provider-crash",
            DelegationCallKind::Model,
            &"a".repeat(64),
            upper,
            now,
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(receipt) => receipt,
        _ => panic!("new physical provider call"),
    };
    let physical_id = "f".repeat(64);
    store
        .link_model_receipt(&receipt, &physical_id, now)
        .await
        .unwrap();
    assert!(
        store
            .link_model_receipt(&receipt, &physical_id, now)
            .await
            .is_err()
    );
    assert!(
        store
            .link_model_receipt(&receipt, &"e".repeat(64), now)
            .await
            .is_err()
    );
    store
        .settle_runtime_call(&receipt, None, now + 1)
        .await
        .unwrap();
    assert_eq!(store.reconcile_model_usage(0, 1).await.unwrap().1, 0);
    assert!(
        recorded_goal(&db)
            .await
            .delegation_reservations
            .contains_key(&receipt.reservation_id)
    );
    insert_provider_usage(&db, &physical_id, now, now + 12).await;
    assert_eq!(store.reconcile_model_usage(0, 1).await.unwrap().1, 1);
    let goal = recorded_goal(&db).await;
    assert_eq!(goal.used.input_tokens, 11);
    assert_eq!(goal.used.output_tokens, 7);
    assert_eq!(goal.used.model_calls, 1);
    assert_eq!(goal.used.active_time_ms, 12);
    assert_eq!(goal.state, GoalState::Queued);
    assert_eq!(goal.slice_seq, 0);
    assert_eq!(goal.lease_epoch, 0);
    assert_eq!(store.reconcile_model_usage(0, 1).await.unwrap().1, 0);
    let same_tokens = GoalUsage {
        input_tokens: 11,
        output_tokens: 7,
        model_calls: 1,
        active_time_ms: 999,
        ..GoalUsage::default()
    };
    store
        .settle_runtime_call(&receipt, Some(same_tokens), now + 13)
        .await
        .unwrap();
    assert_eq!(recorded_goal(&db).await.used, goal.used);
    assert!(
        store
            .settle_runtime_call(&receipt, Some(upper), now + 14)
            .await
            .is_err()
    );
}

pub(super) async fn insert_provider_usage(
    db: &DatabaseConnection,
    id: &str,
    started: i64,
    completed: i64,
) {
    let start = chrono::DateTime::from_timestamp_millis(started).unwrap();
    let end = chrono::DateTime::from_timestamp_millis(completed).unwrap();
    let usage = desk_diagnose_core::chat::TokenUsage {
        input_tokens: Some(11),
        output_tokens: Some(7),
        cache_read_tokens: None,
        cache_write_tokens: None,
    };
    crate::entity::model_egress_receipt::ActiveModel {
        receipt_id: Set(id.into()),
        export_authorization_id: Set("export".into()),
        model_call_ordinal: Set(1),
        destination_json: Set("{}".into()),
        envelope_ids_json: Set("[]".into()),
        digests_sha256_json: Set("[]".into()),
        input_lineage_json: Set("[]".into()),
        projection_digest_sha256: Set("digest".into()),
        total_bytes: Set(100),
        state: Set("dispatch_intent".into()),
        authorized_at: Set(start),
        completed_at: Set(None),
        usage_json: Set(Some(serde_json::to_string(&usage).unwrap())),
        usage_recorded_at: Set(Some(end)),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn later_goal_segments_reuse_source_spending_count_and_deadline() {
    let db = database().await;
    let (mut parent, goal) = funded_parent(&db).await;
    let id = parent.delegation_group_id.clone().unwrap();
    let store = SubAgentStore::new(db.clone());
    let now = chrono::Utc::now().timestamp_millis();
    let upper = GoalUsage {
        input_tokens: 20,
        output_tokens: 10,
        model_calls: 1,
        ..GoalUsage::default()
    };
    let receipt = match store
        .reserve_runtime_call(
            &parent,
            "old-segment",
            DelegationCallKind::Model,
            &"c".repeat(64),
            upper,
            now,
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(receipt) => receipt,
        _ => panic!("new physical call"),
    };
    store
        .link_model_receipt(&receipt, "old-segment-provider", now)
        .await
        .unwrap();
    insert_provider_usage(&db, "old-segment-provider", now, now + 1).await;
    let actual = GoalUsage {
        input_tokens: 11,
        output_tokens: 7,
        model_calls: 1,
        active_time_ms: 1,
        ..GoalUsage::default()
    };
    store
        .settle_runtime_call(&receipt, Some(actual), now + 1)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut group = decode_group(&row).unwrap();
    group
        .admit_child(
            "existing-required-task",
            true,
            0,
            now + 2,
            desk_diagnose_core::subagent::policy::initial().limits,
        )
        .unwrap();
    replace_group_on(&txn, &row, &group, now + 2).await.unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    initialize_goal_group_on(&txn, &mut parent, &goal, now + 3)
        .await
        .unwrap();
    initialize_goal_group_on(&txn, &mut parent, &goal, now + 4)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(parent.delegation_group_id.as_deref(), Some(id.as_str()));
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decode_group(&row).unwrap(), group);
    assert_eq!(recorded_goal(&db).await.used, actual);
}

#[tokio::test]
async fn unstarted_model_refunds_both_budgets_without_reopening_a_paused_source() {
    let db = database().await;
    let (parent, _) = funded_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let now = chrono::Utc::now().timestamp_millis();
    let upper = GoalUsage {
        input_tokens: 200,
        output_tokens: 30,
        model_calls: 1,
        active_time_ms: 20,
        ..Default::default()
    };
    let receipt = match store
        .reserve_runtime_call(
            &parent,
            "cancelled-in-queue",
            DelegationCallKind::Model,
            &"a".repeat(64),
            upper,
            now,
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(receipt) => receipt,
        _ => panic!("one new allocation"),
    };
    assert_eq!(store.reconcile_model_usage(0, 50).await.unwrap().1, 0);
    let txn = db.begin().await.unwrap();
    let mut goal = recorded_goal(&txn).await;
    let version = goal.state_version;
    goal.pause_settled(GoalPauseReason::Owner, (now + 1) as u64)
        .unwrap();
    assert!(
        crate::agent_goal_store::replace_on(&txn, &goal, version, goal.lease_epoch, None, None)
            .await
            .unwrap()
    );
    apply_goal_source_on(&txn, "root", "1", "1", "funding-goal", goal.state, now + 1)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        store
            .link_model_receipt(&receipt, "too-late", now + 2)
            .await
            .is_err()
    );
    store
        .settle_runtime_call(&receipt, None, now + 2)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 3)
        .await
        .unwrap();
    assert!(
        store
            .settle_runtime_call(&receipt, Some(upper), now + 3)
            .await
            .is_err()
    );
    let goal = recorded_goal(&db).await;
    assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Owner));
    assert_eq!(goal.used, GoalUsage::default());
    assert_eq!(
        goal.delegation_settlements.get(&receipt.reservation_id),
        Some(&GoalUsage::default())
    );
    assert!(goal.delegation_reservations.is_empty());
    goal.validate().unwrap();
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(parent.delegation_group_id.unwrap()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decode_group(&row).unwrap().budget, Default::default());
}

#[tokio::test]
async fn crashed_unstarted_call_is_reclaimed_only_after_the_original_lease_expires() {
    let db = database().await;
    let (parent, _) = funded_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let now = chrono::Utc::now().timestamp_millis();
    let upper = GoalUsage {
        input_tokens: 30,
        model_calls: 1,
        ..Default::default()
    };
    let receipt = match store
        .reserve_runtime_call(
            &parent,
            "crashed-before-dispatch",
            DelegationCallKind::ContextSummary,
            &"a".repeat(64),
            upper,
            now,
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(receipt) => receipt,
        _ => panic!("one new allocation"),
    };
    assert_eq!(store.reconcile_model_usage(0, 50).await.unwrap().1, 0);
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            lease_deadline: Set(Some(chrono::Utc::now() - chrono::Duration::seconds(1))),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&db)
        .await
        .unwrap();
    assert_eq!(store.reconcile_model_usage(0, 50).await.unwrap().1, 1);
    assert_eq!(store.reconcile_model_usage(0, 50).await.unwrap().1, 0);
    assert!(
        store
            .link_model_receipt(&receipt, "stale-dispatch", now)
            .await
            .is_err()
    );
    assert_eq!(recorded_goal(&db).await.used, GoalUsage::default());
    assert!(recorded_goal(&db).await.delegation_reservations.is_empty());
}

#[tokio::test]
async fn dispatch_linkage_rolls_back_with_a_failed_provider_start_transaction() {
    let db = database().await;
    let (parent, _) = funded_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let now = chrono::Utc::now().timestamp_millis();
    let upper = GoalUsage {
        input_tokens: 30,
        model_calls: 1,
        ..Default::default()
    };
    let receipt = match store
        .reserve_runtime_call(
            &parent,
            "rollback-dispatch",
            DelegationCallKind::Model,
            &"a".repeat(64),
            upper,
            now,
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(receipt) => receipt,
        _ => panic!("one new allocation"),
    };
    let txn = db.begin().await.unwrap();
    SubAgentStore::link_model_receipt_on(&txn, &receipt, "rolled-back-provider", now)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 1)
        .await
        .unwrap();
    assert_eq!(recorded_goal(&db).await.used, GoalUsage::default());
    assert!(
        store
            .link_model_receipt(&receipt, "replay-after-refund", now + 2)
            .await
            .is_err()
    );
}

async fn review_without_planner(
    db: &DatabaseConnection,
) -> (
    PersistedAgentSession,
    desk_diagnose_core::subagent::reservation::ReviewCallAuthority,
) {
    let (mut parent, _) = funded_parent(db).await;
    let now = chrono::Utc::now().timestamp_millis();
    parent.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            lease_token: Set(parent.lease_token as i64),
            lease_deadline: Set(None),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(db)
        .await
        .unwrap();
    let authority = desk_diagnose_core::subagent::reservation::ReviewCallAuthority {
        candidate_id: "source-review".into(),
        lease_epoch: 1,
        lease_owner: "independent-review-worker".into(),
        lease_deadline_ms: (now + 240000) as u64,
        model_config_revision: 1,
        delegation_id: "owner-review-delegation".into(),
        delegation_revision: 1,
        policy_revision: parent.policy_revision as u64,
        source: desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
            request_id: "original-request".into(),
            item_id: "item".into(),
        },
        source_record_id: "original-request:item".into(),
        source_planning_lease_token: None,
        context_hmac_sha256: "a".repeat(64),
        request_sha256: "b".repeat(64),
        model_destination: super::creation::destination(),
        root_conversation_id: "root".into(),
        group_id: parent.delegation_group_id.clone(),
        task_id: None,
        conversation_id: "root".into(),
        actor_id: "1".into(),
        device_id: "1".into(),
        input_revision: parent.input_revision,
        control_revision: parent.control_revision,
        source_epoch: Some(1),
    };
    authority.validate().unwrap();
    let prices = desk_diagnose_core::approval_cost::ApprovalTokenPrices {
        input_micros_per_million: 1000000,
        output_micros_per_million: 1000000,
        cache_read_micros_per_million: 1000000,
        cache_write_micros_per_million: 1000000,
    };
    crate::entity::agent_approval_review::ActiveModel {
        candidate_id: Set(authority.candidate_id.clone()),
        conversation_id: Set("root".into()),
        actor_id: Set("1".into()),
        device_id: Set("1".into()),
        delegation_id: Set("owner-review-delegation".into()),
        model_config_revision: Set(1),
        source_kind: Set("permission_item".into()),
        source_id: Set("original-request:item".into()),
        action_sha256: Set("c".repeat(64)),
        context_hmac_sha256: Set(authority.context_hmac_sha256.clone()),
        call_authority_json: Set(Some(serde_json::to_string(&authority).unwrap())),
        token_prices_json: Set(Some(serde_json::to_string(&prices).unwrap())),
        status: Set("reviewing".into()),
        lease_epoch: Set(1),
        lease_owner: Set(Some(authority.lease_owner.clone())),
        lease_deadline: Set(Some(authority.lease_deadline_ms as i64)),
        reserved_tokens: Set(230),
        reserved_cost_micros: Set(230),
        expires_at: Set(now + 300000),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    (parent, authority)
}

async fn reserve_review(
    db: &DatabaseConnection,
    parent: &PersistedAgentSession,
    review: &desk_diagnose_core::subagent::reservation::ReviewCallAuthority,
) -> desk_diagnose_core::subagent::reservation::DelegationCallReservation {
    let now = chrono::Utc::now().timestamp_millis();
    let txn = db.begin().await.unwrap();
    super::super::review_budget::validate_review_lease_on(&txn, review, now)
        .await
        .unwrap();
    let mut wrong = review.clone();
    wrong.lease_owner = "different-worker".into();
    assert!(
        super::super::review_budget::validate_review_lease_on(&txn, &wrong, now)
            .await
            .is_err()
    );
    assert!(
        super::super::review_budget::validate_review_lease_on(
            &txn,
            review,
            review.lease_deadline_ms as i64
        )
        .await
        .is_err()
    );
    let goal_upper = GoalUsage {
        input_tokens: 200,
        output_tokens: 30,
        model_calls: 1,
        active_time_ms: 100,
        ..Default::default()
    };
    assert!(
        super::super::funding::reserve_call_with_authority_on(
            &txn,
            parent,
            "wrong-review",
            DelegationCallKind::ApprovalReview,
            &review.request_sha256,
            Usage {
                model_calls: 1,
                tool_calls: 0,
                tokens: 230
            },
            Some(goal_upper),
            Some(review),
            now
        )
        .await
        .is_err()
    );
    let receipt = match super::super::funding::reserve_call_with_authority_on(
        &txn,
        parent,
        "review:source-review",
        DelegationCallKind::ApprovalReview,
        &review.request_sha256,
        Usage {
            model_calls: 1,
            tool_calls: 0,
            tokens: 230,
        },
        Some(goal_upper),
        Some(review),
        now,
    )
    .await
    .unwrap()
    {
        BudgetAdmission::Reserved(receipt) => receipt,
        _ => panic!("one review source reservation"),
    };
    assert!(receipt.planning_lease_token.is_none());
    assert_eq!(receipt.review_authority.as_ref(), Some(review));
    crate::entity::agent_approval_review::Entity::update_many()
        .set(crate::entity::agent_approval_review::ActiveModel {
            delegation_reservation_id: Set(Some(receipt.reservation_id.clone())),
            ..Default::default()
        })
        .filter(crate::entity::agent_approval_review::Column::CandidateId.eq(&review.candidate_id))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    receipt
}

#[tokio::test]
async fn independent_review_reserves_without_planning_and_unstarted_pause_refunds_both_sources() {
    let db = database().await;
    let (parent, authority) = review_without_planner(&db).await;
    assert!(!parent.turn_state.is_active());
    let receipt = reserve_review(&db, &parent, &authority).await;
    assert_eq!(
        recorded_goal(&db)
            .await
            .delegation_reservations
            .get(&receipt.reservation_id),
        receipt.source_goal_upper.as_ref()
    );
    let now = chrono::Utc::now().timestamp_millis();
    let store = SubAgentStore::new(db.clone());
    assert!(
        store
            .reserve_runtime_call(
                &parent,
                "borrow-review-as-planner",
                DelegationCallKind::Model,
                &"a".repeat(64),
                GoalUsage {
                    model_calls: 1,
                    input_tokens: 1,
                    ..Default::default()
                },
                now
            )
            .await
            .is_err()
    );
    let txn = db.begin().await.unwrap();
    let mut goal = recorded_goal(&txn).await;
    let version = goal.state_version;
    goal.pause_settled(GoalPauseReason::Owner, now as u64 + 1)
        .unwrap();
    assert!(
        crate::agent_goal_store::replace_on(&txn, &goal, version, goal.lease_epoch, None, None)
            .await
            .unwrap()
    );
    apply_goal_source_on(&txn, "root", "1", "1", "funding-goal", goal.state, now + 1)
        .await
        .unwrap();
    let review = crate::entity::agent_approval_review::Entity::find()
        .filter(
            crate::entity::agent_approval_review::Column::CandidateId.eq(&authority.candidate_id),
        )
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let usage =
        super::super::review_budget::settle_review_call_on(&txn, &review, None, None, now + 2)
            .await
            .unwrap();
    assert!(!usage.dispatched);
    assert_eq!(usage.tokens, Some(0));
    assert_eq!(usage.cost_micros, Some(0));
    txn.commit().await.unwrap();
    let goal = recorded_goal(&db).await;
    assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Owner));
    assert_eq!(goal.used, GoalUsage::default());
    assert!(goal.delegation_reservations.is_empty());
    assert_eq!(
        goal.delegation_settlements.get(&receipt.reservation_id),
        Some(&GoalUsage::default())
    );
}

#[tokio::test]
async fn dispatched_review_usage_is_canonical_and_does_not_reactivate_the_parent() {
    let db = database().await;
    let (parent, authority) = review_without_planner(&db).await;
    let receipt = reserve_review(&db, &parent, &authority).await;
    let now = chrono::Utc::now().timestamp_millis();
    let id = "d".repeat(64);
    insert_provider_usage(&db, &id, now, now + 4).await;
    let txn = db.begin().await.unwrap();
    crate::entity::agent_approval_review::Entity::update_many()
        .set(crate::entity::agent_approval_review::ActiveModel {
            provider_receipt_id: Set(Some(id.clone())),
            provider_receipt_kind: Set(Some("oss_model_egress".into())),
            provider_started_at_ms: Set(Some(now)),
            ..Default::default()
        })
        .filter(
            crate::entity::agent_approval_review::Column::CandidateId.eq(&authority.candidate_id),
        )
        .exec(&txn)
        .await
        .unwrap();
    crate::entity::agent_delegation_reservation::Entity::update_many()
        .set(crate::entity::agent_delegation_reservation::ActiveModel {
            provider_receipt_id: Set(Some(id)),
            provider_receipt_kind: Set(Some("oss_model_egress".into())),
            provider_started_at_ms: Set(Some(now)),
            ..Default::default()
        })
        .filter(
            crate::entity::agent_delegation_reservation::Column::ReservationId
                .eq(&receipt.reservation_id),
        )
        .exec(&txn)
        .await
        .unwrap();
    let review = crate::entity::agent_approval_review::Entity::find()
        .filter(
            crate::entity::agent_approval_review::Column::CandidateId.eq(&authority.candidate_id),
        )
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    assert!(
        super::super::review_budget::settle_review_call_on(
            &txn,
            &review,
            Some(20),
            Some(18),
            now + 5
        )
        .await
        .is_err()
    );
    let usage = super::super::review_budget::settle_review_call_on(
        &txn,
        &review,
        Some(18),
        Some(18),
        now + 5,
    )
    .await
    .unwrap();
    assert!(usage.dispatched);
    assert_eq!(usage.tokens, Some(18));
    assert_eq!(usage.cost_micros, Some(18));
    super::super::review_budget::settle_review_call_on(&txn, &review, None, None, now + 6)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let goal = recorded_goal(&db).await;
    assert_eq!(goal.used.model_calls, 1);
    assert_eq!(goal.used.input_tokens, 11);
    assert_eq!(goal.used.output_tokens, 7);
    assert_eq!(goal.slice_seq, 0);
    assert_eq!(goal.lease_epoch, 0);
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let held = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(!held.turn_state.is_active());
    assert_eq!(held.control_revision, parent.control_revision);
}

// This fixture writes historical provider facts directly. It exercises source
// and cumulative accounting, not HTTP dispatch or live model configuration.
async fn unknown_review_ledger(
    db: &DatabaseConnection,
) -> (
    PersistedAgentSession,
    desk_diagnose_core::subagent::reservation::DelegationCallReservation,
    String,
    i64,
) {
    use desk_diagnose_core::{
        approval_cost::ReviewUsageSettlement,
        approval_delegation::{ApprovalDelegation, ApprovalDelegationStatus},
    };
    let (parent, authority) = review_without_planner(db).await;
    let receipt = reserve_review(db, &parent, &authority).await;
    let now = chrono::Utc::now().timestamp_millis();
    let provider_id = "9".repeat(64);
    let mut delegation = ApprovalDelegation::new(
        authority.delegation_id.clone(),
        "root".into(),
        "1".into(),
        "1".into(),
        "owner-enabled-review".into(),
        now as u64,
    )
    .unwrap();
    delegation.reserve(230, 230).unwrap();
    delegation.settle(230, 230, None, None).unwrap();
    delegation.close(ApprovalDelegationStatus::Closed).unwrap();
    crate::entity::agent_approval_delegation::ActiveModel {
        delegation_id: Set(delegation.delegation_id.clone()),
        conversation_id: Set("root".into()),
        actor_id: Set("1".into()),
        device_id: Set("1".into()),
        state_json: Set(serde_json::to_string(&delegation).unwrap()),
        version: Set(delegation.ledger_version as i64),
        status: Set("closed".into()),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let txn = db.begin().await.unwrap();
    crate::entity::agent_approval_review::Entity::update_many()
        .set(crate::entity::agent_approval_review::ActiveModel {
            provider_receipt_id: Set(Some(provider_id.clone())),
            provider_receipt_kind: Set(Some("oss_model_egress".into())),
            provider_started_at_ms: Set(Some(now)),
            ..Default::default()
        })
        .filter(
            crate::entity::agent_approval_review::Column::CandidateId.eq(&authority.candidate_id),
        )
        .exec(&txn)
        .await
        .unwrap();
    crate::entity::agent_delegation_reservation::Entity::update_many()
        .set(crate::entity::agent_delegation_reservation::ActiveModel {
            provider_receipt_id: Set(Some(provider_id.clone())),
            provider_receipt_kind: Set(Some("oss_model_egress".into())),
            provider_started_at_ms: Set(Some(now)),
            ..Default::default()
        })
        .filter(
            crate::entity::agent_delegation_reservation::Column::ReservationId
                .eq(&receipt.reservation_id),
        )
        .exec(&txn)
        .await
        .unwrap();
    let row = crate::entity::agent_approval_review::Entity::find()
        .filter(
            crate::entity::agent_approval_review::Column::CandidateId.eq(&authority.candidate_id),
        )
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let physical = super::super::review_budget::settle_review_call_on(&txn, &row, None, None, now)
        .await
        .unwrap();
    let unknown = physical
        .settlement(row.reserved_tokens, row.reserved_cost_micros)
        .unwrap();
    assert_eq!(
        unknown,
        ReviewUsageSettlement::from_provider_fact(true, None, None, 230, 230).unwrap()
    );
    crate::agent_approval_usage::record_usage_on(&txn, &row, unknown, now)
        .await
        .unwrap();
    crate::entity::agent_approval_review::Entity::update_many()
        .set(crate::entity::agent_approval_review::ActiveModel {
            status: Set("unavailable".into()),
            lease_owner: Set(None),
            lease_deadline: Set(None),
            ..Default::default()
        })
        .filter(crate::entity::agent_approval_review::Column::Id.eq(row.id))
        .exec(&txn)
        .await
        .unwrap();
    let mut goal = recorded_goal(&txn).await;
    let old_version = goal.state_version;
    goal.pause_settled(GoalPauseReason::Owner, now as u64 + 1)
        .unwrap();
    assert!(
        crate::agent_goal_store::replace_on(&txn, &goal, old_version, goal.lease_epoch, None, None)
            .await
            .unwrap()
    );
    apply_goal_source_on(&txn, "root", "1", "1", "funding-goal", goal.state, now + 1)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    (parent, receipt, provider_id, now)
}

async fn cumulative_review(
    db: &DatabaseConnection,
) -> desk_diagnose_core::approval_delegation::ApprovalDelegation {
    let row = crate::entity::agent_approval_delegation::Entity::find()
        .one(db)
        .await
        .unwrap()
        .unwrap();
    crate::agent_approval_store::decode(&row).unwrap()
}

#[tokio::test]
async fn late_review_usage_corrects_once_after_pause_and_closed_delegation() {
    let db = database().await;
    let (parent, receipt, provider_id, now) = unknown_review_ledger(&db).await;
    let before = cumulative_review(&db).await;
    assert_eq!(before.usage.tokens_used, 230);
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 2, 1)
            .await
            .unwrap(),
        0
    );
    assert!(
        recorded_goal(&db)
            .await
            .delegation_reservations
            .contains_key(&receipt.reservation_id)
    );
    insert_provider_usage(&db, &provider_id, now, now + 4).await;
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 5, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 6, 1)
            .await
            .unwrap(),
        0
    );
    let after = cumulative_review(&db).await;
    assert_eq!(after.usage.tokens_used, 18);
    assert_eq!(after.usage.cost_used_micros, 18);
    assert_eq!(after.usage.reviews_used, 1);
    assert_eq!(after.usage.reviews_reserved, 0);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.status, before.status);
    assert_eq!(after.ledger_version, before.ledger_version + 1);
    let goal = recorded_goal(&db).await;
    assert_eq!(goal.state, GoalState::Paused(GoalPauseReason::Owner));
    assert_eq!(
        goal.used,
        GoalUsage {
            input_tokens: 11,
            output_tokens: 7,
            model_calls: 1,
            active_time_ms: 4,
            ..Default::default()
        }
    );
    assert_eq!(goal.slice_seq, 0);
    assert!(goal.delegation_reservations.is_empty());
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let held = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(held.turn_state, parent.turn_state);
    assert_eq!(held.control_revision, parent.control_revision);
    let review = crate::entity::agent_approval_review::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.status, "unavailable");
    assert!(review.decision_json.is_none());
    assert!(review.lease_owner.is_none());
    assert_eq!(review.usage_settlement_state.as_deref(), Some("known"));
}

#[tokio::test]
async fn cumulative_unknown_review_pins_an_already_settled_source_until_correction() {
    let db = database().await;
    let (parent, receipt, provider_id, now) = unknown_review_ledger(&db).await;
    insert_provider_usage(&db, &provider_id, now, now + 4).await;
    let txn = db.begin().await.unwrap();
    let row = crate::entity::agent_approval_review::Entity::find()
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    super::super::review_budget::settle_review_call_on(&txn, &row, None, None, now + 5)
        .await
        .unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "funding-goal",
        GoalState::Failed,
        now + 6,
    )
    .await
    .unwrap();
    session_row::Entity::delete_many()
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let allocation = crate::entity::agent_delegation_reservation::Entity::find()
        .filter(
            crate::entity::agent_delegation_reservation::Column::ReservationId
                .eq(&receipt.reservation_id),
        )
        .filter(
            crate::entity::agent_delegation_reservation::Column::ReservationId
                .in_subquery(crate::agent_approval_usage::pinned_reservation_ids()),
        )
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(allocation.state, "settled");
    assert_eq!(
        super::super::retention::purge_groups(&db, now + 10000)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 7, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        super::super::retention::purge_groups(&db, now + 10000)
            .await
            .unwrap(),
        1
    );
    assert!(
        group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(parent.delegation_group_id.unwrap()))
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(cumulative_review(&db).await.usage.tokens_used, 18);
}

#[tokio::test]
async fn malformed_review_accounting_rotates_without_hiding_a_later_known_receipt() {
    use sea_orm::IntoActiveModel;
    let db = database().await;
    let (_, _, provider_id, now) = unknown_review_ledger(&db).await;
    let good = crate::entity::agent_approval_review::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut malformed = good.clone().into_active_model();
    malformed.id = sea_orm::ActiveValue::NotSet;
    malformed.candidate_id = Set("malformed-prior-review".into());
    malformed.call_authority_json = Set(Some("{}".into()));
    malformed.usage_reconcile_at_ms = Set(Some(now - 100));
    let malformed = malformed.insert(&db).await.unwrap();
    insert_provider_usage(&db, &provider_id, now, now + 4).await;
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 5, 1)
            .await
            .unwrap(),
        0
    );
    let stamped = crate::entity::agent_approval_review::Entity::find_by_id(malformed.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stamped.usage_reconcile_at_ms, Some(now + 5));
    assert_eq!(stamped.updated_at, malformed.updated_at);
    assert_eq!(
        stamped.usage_settlement_json,
        malformed.usage_settlement_json
    );
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 6, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(cumulative_review(&db).await.usage.tokens_used, 18);
    assert_eq!(
        crate::agent_approval_usage::reconcile_usage(&db, now + 7, 1)
            .await
            .unwrap(),
        0
    );
    assert_eq!(cumulative_review(&db).await.usage.reviews_used, 1);
}
