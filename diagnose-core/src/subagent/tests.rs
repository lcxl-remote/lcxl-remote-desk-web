use super::*;
use crate::{
    chat::ToolCall,
    registry::ToolEffect,
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::{AgentScope, ExecutionMode};
use group::{DelegationGroup, SourceAdmission};
use state::{CompletionDisposition, CompletionFacts, RequiredReceipt, SubAgentRun, TaskDependency};
use wait::{ParentWait, TaskFence, WaitEvaluation, WaitMode};

#[test]
fn source_controls_fence_old_planning_and_keep_task_and_automation_usage() {
    let mut session = child();
    assert_eq!(session.latest_input_seq, 1);
    assert_eq!(session.handled_input_seq, 0);
    session
        .begin_turn("old-turn", None, None, 1, scope(), "2026-09-30T00:00:00Z")
        .unwrap();
    session.adopt_trigger(TriggerOrigin::DelegatedTask, "old-turn");
    session.automation_turns_used = 3;
    session.subagent_report_corrections_used = 1;
    let mut task = run();
    task.report_corrections_used = 1;
    let original_lease = session.lease_token;
    let original_chain = session.chain_id.clone();
    task.pause_source(2, "2026-09-30T00:00:01Z").unwrap();
    control::synchronize_session(&mut session, &task, "2026-09-30T00:00:01Z").unwrap();
    assert_eq!(session.latest_input_seq, 1);
    assert_eq!(session.turn_state, crate::session::TurnState::Idle);
    assert!(session.lease_token > original_lease);
    assert_eq!(session.automation_turns_used, 3);
    assert_eq!(session.chain_id, original_chain);
    assert_eq!(session.subagent_report_corrections_used, 1);
    assert_eq!(session.agent_role.binding().unwrap().deadline_ms, 10_000);
    task.resume_source(3, "2026-09-30T00:00:02Z").unwrap();
    control::synchronize_session(&mut session, &task, "2026-09-30T00:00:02Z").unwrap();
    assert_eq!(session.latest_input_seq, 1);
    task.adjust(
        task.fence(),
        "Inspect the new symptom".into(),
        vec!["Cite the actual observation".into()],
        "2026-09-30T00:00:03Z",
    )
    .unwrap();
    control::synchronize_session(&mut session, &task, "2026-09-30T00:00:03Z").unwrap();
    assert_eq!(session.input_revision, 2);
    assert_eq!(session.latest_input_seq, 2);
    assert_eq!(session.handled_input_seq, 0);
    assert_eq!(session.control_revision, 2);
    assert_eq!(session.automation_turns_used, 3);
    assert_eq!(session.subagent_report_corrections_used, 1);
    assert_eq!(session.trigger_origin, TriggerOrigin::DelegatedTask);
    task.validate_session(&session).unwrap();
    let stored = session.encode_json_for_storage().unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&stored)
            .unwrap()
            .latest_input_seq,
        2
    );
    let mut forged = task.clone();
    forged.binding.root_conversation_id = "another-root".into();
    assert!(control::synchronize_session(&mut session, &forged, "later").is_err());
    assert_eq!(session.encode_json_for_storage().unwrap(), stored);
}

fn scope() -> AgentScope {
    AgentScope {
        granted: Vec::new(),
        mode: ExecutionMode::ConfirmEachAction,
        expires_at: None,
        policy_name: None,
    }
}

fn binding() -> DelegatedTaskBinding {
    DelegatedTaskBinding {
        root_conversation_id: "root".into(),
        group_id: "group".into(),
        task_id: "child-task".into(),
        source: DelegationSource::Goal {
            goal_id: "goal".into(),
        },
        objective: "Investigate one independent failure".into(),
        acceptance_criteria: vec!["Cite observed evidence and verified receipts".into()],
        input_revision: 1,
        control_revision: 1,
        source_epoch: 1,
        deadline_ms: 10_000,
    }
}

fn child() -> PersistedAgentSession {
    PersistedAgentSession::new_subagent(
        "child-session",
        "owner",
        "device",
        1,
        scope(),
        binding(),
        "2026-09-30T00:00:00Z",
    )
    .unwrap()
}

fn run() -> SubAgentRun {
    SubAgentRun {
        child_conversation_id: "child-session".into(),
        actor_id: "owner".into(),
        device_id: "device".into(),
        name: "Investigate".into(),
        binding: binding(),
        state: SubAgentState::Queued,
        state_revision: 1,
        source_paused: false,
        dependencies: Vec::new(),
        partial_report: None,
        terminal_report: None,
        failure_reason: None,
        report_corrections_used: 0,
        created_at: "2026-09-30T00:00:00Z".into(),
        updated_at: "2026-09-30T00:00:00Z".into(),
    }
}

fn complete() -> TaskFinalReport {
    TaskFinalReport {
        assessment: TaskAssessment::Complete,
        summary: "Investigated".into(),
        findings: vec!["Observed failure".into()],
        delivered: vec!["Evidence and verified repair".into()],
        remaining: Vec::new(),
        evidence_refs: vec!["observation-1".into()],
        receipt_refs: vec!["receipt-1".into()],
        reason: None,
    }
}

fn facts() -> CompletionFacts {
    CompletionFacts {
        accepted_receipt_ids: vec!["receipt-1".into()],
        available_evidence_ids: vec!["observation-1".into()],
        required_receipts: vec![RequiredReceipt {
            receipt_id: "receipt-1".into(),
            succeeded: true,
            verification_complete: true,
        }],
        incomplete_action_ids: Vec::new(),
    }
}

fn group() -> DelegationGroup {
    DelegationGroup {
        group_id: "group".into(),
        root_conversation_id: "root".into(),
        actor_id: "owner".into(),
        device_id: "device".into(),
        source: binding().source,
        source_epoch: 1,
        source_admission: SourceAdmission::Open,
        parent_input_revision: 1,
        parent_control_revision: 1,
        parent_active: true,
        limits: budget::DelegationLimits {
            total: budget::Usage {
                model_calls: 10,
                tool_calls: 20,
                tokens: 1000,
            },
            max_context_bytes: 4096,
            max_result_bytes: 8192,
            deadline_ms: 10_000,
        },
        budget: budget::BudgetLedger::default(),
        tasks_created: 0,
        required_task_ids: Vec::new(),
        version: 1,
    }
}

#[test]
fn child_role_survives_storage_and_cannot_adopt_top_level_identity() {
    let mut session = child();
    session.adopt_client_metadata(
        Some("public-chat-id"),
        AgentSessionSurface::TerminalAiAssistant,
    );
    assert!(session.client_conversation_id.is_none());
    assert_eq!(session.surface, AgentSessionSurface::AiAssistant);
    let restored =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    assert_eq!(restored.agent_role, session.agent_role);
    assert!(restored.agent_role.binding().is_some());
    assert!(session.begin_focus_epoch(2, Vec::new()).is_err());
}

#[test]
fn child_retains_business_effects_and_rejects_all_orchestration_effects() {
    let child = child();
    for effect in [
        ToolEffect::ReadOnly,
        ToolEffect::Mutating,
        ToolEffect::PermissionPlanning,
        ToolEffect::CapabilityDiscovery,
        ToolEffect::ConversationHistory,
        ToolEffect::DirectoryPlanning,
        ToolEffect::WaitTask,
        ToolEffect::RunProjection,
        ToolEffect::ScheduleQuery,
    ] {
        assert!(child.agent_role.allows_effect(effect), "{effect:?}");
    }
    for effect in [
        ToolEffect::GoalOpenPlanning,
        ToolEffect::GoalControl,
        ToolEffect::SchedulePlanning,
        ToolEffect::SubAgentPlanning,
        ToolEffect::SubAgentControl,
        ToolEffect::SubAgentQuery,
        ToolEffect::SubAgentWait,
    ] {
        assert!(!child.agent_role.allows_effect(effect), "{effect:?}");
    }
}

#[test]
fn child_creation_parser_rejects_forged_user_origin() {
    let mut child = child();
    child
        .begin_turn("turn", None, None, 1, scope(), "2026-09-30T00:00:00Z")
        .unwrap();
    child.trigger_origin = TriggerOrigin::User;
    let call = ToolCall { id: "call".into(), name: tools::SPAWN.into(), arguments_json: serde_json::json!({"name":"Nested", "task":"Investigate", "acceptance_criteria":["Evidence"], "required_for_completion":true}).to_string() };
    assert!(tools::parse(&child, &call).is_err());
    assert!(
        crate::schedule::proposal::draft(
            &child,
            &ToolCall {
                name: crate::schedule::proposal::REQUEST_SCHEDULE.into(),
                ..call
            }
        )
        .is_err()
    );
}

#[test]
fn child_can_inspect_its_own_schedule_scope_without_obtaining_schedule_writes() {
    let mut child = child();
    child
        .begin_turn("turn", None, None, 1, scope(), "2026-09-30T00:00:00Z")
        .unwrap();
    child.adopt_trigger(TriggerOrigin::DelegatedTask, "turn");
    let registry = crate::schedule::proposal::registry();
    let list = crate::schedule::management_tools::LIST;
    assert!(crate::registry::lookup_for_session(&registry, list, &child).is_some());
    assert!(
        crate::registry::lookup_for_session(
            &registry,
            crate::schedule::management_tools::CANCEL,
            &child
        )
        .is_none()
    );
    assert!(
        crate::registry::lookup_for_session(
            &registry,
            crate::schedule::proposal::REQUEST_SCHEDULE,
            &child
        )
        .is_none()
    );
    let call = ToolCall {
        id: "query".into(),
        name: list.into(),
        arguments_json: "{}".into(),
    };
    assert!(matches!(
        crate::schedule::management_tools::parse(&child, &call),
        Ok(crate::schedule::management_tools::Action::List { .. })
    ));
    let cancel = ToolCall {
        name: crate::schedule::management_tools::CANCEL.into(),
        ..call.clone()
    };
    assert!(crate::schedule::management_tools::parse(&child, &cancel).is_err());
    child.adopt_trigger(TriggerOrigin::ExecCompletion, "receipt-turn");
    assert!(crate::registry::lookup_for_session(&registry, list, &child).is_none());
    assert!(crate::schedule::management_tools::parse(&child, &call).is_err());
}

#[test]
fn scheduled_child_permission_resume_does_not_gain_ai_review() {
    let mut session = child();
    if let AgentRole::SubAgent { binding } = &mut session.agent_role {
        binding.source = DelegationSource::ScheduledOccurrence {
            schedule_id: "schedule".into(),
            occurrence_id: "occurrence".into(),
        };
    }
    session.trigger_origin = TriggerOrigin::PermissionDecision;
    assert!(!session.allows_delegated_review());
    session.trigger_origin = TriggerOrigin::DelegatedTask;
    assert!(!session.allows_delegated_review());
    assert!(!TriggerOrigin::SubAgentCompletion.allows_new_mutation());
    assert!(!TriggerOrigin::SubAgentCompletion.allows_delegated_review());
}

#[test]
fn goal_pause_preserves_dependencies_and_fences_old_planning() {
    let mut task = run();
    task.set_dependencies(
        vec![
            TaskDependency::Approval {
                permission_request_id: "approval".into(),
            },
            TaskDependency::Work {
                work_id: "work".into(),
            },
        ],
        "before",
    )
    .unwrap();
    let old = task.fence();
    let dependencies = task.dependencies.clone();
    task.pause_source(2, "pause").unwrap();
    assert_eq!(task.state, SubAgentState::WaitingSource);
    assert_eq!(task.wait_reason(), Some(SubAgentWaitReason::SourcePaused));
    assert_eq!(task.dependencies, dependencies);
    assert!(task.require_current(old).is_err());
    task.set_dependencies(
        vec![TaskDependency::Approval {
            permission_request_id: "approval".into(),
        }],
        "receipt",
    )
    .unwrap();
    assert_eq!(task.state, SubAgentState::WaitingSource);
    task.resume_source(3, "resume").unwrap();
    assert_eq!(task.state, SubAgentState::WaitingApproval);
    assert_eq!(task.binding.deadline_ms, 10_000);
    assert!(task.require_current(old).is_err());
}

#[test]
fn main_only_stop_disables_interpretation_and_keeps_children_admitted() {
    let mut group = group();
    group
        .admit_child("child-task", true, 0, 1, policy::initial().limits)
        .unwrap();
    group.stop_parent().unwrap();
    assert_eq!(group.source_admission, SourceAdmission::Open);
    assert_eq!(group.source_epoch, 1);
    assert!(!group.can_interpret(1, 1));
    assert!(
        group
            .admit_child("another-child", true, 1, 1, policy::initial().limits)
            .is_err()
    );
    assert!(!group.required_dependencies_terminal(&[]));
    assert!(group.required_dependencies_terminal(&[("child-task".into(), SubAgentState::Failed)]));
    assert!(!group.required_dependencies_complete(&[("child-task".into(), SubAgentState::Failed)]));
}

#[test]
fn report_requires_known_verified_receipts_and_rejects_model_status() {
    let task = run();
    assert_eq!(
        task.evaluate_report(task.fence(), &complete(), &facts())
            .unwrap(),
        CompletionDisposition::Complete
    );
    let mut unverified = facts();
    unverified.required_receipts[0].verification_complete = false;
    assert!(
        task.evaluate_report(task.fence(), &complete(), &unverified)
            .is_err()
    );
    let mut spoofed = complete();
    spoofed.receipt_refs[0] = "another-task-receipt".into();
    assert!(
        task.evaluate_report(task.fence(), &spoofed, &facts())
            .is_err()
    );
    let mut value = serde_json::to_value(complete()).unwrap();
    value["status"] = serde_json::json!("completed");
    assert!(serde_json::from_value::<TaskFinalReport>(value).is_err());
}

#[test]
fn final_answer_with_pending_work_does_not_complete_task() {
    let mut task = run();
    task.set_dependencies(
        vec![TaskDependency::Work {
            work_id: "work".into(),
        }],
        "wait",
    )
    .unwrap();
    assert_eq!(
        task.settle_report(task.fence(), complete(), &facts(), "report")
            .unwrap(),
        CompletionDisposition::Waiting
    );
    assert_eq!(task.state, SubAgentState::WaitingWork);
    assert!(task.terminal_report.is_none());
    assert!(task.partial_report.is_some());
    task.set_dependencies(Vec::new(), "receipt").unwrap();
    let mut pending = complete();
    pending.assessment = TaskAssessment::Pending;
    pending.reason = Some("Waiting".into());
    assert!(
        task.evaluate_report(task.fence(), &pending, &facts())
            .is_err()
    );
}

#[test]
fn cancelled_task_keeps_partial_report_and_never_reopens() {
    let mut task = run();
    task.partial_report = Some(complete());
    task.pause_source(2, "pause").unwrap();
    task.request_cancel(task.fence(), "cancel").unwrap();
    assert!(task.require_current(task.fence()).is_err());
    task.settle_cancel("settled").unwrap();
    let terminal = task.terminal_report.clone();
    assert_eq!(task.state, SubAgentState::Cancelled);
    assert!(!task.resume_source(3, "late-resume").unwrap());
    assert!(task.set_dependencies(Vec::new(), "late-receipt").is_err());
    assert!(
        task.adjust(
            task.fence(),
            "New work".into(),
            vec!["Done".into()],
            "adjust"
        )
        .is_err()
    );
    assert_eq!(task.terminal_report, terminal);
}

#[test]
fn wait_handles_completion_before_registration_and_control_changes() {
    let fence = TaskFence {
        task_id: "child-task".into(),
        input_revision: 1,
        control_revision: 1,
    };
    let wait = ParentWait {
        wait_id: "wait".into(),
        tool_call_id: "call".into(),
        result_message_id: "result".into(),
        group_id: "group".into(),
        source_epoch: 1,
        retry_after_ms: None,
        parent_input_revision: 1,
        parent_control_revision: 1,
        mode: WaitMode::AllTerminal,
        tasks: vec![fence.clone()],
    };
    wait.validate().unwrap();
    assert_eq!(
        wait.evaluate(1, 1, &[(fence.clone(), SubAgentState::Completed)]),
        WaitEvaluation::Ready
    );
    assert_eq!(
        wait.evaluate(1, 1, &[(fence.clone(), SubAgentState::WaitingApproval)]),
        WaitEvaluation::Pending
    );
    assert_eq!(
        wait.evaluate(2, 1, &[(fence.clone(), SubAgentState::Completed)]),
        WaitEvaluation::ParentSuperseded
    );
    let adjusted = TaskFence {
        input_revision: 2,
        ..fence
    };
    assert_eq!(
        wait.evaluate(1, 1, &[(adjusted, SubAgentState::Running)]),
        WaitEvaluation::DependencyChanged
    );
}

#[test]
fn adjustment_and_pause_do_not_reset_budget_or_deadline() {
    let mut task = run();
    task.report_corrections_used = 1;
    let deadline = task.binding.deadline_ms;
    task.adjust(
        task.fence(),
        "Narrower investigation".into(),
        vec!["Cite evidence".into()],
        "adjust",
    )
    .unwrap();
    assert_eq!(task.binding.deadline_ms, deadline);
    assert_eq!(task.report_corrections_used, 1);
    assert_eq!(task.binding.input_revision, 2);
    assert_eq!(task.binding.control_revision, 2);
    let mut ledger = budget::BudgetLedger::default();
    let limits = group().limits;
    ledger
        .reserve(
            limits,
            budget::Usage {
                model_calls: 8,
                tool_calls: 0,
                tokens: 800,
            },
            true,
            1,
        )
        .unwrap();
    assert!(
        ledger
            .reserve(
                limits,
                budget::Usage {
                    model_calls: 1,
                    tool_calls: 0,
                    tokens: 1
                },
                true,
                1
            )
            .is_err()
    );
    ledger
        .reserve(
            limits,
            budget::Usage {
                model_calls: 2,
                tool_calls: 0,
                tokens: 200,
            },
            false,
            1,
        )
        .unwrap();
    assert!(
        ledger
            .reserve(limits, budget::Usage::default(), false, 10_000)
            .is_err()
    );
    assert_eq!(ledger.outstanding.tokens, 1000);
    assert_eq!(ledger.charged.tokens, 0);
}

#[test]
fn rejected_reservation_does_not_partially_mutate_counters() {
    let mut ledger = budget::BudgetLedger::default();
    let before = ledger;
    assert!(
        ledger
            .reserve(
                group().limits,
                budget::Usage {
                    model_calls: 9,
                    tool_calls: 1,
                    tokens: 1
                },
                true,
                1
            )
            .is_err()
    );
    assert_eq!(ledger, before);
    assert!(
        ledger
            .settle(
                budget::Usage {
                    model_calls: 1,
                    ..budget::Usage::default()
                },
                budget::Usage::default(),
                false
            )
            .is_err()
    );
    assert_eq!(ledger, before);
}

#[test]
fn dependency_reconciliation_keeps_live_planning_and_source_pause_overlay() {
    let mut task = run();
    let fence = task.fence();
    task.claim_planning(fence, 1, "claimed").unwrap();
    let revision = task.state_revision;
    assert!(
        !task
            .synchronize_dependencies(Vec::new(), true, "still-running")
            .unwrap()
    );
    assert_eq!(task.state, SubAgentState::Running);
    assert_eq!(task.state_revision, revision);
    let dependencies = vec![TaskDependency::Work {
        work_id: "work:7".into(),
    }];
    assert!(
        task.synchronize_dependencies(dependencies.clone(), true, "work")
            .unwrap()
    );
    assert_eq!(task.state, SubAgentState::WaitingWork);
    task.pause_source(2, "paused").unwrap();
    task.synchronize_dependencies(Vec::new(), false, "receipt-arrived")
        .unwrap();
    assert_eq!(task.state, SubAgentState::WaitingSource);
    task.resume_source(3, "resumed").unwrap();
    assert_eq!(task.state, SubAgentState::Queued);
    task.claim_planning(task.fence(), 2, "new-claim").unwrap();
    task.synchronize_dependencies(Vec::new(), false, "released")
        .unwrap();
    assert_eq!(task.state, SubAgentState::Queued);
    assert!(task.terminal_report.is_none());
}

#[test]
fn missing_execution_proof_blocks_complete_but_allows_an_honest_failure() {
    let task = run();
    let mut missing = facts();
    missing
        .incomplete_action_ids
        .push("manually-disposed:7".into());
    assert!(
        task.evaluate_report(task.fence(), &complete(), &missing)
            .is_err()
    );
    let mut unable = complete();
    unable.assessment = TaskAssessment::Unable;
    unable.reason = Some("The original action outcome cannot be verified".into());
    assert_eq!(
        task.evaluate_report(task.fence(), &unable, &missing)
            .unwrap(),
        CompletionDisposition::Unable
    );
    missing.required_receipts[0].receipt_id = "unaccepted".into();
    assert!(
        task.evaluate_report(task.fence(), &unable, &missing)
            .is_err()
    );
}

#[test]
fn report_can_cite_the_whole_finite_action_budget_without_unbounded_prose() {
    let mut report = complete();
    report.receipt_refs = (0..200)
        .map(|index| format!("action-result-{index}"))
        .collect();
    report.validate().unwrap();
    let encoded = serde_json::to_string(&report).unwrap();
    assert_eq!(
        serde_json::from_str::<TaskFinalReport>(&encoded).unwrap(),
        report
    );
    report.receipt_refs.push("one-too-many".into());
    assert!(report.validate().is_err());
    report.receipt_refs.clear();
    report.evidence_refs = vec!["instruction with spaces".into()];
    assert!(report.validate().is_err());
}

#[test]
fn report_references_accept_identifiers_and_keep_error_explanations_in_findings() {
    let mut value = complete();
    value.findings.push("命令授权失败，未执行设备操作".into());
    value.evidence_refs = vec!["evidence:child-1.observation_2".into()];
    assert_eq!(
        serde_json::from_str::<TaskFinalReport>(&serde_json::to_string(&value).unwrap()).unwrap(),
        value
    );
    for refs in [
        vec!["command authorization failed".into()],
        vec!["命令授权失败".into()],
        vec!["evidence/child".into()],
        vec!["evidence-1".into(), "evidence-1".into()],
    ] {
        value.evidence_refs = refs;
        assert!(value.validate().is_err());
    }
}

#[test]
fn native_verification_cannot_be_replaced_by_a_success_label() {
    use desk_agent_protocol::computer_use::{
        ComputerActionCompleted, ComputerActionResultClass, ComputerActionStepFact,
    };
    let mut native = ComputerActionCompleted {
        work_id: "work".into(),
        action_request_id: "request".into(),
        execution_generation: "generation".into(),
        result: ComputerActionResultClass::Verified,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: true,
            verified: true,
            summary: "Observed expected state".into(),
        }],
        message: None,
        output: None,
    };
    assert!(facts::computer_verified(&native));
    native.facts[0].verified = false;
    assert!(!facts::computer_verified(&native));
    native.facts[0].verified = true;
    native.result = ComputerActionResultClass::ChangedButUnverified;
    assert!(!facts::computer_verified(&native));
}

#[test]
fn zero_exit_without_start_or_with_failure_does_not_prove_command_success() {
    use desk_agent_protocol::{AgentOutcome, ExecOutput, ExecOutputStreams, OperationOutput};
    let mut output = ExecOutput {
        started: true,
        exit_code: Some(0),
        termination_signal: None,
        failure: None,
        diagnostics: Vec::new(),
        streams: ExecOutputStreams::Split {
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        },
        duration_ms: 1,
        redactions: Vec::new(),
    };
    assert!(facts::command_succeeded(&AgentOutcome::Ok(
        OperationOutput::Exec(output.clone())
    )));
    output.started = false;
    assert!(!facts::command_succeeded(&AgentOutcome::Ok(
        OperationOutput::Exec(output.clone())
    )));
    output.started = true;
    output.termination_signal = Some(15);
    assert!(!facts::command_succeeded(&AgentOutcome::Ok(
        OperationOutput::Exec(output)
    )));
}

#[test]
fn adjusted_task_projection_retains_source_labels_locale_and_current_objective() {
    use desk_agent_protocol::data_lineage::{DestinationIdentity, Sensitivity};
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let mut owner = crate::model_message_labels::model_bound_user_message(
        "source-user".into(),
        "Investigate this private symptom".into(),
        destination.clone(),
    )
    .unwrap();
    owner.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Secret;
    let source = creation::CreationEnvelope {
        root_conversation_id: "root".into(),
        actor_id: "owner".into(),
        device_id: "device".into(),
        source: binding().source,
        parent_input_revision: 1,
        parent_control_revision: 1,
        owner_requirement: owner.clone(),
        scheduled_source: None,
        original_read_context: None,
        model_destination: destination.clone(),
    };
    let inputs = vec![owner.data_envelope.unwrap()];
    let task = binding();
    let instruction = projection::runtime_message(
        "old-instruction",
        &serde_json::json!({
        "delegated_task": task.objective, "acceptance_criteria": task.acceptance_criteria }),
        &inputs,
    )
    .unwrap();
    let context = creation::TaskCreationEnvelope {
        source: source.clone(),
        instruction,
        input_envelopes: inputs,
        response_locale: Some("zh-CN".into()),
    };
    context.validate_task(&task).unwrap();
    let mut changed = task.clone();
    changed.input_revision += 1;
    changed.control_revision += 1;
    changed.objective = "Investigate the revised symptom".into();
    let adjustment = crate::model_message_labels::model_bound_user_message(
        "adjustment".into(),
        changed.objective.clone(),
        destination.clone(),
    )
    .unwrap()
    .data_envelope
    .unwrap();
    let adjusted = context
        .adjusted(&changed, adjustment, "child-session")
        .unwrap();
    adjusted.validate_task(&changed).unwrap();
    assert!(adjusted.validate_task(&task).is_err());
    assert_eq!(adjusted.source, source);
    assert_eq!(adjusted.response_locale.as_deref(), Some("zh-CN"));
    assert_eq!(
        adjusted
            .instruction
            .data_envelope
            .as_ref()
            .unwrap()
            .sensitivity,
        Sensitivity::Secret
    );
    assert_eq!(
        adjusted
            .instruction
            .data_envelope
            .as_ref()
            .unwrap()
            .allowed_destinations,
        vec![destination]
    );
    assert_ne!(
        adjusted.instruction.message_id,
        context.instruction.message_id
    );
    assert_eq!(changed.deadline_ms, task.deadline_ms);
}

fn completion_context() -> creation::TaskCreationEnvelope {
    use desk_agent_protocol::data_lineage::{DestinationIdentity, Sensitivity};
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let mut owner = crate::model_message_labels::model_bound_user_message(
        "source-owner".into(),
        "Investigate this original issue".into(),
        destination.clone(),
    )
    .unwrap();
    owner.data_envelope.as_mut().unwrap().sensitivity = Sensitivity::Sensitive;
    owner
        .data_envelope
        .as_mut()
        .unwrap()
        .retention
        .expires_at_unix_ms = Some(9_000);
    let inputs = vec![owner.data_envelope.clone().unwrap()];
    let task = binding();
    let instruction = projection::runtime_message(
        "child-instruction",
        &serde_json::json!({
        "delegated_task": task.objective, "acceptance_criteria": task.acceptance_criteria }),
        &inputs,
    )
    .unwrap();
    creation::TaskCreationEnvelope {
        source: creation::CreationEnvelope {
            root_conversation_id: "root".into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            source: task.source,
            parent_input_revision: 1,
            parent_control_revision: 1,
            owner_requirement: owner,
            scheduled_source: None,
            original_read_context: None,
            model_destination: destination,
        },
        instruction,
        input_envelopes: inputs,
        response_locale: Some("zh-CN".into()),
    }
}

#[test]
fn goal_delegation_uses_the_goal_token_ceiling_and_honors_a_lower_configured_limit() {
    use crate::goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalRun};
    let source = completion_context().source;
    let mut goal = GoalRun::new(
        source.source.goal_id().unwrap().into(),
        source.root_conversation_id.clone(),
        source.actor_id.clone(),
        source.device_id.clone(),
        source.owner_requirement.text.clone(),
        source.owner_requirement.message_id.clone(),
        GoalOpening::OwnerRequest,
        GoalModelBinding::from_destination(&source.model_destination).unwrap(),
        source.parent_input_revision,
        1_000,
        GoalLimits::default(),
    )
    .unwrap();
    assert!(goal.limits.model_tokens > creation::DEFAULT_GROUP_TOKENS);
    assert_eq!(
        source
            .new_group(1_000, Some(&goal))
            .unwrap()
            .limits
            .total
            .tokens,
        goal.limits.model_tokens
    );
    goal.limits.model_tokens = 50_000;
    assert_eq!(
        source
            .new_group(1_000, Some(&goal))
            .unwrap()
            .limits
            .total
            .tokens,
        50_000
    );
}

#[test]
fn child_permission_bridge_retains_task_restrictions_without_fabricating_owner_input() {
    use desk_agent_protocol::data_lineage::Sensitivity;
    let context = completion_context();
    let policy = crate::model_egress::ModelEgressPolicy {
        destination: context.source.model_destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: "child-resume".into(),
        now_unix_ms: 1_000,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: true,
    };
    let bridge = crate::permission_resume::authorized_child_permission_resume_message(
        "child-resume".into(),
        &policy,
        &context,
        &binding(),
    )
    .unwrap();
    assert!(crate::permission_resume::is_permission_resume_message(
        &bridge
    ));
    assert!(
        crate::permission_resume::latest_user_requirement(std::slice::from_ref(&bridge)).is_none()
    );
    assert!(bridge.text.contains(&binding().objective));
    let label = bridge.data_envelope.as_ref().unwrap();
    assert_eq!(label.sensitivity, Sensitivity::Sensitive);
    assert_eq!(label.retention.expires_at_unix_ms, Some(9_000));
    assert_eq!(
        label.allowed_destinations,
        vec![context.source.model_destination.clone()]
    );
    policy
        .authorize_request(crate::seam::ModelRequest::text_only(
            vec![bridge],
            crate::prompt::ResponseFormatSpec::None,
        ))
        .unwrap();
    let mut expired = policy.clone();
    expired.now_unix_ms = 9_001;
    assert!(
        crate::permission_resume::authorized_child_permission_resume_message(
            "late-resume".into(),
            &expired,
            &context,
            &binding()
        )
        .is_err()
    );
    let mut changed = binding();
    changed.objective = "Different task".into();
    assert!(
        crate::permission_resume::authorized_child_permission_resume_message(
            "changed-resume".into(),
            &policy,
            &context,
            &changed
        )
        .is_err()
    );
}

#[test]
fn exact_child_command_projection_is_tool_free_and_preserves_source_and_task_identity() {
    let context = completion_context();
    let mut session = child();
    let mut result = crate::chat::ChatMessage::text(
        "original-result",
        crate::chat::ChatRole::UntrustedOutput,
        "Original command exited with a nonzero status",
    );
    result.data_envelope = Some(
        projection::envelope(
            &result.message_id,
            &result.text,
            "original-command-result",
            &context.input_envelopes,
        )
        .unwrap(),
    );
    session.conversation.push(result.clone());
    session.conversation.push(crate::chat::ChatMessage::text(
        "unrelated",
        crate::chat::ChatRole::Assistant,
        "Unrelated history",
    ));
    let request = crate::seam::ModelRequest::text_only(
        vec![
            crate::chat::ChatMessage::text(
                "system",
                crate::chat::ChatRole::System,
                "Interpret the exact original result",
            ),
            crate::chat::ChatMessage::text(
                "irrelevant",
                crate::chat::ChatRole::User,
                "Synthetic request must be discarded",
            ),
        ],
        crate::prompt::ResponseFormatSpec::None,
    );
    let projected = crate::command_completion::project_child_request(
        request.clone(),
        &session,
        "original-result",
        &context,
    )
    .unwrap();
    assert!(projected.tools.is_empty());
    assert_eq!(
        projected
            .messages
            .iter()
            .filter(|item| item.message_id == result.message_id)
            .count(),
        1
    );
    assert!(
        projected
            .messages
            .iter()
            .all(|item| item.role != crate::chat::ChatRole::User)
    );
    assert!(
        projected
            .messages
            .iter()
            .any(|item| item == &context.instruction)
    );
    assert!(
        projected
            .messages
            .iter()
            .all(|item| item.message_id != "unrelated" && item.message_id != "irrelevant")
    );
    let mut forbidden = request;
    forbidden.tools.push(crate::chat::ToolSpec {
        name: "forbidden".into(),
        description: "No tools in interpretation".into(),
        parameters_schema: serde_json::json!({}),
    });
    assert!(
        crate::command_completion::project_child_request(
            forbidden,
            &session,
            "original-result",
            &context
        )
        .is_err()
    );
    session.agent_role = AgentRole::Main;
    assert!(
        crate::command_completion::project_child_request(
            projected,
            &session,
            "original-result",
            &context
        )
        .is_err()
    );
}

#[test]
fn child_business_preflight_uses_frozen_owner_evidence_without_inheriting_parent_authority() {
    let context = completion_context();
    let mut session = child();
    session
        .bind_delegated_owner_requirement(&context.source)
        .unwrap();
    session.conversation.push(context.instruction.clone());
    session.conversation.clear();
    assert_eq!(
        session.authorization_requirement(),
        Some(&context.source.owner_requirement)
    );
    assert!(crate::permission_resume::latest_user_requirement(&session.conversation).is_none());
    assert!(session.scope_snapshot.granted.is_empty());
    assert!(session.context_attachments.is_empty());
    assert!(session.permission_requests.is_empty());
    let recovered =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    assert_eq!(
        recovered.authorization_requirement(),
        Some(&context.source.owner_requirement)
    );
    let mut different = context.source.clone();
    different.owner_requirement = crate::model_message_labels::model_bound_user_message(
        "new-owner-message".into(),
        "A later parent request".into(),
        different.model_destination.clone(),
    )
    .unwrap();
    assert!(
        session
            .bind_delegated_owner_requirement(&different)
            .is_err()
    );
    assert_eq!(
        session.authorization_requirement(),
        Some(&context.source.owner_requirement)
    );
    session.agent_role = AgentRole::Main;
    assert!(
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).is_err()
    );
}

#[test]
fn directory_decisions_use_original_child_identity_without_parent_context_or_grants() {
    use crate::file_scope::{
        FileScopeSubject,
        transaction::{FileScopeMutation, FileScopeUpdate},
    };
    let mut session = child();
    let update = FileScopeUpdate {
        subject: FileScopeSubject {
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            conversation_id: session.conversation_id.clone(),
        },
        client_conversation_id: session.conversation_id.clone(),
        client_request_id: "owner-decision".into(),
        expected_revision: 0,
        mutation: FileScopeMutation::Decide {
            directory_request_id: "directory-request".into(),
            approve: true,
        },
    };
    assert!(session.client_conversation_id.is_none());
    update.validate_session(&session).unwrap();
    let parent_selector = FileScopeUpdate {
        client_conversation_id: "parent-user-conversation".into(),
        ..update.clone()
    };
    assert!(parent_selector.validate_session(&session).is_err());
    let sibling = FileScopeUpdate {
        subject: FileScopeSubject {
            conversation_id: "sibling".into(),
            ..update.subject.clone()
        },
        ..update.clone()
    };
    assert!(sibling.validate_session(&session).is_err());
    session.client_conversation_id = Some("parent-user-conversation".into());
    assert!(update.validate_session(&session).is_err());
}

#[test]
fn directory_approval_is_a_real_wait_and_source_resume_does_not_skip_it() {
    let mut task = run();
    task.synchronize_dependencies(
        vec![TaskDependency::DirectoryApproval {
            directory_request_id: "directory-request".into(),
        }],
        false,
        "pending",
    )
    .unwrap();
    assert_eq!(task.state, SubAgentState::WaitingApproval);
    task.pause_source(2, "paused").unwrap();
    assert_eq!(task.state, SubAgentState::WaitingSource);
    task.resume_source(3, "resumed").unwrap();
    assert_eq!(task.state, SubAgentState::WaitingApproval);
    task.synchronize_dependencies(Vec::new(), false, "owner-decision")
        .unwrap();
    assert_eq!(task.state, SubAgentState::Queued);
    assert!(!task.state.is_terminal());
}

#[test]
fn stopped_main_requires_valid_new_owner_focus_before_planning_again() {
    let mut session = PersistedAgentSession::new("root", "owner", "device", 1, scope(), "before");
    session.surface = AgentSessionSurface::AiAssistant;
    session.input_revision = 1;
    session.begin_focus_epoch(1, Vec::new()).unwrap();
    control::stop_main_session(&mut session, "stopped").unwrap();
    assert!(session.main_stopped);
    assert!(
        session
            .begin_turn("automatic-completion", None, None, 1, scope(), "later")
            .is_err()
    );
    assert!(
        session
            .begin_focus_epoch(2, vec!["missing-attachment".into()])
            .is_err()
    );
    assert!(session.main_stopped);
    session.begin_focus_epoch(2, Vec::new()).unwrap();
    assert!(!session.main_stopped);
    session
        .begin_turn("new-owner-turn", None, None, 1, scope(), "later")
        .unwrap();
}

#[test]
fn published_result_authority_survives_notice_consumption_and_dependency_waits() {
    use crate::registry::{RegisteredTool, exposed_for_session, lookup_for_session};
    let mut parent = PersistedAgentSession::new(
        "root",
        "owner",
        "device",
        1,
        scope(),
        "2026-09-30T00:00:00Z",
    );
    parent.surface = AgentSessionSurface::AiAssistant;
    parent.input_revision = 1;
    parent
        .begin_turn(
            "occurrence-turn",
            None,
            None,
            1,
            scope(),
            "2026-09-30T00:00:00Z",
        )
        .unwrap();
    parent.adopt_trigger(TriggerOrigin::ScheduledTask, "occurrence-turn");
    parent.subagent_result_only = true;
    parent
        .scope_snapshot
        .granted
        .push(desk_agent_protocol::Capability::SystemInfo);
    let mut catalog = tools::registry();
    for (name, effect) in [
        ("device_read", ToolEffect::ReadOnly),
        ("device_write", ToolEffect::Mutating),
        ("permissions", ToolEffect::PermissionPlanning),
        ("directory", ToolEffect::DirectoryPlanning),
        ("history", ToolEffect::ConversationHistory),
        ("projection", ToolEffect::RunProjection),
    ] {
        catalog.push(RegisteredTool {
            spec: crate::chat::ToolSpec {
                name: name.into(),
                description: "test".into(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
            required_capability: desk_agent_protocol::Capability::SystemInfo,
            effect,
        });
    }
    assert!(parent.trigger_origin.allows_new_mutation());
    assert!(parent.is_subagent_result_turn());
    assert!(!parent.allows_new_mutation());
    assert!(!parent.allows_delegated_review());
    assert!(parent.ready_subagent_notification.is_none());
    let exposed = exposed_for_session(&catalog, &parent);
    assert_eq!(exposed.len(), 6);
    for name in [
        tools::SPAWN,
        tools::CANCEL,
        tools::MESSAGE,
        "device_read",
        "device_write",
        "permissions",
        "directory",
    ] {
        assert!(
            lookup_for_session(&catalog, name, &parent).is_none(),
            "{name}"
        );
    }
    for name in [
        tools::LIST,
        tools::STATUS,
        tools::RESULT,
        tools::WAIT,
        "history",
        "projection",
    ] {
        assert!(
            lookup_for_session(&catalog, name, &parent).is_some(),
            "{name}"
        );
    }
    let call = ToolCall { id: "untrusted-spawn".into(), name: tools::SPAWN.into(), arguments_json: serde_json::json!({
        "name":"another branch", "task":"Investigate again", "acceptance_criteria":["Return facts"], "required_for_completion":false
    }).to_string() };
    assert!(tools::parse(&parent, &call).is_err());
    parent.begin_focus_epoch(2, Vec::new()).unwrap();
    parent.adopt_trigger(TriggerOrigin::User, "new-owner-input");
    assert!(!parent.subagent_result_only);
    assert!(parent.allows_new_mutation());
    assert!(lookup_for_session(&catalog, tools::SPAWN, &parent).is_some());
}

#[test]
fn all_and_any_waits_distinguish_partial_completion_without_ignoring_changed_dependencies() {
    let first = TaskFence {
        task_id: "first".into(),
        input_revision: 1,
        control_revision: 1,
    };
    let second = TaskFence {
        task_id: "second".into(),
        input_revision: 1,
        control_revision: 1,
    };
    for mode in [WaitMode::AllTerminal, WaitMode::AnyTerminal] {
        let wait = ParentWait {
            wait_id: "two-children".into(),
            tool_call_id: "wait-call".into(),
            result_message_id: "wait-result".into(),
            group_id: "group".into(),
            source_epoch: 1,
            retry_after_ms: None,
            parent_input_revision: 1,
            parent_control_revision: 1,
            mode,
            tasks: vec![first.clone(), second.clone()],
        };
        wait.validate().unwrap();
        assert_eq!(
            wait.evaluate(
                1,
                1,
                &[
                    (first.clone(), SubAgentState::Running),
                    (second.clone(), SubAgentState::WaitingApproval),
                ]
            ),
            WaitEvaluation::Pending
        );
        for terminal in [
            SubAgentState::Completed,
            SubAgentState::Failed,
            SubAgentState::Cancelled,
        ] {
            let expected = match mode {
                WaitMode::AllTerminal => WaitEvaluation::Pending,
                WaitMode::AnyTerminal => WaitEvaluation::Ready,
            };
            assert_eq!(
                wait.evaluate(
                    1,
                    1,
                    &[
                        (second.clone(), SubAgentState::Running),
                        (first.clone(), terminal),
                    ]
                ),
                expected
            );
            assert_eq!(
                wait.evaluate(
                    1,
                    1,
                    &[
                        (first.clone(), terminal),
                        (second.clone(), SubAgentState::Completed),
                    ]
                ),
                WaitEvaluation::Ready
            );
        }
        // Any-terminal still validates every dependency before accepting a wakeup.
        let changed = TaskFence {
            control_revision: 2,
            ..second.clone()
        };
        assert_eq!(
            wait.evaluate(
                1,
                1,
                &[
                    (first.clone(), SubAgentState::Completed),
                    (changed, SubAgentState::Running),
                ]
            ),
            WaitEvaluation::DependencyChanged
        );
        assert_eq!(
            wait.evaluate(1, 1, &[(first.clone(), SubAgentState::Completed)]),
            WaitEvaluation::DependencyChanged
        );
    }
}

#[test]
fn unfinished_limit_does_not_cap_cumulative_group_creation() {
    let mut group = group();
    let limits = policy::initial().limits;
    for index in 0..256 {
        group
            .admit_child(&format!("task-{index}"), true, 0, 1, limits)
            .unwrap();
    }
    assert_eq!(group.tasks_created, 256);
    assert!(group.validate().is_ok());
    assert!(
        group
            .admit_child("at-capacity", true, 2, 1, limits)
            .is_err()
    );
    assert_eq!(group.tasks_created, 256);
    group
        .admit_child("slot-released", true, 1, 1, limits)
        .unwrap();
    assert_eq!(group.tasks_created, 257);
    let lowered = desk_agent_protocol::ai_assistant::subagent_policy::SubAgentLimits {
        max_unfinished_per_root: 1,
    };
    assert!(group.admit_child("lowered", true, 1, 1, lowered).is_err());
    assert!(group.validate().is_ok());
    group.stop_parent().unwrap();
    assert!(group.validate().is_ok());
}

#[test]
fn only_unfinished_capacity_is_a_recognized_admission_error() {
    assert!(super::capacity_error(&super::capacity_storage_message(2)).is_some());
    assert!(super::capacity_error("delegation_group_capacity:4").is_none());
    assert!(super::capacity_error("delegation_unfinished_capacity:0").is_none());
    assert!(super::capacity_error("delegation_unfinished_capacity:33").is_none());
}

#[test]
fn normal_text_result_preserves_markdown_and_does_not_guess_business_success_or_waiting() {
    for text in [
        "## 发现\n\n中文正文，没有 JSON。",
        "无法完成：缺少资料。",
        "稍后继续处理。",
        "```json\n{\"assessment\":\"pending\"}\n```",
    ] {
        let result = report::from_answer(text, false, &CompletionFacts::default()).unwrap();
        assert_eq!(result.summary, text);
        assert_eq!(result.assessment, TaskAssessment::Complete);
        assert!(result.receipt_refs.is_empty());
        assert_eq!(
            run()
                .evaluate_report(run().fence(), &result, &CompletionFacts::default())
                .unwrap(),
            CompletionDisposition::Complete
        );
    }
    let long = "正常答复。".repeat(300);
    assert!(long.len() > 2048);
    assert_eq!(
        report::from_answer(&long, false, &CompletionFacts::default())
            .unwrap()
            .summary,
        long
    );
    assert!(report::from_answer(" ", false, &CompletionFacts::default()).is_err());
    assert!(
        report::from_answer(
            &"x".repeat(32 * 1024 + 1),
            false,
            &CompletionFacts::default()
        )
        .is_err()
    );
}

#[test]
fn text_result_uses_only_runtime_receipts_and_cannot_override_dependencies_or_unknown_actions() {
    let good = facts();
    let result = report::from_answer("已完成，receipt 是假的-id", false, &good).unwrap();
    assert_eq!(result.receipt_refs, good.accepted_receipt_ids);
    assert_eq!(result.evidence_refs, good.available_evidence_ids);
    assert!(!result.receipt_refs.contains(&"假的-id".into()));
    let mut task = run();
    task.set_dependencies(
        vec![TaskDependency::Work {
            work_id: "work".into(),
        }],
        "now",
    )
    .unwrap();
    let progress = report::from_answer("已经成功", true, &good).unwrap();
    assert_eq!(progress.assessment, TaskAssessment::Pending);
    assert_eq!(
        task.settle_report(task.fence(), progress, &good, "now")
            .unwrap(),
        CompletionDisposition::Waiting
    );
    assert_eq!(task.state, SubAgentState::WaitingWork);
    for (facts, reason) in [
        (
            CompletionFacts {
                incomplete_action_ids: vec!["unknown".into()],
                ..good.clone()
            },
            "delegated_action_outcome_unknown",
        ),
        (
            CompletionFacts {
                required_receipts: vec![RequiredReceipt {
                    succeeded: false,
                    ..good.required_receipts[0].clone()
                }],
                ..good.clone()
            },
            "delegated_required_action_failed",
        ),
        (
            CompletionFacts {
                required_receipts: vec![RequiredReceipt {
                    verification_complete: false,
                    ..good.required_receipts[0].clone()
                }],
                ..good.clone()
            },
            "delegated_action_verification_incomplete",
        ),
    ] {
        let result = report::from_answer("已完成", false, &facts).unwrap();
        assert_eq!(result.assessment, TaskAssessment::Unable);
        assert_eq!(result.reason.as_deref(), Some(reason));
        let mut task = run();
        assert_eq!(
            task.settle_report(task.fence(), result, &facts, "now")
                .unwrap(),
            CompletionDisposition::Unable
        );
        assert_eq!(task.state, SubAgentState::Failed);
    }
}
