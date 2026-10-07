use super::*;
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole},
    session::TurnState,
    subagent::{
        TaskAssessment, TaskFinalReport, seam::ChildAdmission, state::CompletionDisposition,
    },
};

pub(super) async fn child(db: &DatabaseConnection) -> PersistedAgentSession {
    let (parent, calls) = super::creation::runnable_parent(db).await;
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let run = load(db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&run),
            &run.binding.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("one child claim");
    };
    claimed.session
}

pub(super) fn report() -> TaskFinalReport {
    TaskFinalReport {
        assessment: TaskAssessment::Complete,
        summary: "Investigation finished".into(),
        findings: Vec::new(),
        delivered: Vec::new(),
        remaining: Vec::new(),
        evidence_refs: Vec::new(),
        receipt_refs: Vec::new(),
        reason: None,
    }
}

pub(super) async fn append_report(
    db: &DatabaseConnection,
    session: &mut PersistedAgentSession,
    report: &TaskFinalReport,
) {
    let text = report.summary.clone();
    let sources = session
        .conversation
        .iter()
        .filter_map(|message| message.data_envelope.clone())
        .collect::<Vec<_>>();
    let mut answer = ChatMessage::text("answer", ChatRole::Assistant, text.clone())
        .with_turn_id(session.current_turn_id.clone().unwrap());
    answer.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            "answer",
            &text,
            "model-output",
            &sources,
        )
        .unwrap(),
    );
    session.conversation.push(answer);
    save_child_session(db, session).await.unwrap();
}

#[tokio::test]
async fn report_session_terminal_state_and_inbox_commit_together() {
    let db = database().await;
    let mut session = child(&db).await;
    let binding = session.agent_role.binding().unwrap().clone();
    let task = load(&db, &binding.task_id).await;
    let store = SubAgentStore::new(db.clone());
    assert_eq!(
        store.child_admission(&session).await.unwrap(),
        ChildAdmission::Admitted
    );
    assert_eq!(
        store
            .evaluate_answer_for_turn(&session, task.fence(), &report().summary)
            .await
            .unwrap()
            .unwrap(),
        CompletionDisposition::Complete
    );
    append_report(&db, &mut session, &report()).await;
    assert_eq!(
        load(&db, &binding.task_id).await.state,
        SubAgentState::Running
    );
    session.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    assert_eq!(
        store
            .settle_answer_for_turn(&mut session, task.fence(), report().summary)
            .await
            .unwrap()
            .unwrap(),
        CompletionDisposition::Complete
    );
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&binding.task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let complete = decode_run(&row).unwrap();
    assert_eq!(complete.state, SubAgentState::Completed);
    assert_eq!(complete.terminal_report, Some(report()));
    assert!(row.result_envelope_json.is_some());
    let child_row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(child_row.lease_deadline.is_none());
    let stored = PersistedAgentSession::decode_json(&child_row.state_json).unwrap();
    assert_eq!(stored.turn_state, TurnState::Idle);
    assert_eq!(stored.version, session.version);
    let events = crate::entity::agent_subagent_inbox::Entity::find()
        .filter(crate::entity::agent_subagent_inbox::Column::TaskId.eq(&binding.task_id))
        .filter(crate::entity::agent_subagent_inbox::Column::EventKind.eq("completed"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].model_observed_at_ms.is_none());
    assert!(
        store
            .settle_answer_for_turn(&mut session, task.fence(), report().summary)
            .await
            .is_err()
    );
    assert_eq!(load(&db, &binding.task_id).await, complete);
}

#[tokio::test]
async fn prose_references_do_not_mint_receipts_and_idle_without_an_answer_does_not_end_a_child() {
    let db = database().await;
    let mut session = child(&db).await;
    let task_id = session.agent_role.binding().unwrap().task_id.clone();
    let task = load(&db, &task_id).await;
    let store = SubAgentStore::new(db.clone());
    // IDs and status mentioned in prose are data; the runtime never imports them.
    assert_eq!(
        store
            .evaluate_answer_for_turn(
                &session,
                task.fence(),
                "已成功，receipt_refs: [model-invented-receipt]",
            )
            .await
            .unwrap()
            .unwrap(),
        CompletionDisposition::Complete
    );
    assert_eq!(load(&db, &task_id).await, task);
    session.subagent_report_corrections_used = 1;
    save_child_session(&db, &mut session).await.unwrap();
    assert_eq!(load(&db, &task_id).await.state, SubAgentState::Running);
    assert_eq!(load(&db, &task_id).await.report_corrections_used, 1);
    session.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    store
        .settle_turn_for_task(&mut session, None, false)
        .await
        .unwrap();
    let failed = load(&db, &task_id).await;
    assert_eq!(failed.state, SubAgentState::Failed);
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("delegated_turn_has_no_final_answer")
    );
    assert_eq!(session.turn_state, TurnState::Failed);
    assert!(
        store
            .settle_turn_for_task(&mut session, None, true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn committed_source_pause_rejects_old_report_and_old_history() {
    let db = database().await;
    let (parent, _) = super::funding::funded_parent(&db).await;
    let reference = parent
        .conversation
        .iter()
        .flat_map(|message| &message.tool_calls)
        .next()
        .unwrap();
    let call = desk_diagnose_core::chat::ToolCall {
        id: reference.id.clone(),
        name: reference.name.clone(),
        arguments_json: reference.arguments_json.clone(),
    };
    let store = SubAgentStore::new(db.clone());
    let task = store
        .spawn_for_turn(&parent, &call, &super::creation::spawn_request())
        .await
        .unwrap();
    let run = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&run),
            &task.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("goal child claim");
    };
    let mut session = claimed.session;
    let task_id = session.agent_role.binding().unwrap().task_id.clone();
    let txn = db.begin().await.unwrap();
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&task_id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut task = decode_run(&row).unwrap();
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&task.binding.group_id))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut group = decode_group(&source).unwrap();
    let now = chrono::Utc::now();
    let epoch = group.set_source_admission(SourceAdmission::Paused).unwrap();
    replace_group_on(&txn, &source, &group, now.timestamp_millis())
        .await
        .unwrap();
    task.pause_source(epoch, &now.to_rfc3339()).unwrap();
    replace_run_on(&txn, &row, &task, now.timestamp_millis())
        .await
        .unwrap();
    synchronize_control_on(&txn, &task, now.timestamp_millis())
        .await
        .unwrap();
    append_state_event_on(&txn, &group, &task, now.timestamp_millis())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let store = SubAgentStore::new(db.clone());
    assert_eq!(
        store.child_admission(&session).await.unwrap(),
        ChildAdmission::SourcePaused
    );
    assert!(
        store
            .evaluate_answer_for_turn(
                &session,
                desk_diagnose_core::subagent::state::PlanningFence {
                    input_revision: session.input_revision,
                    control_revision: session.control_revision,
                    source_epoch: session.agent_role.binding().unwrap().source_epoch
                },
                &report().summary
            )
            .await
            .is_err()
    );
    assert!(save_child_session(&db, &mut session).await.is_err());
    assert_eq!(load(&db, &task_id).await, task);
}

#[tokio::test]
async fn runtime_projection_restores_task_text_after_history_compaction() {
    let db = database().await;
    let mut session = child(&db).await;
    let task = session.agent_role.binding().unwrap().clone();
    session.conversation.clear();
    save_child_session(&db, &mut session).await.unwrap();
    let projection = SubAgentStore::new(db.clone())
        .child_projection_for_turn(&session)
        .await
        .unwrap();
    let text: serde_json::Value = serde_json::from_str(&projection.text).unwrap();
    assert_eq!(
        text["runtime_delegation_state"]["objective"],
        task.objective
    );
    assert_eq!(
        text["runtime_delegation_state"]["acceptance_criteria"],
        serde_json::to_value(task.acceptance_criteria).unwrap()
    );
    assert_eq!(
        text["runtime_delegation_state"]["source_epoch"],
        task.source_epoch
    );
    assert!(projection.data_envelope.is_some());
    assert!(session.conversation.is_empty());
    assert_eq!(projection.role, ChatRole::SystemEvent);
    assert_eq!(load(&db, &task.task_id).await.state, SubAgentState::Running);
}
