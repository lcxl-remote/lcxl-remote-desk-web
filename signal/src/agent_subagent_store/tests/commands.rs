//! Delegated input, owner approval and command dispatch share the same fence.
mod completion;
use super::*;
use crate::{
    agent_session_store::SignalAgentSessionStore,
    capability_grant_store::{
        DispatchClaimResult, DispatchIntentResult, PrepareCapabilityCall,
        SignalCapabilityGrantStore,
    },
    entity::{agent_action_item, agent_capability_grant, agent_exec_task, agent_grant_reservation},
};
use desk_agent_protocol::{
    RiskLevel,
    ai_assistant::subagent::{AiAssistantSubAgentControl, SubAgentControlAction},
    authz::ExecAdmissionPolicy,
    capability_grant::{CapabilityGrant, CapabilityRiskTier},
    capability_provider::ProductSurface,
};
use desk_diagnose_core::{
    action_result::ActionResultOrigin,
    capability_availability::CapabilityAvailability,
    capability_grant::CapabilityGrantCall,
    chat::{ChatMessage, ToolCall, ToolCallRef},
    command_confirmation::{CommandConfirmation, CommandPolicyContext},
    dynamic_run::*,
    permission_grant::PermissionGrantIssuanceContext,
    seam::{ExecContext, ToolSeam},
    session::TurnState,
};

async fn approved_child() -> (
    DatabaseConnection,
    SubAgentRun,
    PersistedAgentSession,
    ToolCall,
    CapabilityGrant,
) {
    approved_child_with_call(None).await
}

async fn approved_child_with_call(
    proposed: Option<ToolCall>,
) -> (
    DatabaseConnection,
    SubAgentRun,
    PersistedAgentSession,
    ToolCall,
    CapabilityGrant,
) {
    approved_child_with_source(
        proposed,
        super::creation::destination(),
        desk_agent_protocol::data_lineage::Sensitivity::Secret,
    )
    .await
}

async fn approved_child_with_source(
    proposed: Option<ToolCall>,
    destination: desk_agent_protocol::data_lineage::DestinationIdentity,
    sensitivity: desk_agent_protocol::data_lineage::Sensitivity,
) -> (
    DatabaseConnection,
    SubAgentRun,
    PersistedAgentSession,
    ToolCall,
    CapabilityGrant,
) {
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    let schema = Schema::new(db.get_database_backend());
    for mut table in [
        schema.create_table_from_entity(agent_capability_grant::Entity),
        schema.create_table_from_entity(agent_grant_reservation::Entity),
    ] {
        db.execute(table.if_not_exists()).await.unwrap();
    }
    let (parent, calls) =
        super::creation::runnable_parent_with_source(&db, destination.clone(), sensitivity).await;
    let tasks = SubAgentStore::new(db.clone());
    let task = tasks
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let run = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = tasks
        .claim_child(
            &super::creation::claim_params(&run),
            &task.task_id,
            run.fence(),
            &destination,
        )
        .await
        .unwrap()
    else {
        panic!("child claim required")
    };
    let mut child = claimed.session;
    child.policy_revision =
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
    save_child_session(&db, &mut child).await.unwrap();
    let mut call = proposed.unwrap_or_else(|| ToolCall {
        id: "child-command".into(),
        name: "exec_command".into(),
        arguments_json: desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
            "exec_command", serde_json::json!({"schema_version":1,"shell":"bash","command":"sleep 60","timeout_ms":60000}),
        ).unwrap(),
    });
    call.arguments_json =
        desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
            &call.name,
            serde_json::from_str(&call.arguments_json).unwrap(),
        )
        .unwrap();
    let mut proposal = ChatMessage::assistant_tool_calls(
        "child-command-proposal",
        "Run the requested check",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    );
    proposal.turn_id = child.current_turn_id.clone();
    proposal.data_envelope =
        desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
            child.conversation.last().unwrap().data_envelope.as_ref(),
            &call.id,
            &proposal.text,
            "test_model_output",
        )
        .unwrap();
    child.conversation.push(proposal);
    let policy = CommandPolicyContext {
        actor_id: child.actor_id.clone(),
        target_device_id: child.device_id.clone(),
        target_session_id: "host:session-1".into(),
        policy_revision: child.policy_revision,
        admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        execution_mode: ExecutionMode::ConfirmEachAction,
        max_risk: RiskLevel::Critical,
        available_shells: vec!["bash".into()],
        max_runtime_ms: 60000,
        operator_templates: vec![],
        effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
        policy_version: "test:1".into(),
        exec_pty: false,
        exec_pty_elevation: false,
    };
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry()
        .with_command_policy(policy.clone());
    let capability = registry.capability_for_tool(&call.name).unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let confirmation = policy
        .prepare(&call.arguments_json, child.input_revision)
        .unwrap();
    let resources = confirmation.resource_scope().unwrap();
    let request = PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: "paused-child-permission".into(),
        input_revision: child.input_revision,
        state: PermissionRequestState::Pending,
        items: vec![GrantRequestItem {
            command_confirmation: Some(confirmation),
            launch_confirmation: None,
            item_id: "command".into(),
            provider_id: provider.wire.provider_id.clone(),
            tool_name: call.name.clone(),
            expected_effect: capability.wire.effect,
            resource_scope: resources.clone(),
            operation_scope: vec!["exec_command".into()],
            export_destinations: vec![],
            canonical_input_json: Some(call.arguments_json.clone()),
            canonical_input_digest_sha256: Some(format!(
                "{:x}",
                sha2::Sha256::digest(call.arguments_json.as_bytes())
            )),
            suggested_ttl_seconds: 120,
            suggested_max_uses: 1,
            reason: "Run the delegated check".into(),
        }],
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    child.add_permission_request(request.clone()).unwrap();
    child.last_event_seq += 1;
    let event = PermissionRequestedEvent {
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: "child-command-permission".into(),
            run_id: child.conversation_id.clone(),
            event_seq: child.last_event_seq,
            input_revision: child.input_revision,
            kind: AgentRunEventKind::PermissionRequested,
            correlation_id: Some(request.request_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at: request.created_at.clone(),
        },
        request,
    };
    super::paused_permission::publish(&db, &mut child, &event).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    let now = chrono::Utc::now();
    let inventory = [CapabilityAvailability {
        provider_id: provider.wire.provider_id.clone(),
        capability_id: capability.wire.capability_id.clone(),
        tool_name: call.name.clone(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }];
    SignalAgentSessionStore::new(db.clone())
        .decide_permission_request(
            &child.conversation_id,
            &child.actor_id,
            &child.device_id,
            &event.request.request_id,
            vec![PermissionDecisionItem {
                item_id: "command".into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: resources,
                    operation_scope: vec!["exec_command".into()],
                    export_destinations: vec![],
                    ttl_seconds: 120,
                    max_uses: 1,
                },
            }],
            PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: now.timestamp_millis() as u64,
                implicit_fresh_object_refs: &[],
            },
            &now.to_rfc3339(),
        )
        .await
        .unwrap();
    super::paused_permission::assert_claim(&db, &child.conversation_id, true).await;
    let child = super::paused_permission::session(&db, &child.conversation_id).await;
    CommandConfirmation::approved_for_call(&child, &call.arguments_json).unwrap();
    let grant = SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&child.conversation_id, &child.actor_id, &child.device_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    (
        db.clone(),
        load(&db, &task.task_id).await,
        child,
        call,
        grant,
    )
}

use sha2::Digest;

fn prepared<'a>(
    child: &'a PersistedAgentSession,
    call: &'a ToolCall,
    grant: &'a CapabilityGrant,
) -> PrepareCapabilityCall<'a> {
    PrepareCapabilityCall {
        grant_id: &grant.grant_id,
        call_id: "approved-child-command",
        turn_id: child.current_turn_id.as_deref().unwrap(),
        input_revision: child.input_revision,
        input_watermark: child.latest_input_seq,
        generation: 1,
        canonical_input_json: &call.arguments_json,
        call: CapabilityGrantCall {
            actor_id: &child.actor_id,
            run_id: &child.conversation_id,
            input_revision: child.input_revision,
            surface: grant.surface,
            target_device_id: &child.device_id,
            target_session_id: grant.target_session_id.as_deref(),
            provider_id: &grant.provider_id,
            capability_id: &grant.capability_id,
            tool_name: &call.name,
            tool_schema_version: grant.tool_schema_version,
            effect: grant.effect,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: &grant.resource_scope,
            operation_scope: &grant.operation_scope,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: grant.canonical_input_digest_sha256.as_deref().unwrap(),
            byte_count: call.arguments_json.len() as u64,
            item_count: 1,
            policy_revision: child.policy_revision,
            readiness_revision: grant.readiness_revision,
            now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        },
    }
}

#[tokio::test]
async fn newly_created_child_owner_approval_prepares_and_claims_command_once() {
    let (db, _, child, call, grant) = approved_child().await;
    assert_eq!(child.latest_input_seq, 1);
    assert_eq!(grant.remaining_uses, 1);
    let store = SignalCapabilityGrantStore::new(db.clone());
    let mut invalid = prepared(&child, &call, &grant);
    invalid.input_watermark = 0;
    assert!(store.prepare(invalid).await.is_err());
    assert_eq!(
        agent_action_item::Entity::find().count(&db).await.unwrap(),
        0
    );
    store
        .prepare(prepared(&child, &call, &grant))
        .await
        .unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } = store
        .record_dispatch_intent(prepared(&child, &call, &grant))
        .await
        .unwrap()
    else {
        panic!("approved initial input must dispatch")
    };
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let origin = ActionResultOrigin::capture_confirmed_command(
        &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
        &child,
        &call,
        now,
    )
    .unwrap();
    assert!(matches!(
        store
            .claim_command_dispatch(
                &dispatch_id,
                now,
                "host",
                &child.conversation_id,
                &call.id,
                60000,
                &origin,
                None
            )
            .await
            .unwrap(),
        DispatchClaimResult::Claimed(_)
    ));
    assert!(
        store
            .claim_command_dispatch(
                &dispatch_id,
                now,
                "host",
                &child.conversation_id,
                &call.id,
                60000,
                &origin,
                None,
            )
            .await
            .is_err()
    );
    store
        .validate_claimed_dispatch(&dispatch_id, prepared(&child, &call, &grant))
        .await
        .unwrap();
    assert_eq!(agent_exec_task::Entity::find().count(&db).await.unwrap(), 1);
    assert_eq!(
        agent_action_item::Entity::find().count(&db).await.unwrap(),
        1
    );
    let remaining = store
        .list_for_subject(&child.conversation_id, &child.actor_id, &child.device_id)
        .await
        .unwrap();
    assert_eq!(remaining[0].remaining_uses, 0);
}

#[tokio::test]
async fn adjusting_approved_child_before_dispatch_rejects_the_old_input() {
    let (db, run, child, call, grant) = approved_child().await;
    let store = SignalCapabilityGrantStore::new(db.clone());
    store
        .prepare(prepared(&child, &call, &grant))
        .await
        .unwrap();
    SubAgentStore::new(db.clone())
        .control_for_owner(
            "root",
            "1",
            "1",
            &AiAssistantSubAgentControl {
                client_request_id: "adjust-before-dispatch".into(),
                task_id: run.binding.task_id.clone(),
                expected_input_revision: run.binding.input_revision,
                expected_control_revision: run.binding.control_revision,
                action: SubAgentControlAction::Adjust {
                    message: "Inspect the new symptom instead".into(),
                },
            },
        )
        .await
        .unwrap();
    let changed = super::paused_permission::session(&db, &child.conversation_id).await;
    assert_eq!(changed.latest_input_seq, 2);
    assert_eq!(changed.input_revision, 2);
    assert!(matches!(
        store
            .record_dispatch_intent(prepared(&child, &call, &grant))
            .await
            .unwrap(),
        DispatchIntentResult::SupersededBeforeIntent { .. }
    ));
    assert_eq!(agent_exec_task::Entity::find().count(&db).await.unwrap(), 0);
    let row = agent_capability_grant::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.remaining_uses, 1);
}

#[tokio::test]
async fn missing_child_input_fails_before_command_authorization_or_device_send() {
    let (db, _, mut child, call, _) = approved_child().await;
    child.latest_input_seq = 0;
    save_child_session(&db, &mut child).await.unwrap();
    let tools = crate::remote_tool_edge::SignalAiAssistantTools::new(
        db.clone(),
        desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
        std::sync::Arc::new(desk_signal_facade::model::connection::SharedConnectionMap::default()),
        std::sync::Arc::new(crate::remote_tool_edge::SignalRemoteToolPendingStore::default()),
        "host".into(),
        child.device_id.clone(),
        child.actor_id.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
        vec![],
        vec![],
        vec![],
        None,
        None,
        "Delegated check".into(),
        child.conversation_id.clone(),
        child.current_turn_id.clone().unwrap(),
        child.policy_revision,
        1,
        vec!["bash".into()],
        60000,
    );
    let ctx = ExecContext {
        assistant_turn_fence:
            desk_diagnose_core::action_turn_fence::AssistantTurnFence::from_session(&child).unwrap(),
        conversation_id: child.conversation_id.clone(),
        turn_id: child.current_turn_id.clone().unwrap(),
        tool_call_id: call.id.clone(),
        actor_id: child.actor_id.clone(),
        policy_revision: child.policy_revision,
        scope: child.scope_snapshot.clone(),
        connection_id: None,
    };
    let error = tools.confirm_and_exec(&call, &ctx).await.unwrap_err();
    assert_eq!(error.kind, desk_agent_protocol::AgentErrorKind::Internal);
    assert_eq!(
        error.message,
        "command execution requires an accepted run input"
    );
    assert!(!error.retryable && !error.safe_for_model);
    assert_eq!(
        agent_action_item::Entity::find().count(&db).await.unwrap(),
        0
    );
    assert_eq!(agent_exec_task::Entity::find().count(&db).await.unwrap(), 0);
}

#[actix_web::test]
#[ignore = "requires explicit project-model credentials and public network access"]
async fn live_project_model_subagent_command() {
    use desk_diagnose_core::{
        chat::{ChatRole, StopReason},
        model_profile::WireProtocol,
        prompt::ResponseFormatSpec,
        seam::{ModelRequest, ModelSeam, NullTurnSink},
        subagent::{TaskAssessment, report},
    };
    use std::time::{Duration, Instant};
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
        model: Some(std::env::var("LRD_LIVE_DEEPSEEK_MODEL").unwrap()),
        base_url: Some(std::env::var("LRD_LIVE_DEEPSEEK_BASE_URL").unwrap()),
        api_key: Some(std::env::var("LRD_LIVE_DEEPSEEK_API_KEY").unwrap()),
        request_options: serde_json::json!({"thinking":{"type":"enabled"}}),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 1024,
        ..Default::default()
    };
    let seam = crate::model_dial::SignalModelSeam::from_config(&config).unwrap();
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let mut request = ModelRequest::text_only(
        vec![
            ChatMessage::text(
                "system",
                ChatRole::System,
                "This is an isolated transport and authorization test. Propose one exact command tool call. The runtime will obtain the synthetic owner's separate approval before recording a dispatch; you are not executing a device command. Do not claim success or completed work.",
            ),
            ChatMessage::text(
                "requirement",
                ChatRole::User,
                "Propose exec_command once: shell bash, command sleep 60, timeout_ms 60000, noninteractive. No other command or tool is needed.",
            ),
        ],
        ResponseFormatSpec::None,
    );
    request.tools = vec![
        registry
            .capability_for_tool("exec_command")
            .unwrap()
            .tool_spec
            .clone(),
    ];
    let started = Instant::now();
    let first = tokio::time::timeout(
        Duration::from_secs(90),
        seam.call(request.clone(), &mut NullTurnSink),
    )
    .await
    .expect("command proposal timed out")
    .expect("command proposal failed");
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    let [call] = first.tool_calls.as_slice() else {
        panic!("one command proposal required")
    };
    assert_eq!(call.name, "exec_command");
    let input: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap();
    assert_eq!(input["command"], "sleep 60");
    assert_eq!(input["shell"], "bash");
    let evidence_path = std::env::var("LRD_LIVE_SUBAGENT_EVIDENCE_PATH").unwrap();
    std::fs::write(&evidence_path, serde_json::to_string_pretty(&serde_json::json!({
        "scope":"production_oss_real_model_command_proposal_before_isolated_owner_approval",
        "profile":{"model":config.model,"thinking":"enabled","output_limit":1024},
        "results":[{"case":"child_command","passed":false,"functional_passed":false,
            "model_calls":1,"first_usage":first.usage,"host_commands_sent":0,"stage":"command_proposal_received"}]
    })).unwrap()).unwrap();
    let (db, _, child, call, grant) = approved_child_with_call(Some(call.clone())).await;
    let store = SignalCapabilityGrantStore::new(db.clone());
    store
        .prepare(prepared(&child, &call, &grant))
        .await
        .unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } = store
        .record_dispatch_intent(prepared(&child, &call, &grant))
        .await
        .unwrap()
    else {
        panic!("initial delegated input must dispatch")
    };
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let origin =
        ActionResultOrigin::capture_confirmed_command(&registry, &child, &call, now).unwrap();
    assert!(matches!(
        store
            .claim_command_dispatch(
                &dispatch_id,
                now,
                "host",
                &child.conversation_id,
                &call.id,
                60000,
                &origin,
                None
            )
            .await
            .unwrap(),
        DispatchClaimResult::Claimed(_)
    ));
    store
        .validate_claimed_dispatch(&dispatch_id, prepared(&child, &call, &grant))
        .await
        .unwrap();
    let mut proposal = ChatMessage::assistant_tool_calls(
        "real-proposal",
        first.text.clone(),
        first.tool_calls.iter().map(ToolCall::to_ref).collect(),
    );
    proposal.reasoning = first.provider_meta.display_reasoning.clone();
    proposal.replay_disposition = first.provider_meta.replay.clone();
    request.messages.push(proposal);
    request.messages.push(ChatMessage::tool_result("dispatch-result", &call.id,
        "Runtime: owner approved the exact proposal and durable dispatch was recorded. The synthetic host has not returned a completion receipt. No real device command was sent. The task has an outstanding command dependency."));
    request.messages.push(ChatMessage::system_event("report", format!("{} Explain the current progress in normal Chinese text because the command receipt is still outstanding. Do not claim the command has finished.", report::REPORT_INSTRUCTION)));
    request.tools.clear();
    // Delegated answers always use the ordinary text response format.
    request.response_format = ResponseFormatSpec::None;
    let second = tokio::time::timeout(
        Duration::from_secs(90),
        seam.call(request, &mut NullTurnSink),
    )
    .await
    .expect("pending report timed out")
    .expect("pending report failed");
    let parsed = report::from_answer(
        &second.text,
        true,
        &desk_diagnose_core::subagent::state::CompletionFacts::default(),
    );
    let passed = child.latest_input_seq == 1
        && second.tool_calls.is_empty()
        && parsed.as_ref().is_ok_and(|value| {
            value.assessment == TaskAssessment::Pending
                && value.evidence_refs.is_empty()
                && value.receipt_refs.is_empty()
        });
    let evidence = serde_json::json!({
        "scope":"production_oss_model_transport_real_provider_created_child_synthetic_owner_approval_durable_command_dispatch_no_host_send_no_os_execution",
        "profile":{"model":config.model,"thinking":"enabled","output_limit":1024,"response_format":"none"},
        "results":[{"case":"child_command","passed":passed,"functional_passed":passed,
            "model_calls":2,"initial_child_input_watermark":child.latest_input_seq,
            "command_work_rows":agent_action_item::Entity::find().count(&db).await.unwrap(),
            "command_exec_rows":agent_exec_task::Entity::find().count(&db).await.unwrap(),
            "owner_approved":true,"dispatch_claimed":true,"claimed_send_fence_valid":true,
            "host_commands_sent":0,"pending_report_valid":parsed.is_ok(),
            "first_usage":first.usage,"second_usage":second.usage,"elapsed_ms":started.elapsed().as_millis()}]
    });
    std::fs::write(
        evidence_path,
        serde_json::to_string_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(
        passed,
        "pending command report must follow the reference contract"
    );
}
