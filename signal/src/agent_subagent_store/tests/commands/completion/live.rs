//! Opt-in real thinking model across original completion and strict final settlement.
use super::*;
use desk_diagnose_core::{
    agent_loop::{LoopDeps, LoopOutcome},
    chat::{ChatRole, StopReason},
    model_profile::WireProtocol,
    seam::{ModelSeam, NullTurnSink},
};

#[actix_web::test]
#[ignore = "requires explicit project-model credentials and public network access"]
async fn live_project_model_subagent_completion() {
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
        model: Some(std::env::var("LRD_LIVE_DEEPSEEK_MODEL").unwrap()),
        base_url: Some(std::env::var("LRD_LIVE_DEEPSEEK_BASE_URL").unwrap()),
        api_key: Some(std::env::var("LRD_LIVE_DEEPSEEK_API_KEY").unwrap()),
        request_options: serde_json::json!({"thinking":{"type":"enabled"}}),
        max_context_bytes: Some(131072),
        runtime_max_output_tokens: 1024,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let evidence_path = std::env::var("LRD_LIVE_SUBAGENT_EVIDENCE_PATH").unwrap();
    let model = crate::model_dial::SignalModelSeam::from_config(&config).unwrap();
    let mut request = ModelRequest::text_only(
        vec![
            ChatMessage::text(
                "system",
                ChatRole::System,
                "Isolated model and authorization test. Propose exactly one exec_command. The synthetic owner will approve it separately. No command will be sent or executed.",
            ),
            ChatMessage::text(
                "requirement",
                ChatRole::User,
                "Propose exec_command: bash, sleep 60, timeout_ms 60000, noninteractive.",
            ),
        ],
        ResponseFormatSpec::None,
    );
    request.tools = vec![
        desk_diagnose_core::ai_assistant::ai_assistant_provider_registry()
            .capability_for_tool("exec_command")
            .unwrap()
            .tool_spec
            .clone(),
    ];
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        model.call(request, &mut NullTurnSink),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    assert_eq!(first.tool_calls.len(), 1);
    let input: serde_json::Value =
        serde_json::from_str(&first.tool_calls[0].arguments_json).unwrap();
    assert_eq!(first.tool_calls[0].name, "exec_command");
    assert_eq!(input["command"], "sleep 60");
    let mut evidence = serde_json::json!({"scope":"production_oss_completion_and_normal_child_report_loop_isolated_owner_approval_synthetic_native_receipt_no_host_send_no_os_execution",
        "profile":{"model":config.model,"thinking":"enabled","output_limit":1024},
        "results":[{"case":"child_completion","passed":false,"functional_passed":false,
            "host_commands_sent":0,"first_usage":first.usage,"stage":"proposal_received"}]});
    std::fs::write(
        &evidence_path,
        serde_json::to_string_pretty(&evidence).unwrap(),
    )
    .unwrap();
    let (db, child, policy) = completed_fixture_with_delivery(&config, Some(&first), true).await;
    assert_eq!(deliverable_commands(&db).await.len(), 1);
    evidence["results"][0]["recovery_before_publication"] = true.into();
    let completion = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        SignalAgentExecStore::new(db.clone()).publish_once(),
    )
    .await
    .unwrap();
    evidence["results"][0]["stage"] = "completion_interpreted".into();
    let interpreted =
        super::super::super::paused_permission::session(&db, &child.conversation_id).await;
    let completion_answered = completion.is_ok()
        && interpreted.pending_auto_triggers.is_empty()
        && interpreted.automation_turns_used == 1
        && deliverable_commands(&db).await.is_empty();
    evidence["results"][0]["completion_answered"] = completion_answered.into();
    std::fs::write(
        &evidence_path,
        serde_json::to_string_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(completion_answered, "{completion:?}");
    let tasks = SubAgentStore::new(db.clone());
    let current =
        super::super::super::paused_permission::session(&db, &child.conversation_id).await;
    let task = load(&db, &current.agent_role.binding().unwrap().task_id).await;
    assert_eq!(task.state, SubAgentState::Queued);
    let params = desk_diagnose_core::seam::ClaimTurnParams {
        conversation_id: current.conversation_id.clone(),
        actor_id: current.actor_id.clone(),
        device_id: current.device_id.clone(),
        policy_revision: current.policy_revision,
        current_pdp_scope: current.scope_snapshot.clone(),
        turn_id: "final-report".into(),
        request_id: None,
        connection_id: None,
        trigger_origin: desk_diagnose_core::session::TriggerOrigin::DelegatedTask,
        now: chrono::Utc::now().to_rfc3339(),
    };
    let SubAgentClaimOutcome::Claimed(claimed) = tasks
        .claim_child(
            &params,
            &task.binding.task_id,
            task.fence(),
            &policy.destination,
        )
        .await
        .unwrap()
    else {
        panic!("original child report claim required")
    };
    let sessions = SignalAgentSessionStore::new(db.clone());
    let heartbeat = crate::agent_runtime::SignalStoreHeartbeat {
        store: sessions.clone(),
    };
    let model = crate::assistant_model::MeteredModel {
        fresh_task: None,
        inner: crate::model_dial::SignalModelSeam::from_config(&config)
            .unwrap()
            .with_context_db(db.clone()),
        db: db.clone(),
        destination: policy.destination.clone(),
        selected_source_tools: policy.selected_source_tools.clone(),
        export_authorization_id: "final-child-report-export".into(),
        permission_resume: false,
        completed_compression_receipt: std::cell::RefCell::new(None),
        model_call_ordinal: std::sync::atomic::AtomicU64::new(0),
    };
    struct NoDeviceTools;
    #[async_trait::async_trait(?Send)]
    impl ToolSeam for NoDeviceTools {
        async fn run_read(
            &self,
            _: &ToolCall,
        ) -> Result<desk_diagnose_core::seam::ToolRunOutput, desk_agent_protocol::AgentError>
        {
            Err(desk_diagnose_core::subagent::invalid(
                "no device tools in this isolated report check",
            ))
        }
    }
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let clock = || chrono::Utc::now().to_rfc3339();
    let deps = LoopDeps {
        session_seam: &sessions,
        model: &model,
        tools: &NoDeviceTools,
        content_safety: desk_diagnose_core::content_safety::ContentSafetyMode::Disabled,
        registry: &[],
        provider_registry: Some(&registry),
        capability_inventory: None,
        capability_permission_candidates: &[],
        capability_catalog_metrics: None,
        permission_continuation_exact_tools: &[],
        response_format: ResponseFormatSpec::None,
        system_prompt: desk_diagnose_core::ai_assistant::build_ai_assistant_system_message(None),
        response_locale: None,
        interactive_user_home: None,
        interactive_user_home_incarnation: None,
        max_steps_per_turn: 2,
        max_same_tool_per_turn: 1,
        clock: &clock,
        heartbeat: Some(&heartbeat),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        desk_diagnose_core::agent_loop::run_preclaimed_subagent_turn(
            &deps,
            claimed.session,
            &mut NullTurnSink,
        ),
    )
    .await
    .unwrap();
    let stored = super::super::super::paused_permission::session(&db, &child.conversation_id).await;
    let task = load(&db, &stored.agent_role.binding().unwrap().task_id).await;
    let valid = match &result {
        Ok(LoopOutcome::Answered(text)) => {
            !text.trim().is_empty()
                && task
                    .terminal_report
                    .as_ref()
                    .is_some_and(|report| report.summary == *text)
        }
        _ => false,
    };
    let receipts = crate::entity::model_egress_receipt::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let passed = valid && task.state == SubAgentState::Completed;
    evidence["results"][0]["passed"] = passed.into();
    evidence["results"][0]["functional_passed"] = passed.into();
    evidence["results"][0]["stage"] = "final_text_settled".into();
    evidence["results"][0]["task_state"] = serde_json::to_value(task.state).unwrap();
    evidence["results"][0]["model_calls"] = (receipts.len() + 1).into();
    evidence["results"][0]["audited_request_content_bytes"] = serde_json::json!(
        receipts
            .iter()
            .map(|receipt| receipt.total_bytes)
            .collect::<Vec<_>>()
    );
    if let Some(usage) = stored
        .context_usage_basis
        .as_ref()
        .and_then(|basis| basis.usage(&stored.conversation))
    {
        evidence["results"][0]["context_usage"] =
            serde_json::json!({"used_bytes":usage.used_bytes,"limit_bytes":usage.limit_bytes});
    }
    evidence["results"][0]["audited_usage"] = serde_json::json!(
        receipts
            .iter()
            .map(|r| r
                .usage_json
                .as_ref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()))
            .collect::<Vec<_>>()
    );
    evidence["results"][0]["report_corrections"] = stored.subagent_report_corrections_used.into();
    evidence["results"][0]["command_work_rows"] = agent_action_item::Entity::find()
        .count(&db)
        .await
        .unwrap()
        .into();
    evidence["results"][0]["command_exec_rows"] = agent_exec_task::Entity::find()
        .count(&db)
        .await
        .unwrap()
        .into();
    evidence["results"][0]["elapsed_ms"] = (started.elapsed().as_millis() as u64).into();
    std::fs::write(
        evidence_path,
        serde_json::to_string_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(passed, "strict child report failed: {result:?}");
}

#[actix_web::test]
#[ignore = "requires explicit project-model credentials and public network access"]
async fn live_project_model_subagent_concise_result() {
    use desk_diagnose_core::subagent::{
        report::{REPORT_INSTRUCTION, from_answer},
        result::SubAgentModelResult,
        state::CompletionFacts,
        tools::{SPAWN, SpawnRequest},
    };
    use sha2::{Digest, Sha256};
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
        model: Some(std::env::var("LRD_LIVE_DEEPSEEK_MODEL").unwrap()),
        base_url: Some(std::env::var("LRD_LIVE_DEEPSEEK_BASE_URL").unwrap()),
        api_key: Some(std::env::var("LRD_LIVE_DEEPSEEK_API_KEY").unwrap()),
        request_options: serde_json::json!({"thinking":{"type":"enabled"}}),
        max_context_bytes: Some(131072),
        runtime_max_output_tokens: 4096,
        ..Default::default()
    };
    let model = crate::model_dial::SignalModelSeam::from_config(&config).unwrap();
    let mut parent_system =
        desk_diagnose_core::ai_assistant::build_ai_assistant_system_message(None);
    desk_diagnose_core::ai_assistant::scope_disclosed_instructions(&mut parent_system, &[]);
    let mut request = ModelRequest::text_only(
        vec![
            parent_system,
            ChatMessage::text(
                "user",
                ChatRole::User,
                "创建一个子 agent，在当前设备执行一次 sleep 60，结束后告诉我结果。",
            ),
        ],
        ResponseFormatSpec::None,
    );
    request.tools = vec![
        desk_diagnose_core::subagent::tools::registry()
            .into_iter()
            .find(|tool| tool.spec.name == SPAWN)
            .unwrap()
            .spec,
    ];
    let proposal = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        model.call(request, &mut NullTurnSink),
    )
    .await
    .unwrap()
    .unwrap();
    let proposal_check = serde_json::json!({"scope":"configured_thinking_model_concise_delegation_check_no_host_send",
        "profile":{"model":config.model,"thinking":"enabled","output_limit":4096},
        "results":[{"case":"concise_result","passed":false,"functional_passed":false,
            "stage":"delegation_proposal","host_commands_sent":0,"model_calls":1,
            "stop_reason":proposal.stop_reason,"usage":[proposal.usage],"report_correction_calls":0,
            "tool_call_count":proposal.tool_calls.len(),
            "arguments_json_valid":proposal.tool_calls.first().map(|call|serde_json::from_str::<serde_json::Value>(&call.arguments_json).is_ok()),
            "arguments_bytes":proposal.tool_calls.first().map(|call|call.arguments_json.len())}]});
    std::fs::write(
        std::env::var("LRD_LIVE_SUBAGENT_EVIDENCE_PATH").unwrap(),
        serde_json::to_string_pretty(&proposal_check).unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    if let Ok(path) = std::env::var("LRD_LIVE_CONCISE_PROPOSAL_DIAGNOSTIC") {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(&serde_json::to_vec(&proposal.tool_calls).unwrap())
            .unwrap();
    }
    assert_eq!(proposal.stop_reason, StopReason::ToolUse);
    assert_eq!(proposal.tool_calls.len(), 1);
    assert_eq!(proposal.tool_calls[0].name, SPAWN);
    let delegated: SpawnRequest =
        serde_json::from_str(&proposal.tool_calls[0].arguments_json).unwrap();
    desk_diagnose_core::subagent::role::validate_task(
        &delegated.task,
        &delegated.acceptance_criteria,
    )
    .unwrap();
    let mut system = desk_diagnose_core::ai_assistant::build_ai_assistant_system_message(None);
    desk_diagnose_core::ai_assistant::scope_disclosed_instructions(&mut system, &[]);
    system.text.push('\n');
    system.text.push_str(REPORT_INSTRUCTION);
    let input = serde_json::json!({"task":delegated.task,"acceptance_criteria":delegated.acceptance_criteria});
    // Isolated completed-command fixture, like the runtime completion context.
    // No host action is sent: the evidence records this as synthetic receipt data.
    let completion = serde_json::json!({"phase":"command_completed",
        "receipt":{"receipt_id":"synthetic-receipt","command":"sleep 60","started":true,
            "exit_code":0,"failure":null,"termination_signal":null,"duration_ms":60100,"stdout":"","stderr":""},
        "rule":"The original command has completed. Report its recorded outcome; do not execute it again."});
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        model.call(
            ModelRequest::text_only(
                vec![
                    system,
                    ChatMessage::text("child-input", ChatRole::User, &input.to_string()),
                    ChatMessage::system_event("completion-fixture", &completion.to_string()),
                ],
                ResponseFormatSpec::None,
            ),
            &mut NullTurnSink,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let facts = CompletionFacts {
        accepted_receipt_ids: vec!["synthetic-receipt".into()],
        ..Default::default()
    };
    #[cfg(unix)]
    if let Ok(path) = std::env::var("LRD_LIVE_CONCISE_PROPOSAL_DIAGNOSTIC") {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(format!("{path}.answer"))
            .unwrap();
        file.write_all(answer.text.as_bytes()).unwrap();
    }
    let report = from_answer(&answer.text, false, &facts).unwrap();
    let full = desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentResult {
        task: desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentSummary {
            task_id: "synthetic-task".into(),
            child_session_id: "synthetic-child".into(),
            group_id: "synthetic-group".into(),
            name: delegated.name,
            state: SubAgentState::Completed,
            wait_reason: None,
            input_revision: 1,
            control_revision: 1,
            state_revision: 2,
            source_goal_id: None,
            source: desk_agent_protocol::ai_assistant::subagent::SubAgentSource::UserInput {
                input_revision: 1,
            },
            created_at: "test-start".into(),
            updated_at: "test-end".into(),
        },
        objective: delegated.task,
        acceptance_criteria: delegated.acceptance_criteria,
        report: Some(report),
        failure_reason: None,
    };
    let compact = SubAgentModelResult::from_result(&full, false);
    let detailed = SubAgentModelResult::from_result(&full, true);
    let functional_passed = answer.stop_reason == StopReason::EndTurn
        && answer.tool_calls.is_empty()
        && !answer.text.trim().is_empty()
        && compact.answer.as_deref() == Some(answer.text.as_str())
        && compact.objective.is_none()
        && detailed.objective.as_deref() == Some(full.objective.as_str());
    let passed = functional_passed && full.objective.len() <= 600 && answer.text.len() <= 768;
    let evidence = serde_json::json!({"scope":"configured_thinking_model_real_shared_spawn_description_and_child_prompt_shared_result_projection_synthetic_receipt_no_agent_store_no_host_send",
        "profile":{"model":config.model,"thinking":"enabled","output_limit":4096},
        "results":[{"case":"concise_result","passed":passed,"functional_passed":functional_passed,"host_commands_sent":0,"model_calls":2,
            "delegation_arguments_bytes":proposal.tool_calls[0].arguments_json.len(),"delegated_task_bytes":full.objective.len(),
            "criteria_count":full.acceptance_criteria.len(),"answer_bytes":answer.text.len(),"answer_characters":answer.text.chars().count(),
            "full_result_bytes":serde_json::to_vec(&full).unwrap().len(),"compact_result_bytes":serde_json::to_vec(&compact).unwrap().len(),
            "detailed_result_bytes":serde_json::to_vec(&detailed).unwrap().len(),"answer_sha256":format!("{:x}",Sha256::digest(answer.text.as_bytes())),
            "original_answer_preserved":compact.answer.as_deref()==Some(answer.text.as_str()),"original_task_available_on_request":true,
            "usage":[proposal.usage,answer.usage],"report_correction_calls":0}]});
    std::fs::write(
        std::env::var("LRD_LIVE_SUBAGENT_EVIDENCE_PATH").unwrap(),
        serde_json::to_string_pretty(&evidence).unwrap(),
    )
    .unwrap();
    assert!(
        passed,
        "simple-task delegation/answer length or lossless projection check failed"
    );
}
