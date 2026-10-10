use super::*;
mod candidates;
mod commands;
mod compaction_races;
mod creation;
mod funding;
mod goal_role_guard;
mod input_sources;
mod lifecycle;
mod main_stop;
mod main_tools;
mod model_input_metrics;
mod native_cancel;
mod notification;
mod observation;
mod parent_wait;
mod paused_completion;
mod paused_permission;
mod policy;
mod recovery;
mod response_restart;
mod retention;
mod role_recovery;
pub(super) mod scheduled;
mod ui_read;

async fn active_parent_for_group(db: &DatabaseConnection, group_id: &str) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut parent = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    parent.delegation_group_id = Some(group_id.into());
    parent
        .begin_turn("main-turn", None, None, 1, scope(), "1970-01-01T00:00:00Z")
        .unwrap();
    parent.version = row.version + 1;
    let deadline = chrono::DateTime::from_timestamp_millis(5_000).unwrap();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            lease_token: Set(parent.lease_token as i64),
            lease_deadline: Set(Some(deadline)),

            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .exec(db)
        .await
        .unwrap();
    parent
}

#[tokio::test]
async fn unknown_usage_stays_reserved_and_late_settlement_does_not_resume_a_paused_source() {
    use desk_diagnose_core::subagent::reservation::DelegationCallKind;
    let db = database().await;
    seed_parent(&db).await;
    seed_task(&db, "a", "goal-a").await;
    let parent = active_parent_for_group(&db, "a").await;
    let upper = Usage {
        model_calls: 1,
        tool_calls: 0,
        tokens: 200,
    };
    let digest = "a".repeat(64);
    let txn = db.begin().await.unwrap();
    let reservation = match reserve_budget_on(
        &txn,
        &parent,
        "model-call-1",
        DelegationCallKind::Model,
        &digest,
        upper,
        100,
    )
    .await
    .unwrap()
    {
        BudgetAdmission::Reserved(reservation) => reservation,
        _ => panic!("new logical call"),
    };
    assert!(matches!(
        reserve_budget_on(
            &txn,
            &parent,
            "model-call-1",
            DelegationCallKind::Model,
            &digest,
            upper,
            100
        )
        .await
        .unwrap(),
        BudgetAdmission::AlreadyReserved(_)
    ));
    assert!(
        reserve_budget_on(
            &txn,
            &parent,
            "model-call-1",
            DelegationCallKind::Model,
            &"b".repeat(64),
            upper,
            100
        )
        .await
        .is_err()
    );
    settle_budget_on(&txn, &reservation, None, 150)
        .await
        .unwrap();
    settle_budget_on(&txn, &reservation, None, 200)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("a"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&row).unwrap();
    assert_eq!(group.budget.outstanding, upper);
    assert_eq!(group.budget.charged, Usage::default());
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        300,
    )
    .await
    .unwrap();
    assert!(
        reserve_budget_on(
            &txn,
            &parent,
            "model-call-2",
            DelegationCallKind::Model,
            &digest,
            upper,
            400
        )
        .await
        .is_err()
    );
    let actual = Usage {
        model_calls: 1,
        tool_calls: 0,
        tokens: 123,
    };
    settle_budget_on(&txn, &reservation, Some(actual), 450)
        .await
        .unwrap();
    settle_budget_on(&txn, &reservation, Some(actual), 500)
        .await
        .unwrap();
    assert!(
        settle_budget_on(&txn, &reservation, Some(upper), 550)
            .await
            .is_err()
    );
    txn.commit().await.unwrap();
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("a"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&row).unwrap();
    assert_eq!(group.source_admission, SourceAdmission::Paused);
    assert_eq!(group.source_epoch, 2);
    assert_eq!(group.budget.outstanding, Usage::default());
    assert_eq!(group.budget.charged, actual);
    assert_eq!(group.budget.child_charged, Usage::default());
}

#[tokio::test]
async fn recent_presentation_keeps_an_unfinished_task_that_is_outside_the_page() {
    let db = database().await;
    seed_parent(&db).await;
    let active = seed_task(&db, "first", "goal-first").await;
    for index in 0..11 {
        let mut run = seed_task(
            &db,
            &format!("ended-{index}"),
            &format!("goal-ended-{index}"),
        )
        .await;
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&run.binding.task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        run.request_cancel(run.fence(), "later").unwrap();
        run.settle_cancel("later").unwrap();
        replace_run_on(&db, &row, &run, 100).await.unwrap();
    }
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let parent = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let presentation = presentation_on(&db, &parent).await.unwrap();
    let page = presentation.tasks.unwrap();
    assert_eq!(page.total, 12);
    assert_eq!(page.unfinished, 1);
    assert_eq!(page.items.len(), 10);
    assert!(page.has_more);
    assert!(
        page.items
            .iter()
            .all(|task| task.task_id != active.binding.task_id)
    );
    assert_eq!(presentation.active_tasks.len(), 1);
    assert_eq!(presentation.active_tasks[0].task_id, active.binding.task_id);
    let next = SubAgentStore::new(db)
        .list_for_owner("root", "1", "1", page.next_cursor.as_deref(), 10)
        .await
        .unwrap();
    assert_eq!(next.items.len(), 2);
    assert!(
        next.items
            .iter()
            .any(|task| task.task_id == active.binding.task_id)
    );
}

#[tokio::test]
async fn explicit_child_adjustment_is_idempotent_and_never_resets_source_limits() {
    use desk_agent_protocol::ai_assistant::subagent::{
        AiAssistantSubAgentControl, SubAgentControlAction,
    };
    let db = database().await;
    seed_parent(&db).await;
    let original = seed_task(&db, "a", "goal-a").await;
    let request = AiAssistantSubAgentControl {
        client_request_id: "adjust-1".into(),
        task_id: original.binding.task_id.clone(),
        expected_input_revision: 1,
        expected_control_revision: 1,
        action: SubAgentControlAction::Adjust {
            message: "Inspect the second symptom".into(),
        },
    };
    let txn = db.begin().await.unwrap();
    let parent = parent_on(&txn, "root", "1", "1").await.unwrap();
    let first = apply_task_control_on(&txn, &parent, &request, 100)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(first.task.input_revision, 2);
    assert_eq!(first.task.control_revision, 2);
    let adjusted = load(&db, &original.binding.task_id).await;
    assert_eq!(adjusted.binding.deadline_ms, original.binding.deadline_ms);
    assert_eq!(adjusted.dependencies, original.dependencies);
    assert_eq!(
        adjusted.report_corrections_used,
        original.report_corrections_used
    );
    let txn = db.begin().await.unwrap();
    let parent = parent_on(&txn, "root", "1", "1").await.unwrap();
    assert_eq!(
        apply_task_control_on(&txn, &parent, &request, 200)
            .await
            .unwrap(),
        first
    );
    let mut conflicting = request.clone();
    conflicting.action = SubAgentControlAction::Adjust {
        message: "Different objective".into(),
    };
    assert!(
        apply_task_control_on(&txn, &parent, &conflicting, 200)
            .await
            .is_err()
    );
    let mut stale = request;
    stale.client_request_id = "adjust-2".into();
    assert!(
        apply_task_control_on(&txn, &parent, &stale, 200)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(load(&db, &original.binding.task_id).await, adjusted);
}

#[tokio::test]
async fn terminal_cancellation_cannot_be_reopened_and_controls_do_not_change_siblings() {
    use desk_agent_protocol::ai_assistant::subagent::{
        AiAssistantSubAgentControl, SubAgentControlAction,
    };
    let db = database().await;
    seed_parent(&db).await;
    let first = seed_task(&db, "a", "goal-a").await;
    let sibling = seed_task(&db, "b", "goal-b").await;
    let request = AiAssistantSubAgentControl {
        client_request_id: "cancel-1".into(),
        task_id: first.binding.task_id.clone(),
        expected_input_revision: 1,
        expected_control_revision: 1,
        action: SubAgentControlAction::Cancel,
    };
    let txn = db.begin().await.unwrap();
    let parent = parent_on(&txn, "root", "1", "1").await.unwrap();
    let result = apply_task_control_on(&txn, &parent, &request, 100)
        .await
        .unwrap();
    assert_eq!(result.task.state, SubAgentState::Cancelled);
    txn.commit().await.unwrap();
    assert_eq!(load(&db, &sibling.binding.task_id).await, sibling);
    let txn = db.begin().await.unwrap();
    let parent = parent_on(&txn, "root", "1", "1").await.unwrap();
    let adjust = AiAssistantSubAgentControl {
        client_request_id: "adjust-cancelled".into(),
        expected_control_revision: result.task.control_revision,
        action: SubAgentControlAction::Adjust {
            message: "Start again".into(),
        },
        ..request
    };
    assert!(
        apply_task_control_on(&txn, &parent, &adjust, 200)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        load(&db, &first.binding.task_id).await.state,
        SubAgentState::Cancelled
    );
}

#[tokio::test]
async fn source_pause_rotates_child_session_fence_but_preserves_approval_recording() {
    let db = database().await;
    seed_parent(&db).await;
    let run = seed_task(&db, "a", "goal-a").await;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut old = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    old.begin_turn("old-turn", None, None, 1, scope(), "1970-01-01T00:00:00Z")
        .unwrap();
    old.version = row.version + 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(old.encode_json_for_storage().unwrap()),
            version: Set(old.version),
            lease_token: Set(old.lease_token as i64),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    let original_fence =
        desk_diagnose_core::action_turn_fence::AssistantTurnFence::from_session(&old)
            .unwrap()
            .unwrap();
    assert!(
        check_child_action_on(&db, &old, &original_fence, 90)
            .await
            .unwrap()
    );
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let paused_row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&run.child_conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let paused = PersistedAgentSession::decode_json(&paused_row.state_json).unwrap();
    assert_eq!(
        paused.turn_state,
        desk_diagnose_core::session::TurnState::Idle
    );
    assert!(paused.lease_token > old.lease_token);
    assert_eq!(paused.agent_role.binding().unwrap().source_epoch, 2);
    assert!(check_child_permission_on(&db, &paused, 110).await.unwrap());
    assert!(
        !check_child_action_on(&db, &old, &original_fence, 110)
            .await
            .unwrap()
    );
    let txn = db.begin().await.unwrap();
    assert!(
        write_child_session_on(
            &txn,
            &old,
            chrono::DateTime::from_timestamp_millis(120).unwrap()
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(&txn, "root", "1", "1", "goal-a", GoalState::Cancelled, 150)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(!check_child_permission_on(&db, &paused, 160).await.unwrap());
}

#[tokio::test]
async fn notifications_roll_back_with_source_controls_and_replay_does_not_consume_them() {
    use crate::entity::agent_subagent_inbox as inbox;
    let db = database().await;
    seed_parent(&db).await;
    let run = seed_task(&db, "a", "goal-a").await;
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    assert_eq!(inbox::Entity::find().count(&txn).await.unwrap(), 1);
    txn.rollback().await.unwrap();
    assert_eq!(inbox::Entity::find().count(&db).await.unwrap(), 0);
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        200,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let group_row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq("a"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&group_row).unwrap();
    let paused = load(&db, &run.binding.task_id).await;
    append_state_event_on(&db, &group, &paused, 300)
        .await
        .unwrap();
    let events = inbox::Entity::find().all(&db).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].created_at, 200);
    assert!(events[0].ui_read_at_ms.is_none());
    assert!(events[0].model_observed_at_ms.is_none());
    assert!(events[0].interpreted_at_ms.is_none());
}

use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::{
    goal::GoalPauseReason,
    subagent::{
        DelegatedTaskBinding, DelegationSource,
        budget::{Allowance, BudgetLedger, DelegationLimits, Usage},
        state::TaskDependency,
    },
};
use sea_orm::{ActiveModelTrait, Schema};

async fn database() -> DatabaseConnection {
    database_at("sqlite::memory:").await
}

async fn database_at(url: &str) -> DatabaseConnection {
    let db = crate::config::test_support::Database::connect(url)
        .await
        .unwrap();
    let schema = Schema::new(db.get_database_backend());
    for statement in [
        schema.create_table_from_entity(session_row::Entity),
        schema.create_table_from_entity(crate::entity::agent_file_recovery_cleanup::Entity),
        schema.create_table_from_entity(crate::entity::agent_permission_resume::Entity),
        schema.create_table_from_entity(crate::entity::agent_action_item::Entity),
        schema.create_table_from_entity(crate::entity::agent_capability_dispatch_outbox::Entity),
        schema.create_table_from_entity(crate::entity::agent_exec_task::Entity),
        schema.create_table_from_entity(group_row::Entity),
        schema.create_table_from_entity(goal_row::Entity),
        schema.create_table_from_entity(crate::entity::model_egress_receipt::Entity),
        schema.create_table_from_entity(run_row::Entity),
        schema.create_table_from_entity(crate::entity::agent_subagent_inbox::Entity),
        schema.create_table_from_entity(crate::entity::agent_delegation_reservation::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_review::Entity),
        schema.create_table_from_entity(crate::entity::agent_approval_delegation::Entity),
    ] {
        db.execute(&statement).await.unwrap();
    }
    crate::db::ensure_lifecycle_tables(&db).await;
    db
}

fn scope() -> AgentScope {
    AgentScope {
        granted: Vec::new(),
        mode: ExecutionMode::ConfirmEachAction,
        expires_at: None,
        policy_name: None,
    }
}

async fn insert_session(db: &DatabaseConnection, session: &PersistedAgentSession) {
    let now = chrono::DateTime::from_timestamp_millis(1).unwrap();
    session_row::ActiveModel {
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(session.lease_token as i64),
        created_at: Set(now),
        updated_at: Set(now),

        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

async fn seed_parent(db: &DatabaseConnection) {
    let mut session =
        PersistedAgentSession::new("root", "1", "1", 1, scope(), "1970-01-01T00:00:00Z");
    session.surface = AgentSessionSurface::AiAssistant;
    session.begin_focus_epoch(1, Vec::new()).unwrap();
    session.input_revision = 1;
    insert_session(db, &session).await;
}

async fn seed_task(db: &DatabaseConnection, group_id: &str, goal_id: &str) -> SubAgentRun {
    let task_id = format!("task-{group_id}");
    let binding = DelegatedTaskBinding {
        root_conversation_id: "root".into(),
        group_id: group_id.into(),
        task_id: task_id.clone(),
        source: DelegationSource::Goal {
            goal_id: goal_id.into(),
        },
        objective: "Investigate".into(),
        acceptance_criteria: vec!["Cite evidence".into()],
        input_revision: 1,
        control_revision: 1,
        source_epoch: 1,
        deadline_ms: 10_000,
    };
    let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        format!("owner-{group_id}"),
        "Investigate".into(),
        destination.clone(),
    )
    .unwrap();
    let source = desk_diagnose_core::subagent::creation::CreationEnvelope {
        root_conversation_id: "root".into(),
        actor_id: "1".into(),
        device_id: "1".into(),
        source: binding.source.clone(),
        parent_input_revision: 1,
        parent_control_revision: 1,
        owner_requirement: owner.clone(),
        scheduled_source: None,
        original_read_context: None,
        model_destination: destination.clone(),
    };
    let inputs = vec![owner.data_envelope.unwrap()];
    let payload = serde_json::json!({"delegated_task": binding.objective, "acceptance_criteria": binding.acceptance_criteria});
    let context = desk_diagnose_core::subagent::creation::TaskCreationEnvelope {
        source: source.clone(),
        input_envelopes: inputs.clone(),
        response_locale: Some("zh-CN".into()),
        instruction: desk_diagnose_core::subagent::projection::runtime_message(
            &format!("child-{group_id}-instruction"),
            &payload,
            &inputs,
        )
        .unwrap(),
    };
    context.validate_task(&binding).unwrap();
    let group = DelegationGroup {
        group_id: group_id.into(),
        root_conversation_id: "root".into(),
        actor_id: "1".into(),
        device_id: "1".into(),
        source: binding.source.clone(),
        source_epoch: 1,
        source_admission: SourceAdmission::Open,
        parent_input_revision: 1,
        parent_control_revision: 1,
        parent_active: true,
        limits: DelegationLimits {
            total: Allowance {
                model_calls: 10,
                tool_calls: 20,
                tokens: Some(1000),
            },
            max_context_bytes: 4096,
            max_result_bytes: 8192,
            deadline_ms: 10_000,
        },
        budget: BudgetLedger::default(),
        tasks_created: 1,
        required_task_ids: vec![task_id.clone()],
        version: 1,
    };
    group_row::ActiveModel {
        group_id: Set(group_id.into()),
        source_key_sha256: Set(source.source_key().unwrap()),
        root_conversation_id: Set("root".into()),
        actor_id: Set("1".into()),
        device_id: Set("1".into()),
        source_goal_id: Set(Some(goal_id.into())),
        source_schedule_id: Set(None),
        source_occurrence_id: Set(None),
        source_admission: Set("open".into()),
        source_epoch: Set(1),
        parent_input_revision: Set(1),
        parent_control_revision: Set(1),
        parent_active: Set(true),
        state_json: Set(serde_json::to_string(&group).unwrap()),
        creation_envelope_json: Set(serde_json::to_string(&source).unwrap()),
        model_binding_json: Set(serde_json::to_string(&destination).unwrap()),
        version: Set(1),
        deadline_ms: Set(10_000),
        created_at: Set(1),
        updated_at: Set(1),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let mut run = SubAgentRun {
        child_conversation_id: format!("child-{group_id}"),
        actor_id: "1".into(),
        device_id: "1".into(),
        name: "Investigate".into(),
        binding,
        state: SubAgentState::Queued,
        state_revision: 1,
        source_paused: false,
        dependencies: Vec::new(),
        partial_report: None,
        terminal_report: None,
        failure_reason: None,
        report_corrections_used: 0,
        created_at: "1970-01-01T00:00:00Z".into(),
        updated_at: "1970-01-01T00:00:00Z".into(),
    };
    run.set_dependencies(
        vec![TaskDependency::Work {
            work_id: "work-1".into(),
        }],
        "1970-01-01T00:00:00Z",
    )
    .unwrap();
    run_row::ActiveModel {
        task_id: Set(task_id.clone()),
        child_conversation_id: Set(run.child_conversation_id.clone()),
        root_conversation_id: Set("root".into()),
        group_id: Set(group_id.into()),
        actor_id: Set("1".into()),
        device_id: Set("1".into()),
        creation_key_sha256: Set(format!("creation-{task_id}")),
        creation_arguments_sha256: Set("arguments".into()),
        creation_envelope_json: Set(serde_json::to_string(&context).unwrap()),
        result_envelope_json: Set(None),
        state: Set(run.state.as_str().into()),
        input_revision: Set(1),
        control_revision: Set(1),
        source_epoch: Set(1),
        state_revision: Set(run.state_revision as i64),
        state_json: Set(serde_json::to_string(&run).unwrap()),
        next_attempt_at_ms: Set(None),
        deadline_ms: Set(10_000),
        created_at: Set(1),
        updated_at: Set(1),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    let mut session = PersistedAgentSession::new_subagent(
        &run.child_conversation_id,
        "1",
        "1",
        1,
        scope(),
        run.binding.clone(),
        "1970-01-01T00:00:00Z",
    )
    .unwrap();
    session
        .bind_delegated_owner_requirement(&context.source)
        .unwrap();
    session.response_locale = context.response_locale.clone();
    insert_session(db, &session).await;
    run
}

async fn load(db: &DatabaseConnection, task_id: &str) -> SubAgentRun {
    decode_run(
        &run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .one(db)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn goal_pause_and_resume_only_change_the_matching_source_group() {
    let db = database().await;
    seed_parent(&db).await;
    let first = seed_task(&db, "a", "goal-a").await;
    let second = seed_task(&db, "b", "goal-b").await;
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let paused = load(&db, &first.binding.task_id).await;
    assert_eq!(paused.state, SubAgentState::WaitingSource);
    assert_eq!(paused.dependencies, first.dependencies);
    assert_eq!(paused.binding.source_epoch, 2);
    assert_eq!(load(&db, &second.binding.task_id).await, second);
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(&txn, "root", "1", "1", "goal-a", GoalState::Queued, 200)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let resumed = load(&db, &first.binding.task_id).await;
    assert_eq!(resumed.state, SubAgentState::WaitingWork);
    assert_eq!(resumed.binding.source_epoch, 3);
    assert_eq!(resumed.binding.deadline_ms, first.binding.deadline_ms);
    assert_eq!(resumed.dependencies, first.dependencies);
}

#[tokio::test]
async fn source_controls_roll_back_together_and_cancellation_never_revives() {
    let db = database().await;
    seed_parent(&db).await;
    let first = seed_task(&db, "a", "goal-a").await;
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(load(&db, &first.binding.task_id).await, first);
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(&txn, "root", "1", "1", "goal-a", GoalState::Cancelled, 200)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let cancelled = load(&db, &first.binding.task_id).await;
    assert_eq!(cancelled.state, SubAgentState::Cancelled);
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(&txn, "root", "1", "1", "goal-a", GoalState::Queued, 300)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(load(&db, &first.binding.task_id).await, cancelled);
}

#[tokio::test]
async fn expired_paused_child_fails_without_extending_deadline() {
    let db = database().await;
    seed_parent(&db).await;
    let first = seed_task(&db, "a", "goal-a").await;
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-a",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    apply_goal_source_on(&txn, "root", "1", "1", "goal-a", GoalState::Queued, 10_000)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let failed = load(&db, &first.binding.task_id).await;
    assert_eq!(failed.state, SubAgentState::Failed);
    assert_eq!(failed.binding.deadline_ms, first.binding.deadline_ms);
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("delegation_deadline_reached")
    );
}

#[tokio::test]
async fn bounded_pages_and_results_are_owner_root_scoped() {
    let db = database().await;
    seed_parent(&db).await;
    let first = seed_task(&db, "a", "goal-a").await;
    seed_task(&db, "b", "goal-b").await;
    let store = SubAgentStore::new(db);
    let page = store
        .list_for_owner("root", "1", "1", None, 1)
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.unfinished, 2);
    assert_eq!(page.items.len(), 1);
    assert!(page.has_more);
    let second_page = store
        .list_for_owner("root", "1", "1", page.next_cursor.as_deref(), 1)
        .await
        .unwrap();
    assert_eq!(second_page.items.len(), 1);
    assert!(!second_page.has_more);
    assert!(
        store
            .result_for_owner("root", "2", "1", &first.binding.task_id)
            .await
            .is_err()
    );
    assert!(
        store
            .result_for_owner("root", "1", "other-device", &first.binding.task_id)
            .await
            .is_err()
    );
    assert!(
        store
            .list_for_owner(&first.child_conversation_id, "1", "1", None, 1)
            .await
            .is_err()
    );
}
