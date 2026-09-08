use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole},
    session::{AgentSessionSurface, TurnState},
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

async fn fixture() -> (ScheduleStore, rehearsal::Model, agent_session::Model) {
    fixture_with_blocker(false).await
}

async fn fixture_with_blocker(
    blocker: bool,
) -> (ScheduleStore, rehearsal::Model, agent_session::Model) {
    let store = super::super::super::tests::store().await;
    let schema = Schema::new(store.db.get_database_backend());
    for table in [
        schema.create_table_from_entity(rehearsal::Entity),
        schema.create_table_from_entity(agent_session::Entity),
        schema.create_table_from_entity(work_item::Entity),
        schema.create_table_from_entity(agent_exec_task::Entity),
    ] {
        store.db.execute(&table).await.unwrap();
    }
    if blocker {
        let mut draft = super::super::super::tests::draft();
        draft.client_create_key = "earlier-blocker".into();
        let task = store.create_draft(1, &draft, 1000).await.unwrap();
        let reserved = store
            .reserve_rehearsal(1, &task.schedule_id, task.revision, "blocker")
            .await
            .unwrap();
        store
            .claim_rehearsal(1, &reserved.rehearsal_id)
            .await
            .unwrap();
    }
    let task = store
        .create_draft(1, &super::super::super::tests::draft(), 1000)
        .await
        .unwrap();
    let reserved = store
        .reserve_rehearsal(1, &task.schedule_id, task.revision, "run")
        .await
        .unwrap();
    let started = store
        .claim_rehearsal(1, &reserved.rehearsal_id)
        .await
        .unwrap();
    let scope = AgentScope {
        granted: vec![],
        mode: ExecutionMode::SuggestOnly,
        expires_at: None,
        policy_name: None,
    };
    let now = chrono::Utc::now();
    let mut session = PersistedAgentSession::new(
        &started.conversation_id,
        "1",
        &started.target_device_id,
        1,
        scope.clone(),
        now.to_rfc3339(),
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.client_conversation_id = Some(started.client_conversation_id.clone());
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.handled_input_seq = 1;
    session
        .begin_turn(
            "original-turn",
            Some(started.rehearsal_id.clone()),
            None,
            1,
            scope,
            now.to_rfc3339(),
        )
        .unwrap();
    session.conversation.push(
        ChatMessage::text(
            format!("rehearsal:{}:input", started.rehearsal_id),
            ChatRole::User,
            &started.prompt,
        )
        .with_turn_id("original-turn"),
    );
    session.conversation.push(
        ChatMessage::text(
            "answer",
            ChatRole::Assistant,
            "The requested task is complete.",
        )
        .with_turn_id("original-turn"),
    );
    session.finish_turn(TurnState::Idle, now.to_rfc3339());
    let row = agent_session::ActiveModel {
        conversation_id: Set(started.conversation_id.clone()),
        actor_id: Set("1".into()),
        device_id: Set(started.target_device_id.clone()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(session.lease_token as i64),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    (store, started, row)
}

#[tokio::test]
async fn answered_rehearsal_freezes_original_snapshot_without_granting_authority() {
    let (store, started, row) = fixture().await;
    let before = store.read(1, &started.schedule_id).await.unwrap();
    assert!(
        store
            .finish_answered_rehearsal(2, &started.rehearsal_id, "The requested task is complete.")
            .await
            .is_err()
    );
    assert!(
        store
            .finish_answered_rehearsal(1, &started.rehearsal_id, "Invented answer")
            .await
            .is_err()
    );
    let mut overflow: entity::ActiveModel = before.clone().into();
    overflow.revision = Set(i64::MAX);
    overflow.update(&store.db).await.unwrap();
    assert!(
        store
            .finish_answered_rehearsal(1, &started.rehearsal_id, "The requested task is complete.")
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_rehearsal(1, &started.rehearsal_id)
            .await
            .unwrap(),
        started
    );
    assert_eq!(
        agent_session::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap(),
        Some(row.clone())
    );
    let restored: entity::ActiveModel = before.clone().into();
    restored.reset_all().update(&store.db).await.unwrap();
    let completed = store
        .finish_answered_rehearsal(1, &started.rehearsal_id, "The requested task is complete.")
        .await
        .unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.completed_session_version, Some(row.version));
    assert_eq!(
        completed.completed_session_sha256,
        Some(digest(&row.state_json))
    );
    assert_eq!(completed.answer_message_id.as_deref(), Some("answer"));
    assert!(completed.finished_at >= completed.started_at);
    let task = store.read(1, &started.schedule_id).await.unwrap();
    assert_eq!(task.status, "awaiting_authorization");
    assert_eq!(task.revision, before.revision + 1);
    assert!(task.authorization_revision.is_none());
    assert!(task.next_run_at.is_none());
    assert_eq!(
        store
            .finish_answered_rehearsal(1, &started.rehearsal_id, "The requested task is complete.")
            .await
            .unwrap(),
        completed
    );
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
}

#[tokio::test]
async fn a_permission_wait_or_unclosed_call_is_not_a_completed_rehearsal() {
    for waiting in [true, false] {
        let (store, started, row) = fixture().await;
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        if waiting {
            session.terminal_permission_request_id = Some("original-approval".into());
        } else {
            session.conversation.push(
                ChatMessage::assistant_tool_calls(
                    "unclosed",
                    "",
                    vec![
                        desk_diagnose_core::chat::ToolCall {
                            id: "call".into(),
                            name: "inspect_desktop_session".into(),
                            arguments_json: "{}".into(),
                        }
                        .to_ref(),
                    ],
                )
                .with_turn_id("original-turn"),
            );
        }
        let mut changed: agent_session::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.update(&store.db).await.unwrap();
        let task = store.read(1, &started.schedule_id).await.unwrap();
        assert!(
            store
                .finish_answered_rehearsal(
                    1,
                    &started.rehearsal_id,
                    "The requested task is complete."
                )
                .await
                .is_err()
        );
        assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
        assert_eq!(
            store
                .read_rehearsal(1, &started.rehearsal_id)
                .await
                .unwrap(),
            started
        );
    }
}

#[tokio::test]
async fn unresolved_or_manually_disposed_actions_block_answered_completion() {
    for state in [
        "capability_dispatching",
        "capability_outcome_unknown",
        "manual",
    ] {
        let (store, started, _) = fixture().await;
        let now = chrono::Utc::now();
        work_item::ActiveModel {
            kind: Set("agent_exec".into()),
            action_request_id: Set("original-action".into()),
            conversation_id: Set(started.conversation_id.clone()),
            turn_id: Set("original-turn".into()),
            tool_call_id: Set("call".into()),
            actor_id: Set("1".into()),
            target_device_id: Set(started.target_device_id.clone()),
            status: Set(if state == "manual" { "done" } else { state }.into()),
            attempt: Set(1),
            policy_revision: Set(1),
            is_side_effecting: Set(true),
            payload_json: Set("{}".into()),
            payload_schema_version: Set(1),
            draft_hash: Set("d".repeat(64)),
            completion_event_id: Set("completion".into()),
            completion_delivery_state: Set("pending".into()),
            manual_resolved_at: Set((state == "manual").then_some(now)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        let task = store.read(1, &started.schedule_id).await.unwrap();
        assert!(
            store
                .finish_answered_rehearsal(
                    1,
                    &started.rehearsal_id,
                    "The requested task is complete."
                )
                .await
                .is_err(),
            "{state}"
        );
        assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
        assert_eq!(
            store
                .read_rehearsal(1, &started.rehearsal_id)
                .await
                .unwrap(),
            started
        );
    }
}

#[tokio::test]
async fn cancelled_rehearsal_requires_no_tools_and_settles_once() {
    let (store, started, row) = fixture().await;
    let before = store.read(1, &started.schedule_id).await.unwrap();
    assert!(
        store
            .finish_cancelled_rehearsal(1, &started.rehearsal_id)
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), before);
    assert_eq!(
        store
            .read_rehearsal(1, &started.rehearsal_id)
            .await
            .unwrap(),
        started
    );

    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session
        .conversation
        .retain(|message| message.role == ChatRole::User);
    session.finish_turn(TurnState::Cancelled, "now");
    let mut changed: agent_session::ActiveModel = row.into();
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    changed.update(&store.db).await.unwrap();
    assert!(
        store
            .finish_rehearsal_termination_for_session(2, &started.conversation_id)
            .await
            .is_err()
    );
    store
        .finish_rehearsal_termination_for_session(1, &started.conversation_id)
        .await
        .unwrap();
    let result = store
        .finish_cancelled_rehearsal(1, &started.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(result.status, "cancelled");
    assert!(result.finished_at.is_some());
    assert!(result.answer_message_id.is_none());
    let task = store.read(1, &started.schedule_id).await.unwrap();
    assert_eq!(task.status, "draft");
    assert!(task.next_run_at.is_none());
    assert_eq!(
        store
            .finish_cancelled_rehearsal(1, &started.rehearsal_id)
            .await
            .unwrap(),
        result
    );
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
    assert!(
        store
            .claim_rehearsal(1, &started.rehearsal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancelled_rehearsal_does_not_release_durable_action_records() {
    for state in ["dispatched", "unknown", "done", "manual"] {
        let (store, started, row) = fixture().await;
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        session
            .conversation
            .retain(|message| message.role == ChatRole::User);
        session.finish_turn(TurnState::Cancelled, "now");
        let mut changed: agent_session::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.update(&store.db).await.unwrap();
        let now = chrono::Utc::now();
        work_item::ActiveModel {
            kind: Set("agent_exec".into()),
            action_request_id: Set("original-action".into()),
            conversation_id: Set(started.conversation_id.clone()),
            turn_id: Set("original-turn".into()),
            tool_call_id: Set("call".into()),
            actor_id: Set("1".into()),
            target_device_id: Set(started.target_device_id.clone()),
            status: Set(if state == "manual" { "done" } else { state }.into()),
            attempt: Set(1),
            policy_revision: Set(1),
            is_side_effecting: Set(true),
            payload_json: Set("{}".into()),
            payload_schema_version: Set(1),
            draft_hash: Set("d".repeat(64)),
            completion_event_id: Set("completion".into()),
            completion_delivery_state: Set("pending".into()),
            manual_resolved_at: Set((state == "manual").then_some(now)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        let before = store.read(1, &started.schedule_id).await.unwrap();
        assert!(
            store
                .finish_rehearsal_termination_for_session(1, &started.conversation_id)
                .await
                .is_err(),
            "{state}"
        );
        assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), before);
        assert_eq!(
            store
                .read_rehearsal(1, &started.rehearsal_id)
                .await
                .unwrap(),
            started
        );
    }
}

#[tokio::test]
async fn failed_rehearsal_requires_no_tools_and_settles_once() {
    let (store, started, row) = fixture().await;
    let before = store.read(1, &started.schedule_id).await.unwrap();
    assert!(
        store
            .finish_failed_rehearsal(1, &started.rehearsal_id)
            .await
            .is_err()
    );
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), before);
    assert_eq!(
        store
            .read_rehearsal(1, &started.rehearsal_id)
            .await
            .unwrap(),
        started
    );

    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session
        .conversation
        .retain(|message| message.role == ChatRole::User);
    session.finish_turn(TurnState::Failed, "now");
    session.handled_input_seq = 0;
    let mut changed: agent_session::ActiveModel = row.into();
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    changed.update(&store.db).await.unwrap();
    assert!(
        store
            .finish_rehearsal_termination_for_session(2, &started.conversation_id)
            .await
            .is_err()
    );
    store
        .finish_rehearsal_termination_for_session(1, &started.conversation_id)
        .await
        .unwrap();
    let result = store
        .finish_failed_rehearsal(1, &started.rehearsal_id)
        .await
        .unwrap();
    assert_eq!(result.status, "failed");
    assert!(result.finished_at.is_some());
    assert!(result.answer_message_id.is_none());
    let task = store.read(1, &started.schedule_id).await.unwrap();
    assert_eq!(task.status, "draft");
    assert!(task.next_run_at.is_none());
    assert_eq!(
        store
            .finish_failed_rehearsal(1, &started.rehearsal_id)
            .await
            .unwrap(),
        result
    );
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
    assert!(
        store
            .claim_rehearsal(1, &started.rehearsal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn failed_rehearsal_does_not_release_durable_action_records() {
    for state in ["dispatched", "unknown", "done", "manual"] {
        let (store, started, row) = fixture().await;
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        session
            .conversation
            .retain(|message| message.role == ChatRole::User);
        session.finish_turn(TurnState::Failed, "now");
        session.handled_input_seq = 0;
        let mut changed: agent_session::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());
        changed.update(&store.db).await.unwrap();
        let now = chrono::Utc::now();
        work_item::ActiveModel {
            kind: Set("agent_exec".into()),
            action_request_id: Set("original-action".into()),
            conversation_id: Set(started.conversation_id.clone()),
            turn_id: Set("original-turn".into()),
            tool_call_id: Set("call".into()),
            actor_id: Set("1".into()),
            target_device_id: Set(started.target_device_id.clone()),
            status: Set(if state == "manual" { "done" } else { state }.into()),
            attempt: Set(1),
            policy_revision: Set(1),
            is_side_effecting: Set(true),
            payload_json: Set("{}".into()),
            payload_schema_version: Set(1),
            draft_hash: Set("d".repeat(64)),
            completion_event_id: Set("completion".into()),
            completion_delivery_state: Set("pending".into()),
            manual_resolved_at: Set((state == "manual").then_some(now)),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        let before = store.read(1, &started.schedule_id).await.unwrap();
        assert!(
            store
                .finish_rehearsal_termination_for_session(1, &started.conversation_id)
                .await
                .is_err(),
            "{state}"
        );
        assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), before);
        assert_eq!(
            store
                .read_rehearsal(1, &started.rehearsal_id)
                .await
                .unwrap(),
            started
        );
    }
}

#[tokio::test]
async fn recovery_settles_persisted_terminal_sessions_without_replaying_them() {
    for terminal in [
        TurnState::Idle,
        TurnState::Failed,
        TurnState::Cancelled,
        TurnState::Running,
    ] {
        let (store, started, row) = fixture().await;
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        if terminal != TurnState::Idle {
            session
                .conversation
                .retain(|message| message.role == ChatRole::User);
            session.finish_turn(terminal, "now");
            session.handled_input_seq = 0;
        }
        let mut changed: agent_session::ActiveModel = row.into();
        changed.state_json = Set(session.encode_json_for_storage().unwrap());

        let saved = changed.update(&store.db).await.unwrap();
        let restored = ScheduleStore::new(store.db.clone());
        assert!(restored.recover_terminal_rehearsals(0, 0).await.is_err());
        assert!(restored.recover_terminal_rehearsals(0, 33).await.is_err());
        let report = restored.recover_terminal_rehearsals(0, 1).await.unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.next_cursor, Some(started.id));
        let expected = match terminal {
            TurnState::Idle => "completed",
            TurnState::Failed => "failed",
            TurnState::Cancelled => "cancelled",
            _ => "running",
        };
        assert_eq!(report.settled, usize::from(terminal != TurnState::Running));
        assert_eq!(report.deferred, usize::from(terminal == TurnState::Running));
        assert_eq!(
            restored
                .read_rehearsal(1, &started.rehearsal_id)
                .await
                .unwrap()
                .status,
            expected
        );
        assert_eq!(
            agent_session::Entity::find_by_id(saved.id)
                .one(&store.db)
                .await
                .unwrap(),
            Some(saved)
        );
        let next = restored
            .recover_terminal_rehearsals(report.next_cursor.unwrap(), 1)
            .await
            .unwrap();
        assert_eq!(next.scanned, 0);
        assert_eq!(next.next_cursor, None);
        let again = restored.recover_terminal_rehearsals(0, 1).await.unwrap();
        assert_eq!(again.settled, 0);
        assert!(
            restored
                .read(1, &started.schedule_id)
                .await
                .unwrap()
                .next_run_at
                .is_none()
        );
    }
}

#[tokio::test]
async fn recovery_cursor_advances_past_unresolved_rehearsals() {
    let (store, ready, saved) = fixture_with_blocker(true).await;
    let first = store.recover_terminal_rehearsals(0, 1).await.unwrap();
    assert_eq!((first.scanned, first.settled, first.deferred), (1, 0, 1));
    let blocker_id = first.next_cursor.unwrap();
    assert!(blocker_id < ready.id);
    let blocker = rehearsal::Entity::find_by_id(blocker_id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let second = store
        .recover_terminal_rehearsals(blocker_id, 1)
        .await
        .unwrap();
    assert_eq!((second.scanned, second.settled, second.deferred), (1, 1, 0));
    assert_eq!(second.next_cursor, Some(ready.id));
    let end = store
        .recover_terminal_rehearsals(ready.id, 1)
        .await
        .unwrap();
    assert_eq!(end.next_cursor, None);
    assert_eq!(end.scanned, 0);
    let wrapped = store.recover_terminal_rehearsals(0, 32).await.unwrap();
    assert_eq!(
        (wrapped.scanned, wrapped.settled, wrapped.deferred),
        (1, 0, 1)
    );
    assert_eq!(
        rehearsal::Entity::find_by_id(blocker_id)
            .one(&store.db)
            .await
            .unwrap(),
        Some(blocker)
    );
    assert_eq!(
        agent_session::Entity::find_by_id(saved.id)
            .one(&store.db)
            .await
            .unwrap(),
        Some(saved)
    );
    assert_eq!(
        store
            .read_rehearsal(1, &ready.rehearsal_id)
            .await
            .unwrap()
            .status,
        "completed"
    );
}

#[tokio::test]
async fn recovery_surfaces_backend_failure_without_releasing_the_task() {
    let (store, started, _) = fixture().await;
    let task = store.read(1, &started.schedule_id).await.unwrap();
    store
        .db
        .execute(
            &sea_orm::sea_query::Table::drop()
                .table(work_item::Entity)
                .to_owned(),
        )
        .await
        .unwrap();
    assert!(matches!(
        store.recover_terminal_rehearsals(0, 1).await,
        Err(ScheduleStoreError::Backend(_))
    ));
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
    assert_eq!(
        store
            .read_rehearsal(1, &started.rehearsal_id)
            .await
            .unwrap(),
        started
    );
}

#[tokio::test]
async fn publication_evidence_binds_completed_rehearsal_to_exact_task() {
    let (store, started, _) = fixture().await;
    let completed = store
        .finish_answered_rehearsal(1, &started.rehearsal_id, "The requested task is complete.")
        .await
        .unwrap();
    let task = store.read(1, &started.schedule_id).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    let proof =
        ScheduleStore::publication_rehearsal_evidence_on(&txn, &task, &started.rehearsal_id)
            .await
            .unwrap();
    assert_eq!(proof.rehearsal_run_id, started.rehearsal_id);
    assert_eq!(proof.conversation_id, started.conversation_id);
    assert_eq!(Some(proof.finished_at), completed.finished_at);
    assert_eq!(proof.input_revision, 1);
    assert_eq!(proof.evidence_sha256.len(), 64);
    let again =
        ScheduleStore::publication_rehearsal_evidence_on(&txn, &task, &started.rehearsal_id)
            .await
            .unwrap();
    assert_eq!(proof, again);
    let definition = desk_agent_protocol::schedule::contract::TaskContract {
        schema_version: 1,
        schedule_id: task.schedule_id.clone(),
        task_revision: task.task_revision as u64,
        contract_revision: 1,
        target_device_id: task.target_device_id.clone(),
        prompt_sha256: digest(&task.prompt),
        permissions: vec![],
        steps: vec![],
        exception_mode: desk_agent_protocol::schedule::contract::TaskExceptionMode::Deny,
        budget: desk_agent_protocol::schedule::contract::TaskBudget {
            max_runs_per_utc_day: 1,
            max_calls_per_run: 1,
            max_model_tokens_per_run: 1000,
            max_runtime_seconds: 60,
        },
    };
    let validated = desk_diagnose_core::schedule::contract::validate_contract(&definition).unwrap();
    let bound = ScheduleStore::publication_contract_scope_evidence_on(
        &txn,
        &task,
        &validated,
        &started.rehearsal_id,
    )
    .await
    .unwrap();
    assert_ne!(proof.evidence_sha256, bound.evidence_sha256);
    let mut different_definition = definition.clone();
    different_definition.prompt_sha256 = "b".repeat(64);
    assert!(
        ScheduleStore::publication_contract_scope_evidence_on(
            &txn,
            &task,
            &desk_diagnose_core::schedule::contract::validate_contract(&different_definition)
                .unwrap(),
            &started.rehearsal_id
        )
        .await
        .is_err()
    );

    let mut changed_task: entity::ActiveModel = task.clone().into();
    changed_task.revision = Set(task.revision + 1);
    changed_task.update(&txn).await.unwrap();
    assert!(
        ScheduleStore::publication_rehearsal_evidence_on(&txn, &task, &started.rehearsal_id)
            .await
            .is_err()
    );
    let original_task: entity::ActiveModel = task.clone().into();
    original_task.reset_all().update(&txn).await.unwrap();

    for field in ["owner", "task", "revision", "prompt", "device"] {
        let mut different = task.clone();
        match field {
            "owner" => different.owner_user_id = 2,
            "task" => different.schedule_id = "other-task".into(),
            "revision" => different.task_revision += 1,
            "prompt" => different.prompt = "Different task".into(),
            "device" => different.target_device_id = "other-device".into(),
            _ => unreachable!(),
        }
        assert!(
            ScheduleStore::publication_rehearsal_evidence_on(
                &txn,
                &different,
                &started.rehearsal_id
            )
            .await
            .is_err(),
            "{field}"
        );
    }
    let mut changed: rehearsal::ActiveModel = completed.into();
    changed.status = Set("running".into());
    changed.update(&txn).await.unwrap();
    assert!(
        ScheduleStore::publication_rehearsal_evidence_on(&txn, &task, &started.rehearsal_id)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
    assert!(task.authorization_revision.is_none());
    assert!(task.next_run_at.is_none());
}

#[tokio::test]
async fn publication_evidence_rejects_chat_only_tool_completion() {
    let (store, started, row) = fixture().await;
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.conversation.insert(
        1,
        ChatMessage::assistant_tool_calls(
            "proposal",
            "",
            vec![
                desk_diagnose_core::chat::ToolCall {
                    id: "unverified-call".into(),
                    name: "read_system_info".into(),
                    arguments_json: "{}".into(),
                }
                .to_ref(),
            ],
        )
        .with_turn_id("original-turn"),
    );
    session.conversation.insert(
        2,
        ChatMessage::tool_result("receipt", "unverified-call", "done")
            .with_turn_id("original-turn"),
    );
    let mut changed: agent_session::ActiveModel = row.into();
    changed.state_json = Set(session.encode_json_for_storage().unwrap());
    changed.update(&store.db).await.unwrap();
    store
        .finish_answered_rehearsal(1, &started.rehearsal_id, "The requested task is complete.")
        .await
        .unwrap();
    let task = store.read(1, &started.schedule_id).await.unwrap();
    let txn = store.db.begin().await.unwrap();
    assert!(matches!(
        ScheduleStore::publication_rehearsal_evidence_on(&txn, &task, &started.rehearsal_id).await,
        Err(ScheduleStoreError::Conflict)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(store.read(1, &started.schedule_id).await.unwrap(), task);
}
