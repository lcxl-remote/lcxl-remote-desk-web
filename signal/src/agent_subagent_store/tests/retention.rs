use super::*;
use crate::entity::{agent_file_recovery_cleanup as tombstone, agent_subagent_inbox as inbox};

async fn parent<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[tokio::test]
async fn deletion_tombstone_source_epochs_and_child_control_share_one_commit() {
    let db = database().await;
    seed_parent(&db).await;
    let original = seed_task(&db, "deleted", "goal-deleted").await;
    let parent = parent(&db).await;
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &parent, 100).await.unwrap();
    assert!(deleted_on(&txn, "root").await.unwrap());
    assert!(
        deleted_on(&txn, &original.child_conversation_id)
            .await
            .unwrap()
    );
    txn.rollback().await.unwrap();
    assert!(!deleted_on(&db, "root").await.unwrap());
    assert_eq!(load(&db, &original.binding.task_id).await, original);
    assert_eq!(inbox::Entity::find().count(&db).await.unwrap(), 0);
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &parent, 200).await.unwrap();
    txn.commit().await.unwrap();
    let run = load(&db, &original.binding.task_id).await;
    assert_eq!(run.state, SubAgentState::Cancelled);
    assert!(run.binding.control_revision > original.binding.control_revision);
    assert!(run.binding.source_epoch > original.binding.source_epoch);
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("deleted"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&row).unwrap();
    assert_eq!(group.source_admission, SourceAdmission::Closed);
    assert!(!group.parent_active);
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &parent, 300).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(load(&db, &original.binding.task_id).await, run);
    assert_eq!(inbox::Entity::find().count(&db).await.unwrap(), 1);
    assert_eq!(
        tombstone::Entity::find_by_id("root")
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .created_at_unix_ms,
        200
    );
}

#[tokio::test]
async fn idle_parent_is_protected_by_an_independently_waiting_child() {
    let db = database().await;
    seed_parent(&db).await;
    seed_task(&db, "active", "goal-active").await;
    let eligible = session_row::Entity::find()
        .filter(reclaim_condition(1_000))
        .all(&db)
        .await
        .unwrap();
    assert!(eligible.is_empty());
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-active",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert!(
        session_row::Entity::find()
            .filter(reclaim_condition(1_000))
            .all(&db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn late_provider_usage_keeps_deleted_group_until_settlement_and_normal_retention() {
    use desk_diagnose_core::subagent::reservation::DelegationCallKind;
    let db = database().await;
    seed_parent(&db).await;
    let task = seed_task(&db, "usage", "goal-usage").await;
    let parent = active_parent_for_group(&db, "usage").await;
    let upper = Usage {
        model_calls: 1,
        tool_calls: 0,
        tokens: 200,
    };
    let txn = db.begin().await.unwrap();
    let call = match reserve_budget_on(
        &txn,
        &parent,
        "original-model",
        DelegationCallKind::Model,
        &"a".repeat(64),
        upper,
        100,
    )
    .await
    .unwrap()
    {
        BudgetAdmission::Reserved(call) => call,
        _ => panic!("new provider boundary"),
    };
    let summary = match reserve_budget_on(
        &txn,
        &parent,
        "original-summary",
        DelegationCallKind::ContextSummary,
        &"b".repeat(64),
        upper,
        110,
    )
    .await
    .unwrap()
    {
        BudgetAdmission::Reserved(call) => call,
        _ => panic!("new summary provider boundary"),
    };
    settle_budget_on(&txn, &summary, None, 160).await.unwrap();
    settle_budget_on(&txn, &call, None, 150).await.unwrap();
    close_root_on(&txn, &parent, 200).await.unwrap();
    // Normal retention has already removed histories after their sources closed.
    session_row::Entity::delete_many()
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    group_row::Entity::update_many()
        .col_expr(
            group_row::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(9_000),
        )
        .filter(group_row::Column::GroupId.eq("usage"))
        .exec(&db)
        .await
        .unwrap();
    assert_eq!(redact_deleted_content(&db, 1_000).await.unwrap(), 1);
    assert_eq!(redact_deleted_content(&db, 1_000).await.unwrap(), 0);
    let redacted_group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("usage"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(redacted_group.content_redacted_at_ms.is_some());
    assert!(redacted_group.creation_envelope_json.is_empty());
    let child = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&task.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let child = PersistedAgentSession::decode_json(&child.state_json).unwrap();
    assert!(child.conversation.is_empty());
    assert!(child.delegated_owner_requirement.is_none());
    let redacted_run = load(&db, &task.binding.task_id).await;
    assert_eq!(redacted_run.state, SubAgentState::Cancelled);
    assert_eq!(
        redacted_run.binding.control_revision,
        child.control_revision
    );
    assert_eq!(
        redacted_run.binding.source_epoch,
        task.binding.source_epoch + 1
    );
    assert!(redacted_run.validate_session(&child).is_ok());
    assert_eq!(purge_groups(&db, 1_000).await.unwrap(), 0);
    assert!(
        group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq("usage"))
            .one(&db)
            .await
            .unwrap()
            .is_some()
    );
    let txn = db.begin().await.unwrap();
    settle_budget_on(
        &txn,
        &call,
        Some(Usage {
            model_calls: 1,
            tool_calls: 0,
            tokens: 123,
        }),
        500,
    )
    .await
    .unwrap();
    settle_budget_on(
        &txn,
        &summary,
        Some(Usage {
            model_calls: 1,
            tool_calls: 0,
            tokens: 111,
        }),
        510,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    session_row::Entity::delete_many()
        .filter(session_row::Column::ConversationId.eq(&task.child_conversation_id))
        .exec(&db)
        .await
        .unwrap();
    assert_eq!(purge_groups(&db, 1_000).await.unwrap(), 1);
    assert!(
        run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&task.binding.task_id))
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(inbox::Entity::find().count(&db).await.unwrap(), 0);
    assert!(deleted_on(&db, "root").await.unwrap());
    assert!(deleted_on(&db, &task.child_conversation_id).await.unwrap());
}

#[tokio::test]
async fn terminal_assistant_retention_does_not_require_an_ai_assistant_execution_identity() {
    let db = database().await;
    let mut session = PersistedAgentSession::new(
        "terminal",
        "diagnostic-actor",
        "diagnostic-device",
        1,
        scope(),
        "1970-01-01T00:00:00Z",
    );
    session.surface = AgentSessionSurface::TerminalAiAssistant;
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &session, 100).await.unwrap();
    assert!(deleted_on(&txn, "terminal").await.unwrap());
    txn.commit().await.unwrap();
    assert_eq!(group_row::Entity::find().count(&db).await.unwrap(), 0);
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 0);
}

#[test]
fn unreadable_removal_closes_children_with_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(bytes)
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(unreadable_removal());
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

async fn unreadable_removal() {
    let db = database().await;
    seed_parent(&db).await;
    let original = seed_task(&db, "unsupported", "goal-unsupported").await;
    for conversation in ["root", original.child_conversation_id.as_str()] {
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(conversation))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&row.state_json).unwrap();
        value["conversation_schema_version"] =
            serde_json::json!(desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION - 1);
        value["assistant_data"] = serde_json::json!({"closed_control_message_ids": []});
        let raw = value.to_string();
        assert!(PersistedAgentSession::decode_json(&raw).is_err());
        session_row::Entity::update_many()
            .col_expr(
                session_row::Column::ConversationSchemaVersion,
                sea_orm::sea_query::Expr::value(
                    i32::from(desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION) - 1,
                ),
            )
            .col_expr(
                session_row::Column::StateJson,
                sea_orm::sea_query::Expr::value(raw),
            )
            .filter(session_row::Column::Id.eq(row.id))
            .exec(&db)
            .await
            .unwrap();
    }
    let before = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&original.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let txn = db.begin().await.unwrap();
    close_root_for_subject_on(&txn, "root", "1", "1", 100)
        .await
        .unwrap();
    assert_eq!(
        decode_run(
            &run_row::Entity::find()
                .filter(run_row::Column::TaskId.eq(&original.binding.task_id))
                .one(&txn)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap()
        .state,
        SubAgentState::Cancelled
    );
    txn.rollback().await.unwrap();
    assert!(!deleted_on(&db, "root").await.unwrap());
    assert_eq!(load(&db, &original.binding.task_id).await, original);
    let txn = db.begin().await.unwrap();
    let pending = close_root_for_subject_on(&txn, "root", "1", "1", 200);
    assert!(std::mem::size_of_val(&pending) <= 16 * 1024);
    pending.await.unwrap();
    txn.commit().await.unwrap();
    let after = session_row::Entity::find_by_id(before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state_json, before.state_json);
    assert!(after.version > before.version && after.lease_token > before.lease_token);
    assert!(after.lease_deadline.is_none());
    assert!(
        deleted_on(&db, &original.child_conversation_id)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&db, &original.binding.task_id).await.state,
        SubAgentState::Cancelled
    );
}

#[test]
fn unreadable_expiry_reclaims_idle_root_and_keeps_live_lease() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .stack_size(bytes)
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(unreadable_expiry());
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

async fn unreadable_expiry() {
    use sea_orm::IntoActiveModel;
    let db = database().await;
    seed_parent(&db).await;
    let original = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&original.state_json).unwrap();
    value["conversation_schema_version"] =
        serde_json::json!(desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION - 1);
    value["assistant_data"] = serde_json::json!({"model_history_exclusions": []});
    value["turn_state"] = serde_json::json!("running");
    let raw = value.to_string();
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::ConversationSchemaVersion,
            sea_orm::sea_query::Expr::value(
                i32::from(desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION) - 1,
            ),
        )
        .col_expr(
            session_row::Column::StateJson,
            sea_orm::sea_query::Expr::value(raw.clone()),
        )
        .filter(session_row::Column::Id.eq(original.id))
        .exec(&db)
        .await
        .unwrap();
    let mut live = original.clone().into_active_model();
    live.id = sea_orm::ActiveValue::NotSet;
    live.conversation_id = Set("live-old".into());
    value["conversation_id"] = serde_json::json!("live-old");
    live.state_json = Set(value.to_string());
    live.conversation_schema_version = Set(Some(
        i32::from(desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION) - 1,
    ));
    let live_until = chrono::Utc::now() + chrono::Duration::hours(1);
    live.lease_deadline = Set(Some(live_until));
    let live = live.insert(&db).await.unwrap();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let pending = crate::agent_session_store::retire_unsupported_sessions(&db, cutoff, 2, 100);
    assert!(
        std::mem::size_of_val(&pending) <= 16 * 1024,
        "retirement future: {}",
        std::mem::size_of_val(&pending)
    );
    pending.await.unwrap();
    assert!(deleted_on(&db, "root").await.unwrap());
    assert!(!deleted_on(&db, "live-old").await.unwrap());
    let unchanged = session_row::Entity::find_by_id(live.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.state_json, live.state_json);
    assert_eq!(unchanged.version, live.version);
    assert_eq!(unchanged.lease_deadline, live.lease_deadline);
    let (_, deleted) =
        crate::usage_retention::cleanup_agent_sessions(&db, cutoff, chrono::Utc::now())
            .await
            .unwrap();
    assert_eq!(deleted, 1);
    assert!(
        session_row::Entity::find_by_id(original.id)
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        session_row::Entity::find_by_id(live.id)
            .one(&db)
            .await
            .unwrap()
            .is_some()
    );
}
