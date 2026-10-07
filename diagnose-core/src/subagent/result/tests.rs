use super::*;
use crate::{
    chat::ToolCall,
    session::PersistedAgentSession,
    subagent::{report, state::CompletionFacts, tools},
};
use desk_agent_protocol::{AgentScope, ExecutionMode, ai_assistant::subagent::SubAgentSource};

fn result() -> AiAssistantSubAgentResult {
    AiAssistantSubAgentResult {
        task: desk_agent_protocol::ai_assistant::subagent::AiAssistantSubAgentSummary {
            task_id: "task".into(),
            child_session_id: "child".into(),
            group_id: "group".into(),
            name: "Pause test".into(),
            state: SubAgentState::Completed,
            wait_reason: None,
            input_revision: 3,
            control_revision: 4,
            state_revision: 7,
            source_goal_id: None,
            source: SubAgentSource::UserInput { input_revision: 1 },
            created_at: "2026-10-06T00:00:00Z".into(),
            updated_at: "2026-10-06T00:01:00Z".into(),
        },
        objective: "Execute sleep 60 once and report its actual outcome. ".repeat(20),
        acceptance_criteria: vec![
            "The actual receipt shows successful exit after about 60 seconds.".into(),
        ],
        report: Some(
            report::from_answer(
                "已执行 sleep 60，耗时 60.1 秒，退出码 0。",
                false,
                &CompletionFacts {
                    accepted_receipt_ids: vec!["action-result".into()],
                    available_evidence_ids: vec!["source-evidence".into()],
                    ..Default::default()
                },
            )
            .unwrap(),
        ),
        failure_reason: None,
    }
}

#[test]
fn compact_result_preserves_deliverable_criteria_facts_and_revision_with_optional_original_task() {
    let full = result();
    let compact = SubAgentModelResult::from_result(&full, false);
    let wire = serde_json::to_value(&compact).unwrap();
    assert_eq!(
        compact.answer.as_deref(),
        Some(full.report.as_ref().unwrap().summary.as_str())
    );
    assert_eq!(compact.acceptance_criteria, full.acceptance_criteria);
    assert_eq!(compact.state_revision, 7);
    assert_eq!(compact.receipt_refs, ["action-result"]);
    assert_eq!(compact.evidence_refs, ["source-evidence"]);
    for omitted in [
        "task",
        "objective",
        "findings",
        "delivered",
        "remaining",
        "reason",
        "failure_reason",
        "wait_reason",
    ] {
        assert!(wire.get(omitted).is_none(), "{omitted}");
    }
    assert!(
        serde_json::to_vec(&compact).unwrap().len() * 2 < serde_json::to_vec(&full).unwrap().len()
    );
    let detailed = SubAgentModelResult::from_result(&full, true);
    assert_eq!(detailed.objective.as_deref(), Some(full.objective.as_str()));
    assert_eq!(detailed.answer, compact.answer);
    assert_eq!(detailed.acceptance_criteria, compact.acceptance_criteria);
    assert_eq!(SubAgentModelResult::from_result(&full, false), compact);
}

#[test]
fn complex_answer_and_nonempty_deliverables_are_preserved_without_clipping_or_rewriting() {
    let mut full = result();
    let report = full.report.as_mut().unwrap();
    report.summary = "## 原始分析\n- 证据与必要细节。\n".repeat(600);
    report.findings = vec!["Original finding".into()];
    report.delivered = vec!["Original deliverable".into()];
    let expected = report.clone();
    let compact = SubAgentModelResult::from_result(&full, false);
    assert_eq!(compact.answer.as_ref(), Some(&expected.summary));
    assert_eq!(compact.findings, expected.findings);
    assert_eq!(compact.delivered, expected.delivered);
    assert!(compact.answer.unwrap().len() > 16 * 1024);
}

#[test]
fn partial_failed_unknown_cancelled_and_waiting_results_do_not_hide_outcome_facts() {
    let mut full = result();
    full.task.state = SubAgentState::Failed;
    full.failure_reason = Some("required action outcome unknown".into());
    let report = full.report.as_mut().unwrap();
    report.assessment = TaskAssessment::Unable;
    report.reason = Some("delegated_action_outcome_unknown".into());
    report.remaining = vec!["Verify the action outcome".into()];
    let compact = SubAgentModelResult::from_result(&full, false);
    assert_eq!(compact.state, SubAgentState::Failed);
    assert_eq!(compact.assessment, Some(TaskAssessment::Unable));
    assert_eq!(compact.reason, full.report.as_ref().unwrap().reason);
    assert_eq!(compact.failure_reason, full.failure_reason);
    assert_eq!(compact.remaining, ["Verify the action outcome"]);
    full.task.state = SubAgentState::WaitingWork;
    full.task.wait_reason = Some(SubAgentWaitReason::BackgroundWork);
    full.report.as_mut().unwrap().assessment = TaskAssessment::Pending;
    let waiting = SubAgentModelResult::from_result(&full, false);
    assert_eq!(
        waiting.wait_reason,
        Some(SubAgentWaitReason::BackgroundWork)
    );
    assert_eq!(waiting.assessment, Some(TaskAssessment::Pending));
    full.task.state = SubAgentState::Cancelled;
    full.report = None;
    let cancelled = serde_json::to_value(SubAgentModelResult::from_result(&full, false)).unwrap();
    assert_eq!(cancelled["state"], "cancelled");
    assert!(cancelled.get("answer").is_none());
    assert_eq!(
        cancelled["failure_reason"],
        "required action outcome unknown"
    );
}

#[test]
fn original_task_read_is_opt_in_boolean_and_cannot_expand_other_tools() {
    let mut parent = PersistedAgentSession::new(
        "root",
        "owner",
        "device",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "now",
    );
    parent.surface = crate::session::AgentSessionSurface::AiAssistant;
    parent.input_revision = 1;
    parent
        .begin_turn("turn", None, None, 1, parent.scope_snapshot.clone(), "now")
        .unwrap();
    let mut call = ToolCall {
        id: "read".into(),
        name: tools::RESULT.into(),
        arguments_json: "{\"task_id\":\"task\"}".into(),
    };
    assert_eq!(
        tools::parse(&parent, &call).unwrap(),
        tools::Operation::ReadResult {
            task_id: "task".into(),
            include_task: false
        }
    );
    call.arguments_json = "{\"task_id\":\"task\",\"include_task\":true}".into();
    assert_eq!(
        tools::parse(&parent, &call).unwrap(),
        tools::Operation::ReadResult {
            task_id: "task".into(),
            include_task: true
        }
    );
    call.name = tools::STATUS.into();
    assert!(tools::parse(&parent, &call).is_err());
    call.name = tools::RESULT.into();
    for invalid in [
        "{\"task_id\":\"task\",\"include_task\":1}",
        "{\"task_id\":\"task\",\"include_task\":\"true\"}",
        "{\"task_id\":\"task\",\"untrusted_extra\":true}",
    ] {
        call.arguments_json = invalid.into();
        assert!(tools::parse(&parent, &call).is_err());
    }
}
