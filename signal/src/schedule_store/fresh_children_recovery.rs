//! Recovery of a persisted child pause or owner stop after the run holder is lost.
use crate::agent_subagent_store as children;
use crate::entity::{
    agent_delegation_group as group_row, agent_schedule_run as run, agent_session,
    agent_subagent_run as child_row,
};
use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

#[tokio::test]
async fn saved_wait_recovers_after_the_original_process_dies_without_another_input_or_occurrence() {
    let (db, schedule, _, parent, _) = children::scheduled_test_wait_fixture().await;
    let before = run::Entity::find()
        .filter(run::Column::RunId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let deadline = chrono::Utc::now().timestamp_millis() - 1;
    run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("running".into()),
            lease_owner: Set(Some("lost-original-node".into())),
            lease_deadline: Set(Some(deadline)),
            result_ref: Set(None),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(before.id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        schedule
            .recover_action_free_fresh_task(&parent.conversation_id)
            .await
            .unwrap()
    );
    assert!(
        !schedule
            .recover_action_free_fresh_task(&parent.conversation_id)
            .await
            .unwrap()
    );
    let after = run::Entity::find_by_id(before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "awaiting_children");
    assert_eq!(after.result_ref, before.result_ref);
    assert_eq!(after.started_at, before.started_at);
    assert_eq!(after.attempt, before.attempt);
    assert_eq!(after.lease_epoch, before.lease_epoch);
    assert!(after.lease_owner.is_none() && after.lease_deadline.is_none());
    let current = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let saved = PersistedAgentSession::decode_json(&current.state_json).unwrap();
    assert_eq!(saved.lease_token, parent.lease_token + 1);
    assert_eq!(saved.input_revision, parent.input_revision);
    assert_eq!(saved.control_revision, parent.control_revision);
    assert_eq!(saved.subagent_wait, parent.subagent_wait);
    assert_eq!(saved.conversation, parent.conversation);
    assert_eq!(saved.current_turn_steps, parent.current_turn_steps);
    assert_eq!(saved.current_turn_tokens, parent.current_turn_tokens);
    assert_eq!(saved.lifetime_steps, parent.lifetime_steps);
    assert_eq!(saved.lifetime_tokens, parent.lifetime_tokens);
    assert!(current.lease_deadline.is_none());
    assert_eq!(
        schedule
            .read(1, &before.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(parent.conversation_id.as_str())
    );
}

#[tokio::test]
async fn stopped_parent_with_a_lost_run_holder_preserves_finite_children_until_terminal() {
    let (db, schedule, store, parent, task_id) = children::scheduled_test_wait_fixture().await;
    store
        .stop_for_owner(
            &parent.conversation_id,
            &parent.actor_id,
            &parent.device_id,
            &AiAssistantStopControl {
                client_request_id: "stop-lost-holder".into(),
                expected_input_revision: parent.input_revision,
                expected_control_revision: parent.control_revision,
                subagent_choice: Some(SubAgentStopChoice::MainOnly),
            },
        )
        .await
        .unwrap();
    let before = run::Entity::find()
        .filter(run::Column::RunId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("running".into()),
            lease_owner: Set(Some("lost-original-node".into())),
            lease_deadline: Set(Some(chrono::Utc::now().timestamp_millis() - 1)),
            result_ref: Set(None),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(before.id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        schedule
            .recover_action_free_fresh_task(&parent.conversation_id)
            .await
            .unwrap()
    );
    let current = run::Entity::find_by_id(before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.status, "awaiting_children");
    assert!(
        current
            .result_ref
            .as_deref()
            .unwrap()
            .starts_with("stopped-children:")
    );
    assert!(current.cancel_requested_at.is_none());
    assert!(
        !schedule
            .settle_fresh_children_wait(&parent.conversation_id)
            .await
            .unwrap()
    );
    let child = child_row::Entity::find()
        .filter(child_row::Column::TaskId.eq(&task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.state, "queued");
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&child.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(group.source_admission, "open");
    assert!(!group.parent_active);
    children::complete_scheduled_test_child(&db, &task_id).await;
    assert!(
        schedule
            .settle_fresh_children_wait(&parent.conversation_id)
            .await
            .unwrap()
    );
    assert_eq!(
        run::Entity::find_by_id(before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
}
