use super::*;
use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};
use desk_diagnose_core::session::TurnState;

fn request(
    parent: &PersistedAgentSession,
    choice: Option<SubAgentStopChoice>,
) -> AiAssistantStopControl {
    AiAssistantStopControl {
        client_request_id: "root-stop-1".into(),
        expected_input_revision: parent.input_revision,
        expected_control_revision: parent.control_revision,
        subagent_choice: choice,
    }
}

async fn stored_parent(db: &DatabaseConnection) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[tokio::test]
async fn stale_empty_child_snapshot_requires_confirmation_without_stopping_anything() {
    let db = database().await;
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    assert!(
        store
            .stop_for_owner("root", "1", "1", &request(&parent, None))
            .await
            .is_err()
    );
    assert_eq!(stored_parent(&db).await, parent);
    assert_eq!(load(&db, &task.task_id).await.state, SubAgentState::Queued);
}

#[tokio::test]
async fn include_children_covers_a_child_created_after_confirmation_snapshot_and_is_idempotent() {
    let db = database().await;
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let first = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let confirmation = request(&parent, Some(SubAgentStopChoice::IncludeSubAgents));
    let second = store
        .spawn_for_turn(&parent, &calls[1], &super::creation::spawn_request())
        .await
        .unwrap();
    let result = store
        .stop_for_owner("root", "1", "1", &confirmation)
        .await
        .unwrap();
    assert_eq!(result.result.stopped_subagents.len(), 2);
    assert!(
        result
            .result
            .stopped_subagents
            .iter()
            .all(|task| task.state == SubAgentState::Cancelled)
    );
    assert_eq!(
        store
            .stop_for_owner("root", "1", "1", &confirmation)
            .await
            .unwrap(),
        result
    );
    assert!(
        store
            .spawn_for_turn(&parent, &calls[2], &super::creation::spawn_request())
            .await
            .is_err()
    );
    let current = stored_parent(&db).await;
    assert!(current.main_stopped);
    assert_eq!(current.turn_state, TurnState::Cancelled);
    assert_eq!(current.input_revision, parent.input_revision);
    assert_eq!(current.control_revision, parent.control_revision + 1);
    assert!(current.lease_token > parent.lease_token);
    assert!(current.unclosed_tool_call_ids().is_empty());
    assert!(current.subagent_wait.is_none() && current.ready_subagent_wait.is_none());
    assert_eq!(
        load(&db, &first.task_id).await.state,
        SubAgentState::Cancelled
    );
    assert_eq!(
        load(&db, &second.task_id).await.state,
        SubAgentState::Cancelled
    );
    let mut conflicting = confirmation;
    conflicting.subagent_choice = Some(SubAgentStopChoice::MainOnly);
    assert!(
        store
            .stop_for_owner("root", "1", "1", &conflicting)
            .await
            .is_err()
    );
    assert_eq!(stored_parent(&db).await, current);
}

#[tokio::test]
async fn main_only_preserves_child_admission_budget_and_deadline_but_closes_parent_planning() {
    let db = database().await;
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let before = load(&db, &task.task_id).await;
    let original = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&before.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let original = decode_group(&original).unwrap();
    let result = store
        .stop_for_owner(
            "root",
            "1",
            "1",
            &request(&parent, Some(SubAgentStopChoice::MainOnly)),
        )
        .await
        .unwrap();
    assert!(result.result.stopped_subagents.is_empty());
    assert_eq!(load(&db, &task.task_id).await, before);
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&before.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&group).unwrap();
    assert!(!group.parent_active);
    assert_eq!(group.source_admission, original.source_admission);
    assert_eq!(group.source_epoch, original.source_epoch);
    assert_eq!(group.limits, original.limits);
    assert_eq!(group.budget, original.budget);
    assert_eq!(group.tasks_created, original.tasks_created);
    assert!(matches!(
        store
            .claim_child(
                &super::creation::claim_params(&before),
                &task.task_id,
                before.fence(),
                &super::creation::destination()
            )
            .await
            .unwrap(),
        SubAgentClaimOutcome::Claimed(_)
    ));
    let mut current = stored_parent(&db).await;
    assert!(
        current
            .begin_turn(
                "automatic",
                None,
                None,
                2,
                scope(),
                chrono::Utc::now().to_rfc3339()
            )
            .is_err()
    );
    assert!(!store.child_resume_available(&current).await.unwrap());
}

#[tokio::test]
async fn stale_parent_input_or_wrong_subject_never_stops_a_newer_conversation() {
    let db = database().await;
    let (mut parent, _) = super::creation::runnable_parent(&db).await;
    let stale = request(&parent, Some(SubAgentStopChoice::IncludeSubAgents));
    parent.begin_focus_epoch(2, Vec::new()).unwrap();
    parent.input_revision = 2;
    parent.delegation_group_id = None;
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    let prior_version = parent.version;
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            lease_deadline: Set(None),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .filter(session_row::Column::Version.eq(prior_version))
        .exec(&db)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    assert!(
        store
            .stop_for_owner("root", "1", "1", &stale)
            .await
            .is_err()
    );
    assert!(
        store
            .stop_for_owner("root", "2", "1", &request(&parent, None))
            .await
            .is_err()
    );
    assert!(
        store
            .stop_for_owner("root", "1", "2", &request(&parent, None))
            .await
            .is_err()
    );
    assert_eq!(stored_parent(&db).await, parent);
}

#[tokio::test]
async fn no_children_stop_fences_generation_and_a_new_owner_input_can_start_again() {
    let db = database().await;
    let (parent, _) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let result = store
        .stop_for_owner("root", "1", "1", &request(&parent, None))
        .await
        .unwrap();
    assert!(result.result.stopped_subagents.is_empty());
    let mut current = stored_parent(&db).await;
    assert!(current.main_stopped);
    current.begin_focus_epoch(2, Vec::new()).unwrap();
    current.input_revision = 2;
    assert!(!current.main_stopped);
    current
        .begin_turn(
            "new-owner-turn",
            Some("new-owner-request".into()),
            None,
            2,
            scope(),
            chrono::Utc::now().to_rfc3339(),
        )
        .unwrap();
    assert_eq!(current.control_revision, result.result.control_revision);
}

#[tokio::test]
async fn retained_children_do_not_fill_the_main_conversation_history_page() {
    let db = database().await;
    seed_parent(&db).await;
    for index in 0..12 {
        let group = format!("history-group-{index}");
        let goal = format!("history-goal-{index}");
        let mut task = seed_task(&db, &group, &goal).await;
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&task.binding.task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        task.request_cancel(task.fence(), "1970-01-01T00:00:01Z")
            .unwrap();
        task.settle_cancel("1970-01-01T00:00:01Z").unwrap();
        replace_run_on(&db, &row, &task, 1000).await.unwrap();
        synchronize_control_on(&db, &task, 1000).await.unwrap();
    }
    let history = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .list_ai_assistant_sessions("1", "1", 1)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].session_id, "root");
}

#[tokio::test]
async fn child_completing_after_main_only_stop_keeps_result_without_restarting_parent() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    // Close unrelated fixture tool slots before stopping, so they cannot mask a wakeup.
    super::notification::release_parent(&db, &mut parent).await;
    assert!(parent.unclosed_tool_call_ids().is_empty());
    store
        .stop_for_owner(
            "root",
            "1",
            "1",
            &request(&parent, Some(SubAgentStopChoice::MainOnly)),
        )
        .await
        .unwrap();
    let stopped = stored_parent(&db).await;
    assert!(stopped.main_stopped);
    assert!(!load(&db, &task.task_id).await.state.is_terminal());
    super::main_tools::complete_child(&db, &task.task_id).await;
    assert_eq!(
        load(&db, &task.task_id).await.state,
        SubAgentState::Completed
    );
    let events = inbox::Entity::find().all(&db).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.task_id == task.task_id && event.event_kind == "completed")
    );
    let params = desk_diagnose_core::seam::ClaimTurnParams {
        conversation_id: "root".into(),
        actor_id: "1".into(),
        device_id: "1".into(),
        policy_revision: stopped.policy_revision,
        current_pdp_scope: stopped.scope_snapshot.clone(),
        turn_id: "forbidden-late-parent-wakeup".into(),
        request_id: None,
        connection_id: None,
        trigger_origin: desk_diagnose_core::session::TriggerOrigin::SubAgentCompletion,
        now: chrono::Utc::now().to_rfc3339(),
    };
    for _ in 0..2 {
        assert!(
            store
                .parent_runtime_candidate("root", "1", "1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .claim_parent_completion(&params, &super::creation::destination())
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        stored_parent(&db).await.encode_json_for_storage().unwrap(),
        stopped.encode_json_for_storage().unwrap()
    );
    assert_eq!(inbox::Entity::find().all(&db).await.unwrap(), events);
    assert!(
        events
            .iter()
            .all(|event| event.model_notified_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
}

use crate::entity::agent_subagent_inbox as inbox;
