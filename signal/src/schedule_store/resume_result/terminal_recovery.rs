use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::chat::ChatMessage;
use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema};

async fn fixture(failed: bool) -> (ScheduleStore, run::Model, PersistedAgentSession) {
    let (store, queued, _, _) = super::super::resume_claim::tests::fixture().await;
    let schema = Schema::new(store.db.get_database_backend());
    store
        .db
        .execute(&schema.create_table_from_entity(agent_action_item::Entity))
        .await
        .unwrap();
    store
        .db
        .execute(&schema.create_table_from_entity(agent_exec_task::Entity))
        .await
        .unwrap();
    let claimed = store
        .claim_conversation_resume(super::super::ContinuationClaim {
            owner: 1,
            run_id: &queued.run_id,
            node_id: "original-node",
            lease_seconds: 90,
            policy_revision: 1,
            scope: AgentScope {
                granted: vec![],
                mode: ExecutionMode::SuggestOnly,
                expires_at: None,
                policy_name: None,
            },
        })
        .await
        .unwrap();
    let mut session = claimed.session;
    session.conversation.push(
        ChatMessage::text("answer", ChatRole::Assistant, "durable answer")
            .with_turn_id(claimed.run.turn_id.clone()),
    );
    session.finish_turn(
        if failed {
            TurnState::Failed
        } else {
            TurnState::Idle
        },
        chrono::Utc::now().to_rfc3339(),
    );
    if failed {
        session.terminal_error = Some(AgentError {
            kind: desk_agent_protocol::AgentErrorKind::Internal,
            message: "synthetic committed failure".into(),
            retryable: true,
            safe_for_model: false,
            error_code: None,
        });
    }
    save_session(&store, &session).await;
    (store, claimed.run, session)
}

async fn save_session(store: &ScheduleStore, session: &PersistedAgentSession) {
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut row: agent_session::ActiveModel = row.into();
    row.state_json = Set(session.encode_json_for_storage().unwrap());

    row.lease_deadline = Set(None);
    row.update(&store.db).await.unwrap();
}

async fn expire(store: &ScheduleStore, work: &run::Model) {
    let mut row: run::ActiveModel = work.clone().into();
    row.lease_deadline = Set(Some(store.database_time().await.unwrap() - 1));
    row.update(&store.db).await.unwrap();
}

#[tokio::test]
async fn ordinary_orphan_sweep_does_not_modify_scheduled_session_or_run() {
    let (store, work, mut session) = fixture(false).await;
    session.turn_state = TurnState::Running;
    save_session(&store, &session).await;
    expire(&store, &work).await;
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original_run = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original_task = store.read(1, &work.schedule_id).await.unwrap();
    let ordinary = crate::agent_session_store::SignalAgentSessionStore::new(store.db.clone());
    let now = chrono::Utc::now();
    for stale_origin in [false, true] {
        let mut scanned = row.clone();
        if stale_origin {
            let mut snapshot = session.clone();
            snapshot.trigger_origin = TriggerOrigin::User;
            scanned.state_json = snapshot.encode_json_for_storage().unwrap();
        }
        assert!(!ordinary.settle_lapsed_session(&scanned, now).await.unwrap());
        assert_eq!(
            agent_session::Entity::find_by_id(row.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            row
        );
        assert_eq!(
            run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            original_run
        );
        assert_eq!(
            store.read(1, &work.schedule_id).await.unwrap(),
            original_task
        );
    }
}

#[tokio::test]
async fn expired_terminal_results_are_recovered_once_without_another_attempt() {
    for failed in [false, true] {
        let (store, work, session) = fixture(failed).await;
        assert!(
            store
                .expired_continuation_candidates(0, 32)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .recover_committed_continuation(1, &work.run_id)
                .await
                .is_err()
        );
        assert!(
            store
                .recover_committed_continuation(2, &work.run_id)
                .await
                .is_err()
        );
        expire(&store, &work).await;
        assert_eq!(
            store
                .expired_continuation_candidates(0, 1)
                .await
                .unwrap()
                .len(),
            1
        );
        let report = store.scan_committed_recovery_once(0).await.unwrap();
        assert_eq!(report.recovered, 1);
        let recovered = store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            recovered.status,
            if failed { "failed" } else { "succeeded" }
        );
        assert_eq!(recovered.lease_epoch, work.lease_epoch);
        assert_eq!(recovered.lease_owner, work.lease_owner);
        assert_eq!(recovered.attempt, 1);
        assert!(
            store
                .read(1, &work.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        assert_eq!(
            store.scan_committed_recovery_once(0).await.unwrap().scanned,
            0
        );
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            PersistedAgentSession::decode_json(&row.state_json).unwrap(),
            session
        );
    }
}

#[tokio::test]
async fn old_answers_and_still_running_sessions_are_not_terminal_evidence() {
    let (store, work, mut session) = fixture(false).await;
    expire(&store, &work).await;
    session.conversation.last_mut().unwrap().turn_id = Some("another-turn".into());
    save_session(&store, &session).await;
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .deferred,
        1
    );
    session.conversation.last_mut().unwrap().turn_id = Some(work.turn_id.clone());
    session.turn_state = TurnState::Running;
    session.execution_state = ExecutionState::Interrupted {
        since: chrono::Utc::now().to_rfc3339(),
    };
    save_session(&store, &session).await;
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .deferred,
        1
    );
    session.turn_state = TurnState::Idle;
    session.execution_state = ExecutionState::Interrupted {
        since: chrono::Utc::now().to_rfc3339(),
    };
    save_session(&store, &session).await;
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .deferred,
        1
    );
    assert_eq!(
        store
            .read(1, &work.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        Some(work.run_id.as_str())
    );
}

#[tokio::test]
async fn failed_recovery_transaction_keeps_the_original_slot_and_can_be_retried() {
    let (store, work, _) = fixture(false).await;
    expire(&store, &work).await;
    let task = store.read(1, &work.schedule_id).await.unwrap();
    let mut overflow: entity::ActiveModel = task.clone().into();
    overflow.revision = Set(i64::MAX);
    overflow.update(&store.db).await.unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .is_err()
    );
    let current = run::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert_eq!(current.status, "running");
    assert!(!current.failure_accounted);
    let mut restored: entity::ActiveModel = task.into();
    restored = restored.reset_all();
    restored.update(&store.db).await.unwrap();
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .recovered,
        1
    );
}

#[tokio::test]
async fn uncertain_action_blocks_terminal_recovery_even_with_a_committed_answer() {
    let (store, work, _) = fixture(false).await;
    expire(&store, &work).await;
    let now = chrono::Utc::now();
    let action = agent_exec_task::ActiveModel {
        exec_request_id: Set("uncertain-action".into()),
        execution_generation: Set("generation".into()),
        conversation_id: Set(work.conversation_id.clone()),
        tool_call_id: Set("call".into()),
        target_connection_id: Set("edge".into()),
        status: Set("unknown".into()),
        disposition_json: Set(None),
        result_text: Set(None),
        event_id: Set("completion".into()),
        delivery_state: Set("pending".into()),
        deadline: Set(now),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .is_err()
    );
    let mut action: agent_exec_task::ActiveModel = action.into();
    action.status = Set("done".into());
    action.update(&store.db).await.unwrap();
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .recovered,
        1
    );
}

async fn permission_fixture() -> (ScheduleStore, run::Model, PersistedAgentSession) {
    let (store, work, mut session) = fixture(false).await;
    session
        .conversation
        .retain(|message| message.role != ChatRole::Assistant);
    session.permission_requests.push(serde_json::from_value(serde_json::json!({
        "schema_version":1, "request_id":"crash-permission", "input_revision":session.input_revision,
        "state":"pending", "created_at":"2026-09-06T00:00:00Z", "items":[{
            "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
            "expected_effect":"read_device", "resource_scope":["target:current_device"],
            "operation_scope":["observe"], "suggested_ttl_seconds":120,
            "suggested_max_uses":1, "reason":"Inspect current device"
        }]
    })).unwrap());
    session.terminal_permission_request_id = Some("crash-permission".into());
    save_session(&store, &session).await;
    expire(&store, &work).await;
    (store, work, session)
}

#[tokio::test]
async fn committed_permission_pause_recovers_original_wait_without_new_attempt() {
    for state in [
        PermissionRequestState::Pending,
        PermissionRequestState::Approved,
        PermissionRequestState::PartiallyApproved,
        PermissionRequestState::Denied,
    ] {
        let (store, work, mut session) = permission_fixture().await;
        session.permission_requests[0].state = state;
        save_session(&store, &session).await;
        let before = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let task = store.read(1, &work.schedule_id).await.unwrap();
        assert_eq!(
            store
                .scan_committed_recovery_once(0)
                .await
                .unwrap()
                .recovered,
            1
        );
        let recovered = run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.status, "awaiting_permission");
        assert_eq!(
            recovered.result_ref.as_deref(),
            Some("permission:crash-permission")
        );
        assert_eq!(recovered.attempt, work.attempt);
        assert_eq!(recovered.lease_epoch, work.lease_epoch);
        assert_eq!(recovered.started_at, work.started_at);
        assert!(recovered.lease_deadline.is_none());
        assert!(recovered.finished_at.is_none());
        assert!(!recovered.failure_accounted);
        assert_eq!(
            store.continuation_candidates(0, 32).await.unwrap(),
            vec![recovered]
        );
        let after = store.read(1, &work.schedule_id).await.unwrap();
        assert_eq!(after.active_run_id, task.active_run_id);
        assert_eq!(after.failure_state_json, task.failure_state_json);
        assert_eq!(
            agent_session::Entity::find_by_id(before.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            store.scan_committed_recovery_once(0).await.unwrap().scanned,
            0
        );
    }
}

#[tokio::test]
async fn permission_recovery_requires_exact_current_durable_pause() {
    for invalid in 0..5 {
        let (store, work, mut session) = permission_fixture().await;
        match invalid {
            0 => session.terminal_permission_request_id = None,
            1 => session.terminal_permission_request_id = Some("different-request".into()),
            2 => session.permission_requests[0].state = PermissionRequestState::Withdrawn,
            3 => session.permission_requests[0].state = PermissionRequestState::Replaced,
            4 => {
                session.input_revision += 1;
                session
                    .begin_focus_epoch(session.input_revision, Vec::new())
                    .unwrap();
            }
            _ => unreachable!(),
        }
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
        save_session(&store, &session).await;
        let before = run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let report = store.scan_committed_recovery_once(0).await.unwrap();
        assert_eq!(report.recovered, 0);
        assert_eq!(report.deferred, 1);
        assert_eq!(
            run::Entity::find_by_id(work.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            store
                .read(1, &work.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(work.run_id.as_str())
        );
    }
}

async fn interrupted_fixture(
    cancelled: bool,
) -> (ScheduleStore, run::Model, PersistedAgentSession) {
    let (store, work, mut session) = fixture(false).await;
    session.turn_state = TurnState::Running;
    session
        .conversation
        .retain(|message| message.turn_id.as_deref() != Some(work.turn_id.as_str()));
    save_session(&store, &session).await;
    expire(&store, &work).await;
    if cancelled {
        store.cancel_run(1, &work.run_id).await.unwrap();
    }
    (store, work, session)
}

#[tokio::test]
async fn model_only_interruption_and_cancellation_settle_once_without_reclaim() {
    for cancelled in [false, true] {
        let (store, work, session) = interrupted_fixture(cancelled).await;
        assert_eq!(
            store
                .scan_committed_recovery_once(0)
                .await
                .unwrap()
                .recovered,
            1
        );
        let result = store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.status,
            if cancelled { "cancelled" } else { "failed" }
        );
        assert_eq!(
            result.error_kind.as_deref(),
            Some(if cancelled {
                "cancelled"
            } else {
                "executor_interrupted"
            })
        );
        assert_eq!(result.attempt, 1);
        assert_eq!(result.lease_epoch, work.lease_epoch);
        assert_eq!(result.started_at, work.started_at);
        assert!(result.failure_accounted);
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let after = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert_eq!(after.turn_state, TurnState::Failed);
        assert_eq!(after.lease_token, session.lease_token);
        assert_eq!(after.input_revision, session.input_revision);
        assert_eq!(after.conversation, session.conversation);
        assert_eq!(after.version, session.version + 1);
        assert!(!after.terminal_error.unwrap().retryable);
        assert!(row.lease_deadline.is_none());
        let task = store.read(1, &work.schedule_id).await.unwrap();
        let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
            serde_json::from_str(&task.failure_state_json).unwrap();
        assert_eq!(failures.consecutive_failures, if cancelled { 0 } else { 1 });
        assert!(task.active_run_id.is_none());
        assert_eq!(
            store.scan_committed_recovery_once(0).await.unwrap().scanned,
            0
        );
        assert_eq!(
            agent_session::Entity::find_by_id(row.id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            row
        );
    }
}

#[tokio::test]
async fn interruption_rolls_back_session_and_run_if_task_settlement_fails() {
    let (store, work, _) = interrupted_fixture(false).await;
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original_run = run::Entity::find_by_id(work.id)
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let task = store.read(1, &work.schedule_id).await.unwrap();
    let mut changed: entity::ActiveModel = task.clone().into();
    changed.revision = Set(i64::MAX);
    changed.update(&store.db).await.unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .is_err()
    );
    assert_eq!(
        agent_session::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        row
    );
    assert_eq!(
        run::Entity::find_by_id(work.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        original_run
    );
    let mut restored: entity::ActiveModel = task.into();
    restored.revision = Set(2);
    restored.update(&store.db).await.unwrap();
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .recovered,
        1
    );
}

#[tokio::test]
async fn interruption_preserves_live_lease_and_records_untracked_read_as_unavailable() {
    let (store, work, mut session) = interrupted_fixture(false).await;
    let original = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut live: agent_session::ActiveModel = original.clone().into();
    live.lease_deadline = Set(Some(chrono::Utc::now() + chrono::Duration::minutes(2)));
    live.update(&store.db).await.unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .is_err()
    );
    session.conversation.push(
        ChatMessage::text("tool-proposal", ChatRole::Assistant, "")
            .with_turn_id(work.turn_id.clone()),
    );
    session.conversation.last_mut().unwrap().replay_disposition =
        Some(desk_diagnose_core::replay::ReplayDisposition::Unavailable {
            source_context_key: None,
            reason: desk_diagnose_core::replay::ReplayUnavailableReason::UnsupportedCodec,
        });
    session.conversation.last_mut().unwrap().tool_calls.push(
        desk_diagnose_core::chat::ToolCallRef {
            id: "unresolved-read".into(),
            name: "inspect_desktop_session".into(),
            arguments_json: "{}".into(),
        },
    );
    PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    save_session(&store, &session).await;
    let before = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let task = store.read(1, &work.schedule_id).await.unwrap();
    let mut overflow: entity::ActiveModel = task.clone().into();
    overflow.revision = Set(i64::MAX);
    overflow.update(&store.db).await.unwrap();
    assert!(
        store
            .recover_committed_continuation(1, &work.run_id)
            .await
            .is_err()
    );
    assert_eq!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    let restored: entity::ActiveModel = task.into();
    restored.reset_all().update(&store.db).await.unwrap();
    assert_eq!(
        store
            .scan_committed_recovery_once(0)
            .await
            .unwrap()
            .recovered,
        1
    );
    assert_eq!(
        store
            .read(1, &work.schedule_id)
            .await
            .unwrap()
            .active_run_id
            .as_deref(),
        None
    );
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let saved = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(saved.turn_state, TurnState::Failed);
    assert!(saved.unclosed_tool_call_ids().is_empty());
    let result = saved
        .conversation
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("unresolved-read"))
        .unwrap();
    assert!(result.text.contains("result is unavailable"));
    assert!(!result.text.contains("not executed"));
    let work = store
        .recover_committed_continuation(1, &work.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(work.attempt, 1);
    assert_eq!(
        agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        row
    );
}

#[tokio::test]
async fn untracked_mutations_and_unknown_tools_cannot_be_closed_as_reads() {
    for tool in [
        "execute_confirmed_command",
        "browser_open_page",
        "unregistered_tool",
    ] {
        let (store, work, mut session) = interrupted_fixture(false).await;
        let mut proposal = ChatMessage::text("proposal", ChatRole::Assistant, "")
            .with_turn_id(work.turn_id.clone());
        proposal.replay_disposition =
            Some(desk_diagnose_core::replay::ReplayDisposition::Unavailable {
                source_context_key: None,
                reason: desk_diagnose_core::replay::ReplayUnavailableReason::UnsupportedCodec,
            });
        proposal
            .tool_calls
            .push(desk_diagnose_core::chat::ToolCallRef {
                id: "untracked".into(),
                name: tool.into(),
                arguments_json: "{}".into(),
            });
        session.conversation.push(proposal);
        save_session(&store, &session).await;
        let before = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .recover_committed_continuation(1, &work.run_id)
                .await
                .is_err()
        );
        assert_eq!(
            agent_session::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            store
                .read(1, &work.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .as_deref(),
            Some(work.run_id.as_str())
        );
    }
}

#[tokio::test]
async fn untracked_read_with_a_persisted_result_is_preserved_during_cancellation() {
    let (store, work, mut session) = interrupted_fixture(true).await;
    let unavailable = desk_diagnose_core::replay::ReplayDisposition::Unavailable {
        source_context_key: None,
        reason: desk_diagnose_core::replay::ReplayUnavailableReason::UnsupportedCodec,
    };
    let mut proposal = ChatMessage::text("read-proposal", ChatRole::Assistant, "")
        .with_turn_id(work.turn_id.clone());
    proposal.replay_disposition = Some(unavailable.clone());
    proposal
        .tool_calls
        .push(desk_diagnose_core::chat::ToolCallRef {
            id: "completed-read".into(),
            name: "inspect_desktop_session".into(),
            arguments_json: "{}".into(),
        });
    let mut result = ChatMessage::tool_result(
        "original-read-result",
        "completed-read",
        "original saved observation",
    );
    result.turn_id = Some(work.turn_id.clone());
    result.replay_disposition = Some(unavailable);
    session.conversation.extend([proposal, result]);
    save_session(&store, &session).await;
    let original_messages = session.conversation.clone();
    let recovered = store
        .recover_committed_continuation(1, &work.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.status, "cancelled");
    let row = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let saved = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(saved.conversation, original_messages);
    assert!(saved.unclosed_tool_call_ids().is_empty());
    let task = store.read(1, &work.schedule_id).await.unwrap();
    let failures: desk_diagnose_core::schedule::lifecycle::FailureState =
        serde_json::from_str(&task.failure_state_json).unwrap();
    assert_eq!(failures.consecutive_failures, 0);
}
