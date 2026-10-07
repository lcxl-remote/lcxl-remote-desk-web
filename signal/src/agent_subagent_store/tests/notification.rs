use super::*;
use crate::entity::agent_subagent_inbox as inbox;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ModelTurn, StopReason},
    model_egress::ModelEgressPolicy,
    seam::ClaimTurnParams,
    session::{TriggerOrigin, TurnState},
    subagent::runtime::RuntimeTurn,
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

fn params(parent: &PersistedAgentSession) -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: parent.conversation_id.clone(),
        actor_id: parent.actor_id.clone(),
        device_id: parent.device_id.clone(),
        policy_revision: 2,
        current_pdp_scope: scope(),
        turn_id: "notice-turn".into(),
        request_id: Some("notice-request".into()),
        connection_id: None,
        trigger_origin: TriggerOrigin::SubAgentCompletion,
        now: chrono::Utc::now().to_rfc3339(),
    }
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
async fn completion_without_explicit_wait_claims_one_real_server_notice_and_requires_provider_evidence()
 {
    let db = database().await;
    let (_, task) = completed_without_wait(&db).await;
    let store = SubAgentStore::new(db.clone());
    let RuntimeTurn::ParentCompletion { session, .. } = store
        .parent_runtime_candidate("root", "1", "1")
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("original user source");
    };
    assert!(session.subagent_wait.is_none());
    assert!(session.ready_subagent_wait.is_none());
    let notification = session.ready_subagent_notification.as_ref().unwrap();
    notification.validate().unwrap();
    assert_eq!(notification.events[0].task_id, task);
    let message = session
        .conversation
        .iter()
        .find(|message| message.message_id == notification.message_id)
        .unwrap();
    assert_eq!(message.role, ChatRole::SystemEvent);
    assert!(message.tool_calls.is_empty());
    assert!(message.tool_call_id.is_none());
    assert!(
        session
            .context_protection_set()
            .protected_message_ids
            .contains(&message.message_id)
    );
    let again = store
        .parent_runtime_candidate("root", "1", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        again.session().ready_subagent_notification,
        session.ready_subagent_notification
    );
    let mut wrong = super::creation::destination();
    if let desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_revision,
        ..
    } = &mut wrong
    {
        *connection_revision += 1;
    }
    assert!(
        store
            .claim_parent_completion(&params(&session), &wrong)
            .await
            .unwrap()
            .is_none()
    );
    let mut claimed = store
        .claim_parent_completion(&params(&session), &super::creation::destination())
        .await
        .unwrap()
        .unwrap()
        .session;
    assert!(!claimed.trigger_origin.allows_new_mutation());
    assert!(!claimed.trigger_origin.allows_delegated_review());
    assert!(
        store
            .claim_parent_completion(&params(&session), &super::creation::destination())
            .await
            .unwrap()
            .is_none()
    );
    let current = claimed.clone();
    answer(&db, &mut claimed, false).await;
    assert!(
        save_main_delegation_session(&db, &mut claimed)
            .await
            .is_err()
    );
    let event = inbox::Entity::find()
        .filter(inbox::Column::TaskId.eq(&task))
        .filter(inbox::Column::EventKind.eq("completed"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event.notification_attempted_turn_id.as_deref(),
        Some("notice-turn")
    );
    assert!(event.model_notified_at_ms.is_none());
    assert!(event.model_observed_at_ms.is_none());
    assert!(event.interpreted_at_ms.is_none());
    claimed = current;
    answer(&db, &mut claimed, true).await;
    save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    let delivered = inbox::Entity::find_by_id(event.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(delivered.model_notified_at_ms.is_some());
    assert!(delivered.model_observed_at_ms.is_none());
    assert!(delivered.interpreted_at_ms.is_none());
    assert!(delivered.ui_read_at_ms.is_none());
    claimed.ready_subagent_notification = None;
    claimed.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut claimed)
        .await
        .unwrap();
    assert!(
        store
            .parent_runtime_candidate("root", "1", "1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn notification_and_session_save_roll_back_together_and_retry_wait_does_not_consume_the_inbox()
 {
    let db = database().await;
    completed_without_wait(&db).await;
    let store = SubAgentStore::new(db.clone());
    let runtime = store
        .parent_runtime_candidate("root", "1", "1")
        .await
        .unwrap()
        .unwrap();
    let mut parent = store
        .claim_parent_completion(&params(runtime.session()), &super::creation::destination())
        .await
        .unwrap()
        .unwrap()
        .session;
    let current = parent.clone();
    answer(&db, &mut parent, true).await;
    let txn = db.begin().await.unwrap();
    acknowledge_results_on(
        &txn,
        &current,
        &parent,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .unwrap();
    parent.version += 100;
    assert!(
        write_child_session_on(&txn, &parent, chrono::Utc::now())
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert!(
        inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.model_notified_at_ms.is_none())
    );
    parent = current;
    parent
        .ready_subagent_notification
        .as_mut()
        .unwrap()
        .retry_after_ms = Some(chrono::Utc::now().timestamp_millis() + 30_000);
    parent.finish_turn(TurnState::Failed, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert!(
        store
            .parent_runtime_candidate("root", "1", "1")
            .await
            .unwrap()
            .is_none()
    );
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
