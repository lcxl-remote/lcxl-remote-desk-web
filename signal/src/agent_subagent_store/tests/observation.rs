use super::*;
use desk_agent_protocol::data_lineage::DataEnvelope;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ModelTurn, StopReason},
    model_egress::ModelEgressPolicy,
    subagent::{
        reservation::{CallAdmission, DelegationCallKind},
        seam::AcceptedResultObservation,
        tools::{self, Operation},
    },
};

pub(super) async fn read_report(db: &DatabaseConnection) -> PersistedAgentSession {
    read_report_with_task(db, false).await
}

async fn read_report_with_task(
    db: &DatabaseConnection,
    include_task: bool,
) -> PersistedAgentSession {
    read_report_source(db, include_task, false).await
}

async fn read_report_source(
    db: &DatabaseConnection,
    include_task: bool,
    retain_history: bool,
) -> PersistedAgentSession {
    let (mut parent, calls) = super::creation::runnable_parent_with_source(
        db,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::UserContent,
    )
    .await;
    if retain_history && parent.assistant_data.is_none() {
        let txn = db.begin().await.unwrap();

        crate::assistant_data_store::accept_input_on(
            &txn,
            &mut parent,
            "UTC",
            None,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
        let row = session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
            .one(&txn)
            .await
            .unwrap()
            .unwrap();
        session_row::Entity::update_many()
            .col_expr(
                session_row::Column::StateJson,
                sea_orm::sea_query::Expr::value(parent.encode_json_for_storage().unwrap()),
            )
            .filter(session_row::Column::Id.eq(row.id))
            .filter(session_row::Column::Version.eq(row.version))
            .exec(&txn)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    super::main_tools::complete_child(db, &task.task_id).await;
    let call = super::main_tools::committed_call(
        db,
        &mut parent,
        "read-observation",
        tools::RESULT,
        serde_json::json!({"task_id": &task.task_id, "include_task": include_task}),
    )
    .await;
    store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::ReadResult {
                task_id: task.task_id,
                include_task,
            },
            "observed-report",
        )
        .await
        .unwrap();
    parent
}

pub(super) async fn response(
    db: &DatabaseConnection,
    parent: &mut PersistedAgentSession,
    record_provider: bool,
) {
    let input = parent
        .conversation
        .iter()
        .find(|message| message.message_id == "observed-report")
        .unwrap()
        .data_envelope
        .clone()
        .unwrap();
    let now = chrono::Utc::now();
    let policy = ModelEgressPolicy {
        destination: super::creation::destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "delegated-observation-export".into(),
        now_unix_ms: now.timestamp_millis() as u64,
        byte_cap: 1024 * 1024,
        permission_resume: false,
    };
    let text = "The independent investigation is finished.";
    let turn = ModelTurn {
        text: text.into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let output = policy
        .derive_model_output_envelope(&turn, std::slice::from_ref(&input))
        .unwrap();
    if record_provider {
        record_accepted_provider(db, parent, &policy, &input, &output).await;
    }
    let mut message = ChatMessage::text("observation-answer", ChatRole::Assistant, text)
        .with_turn_id(parent.current_turn_id.clone().unwrap());
    message.data_envelope = Some(output);
    parent.conversation.push(message);
    let accepted = AcceptedResultObservation {
        result: parent.observed_subagent_results[0].clone(),
        response_message_id: "observation-answer".into(),
    };
    parent.accepted_subagent_observations.push(accepted.clone());
    parent.interpreted_subagent_results.push(accepted);
}

pub(super) async fn record_accepted_provider(
    db: &DatabaseConnection,
    parent: &PersistedAgentSession,
    policy: &ModelEgressPolicy,
    input: &DataEnvelope,
    output: &DataEnvelope,
) {
    let now = chrono::Utc::now();
    let store = SubAgentStore::new(db.clone());
    let upper = desk_diagnose_core::goal::GoalUsage {
        input_tokens: 100,
        output_tokens: 30,
        model_calls: 1,
        active_time_ms: 100,
        ..Default::default()
    };
    let CallAdmission::Reserved(reservation) = store
        .reserve_runtime_call(
            parent,
            "observed-physical-model",
            DelegationCallKind::Model,
            &"a".repeat(64),
            upper,
            now.timestamp_millis(),
        )
        .await
        .unwrap()
    else {
        panic!("physical model reservation");
    };
    let id = "b".repeat(64);
    store
        .link_model_receipt(&reservation, &id, now.timestamp_millis())
        .await
        .unwrap();
    let audit = desk_diagnose_core::sink_authorizer::SinkProjectionAudit {
        destination: policy.destination.clone(),
        envelope_ids: vec![input.envelope_id.clone()],
        digests_sha256: vec![input.digest_sha256.clone()],
        total_bytes: 1024,
    };
    let receipts = crate::model_egress_store::SignalModelEgressStore::new(db.clone());
    if desk_diagnose_core::ai_assistant::data_access::eligible_origin(parent) {
        use desk_diagnose_core::seam::SessionSeam;
        let message = match parent
            .conversation
            .iter()
            .find(|message| message.data_envelope.as_ref() == Some(input))
        {
            Some(message) => message.clone(),
            None => {
                let projection = store.main_projection_for_turn(parent).await.unwrap();
                assert_eq!(projection.data_envelope.as_ref(), Some(input));
                projection
            }
        };
        let mut request = desk_diagnose_core::seam::ModelRequest::text_only(
            vec![message],
            desk_diagnose_core::prompt::ResponseFormatSpec::None,
        );
        request.context_conversation_id = Some(parent.conversation_id.clone());
        request.assistant_data =
            crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                .assistant_data_request_manifest(parent, &request, now.timestamp_millis() as u64)
                .await
                .unwrap()
                .map(Box::new);
        let authorized = policy.authorize_request(request).unwrap();
        let snapshot = authorized.input_snapshot().unwrap();
        let txn = db.begin().await.unwrap();
        let prepared =
            crate::assistant_data_store::model_gate::prepare_on(&txn, &authorized.request)
                .await
                .unwrap();
        crate::model_egress_store::SignalModelEgressStore::record_snapshot_intent_on(
            &txn,
            id.clone(),
            policy.export_authorization_id.clone(),
            1,
            &snapshot,
        )
        .await
        .unwrap();
        crate::model_egress_store::SignalModelEgressStore::link_context_dispatch_on(
            &txn,
            &id,
            &parent.conversation_id,
        )
        .await
        .unwrap();
        crate::assistant_data_store::model_gate::authorize_on(
            &txn,
            prepared.as_deref(),
            &authorized.request,
            &id,
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
    } else {
        receipts
            .record_dispatch_intent(
                id.clone(),
                policy.export_authorization_id.clone(),
                1,
                &audit,
                std::slice::from_ref(input),
            )
            .await
            .unwrap();
    }
    if !desk_diagnose_core::ai_assistant::data_access::eligible_origin(parent) {
        receipts
            .link_context_dispatch(&id, &parent.conversation_id)
            .await
            .unwrap();
    }
    receipts.mark_succeeded(&id, output).await.unwrap();
}

#[tokio::test]
async fn accepted_model_response_and_interpretation_consume_exact_event_atomically() {
    let db = database().await;
    let mut parent = read_report(&db).await;
    response(&db, &mut parent, true).await;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let events = crate::entity::agent_subagent_inbox::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let complete = events
        .iter()
        .find(|event| event.event_kind == "completed")
        .unwrap();
    assert!(complete.model_observed_at_ms.is_some());
    assert!(complete.interpreted_at_ms.is_some());
    assert!(complete.ui_read_at_ms.is_none());
    assert_eq!(
        complete.observed_message_id.as_deref(),
        Some("observed-report")
    );
    assert!(
        events
            .iter()
            .filter(|event| event.event_kind != "completed")
            .all(|event| event.model_observed_at_ms.is_none())
    );
    let original = complete.clone();
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert_eq!(
        crate::entity::agent_subagent_inbox::Entity::find_by_id(original.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn original_task_read_preserves_current_result_review_and_parent_completion_gate() {
    let db = database().await;
    let mut parent = read_report_with_task(&db, true).await;
    let message = parent
        .conversation
        .iter()
        .find(|message| message.message_id == "observed-report")
        .unwrap();
    let result: desk_diagnose_core::subagent::result::SubAgentModelResult =
        serde_json::from_str(&message.text).unwrap();
    assert!(
        result
            .objective
            .as_ref()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(result.answer.is_some());
    let store = SubAgentStore::new(db.clone());
    assert!(
        !store
            .required_children_complete_for_turn(&parent)
            .await
            .unwrap()
    );
    response(&db, &mut parent, true).await;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert!(
        store
            .required_children_complete_for_turn(&parent)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn labelled_answer_without_original_successful_provider_receipt_does_not_consume_a_result() {
    let db = database().await;
    let mut parent = read_report(&db).await;
    let version = parent.version;
    response(&db, &mut parent, false).await;
    assert!(
        save_main_delegation_session(&db, &mut parent)
            .await
            .is_err()
    );
    assert_eq!(parent.version, version);
    let stored = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        PersistedAgentSession::decode_json(&stored.state_json)
            .unwrap()
            .accepted_subagent_observations
            .is_empty()
    );
    assert!(
        crate::entity::agent_subagent_inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.model_observed_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
}

#[tokio::test]
async fn session_save_failure_rolls_back_both_observation_and_interpretation() {
    let db = database().await;
    let mut parent = read_report(&db).await;
    let current = parent.clone();
    response(&db, &mut parent, true).await;
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
        crate::entity::agent_subagent_inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .iter()
            .all(|event| event.model_observed_at_ms.is_none() && event.interpreted_at_ms.is_none())
    );
    assert_eq!(
        session_row::Entity::find()
            .filter(session_row::Column::ConversationId.eq("root"))
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .version,
        current.version
    );
}

#[tokio::test]
async fn finished_child_requires_current_model_review_before_parent_completion() {
    let db = database().await;
    let mut parent = read_report(&db).await;
    let txn = db.begin().await.unwrap();
    assert!(
        !super::super::main_tools::required_children_complete_on(&txn, &parent)
            .await
            .unwrap()
    );
    txn.rollback().await.unwrap();
    let store = SubAgentStore::new(db.clone());
    assert!(
        !store
            .required_children_complete_for_turn(&parent)
            .await
            .unwrap()
    );
    response(&db, &mut parent, true).await;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert!(
        store
            .required_children_complete_for_turn(&parent)
            .await
            .unwrap()
    );
    let txn = db.begin().await.unwrap();
    assert!(
        super::super::main_tools::required_children_complete_on(&txn, &parent)
            .await
            .unwrap()
    );
    let mut stale = parent.clone();
    stale.interpreted_subagent_results[0].result.state_revision -= 1;
    assert!(
        !super::super::main_tools::required_children_complete_on(&txn, &stale)
            .await
            .unwrap()
    );
    let mut stale = parent.clone();
    stale.interpreted_subagent_results[0]
        .result
        .parent_input_revision += 1;
    assert!(
        !super::super::main_tools::required_children_complete_on(&txn, &stale)
            .await
            .unwrap()
    );
    let mut stale = parent.clone();
    stale.interpreted_subagent_results[0]
        .result
        .parent_control_revision += 1;
    assert!(
        !super::super::main_tools::required_children_complete_on(&txn, &stale)
            .await
            .unwrap()
    );
    let mut accepted_only = parent.clone();
    accepted_only.interpreted_subagent_results.clear();
    assert!(
        !super::super::main_tools::required_children_complete_on(&txn, &accepted_only)
            .await
            .unwrap()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn native_inventory_parent_observation_is_captured_after_original_result_acknowledgement() {
    let db = database().await;
    let mut parent = read_report_source(&db, false, true).await;
    assert!(parent.assistant_data.is_some());
    // Completion is retained by its original transaction before the parent
    // observes the result or calls another model.
    let completed = crate::entity::agent_assistant_history::Entity::find()
        .filter(crate::entity::agent_assistant_history::Column::Kind.eq("native_status"))
        .all(&db)
        .await
        .unwrap();
    assert!(completed.iter().any(|row| {
        let proof = serde_json::from_str(&row.retention_proof_json).unwrap();
        crate::assistant_data_store::read_native_history(row, &proof).unwrap().unwrap().status_snapshot.unwrap().items
            .iter().any(|item| item.kind == desk_diagnose_core::ai_assistant::native_history::NativeHistoryTaskKind::Subagent && item.state == "completed")
    }));
    assert!(
        completed
            .iter()
            .all(|row| !row.content.contains("Investigation finished"))
    );

    for state in ["queued", "running"] {
        assert!(completed.iter().any(|row| {
            let proof = serde_json::from_str(&row.retention_proof_json).unwrap();
            crate::assistant_data_store::read_native_history(row, &proof).unwrap().unwrap().status_snapshot.unwrap().items
                .iter().any(|item| item.kind == desk_diagnose_core::ai_assistant::native_history::NativeHistoryTaskKind::Subagent && item.state == state)
        }), "original child transition must be retained: {state}");
    }
    parent.trigger_origin = desk_diagnose_core::session::TriggerOrigin::SubAgentCompletion;
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            sea_orm::sea_query::Expr::value(parent.encode_json_for_storage().unwrap()),
        )
        .filter(session_row::Column::Id.eq(row.id))
        .filter(session_row::Column::Version.eq(row.version))
        .exec(&db)
        .await
        .unwrap();
    response(&db, &mut parent, true).await;
    let transcript = parent.conversation.clone();
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert_eq!(parent.conversation, transcript);
    assert!(desk_diagnose_core::ai_assistant::data_access::eligible_origin(&parent));
    let observed = crate::entity::agent_subagent_inbox::Entity::find()
        .filter(crate::entity::agent_subagent_inbox::Column::EventKind.eq("completed"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(observed.model_observed_at_ms.is_some());
    assert!(observed.interpreted_at_ms.is_some());
    let rows = crate::entity::agent_assistant_history::Entity::find()
        .filter(crate::entity::agent_assistant_history::Column::Kind.eq("native_status"))
        .all(&db)
        .await
        .unwrap();
    assert!(!rows.is_empty());
    let latest = rows
        .iter()
        .max_by_key(|row| row.admitted_generation)
        .unwrap();
    let proof = serde_json::from_str(&latest.retention_proof_json).unwrap();
    let snapshot = crate::assistant_data_store::read_native_history(latest, &proof)
        .unwrap()
        .unwrap()
        .status_snapshot
        .unwrap();
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.native_id == observed.task_id && item.state == "completed")
    );
    let before = rows;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    assert_eq!(
        crate::entity::agent_assistant_history::Entity::find()
            .filter(crate::entity::agent_assistant_history::Column::Kind.eq("native_status"))
            .all(&db)
            .await
            .unwrap(),
        before
    );
}
