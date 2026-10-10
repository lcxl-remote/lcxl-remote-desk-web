use super::*;
use crate::goal::GoalUsage;
use crate::subagent::{
    DelegatedTaskBinding, DelegationSource,
    creation::CreationEnvelope,
    reservation::{CallAdmission, DelegationCallKind, DelegationCallReservation},
    seam::{ChildAdmission, SubAgentSeam, ToolReceipt},
    state::{CompletionDisposition, CompletionFacts, PlanningFence, SubAgentRun},
    tools::Operation,
};
use std::cell::Cell;

struct HealthyChildLease;
impl crate::seam::LeaseHeartbeat for HealthyChildLease {
    fn start(&self, _: String, _: u64) -> Box<dyn crate::seam::HeartbeatGuard> {
        Box::new(NoopHeartbeatGuard)
    }
    fn is_healthy(&self) -> bool {
        true
    }
}

struct AuditedChildModel<'a>(&'a ScriptModel);
#[async_trait(?Send)]
impl ModelSeam for AuditedChildModel<'_> {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        Ok(request
            .caller_output_hard_cap
            .unwrap_or(128_000)
            .min(128_000))
    }

    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(super::scheduled_continuation::scheduled_policy()))
    }
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.0.context_policy(requirements).await
    }
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        let policy = super::scheduled_continuation::scheduled_policy();
        let authorized = policy
            .authorize_request(request)
            .unwrap_or_else(|error| panic!("child request audit: {error:?}"));
        let mut turn = self.0.call(authorized.request, sink).await?;
        // Discarded empty responses have no bytes to persist. Other scripted
        // responses use the same output-lineage derivation as the live adapter.
        if !crate::model_egress::model_turn_content_bytes(&turn)
            .unwrap()
            .is_empty()
        {
            turn.provider_meta.data_envelope = Some(
                policy
                    .derive_model_output_envelope(&turn, &authorized.input_envelopes)
                    .map_err(|error| error.agent_error())?,
            );
        }
        Ok(turn)
    }
}

struct AuditedChildTools {
    inner: RecordingTools,
    label: DataEnvelope,
}
#[async_trait(?Send)]
impl ToolSeam for AuditedChildTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.inner.run_read(call).await
    }
    fn read_data_envelope(
        &self,
        call: &ToolCall,
        output: &ToolRunOutput,
    ) -> Result<Option<DataEnvelope>, AgentError> {
        crate::model_message_labels::internal_tool_result_envelope(
            Some(&self.label),
            &call.id,
            &output.content,
            &call.name,
        )
    }
}

struct ChildSessions {
    budget: super::delegation_budget::BudgetSessions,
    run: RefCell<SubAgentRun>,
    paused: Cell<bool>,
}

impl ChildSessions {
    fn prepared() -> (Self, PersistedAgentSession) {
        let now = "2026-06-20T00:00:00Z";
        let binding = DelegatedTaskBinding {
            root_conversation_id: "root".into(),
            group_id: "group".into(),
            task_id: "task".into(),
            source: DelegationSource::UserInput { input_revision: 1 },
            objective: "Investigate one symptom".into(),
            acceptance_criteria: vec!["Return factual findings".into()],
            input_revision: 1,
            control_revision: 1,
            source_epoch: 1,
            deadline_ms: 2_000_000_000_000,
        };
        let mut child = PersistedAgentSession::new_subagent(
            "child",
            "actor",
            "device",
            1,
            scope(),
            binding.clone(),
            now,
        )
        .unwrap();
        let owner = crate::model_message_labels::model_bound_user_message(
            "source".into(),
            "Investigate independent symptoms".into(),
            super::scheduled_continuation::scheduled_policy().destination,
        )
        .unwrap();
        let mut parent = PersistedAgentSession::new("root", "actor", "device", 1, scope(), now);
        parent.surface = crate::session::AgentSessionSurface::AiAssistant;
        parent.input_revision = 1;
        parent.control_revision = 1;
        let source =
            CreationEnvelope::capture(&parent, binding.source.clone(), owner, None).unwrap();
        child.bind_delegated_owner_requirement(&source).unwrap();
        child
            .conversation
            .push(source.child_source_message("child").unwrap());
        child
            .begin_turn("child-turn", None, None, 1, scope(), now)
            .unwrap();
        child.adopt_trigger(TriggerOrigin::DelegatedTask, "child-turn");
        let mut run = SubAgentRun {
            child_conversation_id: "child".into(),
            actor_id: "actor".into(),
            device_id: "device".into(),
            name: "One symptom".into(),
            binding,
            state: crate::subagent::SubAgentState::Queued,
            state_revision: 1,
            source_paused: false,
            dependencies: Vec::new(),
            partial_report: None,
            terminal_report: None,
            failure_reason: None,
            report_corrections_used: 0,
            created_at: now.into(),
            updated_at: now.into(),
        };
        run.claim_planning(run.fence(), 1_000, now).unwrap();
        let budget = super::delegation_budget::BudgetSessions::default();
        *budget.sessions.inner.borrow_mut() = Some(child.clone());
        (
            Self {
                budget,
                run: RefCell::new(run),
                paused: Cell::new(false),
            },
            child,
        )
    }
}

#[async_trait(?Send)]
impl SessionSeam for ChildSessions {
    async fn claim_turn(&self, _: ClaimTurnParams) -> Result<PersistedAgentSession, ClaimError> {
        panic!("child execution must keep its preclaimed lease");
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.run.borrow_mut().report_corrections_used = session.subagent_report_corrections_used;
        self.budget.save(session).await
    }
    async fn latest_input_revision(&self, id: &str) -> Result<Option<u64>, AgentError> {
        self.budget.sessions.latest_input_revision(id).await
    }
    fn subagents(&self, _: &PersistedAgentSession) -> Option<&dyn SubAgentSeam> {
        Some(self)
    }
    async fn reserve_delegation_call(
        &self,
        session: &PersistedAgentSession,
        id: &str,
        kind: DelegationCallKind,
        digest: &str,
        upper: GoalUsage,
        now: &str,
    ) -> Result<CallAdmission, AgentError> {
        self.budget
            .reserve_delegation_call(session, id, kind, digest, upper, now)
            .await
    }
    async fn settle_delegation_call(
        &self,
        receipt: &DelegationCallReservation,
        actual: Option<GoalUsage>,
        now: &str,
    ) -> Result<(), AgentError> {
        self.budget
            .settle_delegation_call(receipt, actual, now)
            .await
    }
}

#[async_trait(?Send)]
impl SubAgentSeam for ChildSessions {
    async fn planning_projection(
        &self,
        session: &PersistedAgentSession,
    ) -> Result<Option<ChatMessage>, AgentError> {
        let binding = session.agent_role.binding().unwrap();
        let label = session.conversation[0].data_envelope.clone().unwrap();
        crate::subagent::projection::runtime_message(
            "child-projection",
            &serde_json::json!({"objective": binding.objective,
            "acceptance_criteria": binding.acceptance_criteria}),
            &[label],
        )
        .map(Some)
    }
    async fn execute(
        &self,
        _: &mut PersistedAgentSession,
        _: &ToolCall,
        _: Operation,
        _: &str,
        _: &crate::model_observability::tool::ToolObservation,
    ) -> Result<ToolReceipt, AgentError> {
        panic!("a child cannot reach delegation tools");
    }
    async fn required_children_complete(
        &self,
        _: &PersistedAgentSession,
    ) -> Result<bool, AgentError> {
        panic!("a child cannot control a goal");
    }
    async fn validate_child_admission(
        &self,
        _: &PersistedAgentSession,
    ) -> Result<ChildAdmission, AgentError> {
        Ok(if self.paused.get() {
            ChildAdmission::SourcePaused
        } else {
            ChildAdmission::Admitted
        })
    }
    async fn evaluate_child_answer(
        &self,
        _: &PersistedAgentSession,
        expected: PlanningFence,
        answer: &str,
    ) -> Result<CompletionDisposition, AgentError> {
        let run = self.run.borrow();
        let facts = CompletionFacts::default();
        let report =
            crate::subagent::report::from_answer(answer, !run.dependencies.is_empty(), &facts)?;
        run.evaluate_report(expected, &report, &facts)
            .map_err(crate::subagent::invalid)
    }
    async fn settle_child_answer(
        &self,
        session: &mut PersistedAgentSession,
        expected: PlanningFence,
        answer: String,
    ) -> Result<CompletionDisposition, AgentError> {
        let report = crate::subagent::report::from_answer(
            &answer,
            !self.run.borrow().dependencies.is_empty(),
            &CompletionFacts::default(),
        )?;
        let disposition = self
            .run
            .borrow_mut()
            .settle_report(
                expected,
                report,
                &CompletionFacts::default(),
                "2026-06-20T00:00:01Z",
            )
            .map_err(crate::subagent::invalid)?;
        self.save(session).await?;
        Ok(disposition)
    }
    async fn settle_child_turn(
        &self,
        session: &mut PersistedAgentSession,
        failure: Option<&str>,
        _: bool,
    ) -> Result<(), AgentError> {
        if let Some(reason) = failure {
            self.run
                .borrow_mut()
                .fail(reason, "2026-06-20T00:00:01Z")
                .unwrap();
        }
        self.save(session).await
    }
}

fn final_report() -> String {
    serde_json::json!({"assessment": "complete", "summary": "Investigation finished", "findings": ["No device change was needed"],
        "delivered": ["Findings"], "remaining": [], "evidence_refs": [], "receipt_refs": [], "reason": null}).to_string()
}

#[tokio::test]
async fn claimed_child_returns_normal_text_without_report_repair_or_extra_call() {
    let (sessions, child) = ChildSessions::prepared();
    let script = ScriptModel {
        turns: RefCell::new(
            [thinking_answer(
                "无法完成全部调查：当前缺少业务资料。\n\n已有发现：未修改设备。",
            )]
            .into(),
        ),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let model = super::scheduled_continuation::ScheduledModel(&script);
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".into();
    let heartbeat = HealthyChildLease;
    let mut runtime = deps(&sessions.budget.sessions, &model, &tools, &[], &clock);
    runtime.session_seam = &sessions;
    runtime.heartbeat = Some(&heartbeat);
    runtime.response_format = crate::prompt::ResponseFormatSpec::JsonSchema {
        name: "parent-output".into(),
        schema: serde_json::json!({"type":"object"}),
    };
    let outcome = run_preclaimed_subagent_turn(&runtime, child, &mut NullTurnSink)
        .await
        .unwrap();
    assert!(matches!(outcome, LoopOutcome::Answered(_)));
    assert_eq!(
        sessions.run.borrow().state,
        crate::subagent::SubAgentState::Completed
    );
    assert_eq!(sessions.run.borrow().report_corrections_used, 0);
    assert_eq!(
        sessions
            .run
            .borrow()
            .terminal_report
            .as_ref()
            .unwrap()
            .summary,
        "无法完成全部调查：当前缺少业务资料。\n\n已有发现：未修改设备。"
    );
    assert_eq!(script.requests.borrow().len(), 1);
    assert!(script.requests.borrow().iter().all(|request| matches!(
        request.response_format,
        crate::prompt::ResponseFormatSpec::None
    )));
    assert_eq!(sessions.budget.reservations.borrow().len(), 1);
    assert!(
        sessions
            .budget
            .reservations
            .borrow()
            .iter()
            .all(|receipt| receipt.task_id.as_deref() == Some("task")
                && receipt.root_conversation_id == "root")
    );
    assert!(script.requests.borrow().iter().all(|request| {
        request
            .messages
            .iter()
            .any(|message| message.text.contains("Investigate one symptom"))
    }));
    assert!(tools.calls.borrow().is_empty());
    assert!(script.requests.borrow()[0].messages.iter().all(|message| {
        !message.text.contains("delegated_report_correction")
            && !message.text.contains("MUST be one report JSON")
    }));
    let stored = sessions.budget.sessions.inner.borrow();
    let final_answer = stored
        .as_ref()
        .unwrap()
        .conversation
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::Assistant)
        .unwrap();
    assert!(matches!(
        final_answer.replay_disposition,
        Some(ReplayDisposition::Present { .. })
    ));
}

#[tokio::test]
async fn source_pause_before_planning_neither_calls_a_model_nor_completes_the_task() {
    let (sessions, child) = ChildSessions::prepared();
    sessions.paused.set(true);
    let model = ScriptModel {
        turns: RefCell::new([answer(&final_report())].into()),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(Vec::new())),
        reply: "unused".into(),
    };
    let clock = || "2026-06-20T00:00:01Z".into();
    let heartbeat = HealthyChildLease;
    let mut runtime = deps(&sessions.budget.sessions, &model, &tools, &[], &clock);
    runtime.session_seam = &sessions;
    runtime.heartbeat = Some(&heartbeat);
    let outcome = run_preclaimed_subagent_turn(&runtime, child, &mut NullTurnSink)
        .await
        .unwrap();
    assert!(matches!(outcome, LoopOutcome::DelegationSourcePaused));
    assert!(model.requests.borrow().is_empty());
    assert!(sessions.budget.reservations.borrow().is_empty());
    assert!(sessions.run.borrow().terminal_report.is_none());
}

#[tokio::test]
async fn child_recovery_notices_keep_delegated_model_authority_without_user_messages() {
    let truncated = ModelTurn {
        text: "partial report".into(),
        stop_reason: StopReason::MaxTokens,
        provider_meta: ProviderResponseMeta::without_reasoning(StopReason::MaxTokens),
        ..Default::default()
    };
    let malformed_plan = tool_use_args(
        "malformed-plan",
        crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME,
        "{\"items\":[",
    );
    for (initial, marker_kind) in [
        (vec![answer("")], "empty_end_turn_retry"),
        (vec![truncated], "truncated_turn_retry"),
        (
            vec![tool_use("read", "sysinfo"), malformed_plan],
            "post_tool_permission_protocol_retry",
        ),
    ] {
        let (sessions, child) = ChildSessions::prepared();
        assert!(
            child
                .conversation
                .iter()
                .all(|message| message.role != ChatRole::User)
        );
        let projection_label = sessions
            .planning_projection(&child)
            .await
            .unwrap()
            .unwrap()
            .data_envelope
            .unwrap();
        let script = ScriptModel {
            turns: RefCell::new(
                initial
                    .into_iter()
                    .chain([answer(&final_report())])
                    .collect(),
            ),
            requests: Rc::new(RefCell::new(Vec::new())),
        };
        let model = AuditedChildModel(&script);
        let tools = AuditedChildTools {
            inner: RecordingTools {
                calls: Rc::new(RefCell::new(Vec::new())),
                reply: "read completed".into(),
            },
            label: projection_label.clone(),
        };
        let registry = vec![
            read_tool("sysinfo", Capability::SystemInfo),
            RegisteredTool {
                spec: ToolSpec {
                    name: crate::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME.into(),
                    description: "request permission".into(),
                    parameters_schema: serde_json::json!({"type": "object"}),
                },
                required_capability: Capability::SystemInfo,
                effect: ToolEffect::RunProjection,
            },
        ];
        let clock = || "2026-06-20T00:00:01Z".into();
        let heartbeat = HealthyChildLease;
        let mut runtime = deps(&sessions.budget.sessions, &model, &tools, &registry, &clock);
        runtime.session_seam = &sessions;
        runtime.heartbeat = Some(&heartbeat);
        let outcome = run_preclaimed_subagent_turn(&runtime, child, &mut NullTurnSink)
            .await
            .unwrap();
        assert!(
            matches!(outcome, LoopOutcome::Answered(_)),
            "{marker_kind}: {outcome:?}"
        );
        let requests = script.requests.borrow();
        let recovery = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|message| {
                message
                    .data_envelope
                    .as_ref()
                    .is_some_and(|envelope| envelope.provenance.source_tool_name == marker_kind)
            })
            .unwrap_or_else(|| panic!("missing labelled {marker_kind}"));
        assert_eq!(recovery.role, ChatRole::SystemEvent);
        let label = recovery.data_envelope.as_ref().unwrap();
        assert_eq!(
            label.allowed_destinations,
            projection_label.allowed_destinations
        );
        assert_eq!(
            label.provenance.source_envelope_ids,
            vec![projection_label.envelope_id]
        );
        for request in requests.iter() {
            super::scheduled_continuation::scheduled_policy()
                .authorize_request(request.clone())
                .unwrap();
            assert!(
                request
                    .messages
                    .iter()
                    .all(|message| message.role != ChatRole::User)
            );
        }
        assert_eq!(
            sessions.budget.reservations.borrow().len(),
            requests.len() + tools.inner.calls.borrow().len()
        );
        assert_eq!(
            tools.inner.calls.borrow().len(),
            usize::from(marker_kind == "post_tool_permission_protocol_retry")
        );
        assert_eq!(
            sessions.run.borrow().state,
            crate::subagent::SubAgentState::Completed
        );
    }
}

#[tokio::test]
async fn child_cannot_reach_delegation_goal_or_schedule_creation_even_when_model_returns_them() {
    for name in [
        crate::subagent::tools::SPAWN,
        crate::goal_tools::REQUEST_GOAL_TOOL_NAME,
        crate::schedule::proposal::REQUEST_SCHEDULE,
    ] {
        let (sessions, child) = ChildSessions::prepared();
        let script = ScriptModel {
            turns: RefCell::new([tool_use("forbidden-call", name), answer(&final_report())].into()),
            requests: Rc::new(RefCell::new(Vec::new())),
        };
        let model = super::scheduled_continuation::ScheduledModel(&script);
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(Vec::new())),
            reply: "unused".into(),
        };
        let mut registry = crate::subagent::tools::registry();
        registry.extend(crate::goal_tools::open_registry());
        registry.extend(crate::schedule::proposal::registry());
        let clock = || "2026-06-20T00:00:01Z".into();
        let heartbeat = HealthyChildLease;
        let mut runtime = deps(&sessions.budget.sessions, &model, &tools, &registry, &clock);
        runtime.session_seam = &sessions;
        runtime.heartbeat = Some(&heartbeat);
        run_preclaimed_subagent_turn(&runtime, child, &mut NullTurnSink)
            .await
            .unwrap();
        assert!(
            script
                .requests
                .borrow()
                .iter()
                .all(|request| request.tools.iter().all(|tool| tool.name != name))
        );
        assert!(tools.calls.borrow().is_empty());
        let saved = sessions.budget.sessions.inner.borrow();
        assert!(
            saved
                .as_ref()
                .unwrap()
                .conversation
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some("forbidden-call"))
        );
        assert!(saved.as_ref().unwrap().agent_role.binding().is_some());
    }
}
