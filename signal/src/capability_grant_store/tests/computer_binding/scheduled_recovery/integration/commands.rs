use super::*;
use crate::agent_exec_store::SignalAgentExecStore;
use desk_diagnose_core::{
    action_result::ActionResultOrigin,
    chat::{ToolCall, ToolCallRef},
};

async fn fixture(db: DatabaseConnection) -> (ScheduleStore, String, String) {
    insert_session(&db, 1, 1).await;
    let row = agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.actor_id = "1".into();
    session.version = row.version;
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session
        .begin_turn(
            "turn-1",
            None,
            None,
            7,
            session.scope_snapshot.clone(),
            Utc::now().to_rfc3339(),
        )
        .unwrap();
    let mut call = ToolCall {
        id: "original-command".into(),
        name: desk_diagnose_core::command_confirmation::COMMAND_TOOL.into(),
        arguments_json: r#"{"schema_version":1,"shell":"bash","command":"pwd","timeout_ms":60000}"#
            .into(),
    };
    call.arguments_json =
        desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).unwrap(),
        )
        .unwrap();
    let destination = DestinationIdentity::Model {
        connection_id: "model-1".into(),
        connection_revision: 1,
        model_id: "test".into(),
        profile_revision: 1,
    };
    let user = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "user-1".into(),
        "Show the working directory".into(),
        destination.clone(),
    )
    .unwrap();
    let mut proposal = ChatMessage::assistant_tool_calls(
        "command-proposal",
        "Run the approved command",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    );
    proposal.turn_id = Some("turn-1".into());
    proposal.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            user.data_envelope.as_ref(),
            &call.id,
            &proposal.text,
            "test_model_output",
        )
        .unwrap();
    session.conversation.extend([user, proposal]);
    let mut row: agent_session::ActiveModel = row.into();
    row.actor_id = Set("1".into());
    row.lease_token = Set(session.lease_token as i64);
    row.state_json = Set(session.encode_json_for_storage().unwrap());
    row.update(&db).await.unwrap();
    claim_original(&db, &mut session).await;
    let exec = SignalAgentExecStore::new(db.clone());
    // Separate table IDs deliberately differ; a previous known command must not
    // block the current occurrence or be mistaken for its capability work ID.
    exec.create(
        "old-command",
        "old-generation",
        "run-1",
        "old-call",
        "old-host",
        Utc::now(),
    )
    .await
    .unwrap();
    exec.mark_unsent("old-generation").await.unwrap();
    let registry = desk_diagnose_core::device_assistant::device_assistant_provider_registry();
    let mut origin = ActionResultOrigin::capture(&registry, &session, &call).unwrap();
    let now = Utc::now().timestamp_millis() as u64;
    let context = desk_diagnose_core::command_completion::CommandCompletionContext::capture(
        &session,
        destination,
        now,
        60000,
    )
    .unwrap();
    origin.retention.expires_at_unix_ms = Some(context.expires_at_unix_ms);
    origin.command_completion = Some(context);
    let capability = registry.capability_for_tool(&call.name).unwrap();
    let mut permission = grant(1);
    permission.actor_id = "1".into();
    permission.provider_id = origin.provider_id.clone();
    permission.capability_id = capability.wire.capability_id.clone();
    permission.tool_name = call.name.clone();
    permission.effect = CapabilityEffect::ExecuteCommand;
    permission.risk_tier = CapabilityRiskTier::R3;
    permission.use_policy = CapabilityGrantUsePolicy::OneShotExact;
    let digest = format!("{:x}", Sha256::digest(call.arguments_json.as_bytes()));
    permission.canonical_input_digest_sha256 = Some(digest.clone());
    permission.issued_at_unix_ms = now - 1;
    permission.expires_at_unix_ms = now + 300000;
    let store = SignalCapabilityGrantStore::new(db.clone());
    store.issue(&permission).await.unwrap();
    let call_id = stable_id(
        "capability-call",
        &format!(
            "run-1:{}:{}",
            session.current_turn_id.as_deref().unwrap(),
            call.id
        ),
    );
    let request = || {
        let mut input = request(
            &call_id,
            &call.arguments_json,
            &digest,
            &permission.resource_scope,
            &permission.operation_scope,
            1,
        );
        input.turn_id = session.current_turn_id.as_deref().unwrap();
        input.call.actor_id = "1";
        input.call.provider_id = &permission.provider_id;
        input.call.capability_id = &permission.capability_id;
        input.call.tool_name = &call.name;
        input.call.effect = permission.effect;
        input.call.risk_tier = permission.risk_tier;
        input.call.now_unix_ms = now;
        input
    };
    store.prepare(request()).await.unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } =
        store.record_dispatch_intent(request()).await.unwrap()
    else {
        panic!("missing intent")
    };
    store
        .claim_command_dispatch(
            &dispatch_id,
            now,
            "host",
            "run-1",
            &call.id,
            60000,
            &origin,
            None,
        )
        .await
        .unwrap();
    (
        ScheduleStore::new(db),
        session.current_request_id.unwrap(),
        dispatch_id,
    )
}

#[tokio::test]
async fn scheduled_command_recovery_uses_original_exec_identity_and_atomic_receipts() {
    for mode in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let db = file_db(&dir.path().join("command.db")).await;
        let (store, run_id, generation) = fixture(db.clone()).await;
        let exec = SignalAgentExecStore::new(db.clone());
        let native = desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed {
            outcome: desk_agent_protocol::AgentOutcome::Err(desk_agent_protocol::AgentError {
                kind: desk_agent_protocol::AgentErrorKind::Internal,
                message: "original command failed".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        };
        if mode == 1 {
            exec.mark_running(&generation).await.unwrap();
        }
        if mode >= 2 {
            exec.finalize("host", &generation, &native).await.unwrap();
        }
        if mode == 3 {
            let command = exec.find_by_generation(&generation).await.unwrap().unwrap();
            let (output, receipt) = exec.command_result(&command).await.unwrap().unwrap();
            let row = agent_session::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            let mut saved = PersistedAgentSession::decode_json(&row.state_json).unwrap();
            let mut result = ChatMessage::tool_result(
                "foreground-result",
                &command.tool_call_id,
                output.content,
            );
            result.turn_id = saved.current_turn_id.clone();
            result.data_envelope = Some(receipt.envelope);
            saved.conversation.push(result);
            crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
                .save(&mut saved)
                .await
                .unwrap();
        }
        let original = agent_schedule_run::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut expired: agent_schedule_run::ActiveModel = original.clone().into();
        expired.lease_deadline = Set(Some(Utc::now().timestamp_millis() - 1));
        if mode == 0 {
            expired.cancel_requested_at = Set(Some(Utc::now().timestamp_millis()));
        }
        expired.update(&db).await.unwrap();
        let mut expired: agent_session::ActiveModel = agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
        expired.lease_deadline = Set(None);
        expired.update(&db).await.unwrap();
        let before = agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let command = exec.find_by_generation(&generation).await.unwrap().unwrap();
        assert_ne!(command.id, outbox.work_id);
        let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
        let now = Utc::now().to_rfc3339();
        assert_eq!(
            sessions
                .deliver_work_completion_with_envelope(
                    "run-1",
                    command.id,
                    desk_diagnose_core::session::WorkKind::AgentExec,
                    &command.event_id,
                    &generation,
                    &command.tool_call_id,
                    &command.exec_request_id,
                    "not delivered outside schedule settlement",
                    None,
                    &now,
                )
                .await
                .unwrap(),
            crate::agent_session_store::EventAppend::Busy
        );
        assert_eq!(
            sessions
                .mark_execution_unknown("run-1", &generation, &command.tool_call_id, &now)
                .await
                .unwrap(),
            crate::agent_session_store::EventAppend::Busy
        );
        assert_eq!(
            agent_session::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        if mode == 2 {
            let mut changed = PersistedAgentSession::decode_json(&before.state_json).unwrap();
            let proposal = changed
                .conversation
                .iter_mut()
                .find(|message| !message.tool_calls.is_empty())
                .unwrap();
            let call = &mut proposal.tool_calls[0];
            let mut arguments: serde_json::Value =
                serde_json::from_str(&call.arguments_json).unwrap();
            arguments["command"] = serde_json::json!("hostname");
            call.arguments_json =
                desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
                    &call.name, arguments,
                )
                .unwrap();
            let mut tampered: agent_session::ActiveModel = before.clone().into();
            tampered.state_json = Set(changed.encode_json_for_storage().unwrap());
            tampered.update(&db).await.unwrap();
            assert!(
                store
                    .recover_committed_continuation(1, &run_id)
                    .await
                    .is_err()
            );
            assert_eq!(
                agent_capability_dispatch_outbox::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                outbox
            );
            assert_eq!(
                exec.find_by_generation(&generation)
                    .await
                    .unwrap()
                    .unwrap()
                    .delivery_state,
                crate::agent_exec_store::DELIVERY_PENDING
            );
            let restored: agent_session::ActiveModel = before.clone().into();
            restored.reset_all().update(&db).await.unwrap();
            let task = store.read(1, &original.schedule_id).await.unwrap();
            let mut overflow: agent_schedule::ActiveModel = task.clone().into();
            overflow.revision = Set(i64::MAX);
            overflow.update(&db).await.unwrap();
            assert!(
                store
                    .recover_committed_continuation(1, &run_id)
                    .await
                    .is_err()
            );
            assert_eq!(
                agent_session::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                before
            );
            assert_eq!(
                agent_capability_dispatch_outbox::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                outbox
            );
            let restored: agent_schedule::ActiveModel = task.into();
            restored.reset_all().update(&db).await.unwrap();
        }
        let recovered = store
            .recover_committed_continuation(1, &run_id)
            .await
            .unwrap()
            .unwrap();
        let row = agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let projected = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        assert_eq!(row.version, before.version + 1);
        assert!(projected.unclosed_tool_call_ids().is_empty());
        assert_eq!(recovered.attempt, 1);
        assert_eq!(recovered.lease_epoch, original.lease_epoch);
        if mode == 1 {
            assert_eq!(recovered.status, "running");
            assert!(
                matches!(&projected.execution_state, ExecutionState::Executing { action } if action.kind == desk_diagnose_core::session::WorkKind::AgentExec && action.work_id == command.id)
            );
            assert!(
                store
                    .recover_committed_continuation(1, &run_id)
                    .await
                    .is_err()
            );
            assert_eq!(
                agent_session::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                row
            );
            exec.finalize("host", &generation, &native).await.unwrap();
            exec.publish_once().await.unwrap();
            assert_eq!(
                agent_session::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                row
            );
            assert_eq!(
                store
                    .recover_committed_continuation(1, &run_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                "failed"
            );
        } else {
            assert_eq!(
                recovered.status,
                if mode == 0 {
                    "outcome_unknown"
                } else {
                    "failed"
                }
            );
            assert_eq!(
                matches!(
                    projected.execution_state,
                    ExecutionState::OutcomeUnknown { .. }
                ),
                mode == 0
            );
            assert_eq!(
                store
                    .recover_committed_continuation(1, &run_id)
                    .await
                    .unwrap()
                    .unwrap(),
                recovered
            );
            assert_eq!(
                agent_session::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                row
            );
        }
        assert!(
            store
                .read(1, &original.schedule_id)
                .await
                .unwrap()
                .active_run_id
                .is_none()
        );
        if mode != 0 {
            assert_eq!(
                exec.find_by_generation(&generation)
                    .await
                    .unwrap()
                    .unwrap()
                    .delivery_state,
                crate::agent_exec_store::DELIVERY_CONSUMED
            );
            let settled = agent_session::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                sessions
                    .deliver_work_completion_with_envelope(
                        "run-1",
                        command.id,
                        desk_diagnose_core::session::WorkKind::AgentExec,
                        &command.event_id,
                        &generation,
                        &command.tool_call_id,
                        &command.exec_request_id,
                        "stale publisher snapshot",
                        None,
                        &now,
                    )
                    .await
                    .unwrap(),
                crate::agent_session_store::EventAppend::AlreadyPresent
            );
            exec.publish_once().await.unwrap();
            assert_eq!(
                agent_session::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap(),
                settled
            );
        }
        assert_eq!(
            agent_capability_grant::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .remaining_uses,
            0
        );
        assert_eq!(agent_exec_task::Entity::find().count(&db).await.unwrap(), 2);
    }
}
