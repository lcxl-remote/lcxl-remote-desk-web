use super::*;
use crate::goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalRun, GoalSegmentEnd};

fn fixture(background: bool) -> (MemSession, PersistedAgentSession) {
    let now = "2026-06-20T00:00:00Z";
    let at = chrono::DateTime::parse_from_rfc3339(now)
        .unwrap()
        .timestamp_millis() as u64;
    let mut session = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), now);
    session.surface = AgentSessionSurface::AiAssistant;
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    session.conversation.push(
        crate::model_message_labels::model_bound_user_message(
            "user".into(),
            "Finish the verified report".into(),
            destination,
        )
        .unwrap(),
    );
    let mut goal = GoalRun::new(
        "goal".into(),
        "conv".into(),
        "actor".into(),
        "device".into(),
        "Finish the verified report".into(),
        "user".into(),
        GoalOpening::OwnerRequest,
        GoalModelBinding {
            connection_id: "gateway".into(),
            connection_revision: 1,
            profile_revision: 1,
            model_id: "model".into(),
        },
        1,
        at - 1000,
        GoalLimits {
            model_tokens: 10_000_000,
            ..GoalLimits::default()
        },
    )
    .unwrap();
    goal.claim_slice(at).unwrap();
    session.begin_goal_segment("goal", "user", 1, 1, 1).unwrap();
    session
        .begin_turn("turn-1", Some("req".into()), None, 1, scope(), now)
        .unwrap();
    session.adopt_trigger(TriggerOrigin::GoalContinuation, "turn-1");
    if background {
        session.execution_state = ExecutionState::Executing {
            action: crate::session::ActionIdentity::agent_exec(1, "task", "generation"),
        };
    }
    let store = MemSession {
        goal: RefCell::new(Some(goal)),
        inner: RefCell::new(Some(session.clone())),
        ..Default::default()
    };
    (store, session)
}

async fn run_script(
    turns: Vec<ModelTurn>,
    background: bool,
) -> (LoopOutcome, MemSession, Vec<ModelRequest>) {
    let (store, session) = fixture(background);
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(turns.into()),
        requests: requests.clone(),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "observed".into(),
    };
    let registry = crate::goal_tools::registry();
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let result = run_preclaimed_goal_slice(
        &deps(&store, &model, &tools, &registry, &clock),
        session,
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    let captured = requests.borrow().clone();
    (result, store, captured)
}

#[tokio::test]
async fn normal_background_work_can_continue_and_wait_without_model_task_ids() {
    for decision in ["continue", "wait"] {
        let (outcome, store, requests) = run_script(
            vec![tool_use_args(
                "control",
                "control_goal",
                &serde_json::json!({"decision":decision,"message":"Next step"}).to_string(),
            )],
            true,
        )
        .await;
        assert!(matches!(outcome, LoopOutcome::GoalControlled { .. }));
        let ends = store.goal_ends.borrow();
        if decision == "wait" {
            let GoalSegmentEnd::Control(crate::goal::GoalControl::WaitForWork { wait, .. }) =
                &ends[0]
            else {
                panic!("native wait missing")
            };
            assert_eq!(wait.members[0].action_request_id, "task");
        }
        let schema = &requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "control_goal")
            .unwrap()
            .parameters_schema;
        assert_eq!(
            schema["required"],
            serde_json::json!(["decision", "message"])
        );
    }
}

#[tokio::test]
async fn an_early_complete_is_a_correctable_tool_result() {
    let (outcome, store, requests) = run_script(
        vec![
            tool_use_args(
                "complete",
                "control_goal",
                r#"{"decision":"complete","message":"done"}"#,
            ),
            tool_use_args(
                "wait",
                "control_goal",
                r#"{"decision":"wait","message":"Wait for results"}"#,
            ),
        ],
        true,
    )
    .await;
    assert!(matches!(
        outcome,
        LoopOutcome::GoalControlled {
            control: crate::goal::GoalControl::WaitForWork { .. },
            ..
        }
    ));
    assert_eq!(store.goal_ends.borrow().len(), 1);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.text.contains("goal_dependencies_pending"))
    );
}

#[tokio::test]
async fn ordinary_text_with_background_work_does_not_infer_a_wait() {
    let (outcome, store, _) = run_script(
        vec![
            answer("Should I change the target?"),
            answer("Please clarify"),
        ],
        true,
    )
    .await;
    assert!(matches!(outcome, LoopOutcome::Answered(_)));
    assert_eq!(
        *store.goal_ends.borrow(),
        vec![GoalSegmentEnd::Pause(
            crate::goal::GoalPauseReason::NeedsNextStep
        )]
    );
}

#[tokio::test]
async fn normal_step_limit_yields_without_claiming_a_goal_completed() {
    let (store, mut session) = fixture(true);
    session.current_turn_steps = 1;
    let model = ScriptModel {
        turns: RefCell::new(vec![].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = crate::goal_tools::registry();
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut dependencies = deps(&store, &model, &tools, &registry, &clock);
    dependencies.max_steps_per_turn = 1;
    let outcome = run_preclaimed_goal_slice(&dependencies, session, &mut NullTurnSink)
        .await
        .unwrap();
    assert_eq!(outcome, LoopOutcome::GoalYielded);
    assert_eq!(*store.goal_ends.borrow(), vec![GoalSegmentEnd::Yield]);
    assert!(model.requests.borrow().is_empty());
}

#[tokio::test]
async fn the_current_complete_call_is_not_its_own_pending_dependency() {
    let mut turn = tool_use_args(
        "complete",
        "control_goal",
        r#"{"decision":"complete","message":"Verified report finished"}"#,
    );
    turn.text = "The report is verified".into();
    turn.provider_meta.data_envelope = crate::model_message_labels::model_bound_user_message(
        "model-response".into(),
        turn.text.clone(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap()
    .data_envelope;
    let (outcome, _, _) = run_script(vec![turn], false).await;
    assert!(matches!(
        outcome,
        LoopOutcome::GoalControlled {
            control: crate::goal::GoalControl::Complete { .. },
            ..
        }
    ));
}

struct FailingModel(AgentErrorKind);
#[async_trait(?Send)]
impl ModelSeam for FailingModel {
    fn model_output_token_limit(&self, _: &ModelRequest) -> Result<i64, AgentError> {
        Ok(128_000)
    }
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        ScriptModel {
            turns: RefCell::new(vec![].into()),
            requests: Rc::new(RefCell::new(vec![])),
        }
        .context_policy(requirements)
        .await
    }
    async fn call(&self, _: ModelRequest, _: &mut dyn TurnSink) -> Result<ModelTurn, AgentError> {
        Err(AgentError {
            kind: self.0,
            message: "model not available".into(),
            retryable: self.0 == AgentErrorKind::ModelUnavailable,
            safe_for_model: true,
            error_code: None,
        })
    }
}

#[tokio::test]
async fn normal_background_work_keeps_model_errors_out_of_recovery() {
    for kind in [
        AgentErrorKind::ModelUnavailable,
        AgentErrorKind::ModelRejected,
    ] {
        let (store, session) = fixture(true);
        let model = FailingModel(kind);
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let registry = crate::goal_tools::registry();
        let clock = || "2026-06-20T00:00:01Z".to_string();
        assert!(
            run_preclaimed_goal_slice(
                &deps(&store, &model, &tools, &registry, &clock),
                session,
                &mut NullTurnSink
            )
            .await
            .is_err()
        );
        let ends = store.goal_ends.borrow();
        if kind == AgentErrorKind::ModelUnavailable {
            assert!(matches!(ends[0], GoalSegmentEnd::WaitForModel { .. }));
        } else {
            assert_eq!(ends[0], GoalSegmentEnd::BlockModel);
        }
        assert_eq!(
            store
                .inner
                .borrow()
                .as_ref()
                .unwrap()
                .execution_state
                .tasks()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn a_missing_goal_control_correction_is_not_a_normal_step_yield() {
    let (store, session) = fixture(true);
    let model = ScriptModel {
        turns: RefCell::new(vec![answer("Waiting for work")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = crate::goal_tools::registry();
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut dependencies = deps(&store, &model, &tools, &registry, &clock);
    dependencies.max_steps_per_turn = 1;
    let outcome = run_preclaimed_goal_slice(&dependencies, session, &mut NullTurnSink)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        LoopOutcome::Answered(_) | LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)
    ));
    assert_eq!(
        *store.goal_ends.borrow(),
        vec![GoalSegmentEnd::Pause(
            crate::goal::GoalPauseReason::NeedsNextStep
        )]
    );
}

#[tokio::test]
async fn a_ready_receipt_is_included_once_without_a_formatting_marker() {
    let (store, mut session) = fixture(false);
    let mut wait = crate::goal_continuation::GoalWorkWait::new(
        store.goal.borrow().as_ref().unwrap(),
        &session,
        "wait",
        vec![crate::session::ActionIdentity::agent_exec(
            1,
            "task",
            "generation",
        )],
    )
    .unwrap();
    wait.ready = true;
    wait.ready_event_ids = vec!["receipt".into()];
    store.goal.borrow_mut().as_mut().unwrap().work_wait = Some(wait);
    let mut receipt = crate::model_message_labels::model_bound_user_message(
        "receipt".into(),
        "Result verified".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    receipt.role = ChatRole::UntrustedOutput;
    receipt.background_task_id = Some("task".into());
    assert!(receipt.pending_delivery_format.is_none());
    session.conversation.push(receipt);
    let model = ScriptModel {
        turns: RefCell::new(
            vec![tool_use_args(
                "continue",
                "control_goal",
                r#"{"decision":"continue","message":"Process result"}"#,
            )]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = crate::goal_tools::registry();
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let outcome = run_preclaimed_goal_slice(
        &deps(&store, &model, &tools, &registry, &clock),
        session,
        &mut NullTurnSink,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::GoalControlled { .. }));
    assert_eq!(
        model.requests.borrow()[0]
            .messages
            .iter()
            .filter(|message| message.message_id == "receipt")
            .count(),
        1
    );
}
