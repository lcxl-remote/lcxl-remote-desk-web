use super::*;
use crate::entity::agent_exec_task as command;
use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};

pub(super) async fn seed_native(
    db: &DatabaseConnection,
    conversation: &str,
    generation: &str,
) -> command::Model {
    let store = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
    store
        .create(
            &format!("action-{generation}"),
            generation,
            conversation,
            &format!("call-{generation}"),
            "original-host",
            chrono::DateTime::from_timestamp_millis(20_000).unwrap(),
        )
        .await
        .unwrap();
    store.mark_running(generation).await.unwrap();
    command::Entity::find()
        .filter(command::Column::ExecutionGeneration.eq(generation))
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

async fn native<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    id: i64,
) -> command::Model {
    command::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

fn assert_stop_only(before: &command::Model, after: &command::Model) {
    assert!(after.cancel_requested_at.is_some());
    assert_eq!(after.cancel_requested_by.as_deref(), Some("1"));
    let mut facts = after.clone();
    facts.cancel_requested_at = before.cancel_requested_at;
    facts.cancel_requested_by = before.cancel_requested_by.clone();
    assert_eq!(&facts, before);
}

#[tokio::test]
async fn source_pause_keeps_native_work_and_cancel_intent_rolls_back_with_source_control() {
    let db = database().await;
    seed_parent(&db).await;
    let task = seed_task(&db, "source", "goal-source").await;
    let sibling = seed_task(&db, "sibling", "goal-sibling").await;
    let original = seed_native(&db, &task.child_conversation_id, "original").await;
    let unrelated = seed_native(&db, &sibling.child_conversation_id, "unrelated").await;
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-source",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(native(&db, original.id).await, original);
    assert_eq!(native(&db, unrelated.id).await, unrelated);
    assert_eq!(
        load(&db, &task.binding.task_id).await.state,
        SubAgentState::WaitingSource
    );
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-source",
        GoalState::Cancelled,
        200,
    )
    .await
    .unwrap();
    assert_stop_only(&original, &native(&txn, original.id).await);
    assert_eq!(native(&txn, unrelated.id).await, unrelated);
    txn.rollback().await.unwrap();
    assert_eq!(native(&db, original.id).await, original);
    assert_eq!(
        load(&db, &task.binding.task_id).await.state,
        SubAgentState::WaitingSource
    );
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-source",
        GoalState::Cancelled,
        300,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_stop_only(&original, &native(&db, original.id).await);
    assert_eq!(native(&db, unrelated.id).await, unrelated);
    assert_eq!(
        load(&db, &task.binding.task_id).await.state,
        SubAgentState::Cancelled
    );
}

#[tokio::test]
async fn main_stop_scope_applies_to_original_native_work_without_synthesizing_results() {
    for choice in [
        SubAgentStopChoice::MainOnly,
        SubAgentStopChoice::IncludeSubAgents,
    ] {
        let db = database().await;
        let (parent, calls) = super::creation::runnable_parent(&db).await;
        let store = SubAgentStore::new(db.clone());
        let task = store
            .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
            .await
            .unwrap();
        let run = load(&db, &task.task_id).await;
        let main_command = seed_native(&db, "root", "main-command").await;
        let child_command = seed_native(&db, &run.child_conversation_id, "child-command").await;
        let other_command = seed_native(&db, "other-chat", "other-command").await;
        let control = AiAssistantStopControl {
            client_request_id: "native-scope-stop".into(),
            expected_input_revision: parent.input_revision,
            expected_control_revision: parent.control_revision,
            subagent_choice: Some(choice),
        };
        store
            .stop_for_owner("root", "1", "1", &control)
            .await
            .unwrap();
        assert_stop_only(&main_command, &native(&db, main_command.id).await);
        if choice == SubAgentStopChoice::IncludeSubAgents {
            assert_stop_only(&child_command, &native(&db, child_command.id).await);
            assert_eq!(
                load(&db, &task.task_id).await.state,
                SubAgentState::Cancelled
            );
        } else {
            assert_eq!(native(&db, child_command.id).await, child_command);
            assert_eq!(load(&db, &task.task_id).await, run);
        }
        assert_eq!(native(&db, other_command.id).await, other_command);
    }
}

#[tokio::test]
async fn expired_child_requests_original_native_stop_and_does_not_claim_execution_cancelled() {
    let db = database().await;
    seed_parent(&db).await;
    let task = seed_task(&db, "expired", "goal-expired").await;
    let original = seed_native(&db, &task.child_conversation_id, "expired-native").await;
    let now = chrono::DateTime::from_timestamp_millis(task.binding.deadline_ms + 1).unwrap();
    let store = SubAgentStore::new(db.clone());
    assert!(
        store
            .expire_task_at(&task.binding.task_id, now)
            .await
            .unwrap()
    );
    assert_stop_only(&original, &native(&db, original.id).await);
    assert_eq!(
        load(&db, &task.binding.task_id).await.state,
        SubAgentState::Failed
    );
    let after = native(&db, original.id).await;
    assert!(
        !store
            .expire_task_at(&task.binding.task_id, now)
            .await
            .unwrap()
    );
    assert_eq!(native(&db, original.id).await, after);
}

#[tokio::test]
async fn individual_command_cancel_is_generation_scoped_and_preserves_native_facts() {
    let db = database().await;
    seed_parent(&db).await;
    let task = seed_task(&db, "individual", "goal-individual").await;
    let original = seed_native(&db, &task.child_conversation_id, "original-individual").await;
    let sibling = seed_native(&db, "root", "main-individual").await;
    let store = SubAgentStore::new(db.clone());
    for (session, actor, device, request, generation) in [
        (
            "root",
            "1",
            "1",
            "action-original-individual",
            "original-individual",
        ),
        (
            task.child_conversation_id.as_str(),
            "2",
            "1",
            "action-original-individual",
            "original-individual",
        ),
        (
            task.child_conversation_id.as_str(),
            "1",
            "2",
            "action-original-individual",
            "original-individual",
        ),
        (
            task.child_conversation_id.as_str(),
            "1",
            "1",
            "action-other",
            "original-individual",
        ),
        (
            task.child_conversation_id.as_str(),
            "1",
            "1",
            "action-original-individual",
            "superseded",
        ),
    ] {
        assert!(
            store
                .cancel_command_for_owner(session, actor, device, request, generation)
                .await
                .is_err()
        );
        assert_eq!(native(&db, original.id).await, original);
    }
    let txn = db.begin().await.unwrap();
    apply_goal_source_on(
        &txn,
        "root",
        "1",
        "1",
        "goal-individual",
        GoalState::Paused(GoalPauseReason::Owner),
        100,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert!(
        store
            .cancel_command_for_owner(
                &task.child_conversation_id,
                "1",
                "1",
                "action-original-individual",
                "original-individual"
            )
            .await
            .unwrap()
    );
    let cancelled = native(&db, original.id).await;
    assert_stop_only(&original, &cancelled);
    assert_eq!(native(&db, sibling.id).await, sibling);
    assert_eq!(
        load(&db, &task.binding.task_id).await.state,
        SubAgentState::WaitingSource
    );
    assert!(
        store
            .cancel_command_for_owner(
                &task.child_conversation_id,
                "1",
                "1",
                "action-original-individual",
                "original-individual"
            )
            .await
            .unwrap()
    );
    assert_eq!(native(&db, original.id).await, cancelled);
}

#[tokio::test]
async fn deleting_root_closes_children_but_keeps_original_unknown_native_evidence() {
    let db = database().await;
    seed_parent(&db).await;
    let task = seed_task(&db, "deleted-native", "goal-deleted-native").await;
    let parent_row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let parent = PersistedAgentSession::decode_json(&parent_row.state_json).unwrap();
    let original = seed_native(
        &db,
        &task.child_conversation_id,
        "deleted-native-generation",
    )
    .await;
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &parent, 100).await.unwrap();
    assert_stop_only(&original, &native(&txn, original.id).await);
    txn.rollback().await.unwrap();
    assert_eq!(native(&db, original.id).await, original);
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &parent, 200).await.unwrap();
    txn.commit().await.unwrap();
    assert_stop_only(&original, &native(&db, original.id).await);
    let root_eligible = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .filter(reclaim_condition(1_000))
        .one(&db)
        .await
        .unwrap();
    assert!(
        root_eligible.is_none(),
        "independent native reconciliation still protects the root"
    );
    assert_eq!(
        load(&db, &task.binding.task_id).await.state,
        SubAgentState::Cancelled
    );
    assert!(deleted_on(&db, "root").await.unwrap());
    assert!(deleted_on(&db, &task.child_conversation_id).await.unwrap());
}

#[tokio::test]
async fn late_native_receipt_is_durable_and_idempotent_while_goal_is_paused() {
    use desk_diagnose_core::goal::GoalOwnerAction;
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let sibling = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let sibling_before = load(&db, &sibling.task_id).await;
    super::input_sources::release_parent(&db).await;
    super::input_sources::append_input(&db, true, "native-goal-input").await;
    let task = super::input_sources::spawn_goal_child(&db, &store).await;
    super::input_sources::release_parent(&db).await;
    let opened = super::input_sources::goal(&db).await;
    // The isolated fixture represents an already dispatched original command.
    // No device execution, owner approval, or model export is fabricated here.
    let original_run = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&original_run),
            &task.task_id,
            original_run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("original child claim")
    };
    let mut child = claimed.session;
    child.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    save_child_session(&db, &mut child).await.unwrap();
    let original = seed_native(&db, &original_run.child_conversation_id, "paused-native").await;
    let unrelated = seed_native(&db, &sibling_before.child_conversation_id, "other-native").await;
    let paused = super::paused_permission::control(&db, &opened, GoalOwnerAction::Pause)
        .await
        .unwrap();
    let frozen = load(&db, &task.task_id).await;
    assert_eq!(frozen.state, SubAgentState::WaitingSource);
    assert_eq!(native(&db, original.id).await, original);
    receive_paused_result(&db, &original).await;
    let received = native(&db, original.id).await;
    assert_eq!(received.status, "done");
    assert_eq!(native(&db, unrelated.id).await, unrelated);
    assert_eq!(load(&db, &task.task_id).await, frozen);
    assert_eq!(load(&db, &sibling.task_id).await, sibling_before);
    assert!(
        store
            .child_runtime_candidate(&task.task_id)
            .await
            .unwrap()
            .is_none()
    );
    let resumed = super::paused_permission::control(&db, &paused, GoalOwnerAction::Resume)
        .await
        .unwrap();
    assert_eq!(resumed.used, opened.used);
    assert_eq!(resumed.limits, opened.limits);
    assert_eq!(resumed.deadline_unix_ms, opened.deadline_unix_ms);
    let after = load(&db, &task.task_id).await;
    let mut expected_binding = frozen.binding.clone();
    expected_binding.source_epoch += 1;
    assert_eq!(after.binding, expected_binding);
    assert!(
        store
            .child_runtime_candidate(&task.task_id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(native(&db, original.id).await, received);
    assert_eq!(native(&db, unrelated.id).await, unrelated);
}

async fn receive_paused_result(db: &DatabaseConnection, original: &command::Model) {
    use desk_agent_protocol::edge_exec::EdgeExecDisposition;
    use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome};
    let store = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
    let disposition = EdgeExecDisposition::Executed {
        outcome: AgentOutcome::Err(AgentError {
            kind: AgentErrorKind::Cancelled,
            message: "fixture original worker cancellation receipt".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }),
    };
    for (source, generation) in [
        ("other-host", "paused-native"),
        ("original-host", "other-generation"),
    ] {
        assert!(
            store
                .finalize(source, generation, &disposition)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(native(db, original.id).await, *original);
    }
    let received = store
        .finalize("original-host", "paused-native", &disposition)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<EdgeExecDisposition>(received.disposition_json.as_deref().unwrap())
            .unwrap(),
        disposition
    );
    assert_eq!(received.delivery_state, "pending");
    assert_eq!(received.event_id, original.event_id);
    assert_eq!(received.execution_generation, original.execution_generation);
    let duplicate = EdgeExecDisposition::RejectedBeforeDispatch {
        error: EdgeExecDisposition::safe_error(
            AgentErrorKind::Cancelled,
            "fixture duplicate must not overwrite",
            false,
        ),
    };
    assert_eq!(
        store
            .finalize("original-host", "paused-native", &duplicate)
            .await
            .unwrap()
            .unwrap(),
        received
    );
    assert_eq!(native(db, original.id).await, received);
}
