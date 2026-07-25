use super::*;
use crate::chat::{ChatRole, ModelTurn, StopReason, ToolCall, ToolSpec};
use crate::prompt::ResponseFormatSpec;
use crate::seam::{ToolRunOutput, TurnSink, WaitOutcome};
use crate::session::PersistedAgentSession;
use async_trait::async_trait;
use desk_agent_protocol::{AgentScope, Capability, ExecutionMode};
use std::cell::RefCell;
use std::rc::Rc;

/// An in-memory session store: one session, claimed via the pure transition.
#[derive(Default)]
struct MemSession {
    inner: RefCell<Option<PersistedAgentSession>>,
}
#[async_trait(?Send)]
impl SessionSeam for MemSession {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut slot = self.inner.borrow_mut();
        let mut session = slot.take().unwrap_or_else(|| {
            PersistedAgentSession::new(
                params.conversation_id.clone(),
                params.actor_id.clone(),
                params.device_id.clone(),
                params.policy_revision,
                params.current_pdp_scope.clone(),
                params.now.clone(),
            )
        });
        let trigger_origin = params.trigger_origin;
        let turn_id = params.turn_id.clone();
        session
            .begin_turn(
                params.turn_id,
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        session.adopt_trigger(trigger_origin, &turn_id);
        *slot = Some(session.clone());
        Ok(session)
    }
    async fn save(&self, session: &mut PersistedAgentSession) -> Result<(), AgentError> {
        *self.inner.borrow_mut() = Some(session.clone());
        Ok(())
    }
}

/// A scripted model: returns the queued turns in order, recording each request.
struct ScriptModel {
    turns: RefCell<std::collections::VecDeque<ModelTurn>>,
    requests: Rc<RefCell<Vec<ModelRequest>>>,
}
#[async_trait(?Send)]
impl ModelSeam for ScriptModel {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        self.requests.borrow_mut().push(request);
        let turn = self
            .turns
            .borrow_mut()
            .pop_front()
            .expect("a scripted turn");
        sink.on_text_delta(&turn.text);
        Ok(turn)
    }
}

/// A read-tool seam recording the calls it ran.
struct RecordingTools {
    calls: Rc<RefCell<Vec<String>>>,
    reply: String,
}
#[async_trait(?Send)]
impl ToolSeam for RecordingTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.calls.borrow_mut().push(call.name.clone());
        Ok(ToolRunOutput {
            content: format!("{}: {}", call.name, self.reply),
            image_data_url: None,
        })
    }
}

fn read_tool(name: &str, cap: Capability) -> RegisteredTool {
    RegisteredTool {
        spec: ToolSpec {
            name: name.into(),
            description: "read".into(),
            parameters_schema: serde_json::json!({"type":"object"}),
        },
        required_capability: cap,
        effect: ToolEffect::ReadOnly,
    }
}

fn scope() -> AgentScope {
    AgentScope {
        granted: vec![Capability::SystemInfo, Capability::LogRecent],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

fn claim() -> ClaimTurnParams {
    ClaimTurnParams {
        conversation_id: "conv".into(),
        actor_id: "actor".into(),
        device_id: "device".into(),
        policy_revision: 1,
        current_pdp_scope: scope(),
        turn_id: "turn-1".into(),
        request_id: Some("req".into()),
        connection_id: Some("conn".into()),
        trigger_origin: crate::session::TriggerOrigin::User,
        now: "2026-06-20T00:00:00Z".into(),
    }
}

fn answer(text: &str) -> ModelTurn {
    ModelTurn {
        text: text.into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

fn tool_use(id: &str, name: &str) -> ModelTurn {
    tool_use_args(id, name, "{}")
}

fn tool_use_args(id: &str, name: &str, args: &str) -> ModelTurn {
    ModelTurn {
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments_json: args.into(),
        }],
        ..Default::default()
    }
}

struct Collector(Rc<RefCell<String>>);
impl TurnSink for Collector {
    fn on_text_delta(&mut self, delta: &str) {
        self.0.borrow_mut().push_str(delta);
    }
}

fn deps<'a>(
    sess: &'a MemSession,
    model: &'a ScriptModel,
    tools: &'a RecordingTools,
    registry: &'a [RegisteredTool],
    clock: &'a dyn Fn() -> String,
) -> LoopDeps<'a> {
    LoopDeps {
        session_seam: sess,
        model,
        tools,
        registry,
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_context_bytes: crate::DEFAULT_MAX_CONTEXT_BYTES,
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock,
        heartbeat: None,
    }
}

/// A turn that answers immediately: no tools advertised get called, the
/// assistant text is appended, and the turn settles to Idle.
#[tokio::test]
async fn answers_without_tools() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("all good")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".to_string();
    let mut out = String::new();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "how is it?");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    out.push_str(sink.0.borrow().as_str());

    assert_eq!(outcome, LoopOutcome::Answered("all good".into()));
    assert!(tools.calls.borrow().is_empty());
    assert_eq!(out, "all good");
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert_eq!(s.turn_state, TurnState::Idle);
    // user + assistant.
    assert_eq!(s.conversation.len(), 2);
    // The model was offered the granted read tool.
    assert_eq!(model.requests.borrow()[0].tools.len(), 1);
}

/// A tool turn followed by an answer: the read tool runs, its result is
/// appended, and the second model call sees user+assistant+tool+...
#[tokio::test]
async fn runs_read_tool_then_answers() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "sysinfo"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "ok".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let deps = LoopDeps {
        max_steps_per_turn: crate::MAX_SAME_TOOL_PER_TURN + 2,
        ..deps(&sess, &model, &tools, &reg, &clock)
    };
    let outcome = run_agent_turn(&deps, claim(), user, &mut sink)
        .await
        .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*tools.calls.borrow(), vec!["sysinfo"]);
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // user, assistant(tool_calls), tool result, assistant(answer).
    assert_eq!(s.conversation.len(), 4);
    assert_eq!(s.conversation[1].role, ChatRole::Assistant);
    assert_eq!(s.conversation[1].tool_calls.len(), 1);
    assert_eq!(s.conversation[2].role, ChatRole::Tool);
    assert_eq!(s.conversation[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(s.current_turn_steps, 2);
}

/// A model that names a tool it was never shown gets an error tool-result (the
/// conversation stays well-formed) rather than the call being executed.
#[tokio::test]
async fn unexposed_tool_call_becomes_error_result() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "ungranted"), answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    // Registry has a tool the scope does NOT grant.
    let reg = vec![read_tool("ungranted", Capability::ContainerList)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("ok".into()));
    assert!(
        tools.calls.borrow().is_empty(),
        "no read ran for an unexposed tool"
    );
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert_eq!(s.conversation[2].role, ChatRole::Tool);
    assert!(s.conversation[2].text.contains("not available"));
}

/// The per-turn step budget stops a model that keeps calling tools forever.
/// Three distinct tools are cycled so the same-tool cap never trips first.
#[tokio::test]
async fn step_budget_circuit_breaks() {
    let sess = MemSession::default();
    let names = ["sysinfo", "logs", "ports"];
    let turns: std::collections::VecDeque<_> = (0..crate::MAX_STEPS_PER_TURN + 5)
        .map(|i| tool_use(&format!("c{i}"), names[i as usize % names.len()]))
        .collect();
    let model = ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![
        read_tool("sysinfo", Capability::SystemInfo),
        read_tool("logs", Capability::LogRecent),
        read_tool("ports", Capability::NetworkPorts),
    ];
    // A scope granting all three so each tool is exposed.
    let mut params = claim();
    params.current_pdp_scope = AgentScope {
        granted: vec![
            Capability::SystemInfo,
            Capability::LogRecent,
            Capability::NetworkPorts,
        ],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        params,
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)
    );
    let s = sess.inner.borrow();
    assert_eq!(
        s.as_ref().unwrap().current_turn_steps,
        crate::MAX_STEPS_PER_TURN
    );
}

/// A tighter per-turn budget (the terminal copilot uses 2) circuit-breaks
/// sooner than the diagnose default, proving `LoopDeps.max_steps_per_turn`
/// is honored per call.
#[tokio::test]
async fn tight_step_budget_circuit_breaks_at_two() {
    const COPILOT_MAX_STEPS: u32 = 2;
    let sess = MemSession::default();
    let names = ["sysinfo", "logs", "ports"];
    let turns: std::collections::VecDeque<_> = (0..COPILOT_MAX_STEPS + 5)
        .map(|i| tool_use(&format!("c{i}"), names[i as usize % names.len()]))
        .collect();
    let model = ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![
        read_tool("sysinfo", Capability::SystemInfo),
        read_tool("logs", Capability::LogRecent),
        read_tool("ports", Capability::NetworkPorts),
    ];
    let mut params = claim();
    params.current_pdp_scope = AgentScope {
        granted: vec![
            Capability::SystemInfo,
            Capability::LogRecent,
            Capability::NetworkPorts,
        ],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let deps = LoopDeps {
        max_steps_per_turn: COPILOT_MAX_STEPS,
        ..deps(&sess, &model, &tools, &reg, &clock)
    };
    let outcome = run_agent_turn(&deps, params, user, &mut sink)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)
    );
    let s = sess.inner.borrow();
    assert_eq!(s.as_ref().unwrap().current_turn_steps, COPILOT_MAX_STEPS);
}

/// Repeatedly calling the *same* tool trips the same-tool cap before the step
/// budget.
#[tokio::test]
async fn same_tool_repeat_circuit_breaks() {
    let sess = MemSession::default();
    let mut turns: std::collections::VecDeque<_> = (0..=crate::MAX_SAME_TOOL_PER_TURN)
        .map(|i| tool_use(&format!("c{i}"), "sysinfo"))
        .collect();
    turns.push_back(answer("continued"));
    let model = ScriptModel {
        turns: RefCell::new(turns),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let deps = LoopDeps {
        max_steps_per_turn: crate::MAX_SAME_TOOL_PER_TURN + 2,
        ..deps(&sess, &model, &tools, &reg, &clock)
    };
    let outcome = run_agent_turn(&deps, claim(), user, &mut sink)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::CircuitBreak(CircuitBreakReason::SameToolRepeat)
    );
    // The cap is enforced after the (MAX_SAME_TOOL_PER_TURN + 1)-th call.
    assert_eq!(
        tools.calls.borrow().len(),
        crate::MAX_SAME_TOOL_PER_TURN as usize
    );

    // The skipped over-limit call receives a synthetic result, leaving a valid
    // conversation that a user can continue in the next turn.
    let persisted = sess.inner.borrow();
    let last = persisted
        .as_ref()
        .unwrap()
        .conversation
        .last()
        .expect("synthetic result persisted");
    assert_eq!(last.role, ChatRole::Tool);
    let expected_call_id = format!("c{}", crate::MAX_SAME_TOOL_PER_TURN);
    assert_eq!(
        last.tool_call_id.as_deref(),
        Some(expected_call_id.as_str())
    );
    drop(persisted);

    let mut continuation = claim();
    continuation.turn_id = "turn-continued".into();
    let continued = run_agent_turn(
        &deps,
        continuation,
        ChatMessage::text("u2", ChatRole::User, "continue"),
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(continued, LoopOutcome::Answered("continued".into()));
}

/// A protocol violation (EndTurn carrying tool calls) is surfaced and settles
/// the turn to Failed.
#[tokio::test]
async fn protocol_error_fails_turn() {
    let sess = MemSession::default();
    let bad = ModelTurn {
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![ToolCall {
            id: "c".into(),
            name: "sysinfo".into(),
            arguments_json: "{}".into(),
        }],
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([bad].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::ProtocolError(_)));
    assert_eq!(
        sess.inner.borrow().as_ref().unwrap().turn_state,
        TurnState::Failed
    );
}

/// A second turn cannot start while one is running (busy), and a follow-up
/// from a different subject is rejected.
#[tokio::test]
async fn busy_and_subject_guards() {
    // Busy: pre-seed a Running session.
    let sess = MemSession::default();
    {
        let mut s = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t");
        s.begin_turn("prior", None, None, 1, scope(), "t").unwrap();
        *sess.inner.borrow_mut() = Some(s);
    }
    let model = ScriptModel {
        turns: RefCell::new([answer("x")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::TurnBusy);
}

/// The model request is assembled as [system prompt] + conversation: the first
/// message is the agentic system prompt and the user message follows it.
#[tokio::test]
async fn prepends_system_prompt() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "q");
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    let reqs = model.requests.borrow();
    let msgs = &reqs[0].messages;
    assert_eq!(msgs[0].role, ChatRole::System);
    assert!(msgs[0].text.contains("untrusted DATA"));
    assert_eq!(msgs[1].role, ChatRole::User);
    assert_eq!(msgs[1].text, "q");
}

/// Two sequential turns over the same session continue one conversation: the
/// second turn's model call sees the first turn's user + assistant history
/// followed by the new user message (§9 multi-turn continuation).
#[tokio::test]
async fn follow_up_turn_continues_conversation() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("first"), answer("second")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        ChatMessage::text("u1", ChatRole::User, "q1"),
        &mut sink,
    )
    .await
    .unwrap();
    // Second turn: a distinct turn id so minted message ids do not collide.
    let mut c2 = claim();
    c2.turn_id = "turn-2".into();
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        c2,
        ChatMessage::text("u2", ChatRole::User, "q2"),
        &mut sink,
    )
    .await
    .unwrap();
    let reqs = model.requests.borrow();
    let second = &reqs[1].messages;
    let roles: Vec<_> = second.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            ChatRole::System,
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::User
        ]
    );
    assert_eq!(second[1].text, "q1");
    assert_eq!(second[3].text, "q2");
    assert_eq!(
        sess.inner.borrow().as_ref().unwrap().conversation.len(),
        4,
        "both turns persisted in one conversation"
    );
}

/// A tight context budget trims old history out of the model request while the
/// system prompt (prepended on top, not counted) and the newest message stay.
#[tokio::test]
async fn trims_history_to_budget() {
    let sess = MemSession::default();
    {
        let mut s = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t");
        s.conversation.push(ChatMessage::text(
            "old1",
            ChatRole::User,
            "x".repeat(50_000),
        ));
        s.conversation.push(ChatMessage::text(
            "old2",
            ChatRole::Assistant,
            "y".repeat(50_000),
        ));
        *sess.inner.borrow_mut() = Some(s);
    }
    let model = ScriptModel {
        turns: RefCell::new([answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let mut d = deps(&sess, &model, &tools, &reg, &clock);
    d.max_context_bytes = 500;
    run_agent_turn(
        &d,
        claim(),
        ChatMessage::text("u", ChatRole::User, "recent"),
        &mut sink,
    )
    .await
    .unwrap();
    let reqs = model.requests.borrow();
    let msgs = &reqs[0].messages;
    assert_eq!(msgs[0].role, ChatRole::System);
    assert!(
        msgs.iter().all(|m| !m.text.contains(&"x".repeat(100))),
        "the large old user message was trimmed out"
    );
    assert!(
        msgs.iter().any(|m| m.text == "recent"),
        "the newest message is kept"
    );
}

// ---------------------------- Mutating path ----------------------------

use crate::seam::ExecIdentity;

/// A tool seam that scripts mutating outcomes and records read + exec calls.
struct ScriptedTools {
    reads: Rc<RefCell<Vec<String>>>,
    execs: RefCell<std::collections::VecDeque<ExecOutcome>>,
    exec_calls: Rc<RefCell<Vec<String>>>,
    acks: Rc<RefCell<Vec<String>>>,
    waits: RefCell<std::collections::VecDeque<WaitOutcome>>,
    wait_calls: Rc<RefCell<Vec<String>>>,
}
#[async_trait(?Send)]
impl ToolSeam for ScriptedTools {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        self.reads.borrow_mut().push(call.name.clone());
        Ok(ToolRunOutput {
            content: format!("{}: ok", call.name),
            image_data_url: None,
        })
    }
    async fn confirm_and_exec(
        &self,
        call: &ToolCall,
        _ctx: &ExecContext,
    ) -> Result<ExecOutcome, AgentError> {
        self.exec_calls.borrow_mut().push(call.id.clone());
        Ok(self
            .execs
            .borrow_mut()
            .pop_front()
            .expect("a scripted exec outcome"))
    }
    async fn ack_delivery(&self, event_id: &str) -> Result<(), AgentError> {
        self.acks.borrow_mut().push(event_id.to_string());
        Ok(())
    }
    async fn wait_for_task(
        &self,
        exec_request_id: &str,
        _execution_id: &str,
    ) -> Result<WaitOutcome, AgentError> {
        self.wait_calls
            .borrow_mut()
            .push(exec_request_id.to_string());
        Ok(self
            .waits
            .borrow_mut()
            .pop_front()
            .expect("a scripted wait outcome"))
    }
}

fn mutating_tool(name: &str, cap: Capability) -> RegisteredTool {
    RegisteredTool {
        spec: ToolSpec {
            name: name.into(),
            description: "exec".into(),
            parameters_schema: serde_json::json!({"type":"object"}),
        },
        required_capability: cap,
        effect: ToolEffect::Mutating,
    }
}

/// A scope that exposes the mutating exec tool: grants its capability and runs
/// at `ConfirmEachAction`.
fn exec_scope() -> AgentScope {
    AgentScope {
        granted: vec![Capability::ShellExecConfirmed, Capability::SystemInfo],
        mode: ExecutionMode::ConfirmEachAction,
        expires_at: None,
        policy_name: None,
    }
}

fn exec_claim() -> ClaimTurnParams {
    let mut c = claim();
    c.current_pdp_scope = exec_scope();
    c
}

fn tools(execs: Vec<ExecOutcome>) -> ScriptedTools {
    tools_with_waits(execs, vec![])
}

fn tools_with_waits(execs: Vec<ExecOutcome>, waits: Vec<WaitOutcome>) -> ScriptedTools {
    ScriptedTools {
        reads: Rc::new(RefCell::new(vec![])),
        execs: RefCell::new(execs.into()),
        exec_calls: Rc::new(RefCell::new(vec![])),
        acks: Rc::new(RefCell::new(vec![])),
        waits: RefCell::new(waits.into()),
        wait_calls: Rc::new(RefCell::new(vec![])),
    }
}

fn exec_deps<'a>(
    sess: &'a MemSession,
    model: &'a ScriptModel,
    scripted: &'a ScriptedTools,
    registry: &'a [RegisteredTool],
    clock: &'a dyn Fn() -> String,
) -> LoopDeps<'a> {
    LoopDeps {
        session_seam: sess,
        model,
        tools: scripted,
        registry,
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_context_bytes: crate::DEFAULT_MAX_CONTEXT_BYTES,
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock,
        heartbeat: None,
    }
}

/// A mutating tool that the operator approves runs to a known result; its
/// result is appended and the turn settles to Idle with no in-flight execution.
#[tokio::test]
async fn mutating_executes_then_answers() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        output: ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // user, assistant(tool_calls), tool result, assistant(answer).
    assert_eq!(s.conversation.len(), 4);
    assert_eq!(s.conversation[2].role, ChatRole::Tool);
    assert_eq!(s.conversation[2].text, "exit_code=0");
    assert_eq!(s.execution_state, ExecutionState::None);
    assert_eq!(s.turn_state, TurnState::Idle);
}

/// An automation turn ([`TriggerOrigin::ExecCompletion`]) never runs a mutating
/// tool: the exec tool is not advertised (layer 1), and a model that names it
/// anyway is answered with "not available" without the tool seam being called —
/// so a completion cannot self-trigger a new command. The read tool still works.
#[tokio::test]
async fn automation_turn_cannot_start_a_new_command() {
    use crate::session::TriggerOrigin;
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exec_command"),
                tool_use("c2", "read_sys"),
                answer("done"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    // The seam would panic if asked to execute (no scripted outcomes), proving
    // it is never reached for the mutating call.
    let scripted = tools(vec![]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let mut claim = exec_claim();
    claim.trigger_origin = TriggerOrigin::ExecCompletion;
    let user = ChatMessage::text("u", ChatRole::User, "the prior command finished");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        claim,
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    // The tool seam was never asked to execute anything.
    assert!(scripted.exec_calls.borrow().is_empty());

    // Layer 1: the first model call did not advertise the mutating tool, but did
    // advertise the read tool.
    let reqs = model.requests.borrow();
    let first: Vec<_> = reqs[0].tools.iter().map(|t| t.name.clone()).collect();
    assert!(
        !first.contains(&"exec_command".to_string()),
        "an automation turn must not be offered a mutating tool"
    );
    assert!(first.contains(&"read_sys".to_string()));

    // The exec call was rejected as not available (the seam was never reached).
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    let rejected = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert!(rejected.text.contains("not available"));
    assert_eq!(s.execution_state, ExecutionState::None);
}

/// When the model reacts to a request that contained a pending auto-trigger's
/// completion message, the loop drops that pending entry (the model handled it,
/// so no automation turn should fire) — but leaves a pending entry whose
/// completion the model never saw.
#[tokio::test]
async fn reacting_to_a_completion_clears_its_pending_trigger() {
    use crate::session::PendingAutoTrigger;
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("acknowledged")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![]);
    let reg: Vec<RegisteredTool> = vec![];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    // Seed a session whose conversation already carries a completion message
    // (id "done-1") plus a pending entry keyed on it, and a second pending entry
    // ("done-absent") whose message is not in the conversation.
    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.conversation.push(ChatMessage::untrusted_output(
        "done-1",
        "call-1",
        "task-1",
        "exit_code=0",
    ));
    seeded.add_pending_auto_trigger(PendingAutoTrigger {
        work_id: 1,
        execution_id: "e1".into(),
        tool_call_id: "c1".into(),
        event_id: "done-1".into(),
        chain_id: "chain".into(),
        resolution_org_id: None,
        since: "t0".into(),
    });
    seeded.add_pending_auto_trigger(PendingAutoTrigger {
        work_id: 2,
        execution_id: "e2".into(),
        tool_call_id: "c2".into(),
        event_id: "done-absent".into(),
        chain_id: "chain".into(),
        resolution_org_id: None,
        since: "t0".into(),
    });
    *sess.inner.borrow_mut() = Some(seeded);

    let user = ChatMessage::text("u", ChatRole::User, "what happened?");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("acknowledged".into()));

    let s = sess.inner.borrow();
    let pending = &s.as_ref().unwrap().pending_auto_triggers;
    // "done-1" was in the request the model answered, so it is drained; the
    // entry the model never saw survives.
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_id, "done-absent");
}

/// An automation resume appends no message: it runs against the completion
/// already at the tail of the conversation, the model sees it in the request,
/// and reacting drains its pending entry.
#[tokio::test]
async fn resume_runs_against_the_existing_tail_without_appending() {
    use crate::session::{PendingAutoTrigger, TriggerOrigin};
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([answer("looked at it")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![]);
    let reg: Vec<RegisteredTool> = vec![];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));

    let mut seeded = PersistedAgentSession::new("conv", "actor", "device", 1, scope(), "t0");
    seeded.conversation.push(ChatMessage::untrusted_output(
        "done-1",
        "call-1",
        "task-1",
        "exit_code=0",
    ));
    seeded.add_pending_auto_trigger(PendingAutoTrigger {
        work_id: 1,
        execution_id: "e1".into(),
        tool_call_id: "c1".into(),
        event_id: "done-1".into(),
        chain_id: "chain".into(),
        resolution_org_id: None,
        since: "t0".into(),
    });
    let convo_len = seeded.conversation.len();
    *sess.inner.borrow_mut() = Some(seeded);

    let mut claim = claim();
    claim.trigger_origin = TriggerOrigin::ExecCompletion;
    let outcome = resume_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        claim,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Answered("looked at it".into()));

    // The model saw the completion in its request.
    let reqs = model.requests.borrow();
    assert!(
        reqs[0].messages.iter().any(|m| m.message_id == "done-1"),
        "the resumed turn puts the completion in the model request"
    );

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // No user message was appended — only the assistant answer grew the tail.
    assert_eq!(s.conversation.len(), convo_len + 1);
    assert_eq!(s.conversation.last().unwrap().text, "looked at it");
    // The pending entry the model reacted to is drained.
    assert!(s.pending_auto_triggers.is_empty());
}

/// An unknown-outcome execution closes the conversation with a placeholder tool
/// result, records `OutcomeUnknown`, and hides the mutating tool from the next
/// model call (only read-only follow-up); the late result reconciles it later.
#[tokio::test]
async fn mutating_unknown_closes_with_placeholder_and_hides_mutating() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exec_command"),
                tool_use("c2", "read_sys"),
                answer("status"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Unknown(ExecIdentity {
        work_id: 5,
        execution_id: "e1".into(),
        exec_request_id: "r1".into(),
    })]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "2026-06-20T00:00:09Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("status".into()));
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // The placeholder is recorded with the unknown outcome.
    match &s.execution_state {
        ExecutionState::OutcomeUnknown {
            execution_id,
            placeholder_message_id,
            ..
        } => {
            assert_eq!(execution_id, "e1");
            let ph = s
                .conversation
                .iter()
                .find(|m| &m.message_id == placeholder_message_id)
                .unwrap();
            assert_eq!(ph.tool_call_id.as_deref(), Some("c1"));
            assert!(ph.text.contains("outcome unknown"));
        }
        other => panic!("expected OutcomeUnknown, got {other:?}"),
    }
    // The follow-up model call did not advertise the mutating tool (no new
    // mutation while an outcome is unknown), but kept the read tool.
    let reqs = model.requests.borrow();
    let follow_up: Vec<_> = reqs[1].tools.iter().map(|t| t.name.clone()).collect();
    assert!(!follow_up.contains(&"exec_command".to_string()));
    assert!(follow_up.contains(&"read_sys".to_string()));
    // The first model call DID advertise the mutating tool.
    let first: Vec<_> = reqs[0].tools.iter().map(|t| t.name.clone()).collect();
    assert!(first.contains(&"exec_command".to_string()));
}

/// A dispatched-to-background outcome closes the tool call with a task-id result
/// and records `Executing`; the conversation is not degraded (a result is
/// coming) but no second mutation is offered until it completes.
#[tokio::test]
async fn mutating_dispatched_closes_with_task_id_and_hides_mutating() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use("c1", "exec_command"),
                tool_use("c2", "read_sys"),
                answer("status"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Dispatched(ExecIdentity {
        work_id: 8,
        execution_id: "e9".into(),
        exec_request_id: "exec_task9".into(),
    })]);
    let reg = vec![
        mutating_tool("exec_command", Capability::ShellExecConfirmed),
        read_tool("read_sys", Capability::SystemInfo),
    ];
    let clock = || "2026-06-20T00:00:09Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "run a long job");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("status".into()));
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // The dispatch is recorded as an outstanding execution, not an unknown one.
    match &s.execution_state {
        ExecutionState::Executing {
            work_id,
            execution_id,
            exec_request_id,
        } => {
            assert_eq!(*work_id, 8);
            assert_eq!(execution_id, "e9");
            assert_eq!(exec_request_id, "exec_task9");
        }
        other => panic!("expected Executing, got {other:?}"),
    }
    // The tool call is closed with a task-id result naming the running task.
    let closed = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(
        closed.background_task_id.as_deref(),
        Some("exec_task9"),
        "the UI correlation id is persisted separately from model output"
    );
    let dispatch: serde_json::Value = serde_json::from_str(&closed.text).unwrap();
    assert_eq!(dispatch["status"], "background_running");
    assert_eq!(dispatch["background_task_id"], "exec_task9");
    // A dispatched task leaves its completion delivery for the publisher — the
    // foreground never acks it.
    assert!(scripted.acks.borrow().is_empty());
    // The follow-up model call did not advertise the mutating tool (no second
    // mutation while one is running) but kept the read tool.
    let reqs = model.requests.borrow();
    let replayed_dispatch = reqs[1]
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("c1"))
        .expect("the model follow-up sees the dispatch result");
    let replayed_json: serde_json::Value = serde_json::from_str(&replayed_dispatch.text).unwrap();
    assert_eq!(replayed_json["background_task_id"], "exec_task9");
    let follow_up: Vec<_> = reqs[1].tools.iter().map(|t| t.name.clone()).collect();
    assert!(!follow_up.contains(&"exec_command".to_string()));
    assert!(follow_up.contains(&"read_sys".to_string()));
}

/// Seed a session sitting on a dispatched background task, ready for a follow-up
/// turn that waits on it.
fn seeded_executing() -> MemSession {
    let mut s = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        exec_scope(),
        "2026-06-20T00:00:00Z",
    );
    s.execution_state = ExecutionState::Executing {
        work_id: 8,
        execution_id: "e9".into(),
        exec_request_id: "exec_task9".into(),
    };
    MemSession {
        inner: RefCell::new(Some(s)),
    }
}

/// The registry offered while a task is in flight: the exec tool (hidden by the
/// exposure matrix while `Executing`) plus the wait tool.
fn wait_reg() -> Vec<RegisteredTool> {
    let mut reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    reg.extend(crate::wait_tools::wait_tool_registry());
    reg
}

/// A `wait_for_task` that completes clears `Executing`, keys the result on the
/// delivery id (so a racing publisher dedups), and acks the delivery so the
/// publisher stands down.
#[tokio::test]
async fn wait_for_task_completes_clears_executing_and_acks() {
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("it finished"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(
        vec![],
        vec![WaitOutcome::Completed {
            output: ToolRunOutput {
                content: "exit_code=0".into(),
                image_data_url: None,
            },
            event_id: Some("work:8:done".into()),
        }],
    );
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "is it done?");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert_eq!(
        s.execution_state,
        ExecutionState::None,
        "the awaited task settled; mutation is allowed again"
    );
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_eq!(result.message_id, "work:8:done", "keyed on the delivery id");
    assert_eq!(result.text, "exit_code=0");
    assert_eq!(
        *scripted.wait_calls.borrow(),
        vec!["exec_task9".to_string()]
    );
    assert_eq!(*scripted.acks.borrow(), vec!["work:8:done".to_string()]);
}

/// A `wait_for_task` that times out with the task still running leaves it in
/// flight (`Executing`) and does not ack any delivery.
#[tokio::test]
async fn wait_for_task_still_running_keeps_the_task() {
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("still going"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(vec![], vec![WaitOutcome::StillRunning]);
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "is it done?");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    assert!(
        matches!(s.execution_state, ExecutionState::Executing { .. }),
        "the task is still running"
    );
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c2"))
        .unwrap();
    assert_eq!(result.background_task_id.as_deref(), Some("exec_task9"));
    let dispatch: serde_json::Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(dispatch["status"], "background_running");
    assert_eq!(dispatch["background_task_id"], "exec_task9");
    assert!(scripted.acks.borrow().is_empty());
}

/// A `wait_for_task` whose task became unknown degrades to `OutcomeUnknown`,
/// anchored on this call's own result so a late real result can reconcile it.
#[tokio::test]
async fn wait_for_task_unknown_degrades_to_outcome_unknown() {
    let sess = seeded_executing();
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args("c2", "wait_for_task", r#"{"task_id":"exec_task9"}"#),
                answer("its outcome is unknown"),
            ]
            .into(),
        ),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools_with_waits(vec![], vec![WaitOutcome::Unknown]);
    let reg = wait_reg();
    let clock = || "2026-06-20T00:01:00Z".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "is it done?");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    match &s.execution_state {
        ExecutionState::OutcomeUnknown {
            execution_id,
            placeholder_message_id,
            ..
        } => {
            assert_eq!(execution_id, "e9");
            // The placeholder anchors on this wait call's own result message.
            let anchor = s
                .conversation
                .iter()
                .find(|m| &m.message_id == placeholder_message_id)
                .unwrap();
            assert_eq!(anchor.tool_call_id.as_deref(), Some("c2"));
        }
        other => panic!("expected OutcomeUnknown, got {other:?}"),
    }
    assert!(scripted.acks.borrow().is_empty());
}

/// An executed result carrying a stable delivery id keys the tool-result
/// message on that id, so a late completion delivery of the same result dedups
/// against it instead of appending a duplicate.
#[tokio::test]
async fn executed_keys_the_result_message_on_the_delivery_id() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        output: ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        },
        event_id: Some("work:8:done".into()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert_eq!(result.message_id, "work:8:done");
    assert_eq!(result.text, "exit_code=0");
    // The foreground path acked the delivery (post-save) so the publisher stands
    // down.
    assert_eq!(*scripted.acks.borrow(), vec!["work:8:done".to_string()]);
}

/// Two mutating calls in one turn run serially: a rejection halts the rest, so
/// the second is skipped (not executed) but still gets a tool result.
#[tokio::test]
async fn mutating_rejected_skips_remaining_in_turn() {
    let sess = MemSession::default();
    let two = ModelTurn {
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![
            ToolCall {
                id: "c1".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            },
            ToolCall {
                id: "c2".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            },
        ],
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([two, answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Rejected {
        reason: Some("not now".into()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart both");
    let outcome = run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("ok".into()));
    // Only the first call was attempted; the second was skipped.
    assert_eq!(*scripted.exec_calls.borrow(), vec!["c1"]);
    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    // user, assistant(2 calls), rejected result(c1), skipped result(c2), answer.
    assert_eq!(s.conversation.len(), 5);
    assert_eq!(s.conversation[2].tool_call_id.as_deref(), Some("c1"));
    assert!(s.conversation[2].text.contains("rejected"));
    assert_eq!(s.conversation[3].tool_call_id.as_deref(), Some("c2"));
    assert!(s.conversation[3].text.contains("not executed"));
    assert_eq!(s.execution_state, ExecutionState::None);
}

/// A command cancelled before it dispatched closes the call with a truthful
/// "cancelled" result (not "rejected"), leaves the execution machine clean, and
/// halts the rest of the turn.
#[tokio::test]
async fn mutating_cancelled_before_dispatch_closes_truthfully() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Cancelled {
        reason: Some("operator stopped it".into()),
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "restart it");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();

    let s = sess.inner.borrow();
    let s = s.as_ref().unwrap();
    let result = s
        .conversation
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .unwrap();
    assert!(result.text.contains("cancelled"));
    assert!(!result.text.contains("rejected"));
    assert_eq!(
        s.execution_state,
        ExecutionState::None,
        "a never-dispatched cancel leaves the machine clean"
    );
}

/// A backend transport error from the mutating seam (not model-safe) fails the
/// turn rather than becoming a tool result.
#[tokio::test]
async fn mutating_backend_error_fails_turn() {
    struct FailingExec;
    #[async_trait(?Send)]
    impl ToolSeam for FailingExec {
        async fn run_read(&self, _call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            unreachable!("no read in this test")
        }
        async fn confirm_and_exec(
            &self,
            _call: &ToolCall,
            _ctx: &ExecContext,
        ) -> Result<ExecOutcome, AgentError> {
            Err(AgentError {
                kind: desk_agent_protocol::AgentErrorKind::Internal,
                message: "db down".into(),
                retryable: false,
                safe_for_model: false,
                error_code: None,
            })
        }
    }
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let failing = FailingExec;
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "do it");
    let deps = LoopDeps {
        session_seam: &sess,
        model: &model,
        tools: &failing,
        registry: &reg,
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_context_bytes: crate::DEFAULT_MAX_CONTEXT_BYTES,
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock: &clock,
        heartbeat: None,
    };
    let err = run_agent_turn(&deps, exec_claim(), user, &mut sink)
        .await
        .unwrap_err();
    assert_eq!(err.kind, desk_agent_protocol::AgentErrorKind::Internal);
    // The turn settled to Failed.
    assert_eq!(
        sess.inner.borrow().as_ref().unwrap().turn_state,
        TurnState::Failed
    );
}

/// A model-safe pre-dispatch error may explicitly allow the model to correct its
/// arguments. The error result is included in the next request, and a corrected
/// command can execute in the same user turn.
#[tokio::test]
async fn retryable_mutating_error_returns_to_model_for_correction() {
    struct RetryableThenExecuted {
        calls: RefCell<u32>,
    }
    #[async_trait(?Send)]
    impl ToolSeam for RetryableThenExecuted {
        async fn run_read(&self, _call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            unreachable!("no read in this test")
        }

        async fn confirm_and_exec(
            &self,
            _call: &ToolCall,
            _ctx: &ExecContext,
        ) -> Result<ExecOutcome, AgentError> {
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                Err(AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
                    message: r#"{"error_code":"unsupported_exec_shell","requested_shell":"bash","available_shells":["powershell"],"retryable":true}"#.into(),
                    retryable: true,
                    safe_for_model: true,
                    error_code: Some(
                        desk_utils::error::DeskErrorCode::AI_EXEC_SHELL_UNSUPPORTED.code(),
                    ),
                })
            } else {
                Ok(ExecOutcome::Executed {
                    output: ToolRunOutput {
                        content: "exit_code=0".into(),
                        image_data_url: None,
                    },
                    event_id: None,
                })
            }
        }
    }

    let sess = MemSession::default();
    let requests = Rc::new(RefCell::new(vec![]));
    let model = ScriptModel {
        turns: RefCell::new(
            [
                tool_use_args(
                    "c1",
                    "exec_command",
                    r#"{"command":"sleep 1","shell":"bash"}"#,
                ),
                tool_use_args(
                    "c2",
                    "exec_command",
                    r#"{"command":"Start-Sleep 1","shell":"powershell"}"#,
                ),
                answer("done"),
            ]
            .into(),
        ),
        requests: requests.clone(),
    };
    let tools = RetryableThenExecuted {
        calls: RefCell::new(0),
    };
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let mut sink = Collector(Rc::new(RefCell::new(String::new())));
    let user = ChatMessage::text("u", ChatRole::User, "sleep briefly");
    let deps = LoopDeps {
        session_seam: &sess,
        model: &model,
        tools: &tools,
        registry: &reg,
        response_format: ResponseFormatSpec::None,
        system_prompt: crate::agentic_prompt::build_agentic_system_message(None),
        max_context_bytes: crate::DEFAULT_MAX_CONTEXT_BYTES,
        max_steps_per_turn: crate::MAX_STEPS_PER_TURN,
        max_same_tool_per_turn: crate::MAX_SAME_TOOL_PER_TURN,
        clock: &clock,
        heartbeat: None,
    };

    let outcome = run_agent_turn(&deps, exec_claim(), user, &mut sink)
        .await
        .unwrap();

    assert_eq!(outcome, LoopOutcome::Answered("done".into()));
    assert_eq!(*tools.calls.borrow(), 2);
    assert_eq!(requests.borrow().len(), 3);
    assert!(
        requests.borrow()[1]
            .messages
            .iter()
            .any(|message| message.text.contains("unsupported_exec_shell")),
        "the correction step must see the structured shell error"
    );
}

// ---------------------------- Streaming lifecycle ----------------------------

/// A sink that records every lifecycle event in order (text deltas excluded so
/// the assertions key on the structured events).
struct EventLog(Rc<RefCell<Vec<String>>>);
impl TurnSink for EventLog {
    fn on_text_delta(&mut self, _delta: &str) {}
    fn on_tool_started(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        self.0
            .borrow_mut()
            .push(format!("started:{tool_name}:{call_id}:{arguments_json}"));
    }
    fn on_awaiting_approval(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        self.0
            .borrow_mut()
            .push(format!("approval:{tool_name}:{call_id}:{arguments_json}"));
    }
    fn on_tool_finished(
        &mut self,
        call_id: &str,
        ok: bool,
        output: &str,
        background_task_id: Option<&str>,
    ) {
        let background = background_task_id
            .map(|id| format!(":{id}"))
            .unwrap_or_default();
        self.0
            .borrow_mut()
            .push(format!("finished:{call_id}:{ok}:{output}{background}"));
    }
    fn on_answer_committed(&mut self, text: &str) {
        self.0.borrow_mut().push(format!("answer:{text}"));
    }
    fn on_turn_discarded(&mut self) {
        self.0.borrow_mut().push("discarded".into());
    }
}

/// A read-tool turn emits start → finish(ok) → answer events in order.
#[tokio::test]
async fn streams_read_tool_lifecycle_events() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "sysinfo"), answer("done")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "ok".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let user = ChatMessage::text("u", ChatRole::User, "q");
    run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        *log.borrow(),
        vec![
            "started:sysinfo:c1:{}".to_string(),
            "finished:c1:true:sysinfo: ok".to_string(),
            "answer:done".to_string(),
        ]
    );
}

/// A mutating turn emits an awaiting-approval event (not a read start) before
/// the result, then finish(ok) and the answer.
#[tokio::test]
async fn streams_awaiting_approval_event() {
    let sess = MemSession::default();
    let model = ScriptModel {
        turns: RefCell::new([tool_use("c1", "exec_command"), answer("ok")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let scripted = tools(vec![ExecOutcome::Executed {
        output: ToolRunOutput {
            content: "exit_code=0".into(),
            image_data_url: None,
        },
        event_id: None,
    }]);
    let reg = vec![mutating_tool(
        "exec_command",
        Capability::ShellExecConfirmed,
    )];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let user = ChatMessage::text("u", ChatRole::User, "restart");
    run_agent_turn(
        &exec_deps(&sess, &model, &scripted, &reg, &clock),
        exec_claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(
        *log.borrow(),
        vec![
            "approval:exec_command:c1:{}".to_string(),
            "finished:c1:true:exit_code=0".to_string(),
            "answer:ok".to_string(),
        ]
    );
}

/// A truncated turn signals discard (no answer committed).
#[tokio::test]
async fn streams_discarded_on_truncated_turn() {
    let sess = MemSession::default();
    let truncated = ModelTurn {
        text: "half".into(),
        stop_reason: StopReason::MaxTokens,
        ..Default::default()
    };
    let model = ScriptModel {
        turns: RefCell::new([truncated].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "x".into(),
    };
    let reg = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "t".to_string();
    let log = Rc::new(RefCell::new(vec![]));
    let mut sink = EventLog(log.clone());
    let user = ChatMessage::text("u", ChatRole::User, "q");
    let outcome = run_agent_turn(
        &deps(&sess, &model, &tools, &reg, &clock),
        claim(),
        user,
        &mut sink,
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Truncated);
    assert_eq!(*log.borrow(), vec!["discarded".to_string()]);
}
