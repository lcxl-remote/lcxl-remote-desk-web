//! Exercise original occurrence claims and genuine provider notification evidence.
use super::*;
use crate::agent_subagent_store as children;
use crate::entity::{agent_delegation_reservation as cost, agent_subagent_inbox as inbox};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use sea_orm::{PaginatorTrait, TransactionTrait};

async fn saved(
    db: &crate::config::connection::DatabaseConnection,
    root: &str,
) -> PersistedAgentSession {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(root))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

async fn occurrence(db: &crate::config::connection::DatabaseConnection, root: &str) -> run::Model {
    run::Entity::find()
        .filter(run::Column::RunId.eq(root))
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn optional_child_completion_claims_one_notice_in_the_same_finite_occurrence() {
    let (db, schedule, store, parent, task_id) = children::scheduled_test_answer_fixture().await;
    let original = occurrence(&db, &parent.conversation_id).await;
    assert!(super::tests::claim_on(&db, &parent).await.is_err());
    children::complete_scheduled_test_child(&db, &task_id).await;
    assert!(
        store
            .parent_runtime_candidate(&parent.conversation_id, &parent.actor_id, &parent.device_id)
            .await
            .unwrap()
            .is_none()
    );
    let claimed = super::tests::claim_on(&db, &parent).await.unwrap();
    assert!(claimed.is_subagent_result_turn());
    assert!(claimed.subagent_result_only);
    assert!(!claimed.allows_new_mutation());
    assert!(!claimed.allows_delegated_review());
    assert_eq!(claimed.trigger_origin, TriggerOrigin::ScheduledTask);
    assert_eq!(claimed.input_revision, parent.input_revision);
    assert_eq!(claimed.conversation.first(), parent.conversation.first());
    assert_eq!(claimed.current_turn_steps, parent.current_turn_steps);
    assert_eq!(claimed.current_turn_tokens, parent.current_turn_tokens);
    assert_eq!(claimed.lifetime_steps, parent.lifetime_steps);
    assert_eq!(claimed.lifetime_tokens, parent.lifetime_tokens);
    assert!(claimed.subagent_wait.is_none() && claimed.ready_subagent_wait.is_none());
    let notice = claimed.ready_subagent_notification.as_ref().unwrap();
    assert_eq!(notice.events.len(), 1);
    assert_eq!(notice.events[0].task_id, task_id);
    assert_eq!(
        claimed
            .conversation
            .iter()
            .filter(|message| message.role == desk_diagnose_core::chat::ChatRole::User)
            .count(),
        1
    );
    let event = inbox::Entity::find()
        .filter(inbox::Column::EventId.eq(&notice.events[0].event_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event.notification_attempted_turn_id,
        claimed.current_turn_id
    );
    assert!(
        event.model_notified_at_ms.is_none()
            && event.model_observed_at_ms.is_none()
            && event.interpreted_at_ms.is_none()
    );
    assert!(super::tests::claim_on(&db, &claimed).await.is_err());
    let current = occurrence(&db, &parent.conversation_id).await;
    assert_eq!(current.started_at, original.started_at);
    assert_eq!(current.attempt, original.attempt);
    assert_eq!(current.result_ref, original.result_ref);
    assert_eq!(current.lease_epoch, original.lease_epoch + 1);
    assert_eq!(
        schedule
            .read(1, &original.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(original.run_id.as_str())
    );
}

#[tokio::test]
async fn notification_requires_the_original_accepted_provider_receipt_before_delivery_and_final_settlement()
 {
    let (db, schedule, _, parent, task_id) = children::scheduled_test_answer_fixture().await;
    children::complete_scheduled_test_child(&db, &task_id).await;
    let mut claimed = super::tests::claim_on(&db, &parent).await.unwrap();
    let original = claimed.clone();
    let event_id = claimed.ready_subagent_notification.as_ref().unwrap().events[0]
        .event_id
        .clone();
    children::record_test_notice_answer(&db, &mut claimed, false).await;
    assert!(
        children::save_main_delegation_session(&db, &mut claimed)
            .await
            .is_err()
    );
    assert!(
        inbox::Entity::find()
            .filter(inbox::Column::EventId.eq(&event_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .model_notified_at_ms
            .is_none()
    );
    claimed = original;
    children::record_test_notice_answer(&db, &mut claimed, true).await;
    children::save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    let event = inbox::Entity::find()
        .filter(inbox::Column::EventId.eq(&event_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(event.model_notified_at_ms.is_some());
    assert!(event.model_observed_at_ms.is_none() && event.interpreted_at_ms.is_none());
    let answer = claimed.conversation.last().unwrap().text.clone();
    claimed.ready_subagent_notification = None;
    claimed.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    children::save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    let active = occurrence(&db, &claimed.conversation_id).await;
    let ended = schedule
        .finish_answered_fresh_task(
            super::super::FreshTaskLease {
                owner: 1,
                run_id: &claimed.conversation_id,
                node_id: "children-node",
                run_epoch: active.lease_epoch,
                session_token: claimed.lease_token,
            },
            &answer,
        )
        .await
        .unwrap();
    assert_eq!(ended.status, "succeeded");
    assert_eq!(ended.attempt, 1);
    assert!(ended.failure_accounted && ended.finished_at.is_some());
    assert!(super::tests::claim_on(&db, &claimed).await.is_err());
    assert_eq!(
        cost::Entity::find()
            .filter(cost::Column::OperationKind.eq("model"))
            .count(&db)
            .await
            .unwrap(),
        1
    );
    assert!(
        schedule
            .read(1, &ended.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .is_none()
    );
}

#[tokio::test]
async fn retryable_capacity_failure_keeps_the_exact_notice_without_marking_it_delivered_or_resetting_quota()
 {
    let (db, schedule, _, parent, task_id) = children::scheduled_test_answer_fixture().await;
    children::complete_scheduled_test_child(&db, &task_id).await;
    let mut claimed = super::tests::claim_on(&db, &parent).await.unwrap();
    let original = occurrence(&db, &claimed.conversation_id).await;
    let notice = claimed
        .ready_subagent_notification
        .as_ref()
        .unwrap()
        .clone();
    let error = AgentError {
        kind: AgentErrorKind::ModelUnavailable,
        message: "shared model capacity is busy".into(),
        retryable: true,
        safe_for_model: false,
        error_code: None,
    };
    claimed
        .ready_subagent_notification
        .as_mut()
        .unwrap()
        .retry_after_ms = Some(chrono::Utc::now().timestamp_millis() + 30_000);
    claimed.finish_turn(TurnState::Failed, chrono::Utc::now().to_rfc3339());
    claimed.terminal_error = Some(error.clone());
    children::save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    let waiting = schedule
        .finish_failed_fresh_task(
            super::super::FreshTaskLease {
                owner: 1,
                run_id: &claimed.conversation_id,
                node_id: "children-node",
                run_epoch: original.lease_epoch,
                session_token: claimed.lease_token,
            },
            &error,
        )
        .await
        .unwrap();
    assert_eq!(waiting.status, "awaiting_children");
    assert_eq!(waiting.started_at, original.started_at);
    assert_eq!(waiting.result_ref, original.result_ref);
    assert!(
        !waiting.failure_accounted
            && waiting.lease_owner.is_none()
            && waiting.lease_deadline.is_none()
    );
    assert!(
        !schedule
            .settle_fresh_children_wait(&claimed.conversation_id)
            .await
            .unwrap()
    );
    assert!(super::tests::claim_on(&db, &claimed).await.is_err());
    claimed
        .ready_subagent_notification
        .as_mut()
        .unwrap()
        .retry_after_ms = Some(0);
    children::save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    let retried = super::tests::claim_on(&db, &claimed).await.unwrap();
    assert_eq!(
        retried
            .ready_subagent_notification
            .as_ref()
            .unwrap()
            .message_id,
        notice.message_id
    );
    assert_eq!(
        retried.ready_subagent_notification.as_ref().unwrap().events,
        notice.events
    );
    assert_eq!(retried.current_turn_steps, parent.current_turn_steps);
    assert_eq!(retried.lifetime_steps, parent.lifetime_steps);
    assert!(retried.terminal_error.is_none());
    assert!(
        inbox::Entity::find()
            .filter(inbox::Column::EventId.eq(&notice.events[0].event_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .model_notified_at_ms
            .is_none()
    );
    assert_eq!(
        cost::Entity::find()
            .filter(cost::Column::OperationKind.eq("model"))
            .count(&db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn crashed_notification_claim_preserves_the_same_source_counters_and_event_for_recovery() {
    let (db, schedule, _, parent, task_id) = children::scheduled_test_answer_fixture().await;
    children::complete_scheduled_test_child(&db, &task_id).await;
    let claimed = super::tests::claim_on(&db, &parent).await.unwrap();
    let before = occurrence(&db, &claimed.conversation_id).await;
    let expired = chrono::Utc::now().timestamp_millis() - 1;
    run::Entity::update_many()
        .set(run::ActiveModel {
            lease_deadline: Set(Some(expired)),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(before.id))
        .exec(&db)
        .await
        .unwrap();
    agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            lease_deadline: Set(Some(
                chrono::DateTime::from_timestamp_millis(expired).unwrap(),
            )),
            ..Default::default()
        })
        .filter(agent_session::Column::ConversationId.eq(&claimed.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        schedule
            .recover_action_free_fresh_task(&claimed.conversation_id)
            .await
            .unwrap()
    );
    let after = occurrence(&db, &claimed.conversation_id).await;
    let current = saved(&db, &claimed.conversation_id).await;
    assert_eq!(after.status, "awaiting_children");
    assert_eq!(after.result_ref, before.result_ref);
    assert_eq!(after.started_at, before.started_at);
    assert_eq!(after.attempt, before.attempt);
    assert_eq!(after.lease_epoch, before.lease_epoch);
    assert_eq!(
        current.ready_subagent_notification,
        claimed.ready_subagent_notification
    );
    assert_eq!(current.current_turn_steps, claimed.current_turn_steps);
    assert_eq!(current.lifetime_steps, claimed.lifetime_steps);
    assert_eq!(current.input_revision, claimed.input_revision);
    assert!(current.lease_token > claimed.lease_token);
    let recovered = super::tests::claim_on(&db, &current).await.unwrap();
    assert!(recovered.is_subagent_result_turn());
    assert_result_only_authority(&db, &recovered, &task_id).await;
}

async fn assert_result_only_authority(
    db: &crate::config::connection::DatabaseConnection,
    recovered: &PersistedAgentSession,
    task_id: &str,
) {
    use crate::entity::agent_subagent_run as child;
    use desk_diagnose_core::{chat::ToolCall, subagent::tools};
    let store = children::SubAgentStore::new(db.clone());
    let before = saved(db, &recovered.conversation_id).await;
    let tasks = child::Entity::find().all(db).await.unwrap();
    let events = inbox::Entity::find().all(db).await.unwrap();
    let source = occurrence(db, &recovered.conversation_id).await;
    // Establish that the current holder is valid before testing forged authority.
    let txn = db.begin().await.unwrap();
    children::parent_planning_on(&txn, recovered, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    for forge_origin in [false, true] {
        let mut forged = recovered.clone();
        forged.subagent_result_only = false;
        if forge_origin {
            forged.trigger_origin = TriggerOrigin::User;
        }
        let txn = db.begin().await.unwrap();
        assert!(
            children::parent_planning_on(&txn, &forged, chrono::Utc::now().timestamp_millis())
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    }
    for (name, arguments) in [
        (
            tools::SPAWN,
            serde_json::json!({"name":"forbidden", "task":"Start new work", "acceptance_criteria":["Return evidence"], "required_for_completion":false}),
        ),
        (
            tools::CANCEL,
            serde_json::json!({"task_id":task_id,"expected_input_revision":1,"expected_control_revision":1}),
        ),
        (
            tools::MESSAGE,
            serde_json::json!({"task_id":task_id,"expected_input_revision":1,"expected_control_revision":1,"message":"Change the task"}),
        ),
    ] {
        let call = ToolCall {
            id: format!("raw-{name}"),
            name: name.into(),
            arguments_json: arguments.to_string(),
        };
        assert!(tools::parse(recovered, &call).is_err());
        let mut forged = recovered.clone();
        forged.subagent_result_only = false;
        forged.ready_subagent_notification = None;
        let operation = tools::parse(&forged, &call).unwrap();
        assert!(
            store
                .execute_main_tool(&mut recovered.clone(), &call, operation, "forbidden-result")
                .await
                .is_err()
        );
    }
    assert_eq!(saved(db, &recovered.conversation_id).await, before);
    assert_eq!(child::Entity::find().all(db).await.unwrap(), tasks);
    assert_eq!(inbox::Entity::find().all(db).await.unwrap(), events);
    assert_eq!(occurrence(db, &recovered.conversation_id).await, source);
}

async fn restart_snapshot(db: &crate::config::connection::DatabaseConnection) -> serde_json::Value {
    use crate::entity::{agent_delegation_group as group, agent_task_budget_reservation as quota};
    use sea_orm::QueryOrder;
    serde_json::json!({
        "sessions": agent_session::Entity::find().order_by_asc(agent_session::Column::Id).all(db).await.unwrap(),
        "runs": run::Entity::find().order_by_asc(run::Column::Id).all(db).await.unwrap(),
        "groups": group::Entity::find().order_by_asc(group::Column::Id).all(db).await.unwrap(),
        "quota": quota::Entity::find().order_by_asc(quota::Column::Id).all(db).await.unwrap(),
        "inbox": inbox::Entity::find().order_by_asc(inbox::Column::Id).all(db).await.unwrap(),
    })
}

#[tokio::test]
async fn scheduled_optional_child_continues_in_original_occurrence_after_process_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("scheduled.sqlite");
    let (db, schedule, store, parent, task) = children::scheduled_test_answer_fixture_at(Some(
        &format!("sqlite://{}?mode=rwc", file.display()),
    ))
    .await;
    let expected = dir.path().join("expected.json");
    std::fs::write(
        &expected,
        serde_json::to_vec(&serde_json::json!({
            "snapshot":restart_snapshot(&db).await,
            "root":parent.conversation_id,
            "task":task,
        }))
        .unwrap(),
    )
    .unwrap();
    drop(schedule);
    drop(store);
    db.close().await.unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(
            concat!(module_path!(), "::scheduled_reopen_worker")
                .split_once("::")
                .unwrap()
                .1,
        )
        .arg("--ignored")
        .arg("--nocapture")
        .env("LRD_SCHEDULED_REOPEN_DB", &file)
        .env("LRD_SCHEDULED_REOPEN_EXPECTED", &expected)
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    if !output.status.success() {
        eprintln!("Retained failed fixture: {}", dir.keep().display());
    }
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[tokio::test]
#[ignore = "launched only by scheduled process reopen test"]
async fn scheduled_reopen_worker() {
    let file = std::env::var("LRD_SCHEDULED_REOPEN_DB").unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var("LRD_SCHEDULED_REOPEN_EXPECTED").unwrap()).unwrap(),
    )
    .unwrap();
    let db = crate::config::test_support::Database::connect(format!("sqlite://{file}?mode=rw"))
        .await
        .unwrap();
    assert_eq!(restart_snapshot(&db).await, expected["snapshot"]);
    let root = expected["root"].as_str().unwrap();
    let task = expected["task"].as_str().unwrap();
    let parent = saved(&db, root).await;
    let before = occurrence(&db, root).await;
    children::complete_scheduled_test_child(&db, task).await;
    let mut claimed = super::tests::claim_on(&db, &parent).await.unwrap();
    assert!(claimed.is_subagent_result_turn());
    assert_result_only_authority(&db, &claimed, task).await;
    let after = occurrence(&db, root).await;
    assert_eq!(after.run_id, before.run_id);
    assert_eq!(after.attempt, before.attempt);
    assert_eq!(after.started_at, before.started_at);
    let current = restart_snapshot(&db).await;
    assert_eq!(current["groups"].as_array().unwrap().len(), 1);
    for field in [
        "group_id",
        "source_schedule_id",
        "source_occurrence_id",
        "deadline_ms",
        "creation_envelope_json",
    ] {
        assert_eq!(
            current["groups"][0][field], expected["snapshot"]["groups"][0][field],
            "{field}"
        );
    }
    assert_eq!(current["quota"], expected["snapshot"]["quota"]);
    children::record_test_notice_answer(&db, &mut claimed, true).await;
    children::save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    let id = &claimed.ready_subagent_notification.as_ref().unwrap().events[0].event_id;
    let event = inbox::Entity::find()
        .filter(inbox::Column::EventId.eq(id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(event.model_notified_at_ms.is_some());
    assert!(super::tests::claim_on(&db, &claimed).await.is_err());
    db.close().await.unwrap();
}
