//! The agentic tool-calling loop (pure orchestration; runtime-agnostic).
//!
//! [`run_agent_turn`] drives one conversational turn over the seams: it claims
//! the turn (atomically, via [`SessionSeam`]), then repeatedly calls the model
//! ([`ModelSeam`]), validates each turn with [`classify_model_turn`], and either
//! returns the final answer or runs the requested read tools ([`ToolSeam`]) and
//! loops. An outer wrapper guarantees the turn machine is always settled
//! (`finish_turn`) on every exit path.
//!
//! Only read tools run here; the mutating path (approval + real execution) is
//! added in a later PR. The same exposure matrix ([`registry::exposed_specs`] /
//! [`registry::lookup_exposed`]) both advertises tools to the model and validates
//! a returned call, so a model can never invoke a tool it was not shown.
//!
//! Circuit breakers are turn-level (reset when the turn is claimed): a per-turn
//! step budget ([`MAX_STEPS_PER_TURN`]) and a same-tool repeat cap
//! ([`MAX_SAME_TOOL_PER_TURN`]).
//!
//! [`MAX_STEPS_PER_TURN`]: crate::MAX_STEPS_PER_TURN
//! [`MAX_SAME_TOOL_PER_TURN`]: crate::MAX_SAME_TOOL_PER_TURN

use std::collections::HashMap;

use desk_agent_protocol::AgentError;

use crate::chat::{ChatMessage, ModelTurnError, TurnDisposition, classify_model_turn};
use crate::registry::{RegisteredTool, ToolEffect, exposed_specs, lookup_exposed};
use crate::seam::{
    ClaimError, ClaimTurnParams, ModelRequest, ModelSeam, SessionSeam, ToolSeam, TurnSink,
};
use crate::session::{SubjectMismatch, TurnState};

/// Why the loop's circuit breaker stopped a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitBreakReason {
    /// The per-turn step budget was exhausted.
    StepBudget,
    /// One tool was called too many times within the turn.
    SameToolRepeat,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// The model produced a final text answer.
    Answered(String),
    /// The model turn was truncated (`MaxTokens` / `Other`) and discarded.
    Truncated,
    /// A circuit breaker stopped the turn.
    CircuitBreak(CircuitBreakReason),
    /// The model violated the wire contract (inconsistent stop reason / bad args).
    ProtocolError(ModelTurnError),
    /// A turn is already running for this conversation.
    TurnBusy,
    /// The follow-up came from a different subject than the session.
    SubjectRejected(SubjectMismatch),
}

/// The seams + config the loop runs over, borrowed for one turn.
pub struct LoopDeps<'a> {
    pub session_seam: &'a dyn SessionSeam,
    pub model: &'a dyn ModelSeam,
    pub tools: &'a dyn ToolSeam,
    pub registry: &'a [RegisteredTool],
    pub response_format: crate::prompt::ResponseFormatSpec,
    /// Wall-clock source (RFC3339); the core stays free of a time dependency.
    pub clock: &'a dyn Fn() -> String,
}

/// Run one agent turn end to end. `user_message` is appended after the turn is
/// claimed. Returns the [`LoopOutcome`]; only a model / tool / session backend
/// transport failure surfaces as `Err`.
pub async fn run_agent_turn(
    deps: &LoopDeps<'_>,
    claim: ClaimTurnParams,
    user_message: ChatMessage,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let turn_id = claim.turn_id.clone();
    let mut session = match deps.session_seam.claim_turn(claim).await {
        Ok(s) => s,
        Err(ClaimError::Busy) => return Ok(LoopOutcome::TurnBusy),
        Err(ClaimError::Subject(m)) => return Ok(LoopOutcome::SubjectRejected(m)),
        Err(ClaimError::Backend(e)) => return Err(e),
    };

    // Append the user's message and persist before the first model call.
    session.conversation.push(user_message);
    deps.session_seam.save(&session).await?;

    // Run the loop; whatever happens, settle the turn machine and persist once.
    let result = run_inner(deps, &mut session, &turn_id, sink).await;
    let terminal = match &result {
        Ok(LoopOutcome::ProtocolError(_)) | Err(_) => TurnState::Failed,
        // Answered / Truncated / CircuitBreak return to Idle so a follow-up turn
        // can be claimed; Busy / SubjectRejected never claimed a turn here.
        _ => TurnState::Idle,
    };
    session.finish_turn(terminal, (deps.clock)());
    // Surface a save failure only if the loop itself otherwise succeeded.
    let save = deps.session_seam.save(&session).await;
    match (result, save) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), _) => Err(e),
    }
}

/// The loop body: model → validate → answer / discard / run read tools → repeat.
async fn run_inner(
    deps: &LoopDeps<'_>,
    session: &mut crate::session::PersistedAgentSession,
    turn_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let mut seq: u32 = 0;
    let mut mint = move || {
        let id = format!("{turn_id}-{seq}");
        seq += 1;
        id
    };
    let mut same_tool: HashMap<String, u32> = HashMap::new();

    loop {
        if session.turn_step_budget_exhausted() {
            return Ok(LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget));
        }

        let specs = exposed_specs(
            deps.registry,
            &session.scope_snapshot,
            &session.execution_state,
        );
        let request =
            ModelRequest::text_only(session.conversation.clone(), deps.response_format.clone());
        let request = ModelRequest {
            tools: specs,
            ..request
        };

        let turn = deps.model.call(request, sink).await?;
        session.record_step(turn.usage);

        match classify_model_turn(&turn) {
            Err(e) => return Ok(LoopOutcome::ProtocolError(e)),
            Ok(TurnDisposition::Answer) => {
                session.conversation.push(ChatMessage::text(
                    mint(),
                    crate::chat::ChatRole::Assistant,
                    turn.text.clone(),
                ));
                deps.session_seam.save(session).await?;
                return Ok(LoopOutcome::Answered(turn.text));
            }
            // A truncated turn is discarded: nothing is appended or executed.
            Ok(TurnDisposition::Discard) => return Ok(LoopOutcome::Truncated),
            Ok(TurnDisposition::InvokeTools) => {
                // Record the assistant's tool-call message so the conversation
                // stays well-formed when replayed to the model.
                let refs = turn.tool_calls.iter().map(|c| c.to_ref()).collect();
                session.conversation.push(ChatMessage::assistant_tool_calls(
                    mint(),
                    turn.text.clone(),
                    refs,
                ));

                for call in &turn.tool_calls {
                    // Same-tool repeat circuit breaker.
                    let count = same_tool.entry(call.name.clone()).or_insert(0);
                    *count += 1;
                    if *count > crate::MAX_SAME_TOOL_PER_TURN {
                        deps.session_seam.save(session).await?;
                        return Ok(LoopOutcome::CircuitBreak(
                            CircuitBreakReason::SameToolRepeat,
                        ));
                    }

                    let result_text = run_one_tool(deps, session, call).await?;
                    let mut msg = ChatMessage::tool_result(mint(), &call.id, result_text.content);
                    msg.image_data_url = result_text.image_data_url;
                    session.conversation.push(msg);
                }
                deps.session_seam.save(session).await?;
                // Loop again with the tool results in context.
            }
        }
    }
}

/// Run a single validated tool call, returning the (redacted) result to feed back
/// to the model. A call naming a tool that is not exposed under the current scope,
/// or a tool error, is turned into an error tool-result so the conversation stays
/// well-formed and the model can adjust — neither aborts the turn.
async fn run_one_tool(
    deps: &LoopDeps<'_>,
    session: &crate::session::PersistedAgentSession,
    call: &crate::chat::ToolCall,
) -> Result<crate::seam::ToolRunOutput, AgentError> {
    let Some(tool) = lookup_exposed(
        deps.registry,
        &call.name,
        &session.scope_snapshot,
        &session.execution_state,
    ) else {
        return Ok(error_output(format!(
            "tool `{}` is not available in the current scope",
            call.name
        )));
    };

    match tool.effect {
        ToolEffect::ReadOnly => match deps.tools.run_read(call).await {
            Ok(out) => Ok(out),
            // A safe-for-model tool error is reported back as a tool result; the
            // backend transport itself does not fail the turn.
            Err(e) => Ok(error_output(format!("tool error: {}", e.message))),
        },
        // The mutating path (approval + execution) is added in a later PR.
        ToolEffect::Mutating => Ok(error_output(format!(
            "tool `{}` requires execution support that is not enabled",
            call.name
        ))),
    }
}

/// A tool result carrying an error message (no image).
fn error_output(message: String) -> crate::seam::ToolRunOutput {
    crate::seam::ToolRunOutput {
        content: message,
        image_data_url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatRole, ModelTurn, StopReason, ToolCall, ToolSpec};
    use crate::prompt::ResponseFormatSpec;
    use crate::seam::{ToolRunOutput, TurnSink};
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
                    params.tenant_id.clone(),
                    params.actor_id.clone(),
                    params.device_id.clone(),
                    params.policy_revision,
                    params.current_pdp_scope.clone(),
                    params.now.clone(),
                )
            });
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
            *slot = Some(session.clone());
            Ok(session)
        }
        async fn save(&self, session: &PersistedAgentSession) -> Result<(), AgentError> {
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
            tenant_id: None,
            actor_id: "actor".into(),
            device_id: "device".into(),
            policy_revision: 1,
            current_pdp_scope: scope(),
            turn_id: "turn-1".into(),
            request_id: Some("req".into()),
            connection_id: Some("conn".into()),
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
        ModelTurn {
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments_json: "{}".into(),
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
            clock,
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
        let outcome = run_agent_turn(
            &deps(&sess, &model, &tools, &reg, &clock),
            claim(),
            user,
            &mut sink,
        )
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

    /// Repeatedly calling the *same* tool trips the same-tool cap before the step
    /// budget.
    #[tokio::test]
    async fn same_tool_repeat_circuit_breaks() {
        let sess = MemSession::default();
        let turns: std::collections::VecDeque<_> = (0..crate::MAX_STEPS_PER_TURN + 5)
            .map(|i| tool_use(&format!("c{i}"), "sysinfo"))
            .collect();
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
        let outcome = run_agent_turn(
            &deps(&sess, &model, &tools, &reg, &clock),
            claim(),
            user,
            &mut sink,
        )
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
            let mut s =
                PersistedAgentSession::new("conv", None, "actor", "device", 1, scope(), "t");
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
}
