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
