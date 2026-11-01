use super::*;
use crate::entity::agent_subagent_inbox as inbox;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ModelTurn, StopReason},
    model_egress::ModelEgressPolicy,
    session::TurnState,
};

pub(super) async fn release_parent(db: &DatabaseConnection, parent: &mut PersistedAgentSession) {
    // The fixture committed multiple spawn proposals; close their protocol slots
    // without inventing execution for the proposals that were never dispatched.
    let label = parent
        .conversation
        .iter()
        .find(|message| message.message_id == "assistant-calls")
        .unwrap()
        .data_envelope
        .clone()
        .unwrap();
    for call in parent.unclosed_tool_call_ids() {
        let text = "This proposal's durable creation facts are available through task status; no further device action was dispatched.";
        let mut message = ChatMessage::tool_result(format!("closed-{call}"), &call, text);
        message.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                Some(&label),
                &call,
                text,
                "closed_proposal",
            )
            .unwrap();
        parent.conversation.push(message);
    }
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(db, parent).await.unwrap();
}

async fn completed_without_wait(db: &DatabaseConnection) -> (PersistedAgentSession, String) {
    let (mut parent, calls) = super::creation::runnable_parent(db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    super::main_tools::complete_child(db, &task.task_id).await;
    assert!(
        store
            .parent_runtime_candidate("root", "1", "1")
            .await
            .unwrap()
            .is_none()
    );
    release_parent(db, &mut parent).await;
    (parent, task.task_id)
}

pub(super) async fn answer(
    db: &DatabaseConnection,
    parent: &mut PersistedAgentSession,
    provider_receipt: bool,
) {
    let notification = parent.ready_subagent_notification.as_ref().unwrap();
    let input = parent
        .conversation
        .iter()
        .find(|message| message.message_id == notification.message_id)
        .unwrap()
        .data_envelope
        .clone()
        .unwrap();
    let policy = ModelEgressPolicy {
        destination: super::creation::destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "notice-export".into(),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: 1024 * 1024,
        permission_resume: false,
    };
    let turn = ModelTurn {
        text: "A subtask finished; its exact report remains available.".into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let output = policy
        .derive_model_output_envelope(&turn, std::slice::from_ref(&input))
        .unwrap();
    if provider_receipt {
        super::observation::record_accepted_provider(db, parent, &policy, &input, &output).await;
    }
    let mut response = ChatMessage::text("notice-answer", ChatRole::Assistant, turn.text)
        .with_turn_id(parent.current_turn_id.clone().unwrap());
    response.data_envelope = Some(output);
    parent.conversation.push(response);
    parent
        .ready_subagent_notification
        .as_mut()
        .unwrap()
        .accepted_response_message_id = Some("notice-answer".into());
}

#[tokio::test]
async fn completion_updates_the_original_projection_without_claiming_a_new_turn() {
    let db = database().await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    super::main_tools::complete_child(&db, &task.task_id).await;
    let before = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let projection = store.main_projection_for_turn(&parent).await.unwrap();
    let payload: serde_json::Value = serde_json::from_str(&projection.text).unwrap();
    assert!(
        payload["runtime_delegation_state"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value["task"]["task_id"] == task.task_id
                && value["task"]["state"] == "completed")
    );
    assert!(
        store
            .parent_runtime_candidate("root", "1", "1")
            .await
            .unwrap()
            .is_none()
    );
    let after = session_row::Entity::find_by_id(before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
    release_parent(&db, &mut parent).await;
    let before = session_row::Entity::find_by_id(before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..3 {
        assert!(
            store
                .parent_runtime_candidate("root", "1", "1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .root_wait_candidates(0, 32)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
    let after = session_row::Entity::find_by_id(before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
    assert!(parent.ready_subagent_notification.is_none());
    assert!(
        inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.model_notified_at_ms.is_none())
    );
}

#[tokio::test]
async fn failed_main_without_an_explicit_wait_is_not_rescheduled_by_child_notifications() {
    let db = database().await;
    let (mut parent, _) = completed_without_wait(&db).await;
    parent.turn_state = TurnState::Failed;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    for _ in 0..3 {
        assert!(
            store
                .parent_runtime_candidate("root", "1", "1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .root_wait_candidates(0, 32)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
    assert!(
        inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.notification_attempted_turn_id.is_none())
    );
}

#[tokio::test]
async fn stopped_main_and_new_owner_input_keep_old_events_without_automatic_revival() {
    let db = database().await;
    let (mut parent, task) = completed_without_wait(&db).await;
    let store = SubAgentStore::new(db.clone());
    desk_diagnose_core::subagent::control::stop_main_session(
        &mut parent,
        &chrono::Utc::now().to_rfc3339(),
    )
    .unwrap();
    // The owner control rotates the lease as well as control revision.
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            lease_token: Set(parent.lease_token as i64),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        store
            .parent_runtime_candidate("root", "1", "1")
            .await
            .unwrap()
            .is_none()
    );
    parent.begin_focus_epoch(2, Vec::new()).unwrap();
    parent.input_revision = 2;
    parent.delegation_group_id = None;
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            sea_orm::sea_query::Expr::value(parent.encode_json_for_storage().unwrap()),
        )
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        store
            .parent_runtime_candidate("root", "1", "1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(load(&db, &task).await.state.is_terminal());
    assert!(
        inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.model_notified_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
}
