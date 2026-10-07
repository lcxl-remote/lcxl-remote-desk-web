use super::*;
use desk_agent_protocol::ai_assistant::{
    subagent::{AiAssistantStopControl, SubAgentStopChoice},
    subagent_policy::{SubAgentLimits, UpdateSubAgentPolicy},
};
use desk_diagnose_core::chat::{ToolCall, ToolCallRef};

async fn configure(db: &DatabaseConnection, unfinished: u32) {
    let current = crate::subagent_policy::read(db).await.unwrap();
    crate::subagent_policy::update(
        db,
        &UpdateSubAgentPolicy {
            expected_revision: current.revision,
            limits: SubAgentLimits {
                max_unfinished_per_root: unfinished,
            },
        },
    )
    .await
    .unwrap();
}

async fn parent_with_calls(
    db: &DatabaseConnection,
    count: usize,
) -> (PersistedAgentSession, Vec<ToolCall>) {
    let (mut parent, mut calls) = super::creation::runnable_parent(db).await;
    let caller = parent
        .conversation
        .iter_mut()
        .find(|message| message.message_id == "assistant-calls")
        .unwrap();
    for index in 4..=count {
        let call = ToolCall {
            id: format!("spawn-{index}"),
            ..calls[0].clone()
        };
        caller.tool_calls.push(ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        });
        calls.push(call);
    }
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            sea_orm::sea_query::Expr::value(parent.encode_json_for_storage().unwrap()),
        )
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(db)
        .await
        .unwrap();
    (parent, calls)
}

#[tokio::test]
async fn raised_limits_allow_more_tasks_and_lowering_preserves_replay_projection_and_stop() {
    let db = database().await;
    configure(&db, 8).await;
    let (parent, calls) = parent_with_calls(&db, 9).await;
    let store = SubAgentStore::new(db.clone());
    let mut tasks = Vec::new();
    for call in calls.iter().take(8) {
        tasks.push(
            store
                .spawn_for_turn(&parent, call, &super::creation::spawn_request())
                .await
                .unwrap(),
        );
    }
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 8);
    let error = store
        .spawn_for_turn(&parent, &calls[8], &super::creation::spawn_request())
        .await
        .unwrap_err();
    assert!(
        matches!(error, DbErr::Custom(message) if message == desk_diagnose_core::subagent::capacity_storage_message(8))
    );
    configure(&db, 1).await;
    assert_eq!(
        store
            .spawn_for_turn(&parent, &calls[4], &super::creation::spawn_request())
            .await
            .unwrap()
            .task_id,
        tasks[4].task_id
    );
    let projection = store.main_projection_for_turn(&parent).await.unwrap();
    assert!(projection.text.contains("\"maxUnfinishedPerRoot\":1"));
    assert!(projection.text.contains("\"created_in_group\":8"));
    let txn = db.begin().await.unwrap();
    let snapshot = super::super::presentation::presentation_on(&txn, &parent)
        .await
        .unwrap();
    assert_eq!(snapshot.active_tasks.len(), 8);
    assert_eq!(snapshot.tasks.unwrap().unfinished, 8);
    txn.rollback().await.unwrap();
    let error = store
        .spawn_for_turn(&parent, &calls[8], &super::creation::spawn_request())
        .await
        .unwrap_err();
    assert!(
        matches!(error, DbErr::Custom(message) if message == desk_diagnose_core::subagent::capacity_storage_message(1))
    );
    let stopped = store
        .stop_for_owner(
            "root",
            "1",
            "1",
            &AiAssistantStopControl {
                client_request_id: "stop-over-lowered-limit".into(),
                expected_input_revision: parent.input_revision,
                expected_control_revision: parent.control_revision,
                subagent_choice: Some(SubAgentStopChoice::IncludeSubAgents),
            },
        )
        .await
        .unwrap();
    assert_eq!(stopped.result.stopped_subagents.len(), 8);
    for task in tasks {
        assert_eq!(
            load(&db, &task.task_id).await.state,
            SubAgentState::Cancelled
        );
    }
}

#[tokio::test]
async fn terminal_tasks_release_slots_without_a_cumulative_limit() {
    let db = database().await;
    configure(&db, 1).await;
    let (parent, calls) = parent_with_calls(&db, 131).await;
    let store = SubAgentStore::new(db.clone());
    for (index, call) in calls.iter().take(130).enumerate() {
        let task = store
            .spawn_for_turn(&parent, call, &super::creation::spawn_request())
            .await
            .unwrap();
        // A queued child already occupies the only unfinished slot.
        let error = store
            .spawn_for_turn(
                &parent,
                &calls[index + 1],
                &super::creation::spawn_request(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, DbErr::Custom(message)
            if message == desk_diagnose_core::subagent::capacity_storage_message(1)));
        let txn = db.begin().await.unwrap();
        let parent_row = parent_on(&txn, "root", "1", "1").await.unwrap();
        apply_task_control_on(
            &txn,
            &parent_row,
            &desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentControl {
                client_request_id: format!("cancel-releases-slot-{index}"),
                task_id: task.task_id,
                expected_input_revision: 1,
                expected_control_revision: 1,
                action: desk_agent_protocol::ai_assistant::subagent::SubAgentControlAction::Cancel,
            },
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
    }
    store
        .spawn_for_turn(&parent, &calls[130], &super::creation::spawn_request())
        .await
        .unwrap();
    let row = group_row::Entity::find().one(&db).await.unwrap().unwrap();
    let group = decode_group(&row).unwrap();
    assert_eq!(group.tasks_created, 131);
    assert_eq!(group.required_task_ids.len(), 131);
    let txn = db.begin().await.unwrap();
    let history = super::super::group_children_on(&txn, &group).await.unwrap();
    assert_eq!(history.len(), 131);
    assert_eq!(
        history
            .iter()
            .filter(|row| !decode_run(row).unwrap().state.is_terminal())
            .count(),
        1
    );
    txn.rollback().await.unwrap();
    let projection = store.main_projection_for_turn(&parent).await.unwrap();
    assert!(!projection.text.contains("maxCreatedPerGroup"));
    assert!(!projection.text.contains("remaining_group_creations"));
    assert!(projection.text.contains("\"remaining_unfinished_slots\":0"));
}

#[tokio::test]
async fn invalid_configuration_fails_closed_without_creating_a_task() {
    let db = database().await;
    let (parent, calls) = parent_with_calls(&db, 3).await;
    let store = SubAgentStore::new(db.clone());
    crate::subagent_policy::read(&db).await.unwrap();
    crate::entity::subagent_policy::Entity::update_many()
        .col_expr(
            crate::entity::subagent_policy::Column::ConfigJson,
            sea_orm::sea_query::Expr::value("{}"),
        )
        .exec(&db)
        .await
        .unwrap();
    assert!(
        store
            .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
            .await
            .is_err()
    );
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 0);
}

#[tokio::test]
async fn simultaneous_creations_cannot_exceed_the_unfinished_limit() {
    let db = database().await;
    let (parent, calls) = parent_with_calls(&db, 3).await;
    let store = SubAgentStore::new(db.clone());
    let request = super::creation::spawn_request();
    let (first, second, third) = tokio::join!(
        store.spawn_for_turn(&parent, &calls[0], &request),
        store.spawn_for_turn(&parent, &calls[1], &request),
        store.spawn_for_turn(&parent, &calls[2], &request),
    );
    let results = [first, second, third];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
    for result in results.into_iter().filter_map(Result::err) {
        assert!(matches!(result, DbErr::Custom(message)
            if message == desk_diagnose_core::subagent::capacity_storage_message(2)));
    }
    assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 2);
}

#[tokio::test]
async fn completed_and_failed_children_also_release_creation_slots() {
    for completed in [true, false] {
        let db = database().await;
        configure(&db, 1).await;
        let (parent, calls) = parent_with_calls(&db, 3).await;
        let store = SubAgentStore::new(db.clone());
        let task = store
            .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
            .await
            .unwrap();
        let row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&task.task_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut run = decode_run(&row).unwrap();
        let now = chrono::Utc::now();
        if completed {
            run.claim_planning(run.fence(), now.timestamp_millis(), &now.to_rfc3339())
                .unwrap();
            let facts = desk_diagnose_core::subagent::state::CompletionFacts::default();
            let report =
                desk_diagnose_core::subagent::report::from_answer("Done", false, &facts).unwrap();
            run.settle_report(run.fence(), report, &facts, &now.to_rfc3339())
                .unwrap();
        } else {
            run.fail("fixture failure", &now.to_rfc3339()).unwrap();
        }
        replace_run_on(&db, &row, &run, now.timestamp_millis())
            .await
            .unwrap();
        let replacement = store
            .spawn_for_turn(&parent, &calls[1], &super::creation::spawn_request())
            .await
            .unwrap();
        assert_ne!(replacement.task_id, task.task_id);
        assert_eq!(run_row::Entity::find().count(&db).await.unwrap(), 2);
    }
}
