use super::*;
use std::cell::Cell;

/// A second claim is an error: the runtime already owns the occurrence/session.
struct PreclaimedStore<'a>(&'a MemSession);
#[async_trait(?Send)]
impl SessionSeam for PreclaimedStore<'_> {
    async fn claim_turn(&self, _: ClaimTurnParams) -> Result<PersistedAgentSession, ClaimError> {
        panic!("scheduled execution must not claim the session again");
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        self.0.save(session).await
    }
    async fn latest_input_revision(&self, id: &str) -> Result<Option<u64>, AgentError> {
        self.0.latest_input_revision(id).await
    }
    async fn settle_superseded(
        &self,
        session: &PersistedAgentSession,
        now: &str,
    ) -> Result<bool, AgentError> {
        self.0.settle_superseded(session, now).await
    }
}
struct ScheduledHeartbeat {
    healthy: Cell<bool>,
    current: Cell<bool>,
    starts: RefCell<Vec<(String, u64)>>,
    drops: Rc<Cell<usize>>,
}
struct ScheduledGuard(Rc<Cell<usize>>);
impl crate::seam::HeartbeatGuard for ScheduledGuard {}
impl Drop for ScheduledGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}
impl crate::seam::LeaseHeartbeat for ScheduledHeartbeat {
    fn start(&self, conversation_id: String, token: u64) -> Box<dyn crate::seam::HeartbeatGuard> {
        self.starts.borrow_mut().push((conversation_id, token));
        Box::new(ScheduledGuard(self.drops.clone()))
    }
    fn check_current(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + '_>> {
        Box::pin(async move { self.healthy.get() && self.current.get() })
    }
    fn is_healthy(&self) -> bool {
        self.healthy.get()
    }
}
fn scheduled_policy() -> crate::model_egress::ModelEgressPolicy {
    crate::model_egress::ModelEgressPolicy {
        destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
        selected_source_tools: Default::default(),
        export_authorization_id: "scheduled-export".into(),
        now_unix_ms: 1_000,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: true,
    }
}
struct ScheduledModel<'a>(&'a dyn ModelSeam);
#[async_trait(?Send)]
impl ModelSeam for ScheduledModel<'_> {
    fn model_egress_policy(
        &self,
    ) -> Result<Option<crate::model_egress::ModelEgressPolicy>, AgentError> {
        Ok(Some(scheduled_policy()))
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
        self.0.call(request, sink).await
    }
}
fn claimed() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "conversation",
        "owner",
        "device",
        1,
        scope(),
        "2026-09-06T00:00:00Z",
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.begin_focus_epoch(1, Vec::<String>::new()).unwrap();
    session.input_revision = 1;
    session.chain_id = "original-chain".into();
    session.automation_turns_used = 2;
    session.conversation.push(
        crate::model_message_labels::model_bound_user_message(
            "original".into(),
            "Report device status after the scheduled time.".into(),
            scheduled_policy().destination,
        )
        .unwrap(),
    );
    session
        .begin_turn(
            "scheduled-turn",
            Some("scheduled-run".into()),
            None,
            1,
            scope(),
            "2026-09-06T00:00:00Z",
        )
        .unwrap();
    session.adopt_trigger(
        crate::session::TriggerOrigin::ScheduledContinuation,
        "scheduled-turn",
    );
    session
}

#[tokio::test]
async fn preclaimed_turn_reaches_the_model_without_resetting_or_reclaiming_input() {
    exercise_preclaimed(false).await;
}

#[tokio::test]
async fn scheduled_permission_decision_uses_original_input_without_a_second_claim() {
    exercise_preclaimed(true).await;
}

async fn exercise_preclaimed(permission: bool) {
    let mem = MemSession::default();
    let store = PreclaimedStore(&mem);
    let model = ScriptModel {
        turns: RefCell::new([answer("scheduled answer")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-09-06T00:00:01Z".to_string();
    let heartbeat = ScheduledHeartbeat {
        healthy: Cell::new(true),
        current: Cell::new(true),
        starts: RefCell::new(vec![]),
        drops: Rc::new(Cell::new(0)),
    };
    let scheduled_model = ScheduledModel(&model);
    let mut deps = deps(&mem, &scheduled_model, &tools, &[], &clock);
    deps.session_seam = &store;
    deps.heartbeat = Some(&heartbeat);
    let mut session = claimed();
    if permission {
        session.permission_requests.push(serde_json::from_value(serde_json::json!({
            "schema_version":1, "request_id":"decision", "input_revision":1,
            "state":"denied", "created_at":"2026-09-06T00:00:00Z", "items":[{
                "item_id":"read", "provider_id":"desktop.session", "tool_name":"inspect_desktop_session",
                "expected_effect":"read_device", "resource_scope":["target:current_device"],
                "operation_scope":["observe"], "suggested_ttl_seconds":120,
                "suggested_max_uses":1, "reason":"Inspect current device"
            }]
        })).unwrap());
    }
    let lease = session.lease_token;
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let outcome = if permission {
        assert!(
            resume_claimed_scheduled_permission_turn(
                &deps,
                session.clone(),
                "scheduled-run",
                "wrong-request",
                &mut sink
            )
            .await
            .is_err()
        );
        for state in [
            crate::dynamic_run::PermissionRequestState::Pending,
            crate::dynamic_run::PermissionRequestState::Replaced,
            crate::dynamic_run::PermissionRequestState::Withdrawn,
        ] {
            let mut invalid = session.clone();
            invalid.permission_requests[0].state = state;
            assert!(
                resume_claimed_scheduled_permission_turn(
                    &deps,
                    invalid,
                    "scheduled-run",
                    "decision",
                    &mut sink
                )
                .await
                .is_err()
            );
        }
        assert!(model.requests.borrow().is_empty());
        assert!(mem.inner.borrow().is_none());
        resume_claimed_scheduled_permission_turn(
            &deps,
            session,
            "scheduled-run",
            "decision",
            &mut sink,
        )
        .await
    } else {
        resume_claimed_scheduled_turn(&deps, session, "scheduled-run", &mut sink).await
    }
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::Answered(_)));
    assert_eq!(model.requests.borrow().len(), 1);
    assert!(tools.calls.borrow().is_empty());
    let saved = mem.inner.borrow().as_ref().unwrap().clone();
    assert_eq!(saved.input_revision, 1);
    assert_eq!(saved.chain_id, "original-chain");
    assert_eq!(saved.automation_turns_used, 2);
    assert_eq!(saved.lease_token, lease);
    assert_eq!(saved.turn_state, TurnState::Idle);
    assert_eq!(
        saved
            .conversation
            .iter()
            .filter(|m| m.role == ChatRole::User
                && !crate::permission_resume::is_resume_control_message(m))
            .count(),
        1
    );
    assert_eq!(
        *heartbeat.starts.borrow(),
        vec![("conversation".into(), lease)]
    );
    assert_eq!(heartbeat.drops.get(), 1);
    let requests = model.requests.borrow();
    let bridges: Vec<_> = requests[0]
        .messages
        .iter()
        .filter(|message| crate::permission_resume::is_resume_control_message(message))
        .collect();
    assert_eq!(bridges.len(), 1);
    assert_eq!(
        crate::permission_resume::is_permission_resume_message(bridges[0]),
        permission
    );
    assert_eq!(
        saved.trigger_origin,
        crate::session::TriggerOrigin::ScheduledContinuation
    );
    assert_eq!(bridges[0].turn_id.as_deref(), Some("scheduled-turn"));
    assert!(bridges[0].message_id.contains("scheduled-turn"));
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|message| message.message_id.starts_with("runtime-latest-input-"))
    );
    assert_eq!(
        crate::permission_resume::latest_user_requirement(&saved.conversation)
            .unwrap()
            .message_id,
        "original"
    );
}

#[tokio::test]
async fn invalid_binding_or_missing_heartbeat_never_saves_or_calls_the_model() {
    for invalid in 0..3 {
        let mem = MemSession::default();
        let store = PreclaimedStore(&mem);
        let model = ScriptModel {
            turns: RefCell::new([answer("must not happen")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let clock = || "2026-09-06T00:00:01Z".to_string();
        let heartbeat = ScheduledHeartbeat {
            healthy: Cell::new(true),
            current: Cell::new(true),
            starts: RefCell::new(vec![]),
            drops: Rc::new(Cell::new(0)),
        };
        let scheduled_model = ScheduledModel(&model);
        let mut deps = deps(&mem, &scheduled_model, &tools, &[], &clock);
        deps.session_seam = &store;
        if invalid != 2 {
            deps.heartbeat = Some(&heartbeat);
        }
        let mut session = claimed();
        if invalid == 0 {
            session.trigger_origin = crate::session::TriggerOrigin::User;
        }
        let id = if invalid == 1 {
            "other-run"
        } else {
            "scheduled-run"
        };
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));
        assert!(
            resume_claimed_scheduled_turn(&deps, session, id, &mut sink)
                .await
                .is_err()
        );
        assert_eq!(*mem.saves.borrow(), 0);
        assert!(model.requests.borrow().is_empty());
        assert!(heartbeat.starts.borrow().is_empty());
    }
}

#[tokio::test]
async fn stale_save_new_input_and_unhealthy_lease_stop_before_model_execution() {
    for reason in 0..3 {
        let mem = MemSession {
            fail_save_at: (reason == 0).then_some(1),
            ..Default::default()
        };
        if reason == 1 {
            *mem.latest_revision.borrow_mut() = Some(2);
        }
        let store = PreclaimedStore(&mem);
        let model = ScriptModel {
            turns: RefCell::new([answer("must not happen")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let clock = || "2026-09-06T00:00:01Z".to_string();
        let heartbeat = ScheduledHeartbeat {
            healthy: Cell::new(reason != 2),
            current: Cell::new(true),
            starts: RefCell::new(vec![]),
            drops: Rc::new(Cell::new(0)),
        };
        let scheduled_model = ScheduledModel(&model);
        let mut deps = deps(&mem, &scheduled_model, &tools, &[], &clock);
        deps.session_seam = &store;
        deps.heartbeat = Some(&heartbeat);
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));
        let result =
            resume_claimed_scheduled_turn(&deps, claimed(), "scheduled-run", &mut sink).await;
        if reason == 1 {
            assert!(matches!(result, Ok(LoopOutcome::Superseded { .. })));
        } else {
            assert!(result.is_err());
        }
        assert!(model.requests.borrow().is_empty());
        assert!(tools.calls.borrow().is_empty());
        assert_eq!(heartbeat.drops.get(), 1);
    }
}

struct ExpiringModel<'a> {
    inner: ScriptModel,
    heartbeat: &'a ScheduledHeartbeat,
}
#[async_trait(?Send)]
impl ModelSeam for ExpiringModel<'_> {
    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.inner.context_policy(requirements).await
    }
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        let result = self.inner.call(request, sink).await;
        self.heartbeat.current.set(false);
        result
    }
}
#[tokio::test]
async fn lease_loss_during_model_response_cannot_dispatch_its_tool_proposal() {
    let mem = MemSession::default();
    let store = PreclaimedStore(&mem);
    let heartbeat = ScheduledHeartbeat {
        healthy: Cell::new(true),
        current: Cell::new(true),
        starts: RefCell::new(vec![]),
        drops: Rc::new(Cell::new(0)),
    };
    let model = ExpiringModel {
        inner: ScriptModel {
            turns: RefCell::new([tool_use("late-call", "sysinfo")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        },
        heartbeat: &heartbeat,
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-09-06T00:00:01Z".to_string();
    let scheduled_model = ScheduledModel(&model);
    let mut deps = deps(&mem, &scheduled_model, &tools, &registry, &clock);
    deps.session_seam = &store;
    deps.heartbeat = Some(&heartbeat);
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    assert!(
        resume_claimed_scheduled_turn(&deps, claimed(), "scheduled-run", &mut sink)
            .await
            .is_err()
    );
    assert!(
        heartbeat.healthy.get(),
        "background ticker has not noticed the change"
    );
    assert_eq!(model.inner.requests.borrow().len(), 1);
    assert!(tools.calls.borrow().is_empty());
    assert!(
        mem.inner
            .borrow()
            .as_ref()
            .unwrap()
            .conversation
            .iter()
            .all(|message| message.tool_calls.is_empty())
    );
    assert_eq!(heartbeat.drops.get(), 1);
}

struct ExpiringTools<'a> {
    calls: Cell<usize>,
    heartbeat: &'a ScheduledHeartbeat,
}
#[async_trait(?Send)]
impl ToolSeam for ExpiringTools<'_> {
    async fn run_read(&self, _: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.calls.set(self.calls.get() + 1);
        self.heartbeat.current.set(false);
        Ok(ToolRunOutput {
            content: "first result".into(),
            image_data_url: None,
        })
    }
}
#[tokio::test]
async fn lease_loss_between_tools_closes_unexecuted_calls_without_dispatching_them() {
    let mem = MemSession::default();
    let store = PreclaimedStore(&mem);
    let heartbeat = ScheduledHeartbeat {
        healthy: Cell::new(true),
        current: Cell::new(true),
        starts: RefCell::new(vec![]),
        drops: Rc::new(Cell::new(0)),
    };
    let mut turn = tool_use("first", "sysinfo");
    turn.tool_calls.push(ToolCall {
        id: "second".into(),
        name: "sysinfo".into(),
        arguments_json: "{}".into(),
    });
    let model = ScriptModel {
        turns: RefCell::new([turn].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = ExpiringTools {
        calls: Cell::new(0),
        heartbeat: &heartbeat,
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-09-06T00:00:01Z".to_string();
    let scheduled_model = ScheduledModel(&model);
    let mut deps = deps(&mem, &scheduled_model, &tools, &registry, &clock);
    deps.session_seam = &store;
    deps.heartbeat = Some(&heartbeat);
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    assert!(
        resume_claimed_scheduled_turn(&deps, claimed(), "scheduled-run", &mut sink)
            .await
            .is_err()
    );
    assert_eq!(tools.calls.get(), 1);
    let saved = mem.inner.borrow().as_ref().unwrap().clone();
    assert_eq!(
        saved
            .conversation
            .iter()
            .filter(|message| message.role == ChatRole::Tool)
            .count(),
        2
    );
    assert!(saved.conversation.iter().any(|message| {
        message
            .text
            .contains("tool was not run because the turn lease")
    }));
    assert_eq!(heartbeat.drops.get(), 1);
}

#[tokio::test]
async fn scheduled_original_must_remain_exportable_before_any_save_or_model_call() {
    for reason in [0, 1, 3, 4] {
        let mem = MemSession::default();
        let model = ScriptModel {
            turns: RefCell::new([answer("must not happen")].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "unused".into(),
        };
        let clock = || "2026-09-06T00:00:01Z".to_string();
        let heartbeat = ScheduledHeartbeat {
            healthy: Cell::new(true),
            current: Cell::new(true),
            starts: RefCell::new(vec![]),
            drops: Rc::new(Cell::new(0)),
        };
        let scheduled_model = ScheduledModel(&model);
        let model_seam: &dyn ModelSeam = if reason == 0 {
            &model
        } else {
            &scheduled_model
        };
        let mut deps = deps(&mem, model_seam, &tools, &[], &clock);
        deps.heartbeat = Some(&heartbeat);
        let mut session = claimed();
        match reason {
            0 => {}
            1 => session.conversation[0].data_envelope = None,
            3 => session.conversation[0].text.push_str("tampered"),
            _ => session.conversation[0]
                .data_envelope
                .as_mut()
                .unwrap()
                .allowed_destinations
                .clear(),
        }
        let mut sink = Collector(Rc::new(RefCell::new(String::new())));
        assert!(
            resume_claimed_scheduled_turn(&deps, session, "scheduled-run", &mut sink)
                .await
                .is_err()
        );
        assert_eq!(*mem.saves.borrow(), 0);
        assert!(model.requests.borrow().is_empty());
        assert!(tools.calls.borrow().is_empty());
        assert!(heartbeat.starts.borrow().is_empty());
    }
}

#[tokio::test]
async fn fresh_task_turn_rejects_extra_state_and_drives_without_reclaiming() {
    use crate::schedule::{
        contract::parse_contract,
        fresh_session::{FreshSessionInput, initial_session},
    };
    use crate::session::TriggerOrigin;
    use sha2::{Digest, Sha256};
    let prompt = "Produce the scheduled report";
    let contract = parse_contract(&serde_json::json!({
        "schema_version":1,"schedule_id":"task","task_revision":1,"contract_revision":1,
        "target_device_id":"device","prompt_sha256":format!("{:x}", Sha256::digest(prompt.as_bytes())),
        "permissions":[],"steps":[],"exception_mode":"deny",
        "budget":{"max_runs_per_utc_day":1,"max_calls_per_run":1,"max_model_tokens_per_run":1000,"max_runtime_seconds":60}
    }).to_string()).unwrap();
    let mut session = initial_session(
        &contract,
        FreshSessionInput {
            run_id: "schedule-run-first",
            actor_id: "1",
            prompt,
            locale: None,
            policy_revision: 1,
            scope: scope(),
            now: "2026-09-06T00:00:00Z",
        },
    )
    .unwrap();
    session.version = 1;
    let mem = MemSession::default();
    let store = PreclaimedStore(&mem);
    let model = ScriptModel {
        turns: RefCell::new([answer("report")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "unused".into(),
    };
    let clock = || "2026-09-06T00:00:01Z".to_string();
    let heartbeat = ScheduledHeartbeat {
        healthy: Cell::new(true),
        current: Cell::new(true),
        starts: RefCell::new(vec![]),
        drops: Rc::new(Cell::new(0)),
    };
    let model_seam = ScheduledModel(&model);
    let mut deps = deps(&mem, &model_seam, &tools, &[], &clock);
    deps.session_seam = &store;
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    assert!(
        resume_claimed_fresh_task_turn(
            &deps,
            session.clone(),
            &contract,
            "schedule-run-first",
            &mut sink
        )
        .await
        .is_err()
    );
    deps.heartbeat = Some(&heartbeat);
    for change in ["message", "origin", "handled", "chain", "run"] {
        let mut changed = session.clone();
        let mut run = "schedule-run-first";
        match change {
            "message" => changed.conversation.push(ChatMessage::text(
                "old",
                ChatRole::Assistant,
                "old answer",
            )),
            "origin" => changed.trigger_origin = TriggerOrigin::User,
            "handled" => changed.handled_input_seq = 1,
            "chain" => changed.automation_turns_used = 1,
            "run" => run = "schedule-run-other",
            _ => unreachable!(),
        }
        assert!(
            resume_claimed_fresh_task_turn(&deps, changed, &contract, run, &mut sink)
                .await
                .is_err(),
            "{change}"
        );
    }
    assert!(model.requests.borrow().is_empty());
    assert!(mem.inner.borrow().is_none());
    let lease = session.lease_token;
    let result =
        resume_claimed_fresh_task_turn(&deps, session, &contract, "schedule-run-first", &mut sink)
            .await
            .unwrap();
    assert!(matches!(result, LoopOutcome::Answered(_)));
    assert_eq!(model.requests.borrow().len(), 1);
    let saved = mem.inner.borrow().as_ref().unwrap().clone();
    assert_eq!(saved.trigger_origin, TriggerOrigin::ScheduledTask);
    assert_eq!(saved.lease_token, lease);
    assert_eq!(saved.input_revision, 1);
    assert_eq!(
        saved
            .conversation
            .iter()
            .filter(|message| message.role == ChatRole::User)
            .count(),
        1
    );
    assert!(tools.calls.borrow().is_empty());
}
