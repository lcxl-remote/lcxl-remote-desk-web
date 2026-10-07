use super::*;
use desk_diagnose_core::{
    seam::ClaimTurnParams,
    session::{TriggerOrigin, TurnState},
    subagent::{
        tools::{self, Operation},
        wait::WaitMode,
    },
};

pub(super) async fn waiting_parent(db: &DatabaseConnection) -> (PersistedAgentSession, String) {
    let (mut parent, calls) = super::creation::runnable_parent(db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let label = parent
        .conversation
        .iter()
        .find(|message| message.message_id == "assistant-calls")
        .unwrap()
        .data_envelope
        .clone()
        .unwrap();
    for call_id in parent.unclosed_tool_call_ids() {
        let text = "Task status records the dispatched creation; remaining fixture proposals dispatched no device action.";
        let mut result = desk_diagnose_core::chat::ChatMessage::tool_result(
            format!("fixture-{call_id}"),
            &call_id,
            text,
        );
        result.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                Some(&label),
                &call_id,
                text,
                "fixture_control",
            )
            .unwrap();
        parent.conversation.push(result);
    }
    let call = super::main_tools::committed_call(
        db,
        &mut parent,
        "durable-wait",
        tools::WAIT,
        serde_json::json!({"task_ids": [&task.task_id], "mode": "all_terminal"}),
    )
    .await;
    store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::Wait {
                task_ids: vec![task.task_id.clone()],
                mode: WaitMode::AllTerminal,
            },
            "durable-wait-result",
        )
        .await
        .unwrap();
    (parent, task.task_id)
}

fn completion_params(parent: &PersistedAgentSession) -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: parent.conversation_id.clone(),
        actor_id: parent.actor_id.clone(),
        device_id: parent.device_id.clone(),
        policy_revision: 2,
        current_pdp_scope: scope(),
        turn_id: "completion-turn".into(),
        request_id: Some("completion-request".into()),
        connection_id: None,
        trigger_origin: TriggerOrigin::SubAgentCompletion,
        now: chrono::Utc::now().to_rfc3339(),
    }
}

#[tokio::test]
async fn completion_before_parent_release_is_found_after_release_and_claimed_once() {
    let db = database().await;
    let (mut parent, task_id) = waiting_parent(&db).await;
    super::main_tools::complete_child(&db, &task_id).await;
    let store = SubAgentStore::new(db.clone());
    assert_eq!(
        store.resolve_parent_wait("root", "1", "1").await.unwrap(),
        ParentWaitResolution::Busy
    );
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let original_group = parent.delegation_group_id.clone();
    let ParentWaitResolution::Ready(wait) =
        store.resolve_parent_wait("root", "1", "1").await.unwrap()
    else {
        panic!("completion is not lost");
    };
    assert_eq!(wait.tasks[0].task_id, task_id);
    assert_eq!(
        store.resolve_parent_wait("root", "1", "1").await.unwrap(),
        ParentWaitResolution::Ready(wait.clone())
    );
    let wrong_destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "other".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    assert!(
        store
            .claim_parent_completion(&completion_params(&parent), &wrong_destination)
            .await
            .unwrap()
            .is_none()
    );
    let claimed = store
        .claim_parent_completion(&completion_params(&parent), &super::creation::destination())
        .await
        .unwrap()
        .unwrap();
    assert!(claimed.session.agent_role.is_main());
    let ready_message = claimed
        .session
        .conversation
        .iter()
        .find(|message| message.message_id == wait.result_message_id)
        .unwrap()
        .clone();
    // The fixture is Secret and remains ineligible for external export. Check
    // the resolved label's graph shape without granting any egress authority.
    let label = ready_message.data_envelope.as_ref().unwrap();
    assert_eq!(
        label.sensitivity,
        desk_agent_protocol::data_lineage::Sensitivity::Secret
    );
    let audit = desk_diagnose_core::sink_authorizer::SinkProjectionAudit {
        destination: super::creation::destination(),
        envelope_ids: vec![label.envelope_id.clone()],
        digests_sha256: vec![label.digest_sha256.clone()],
        total_bytes: ready_message.text.len(),
    };
    let lineage = desk_diagnose_core::model_egress::ModelInputLineage {
        envelope_id: label.envelope_id.clone(),
        digest_sha256: label.digest_sha256.clone(),
        source_provider_id: label.provenance.source_provider_id.clone(),
        source_tool_name: label.provenance.source_tool_name.clone(),
        source_envelope_ids: label.provenance.source_envelope_ids.clone(),
        public_system_prompt: false,
    };
    desk_diagnose_core::model_egress::validate_model_input_lineage(&audit, &[lineage]).unwrap();
    assert_eq!(
        claimed.session.trigger_origin,
        TriggerOrigin::SubAgentCompletion
    );
    assert!(!claimed.session.trigger_origin.allows_new_mutation());
    assert!(!claimed.session.trigger_origin.allows_delegated_review());
    assert_eq!(claimed.session.delegation_group_id, original_group);
    assert!(claimed.session.ready_subagent_wait.is_some());
    assert!(claimed.session.subagent_wait.is_none());
    assert_eq!(
        claimed.source.owner_requirement.message_id,
        "source-message"
    );
    assert_eq!(
        claimed
            .session
            .conversation
            .iter()
            .filter(|message| message.message_id == wait.result_message_id)
            .count(),
        1
    );
    let result = claimed
        .session
        .conversation
        .iter()
        .find(|message| message.message_id == wait.result_message_id)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.text).unwrap()["status"],
        "ready"
    );
    assert!(
        store
            .claim_parent_completion(&completion_params(&parent), &super::creation::destination())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn progress_and_approval_attention_do_not_resolve_a_wait() {
    let db = database().await;
    let (mut parent, task_id) = waiting_parent(&db).await;
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&task_id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut run = decode_run(&row).unwrap();
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&run.binding.group_id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let group = decode_group(&group).unwrap();
    let now = chrono::Utc::now();
    run.synchronize_dependencies(
        vec![TaskDependency::Approval {
            permission_request_id: "owner-approval".into(),
        }],
        false,
        &now.to_rfc3339(),
    )
    .unwrap();
    replace_run_on(&txn, &row, &run, now.timestamp_millis())
        .await
        .unwrap();
    append_state_event_on(&txn, &group, &run, now.timestamp_millis())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let store = SubAgentStore::new(db.clone());
    assert_eq!(
        store.resolve_parent_wait("root", "1", "1").await.unwrap(),
        ParentWaitResolution::Pending
    );
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let stored = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(stored.subagent_wait, parent.subagent_wait);
    assert!(stored.ready_subagent_wait.is_none());
}

#[tokio::test]
async fn explicit_adjustment_delivers_dependency_changed_instead_of_waiting_for_a_new_task_version()
{
    let db = database().await;
    let (mut parent, task_id) = waiting_parent(&db).await;
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    store
        .control_for_owner(
            "root",
            "1",
            "1",
            &desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentControl {
                client_request_id: "adjust-waited-task".into(),
                task_id,
                expected_input_revision: 1,
                expected_control_revision: 1,
                action:
                    desk_agent_protocol::ai_assistant::subagent::SubAgentControlAction::Adjust {
                        message: "Investigate another symptom".into(),
                    },
            },
        )
        .await
        .unwrap();
    let ParentWaitResolution::Ready(wait) =
        store.resolve_parent_wait("root", "1", "1").await.unwrap()
    else {
        panic!("changed dependency resolves the original wait");
    };
    assert_eq!(wait.tasks[0].input_revision, 1);
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let stored = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let result = stored
        .conversation
        .iter()
        .find(|message| message.message_id == wait.result_message_id)
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(payload["status"], "dependency_changed");
    assert_eq!(payload["tasks"][0]["input_revision"], 2);
}

#[tokio::test]
async fn stopping_only_the_parent_blocks_result_claims_without_closing_existing_children() {
    let db = database().await;
    let (mut parent, task_id) = waiting_parent(&db).await;
    let txn = db.begin().await.unwrap();
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(parent.delegation_group_id.clone().unwrap()))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut group = decode_group(&source).unwrap();
    group.stop_parent().unwrap();
    replace_group_on(&txn, &source, &group, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    parent.finish_turn(TurnState::Cancelled, chrono::Utc::now().to_rfc3339());
    write_child_session_on(&txn, &parent, chrono::Utc::now())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let store = SubAgentStore::new(db.clone());
    assert_eq!(
        store.resolve_parent_wait("root", "1", "1").await.unwrap(),
        ParentWaitResolution::Closed
    );
    assert!(
        store
            .claim_parent_completion(&completion_params(&parent), &super::creation::destination())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(load(&db, &task_id).await.state, SubAgentState::Queued);
    let run = load(&db, &task_id).await;
    assert!(matches!(
        store
            .claim_child(
                &super::creation::claim_params(&run),
                &task_id,
                run.fence(),
                &super::creation::destination()
            )
            .await
            .unwrap(),
        SubAgentClaimOutcome::Claimed(_)
    ));
}

#[tokio::test]
async fn failed_root_model_keeps_the_ready_delivery_and_retry_does_not_consume_notifications() {
    let db = database().await;
    let (mut parent, task_id) = waiting_parent(&db).await;
    super::main_tools::complete_child(&db, &task_id).await;
    parent.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    store
        .resolve_parent_wait(&parent.conversation_id, &parent.actor_id, &parent.device_id)
        .await
        .unwrap();
    let mut claimed = store
        .claim_parent_completion(&completion_params(&parent), &super::creation::destination())
        .await
        .unwrap()
        .unwrap();
    let original = claimed.session.ready_subagent_wait.clone().unwrap();
    claimed
        .session
        .finish_turn(TurnState::Failed, chrono::Utc::now().to_rfc3339());
    claimed
        .session
        .ready_subagent_wait
        .as_mut()
        .unwrap()
        .retry_after_ms = Some(chrono::Utc::now().timestamp_millis() + 30_000);
    save_main_delegation_session(&db, &mut claimed.session)
        .await
        .unwrap();
    assert_eq!(
        store
            .resolve_parent_wait(&parent.conversation_id, &parent.actor_id, &parent.device_id)
            .await
            .unwrap(),
        ParentWaitResolution::Pending
    );
    assert!(
        store
            .claim_parent_completion(&completion_params(&parent), &super::creation::destination())
            .await
            .unwrap()
            .is_none()
    );
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut due = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    due.ready_subagent_wait.as_mut().unwrap().retry_after_ms = Some(0);
    due.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(due.encode_json_for_storage().unwrap()),
            version: Set(due.version),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .exec(&db)
        .await
        .unwrap();
    let mut params = completion_params(&due);
    params.turn_id = "retry-completion-turn".into();
    let retried = store
        .claim_parent_completion(&params, &super::creation::destination())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retried
            .session
            .ready_subagent_wait
            .as_ref()
            .unwrap()
            .wait_id,
        original.wait_id
    );
    assert_eq!(retried.session.input_revision, due.input_revision);
    assert_eq!(retried.session.control_revision, due.control_revision);
    assert!(retried.session.lease_token > claimed.session.lease_token);
    assert!(retried.session.observed_subagent_results.is_empty());
    assert!(
        crate::entity::agent_subagent_inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.model_observed_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
}
