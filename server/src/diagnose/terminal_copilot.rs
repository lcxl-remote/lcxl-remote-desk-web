//! Daemon-side terminal copilot orchestration (Default / DeskServer).
//!
//! Drives the shared agentic loop for an inbound `TerminalCopilotAsk`, reusing
//! the Direct runtime's read-only seams (model adapter + local read tools +
//! in-memory session). It redacts the browser-supplied terminal context
//! fail-closed *before* any model dial, runs at most
//! [`COPILOT_MAX_STEPS_PER_TURN`] read-only tool steps, parses the model's final
//! JSON answer (stamping each suggestion's server-authoritative execution
//! decision), and streams `TerminalCopilotEvent` frames back to the asking
//! control end.
//!
//! Read-only by construction: the registry is the copilot read-tool subset and
//! the scope is `ReadOnly`. No command is ever executed here — suggestions are
//! advice the operator runs themselves.

use desk_agent_protocol::authz::AuthorizationBlock;
use desk_agent_protocol::terminal_copilot::{TerminalCopilotAsk, TerminalCopilotEvent};
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentScope, Capability, ExecutionMode};
use desk_diagnose_core::agent_loop::{LoopDeps, LoopOutcome, run_agent_turn};
use desk_diagnose_core::conversation_key::derive_conversation_key;
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::registry::RegisteredTool;
use desk_diagnose_core::seam::{ClaimTurnParams, ModelSeam, SessionSeam, ToolSeam};
use desk_diagnose_core::terminal_copilot::{
    COPILOT_MAX_STEPS_PER_TURN, CopilotFrameSink, CopilotStreamSink, build_copilot_system_message,
    build_copilot_user_message, copilot_read_tools, parse_copilot_answer,
};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use tokio::sync::broadcast;

use super::redaction::{Redactor, RegexRedactor};

/// Stable local identity for a copilot session on single-machine /
/// remote-signaling links (no manager-resolved user).
const COPILOT_ACTOR: &str = "local-operator";

/// Forwards each [`TerminalCopilotEvent`] the shared [`CopilotStreamSink`] emits
/// to the asking control end: it serializes the notification-style frame
/// (`response_state = None`) and broadcasts it over the daemon's outbound lane.
pub struct SignalingCopilotFrames {
    outbound_tx: broadcast::Sender<String>,
    to_connection_id: Option<String>,
}

impl CopilotFrameSink for SignalingCopilotFrames {
    fn emit(&self, event: TerminalCopilotEvent) {
        let data = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("[copilot] failed to serialise TerminalCopilotEvent: {e}");
                return;
            }
        };
        let frame = SignalingModel::new(
            &event.request_id,
            SignalingType::TerminalCopilotEvent,
            None,
            self.to_connection_id.clone(),
            Some(data),
            None,
        );
        match serde_json::to_string(&frame) {
            Ok(text) => {
                let _ = self.outbound_tx.send(text);
            }
            Err(e) => log::warn!("[copilot] failed to serialise TerminalCopilotEvent frame: {e}"),
        }
    }
}

/// The daemon's copilot stream sink: the shared lifecycle→frame mapping wired to
/// the signaling outbound lane. The frame mapping itself lives in
/// [`desk_diagnose_core::terminal_copilot`] so it cannot drift from the manager.
pub type CopilotTurnSink = CopilotStreamSink<SignalingCopilotFrames>;

/// Build a [`CopilotTurnSink`] that streams a single request's frames back to the
/// asking control end (`to_connection_id`).
pub fn copilot_signaling_sink(
    outbound_tx: broadcast::Sender<String>,
    to_connection_id: Option<String>,
    request_id: String,
) -> CopilotTurnSink {
    CopilotStreamSink::new(
        SignalingCopilotFrames {
            outbound_tx,
            to_connection_id,
        },
        request_id,
    )
}

/// Drive one copilot turn over the given read-only seams, streaming frames to
/// `sink`. Redacts the browser-supplied context fail-closed before any model
/// dial; on a redaction failure it emits a terminal `Error` and never calls the
/// model or tools.
#[allow(clippy::too_many_arguments)]
pub async fn run_copilot_turn(
    session: &dyn SessionSeam,
    model: &dyn ModelSeam,
    tools: &dyn ToolSeam,
    request_id: &str,
    mut ask: TerminalCopilotAsk,
    authz: Option<&AuthorizationBlock>,
    connection_id: Option<String>,
    sink: &mut CopilotTurnSink,
) {
    // Redact the non-authoritative context before it can reach the model.
    let redactor = RegexRedactor::new();
    if let Err(reason) = redact_context(&redactor, &mut ask) {
        log::warn!("[copilot] redaction failed, aborting before model dial: {reason}");
        sink.emit_error(AgentError {
            kind: AgentErrorKind::RedactionFailed,
            message: "failed to redact terminal context".to_string(),
            retryable: false,
            safe_for_model: true,
        });
        return;
    }

    let default_shell = ask.context.shell.clone();
    let registry = copilot_read_tools();
    let scope = copilot_read_scope(&registry, authz);
    let (actor_id, device_id, tenant_id) = subject_for(authz);

    let turn_id = format!("{request_id}-t0");
    let conversation_key = derive_conversation_key(
        tenant_id.as_deref(),
        &actor_id,
        &device_id,
        ask.conversation_id.as_deref(),
        request_id,
    );

    let clock = || chrono::Utc::now().to_rfc3339();
    let now = clock();
    let system_prompt = build_copilot_system_message(ask.mode);
    let user = build_copilot_user_message(&ask);
    let deps = LoopDeps {
        session_seam: session,
        model,
        tools,
        registry: &registry,
        response_format: ResponseFormatSpec::None,
        system_prompt,
        max_context_bytes: desk_diagnose_core::DEFAULT_MAX_CONTEXT_BYTES,
        max_steps_per_turn: COPILOT_MAX_STEPS_PER_TURN,
        clock: &clock,
        heartbeat: None,
    };
    let claim = ClaimTurnParams {
        conversation_id: conversation_key,
        tenant_id,
        actor_id,
        device_id,
        policy_revision: 0,
        current_pdp_scope: scope,
        turn_id,
        request_id: Some(request_id.to_string()),
        connection_id,
        now,
    };

    match run_agent_turn(&deps, claim, user, sink).await {
        Ok(LoopOutcome::Answered(text)) => {
            let (answer, _outcome) = parse_copilot_answer(&text, &default_shell);
            sink.emit_final(answer);
        }
        Ok(other) => sink.emit_error(outcome_error(&other)),
        Err(transport) => sink.emit_error(transport),
    }
}

/// Redact every browser-supplied free-text field fail-closed. Any redactor error
/// aborts the whole turn (the content-free reason is returned for logging).
fn redact_context(redactor: &dyn Redactor, ask: &mut TerminalCopilotAsk) -> Result<(), String> {
    let ctx = &mut ask.context;
    ctx.recent_output = redactor
        .redact(&ctx.recent_output)
        .map_err(|e| e.reason)?
        .text;
    if let Some(err) = ctx.error_text.take() {
        ctx.error_text = Some(redactor.redact(&err).map_err(|e| e.reason)?.text);
    }
    if let Some(last) = ctx.last_command.take() {
        ctx.last_command = Some(redactor.redact(&last).map_err(|e| e.reason)?.text);
    }
    Ok(())
}

/// The read scope a copilot turn runs under: the read-tool subset's
/// capabilities, intersected with the manager-granted scope when present. Always
/// read-only — the copilot never gains the mutating exec capability.
fn copilot_read_scope(
    registry: &[RegisteredTool],
    authz: Option<&AuthorizationBlock>,
) -> AgentScope {
    let mut granted: Vec<Capability> = registry.iter().map(|t| t.required_capability).collect();
    granted.dedup();
    if let Some(a) = authz {
        granted.retain(|cap| a.scope.granted.contains(cap));
    }
    AgentScope {
        granted,
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

/// Resolve the session subject from the manager authorization, falling back to a
/// stable local identity on single-machine / remote-signaling links.
fn subject_for(authz: Option<&AuthorizationBlock>) -> (String, String, Option<String>) {
    match authz {
        Some(a) => (
            a.actor
                .user_id
                .map(|id| format!("user:{id}"))
                .unwrap_or_else(|| COPILOT_ACTOR.to_string()),
            a.device
                .device_id
                .map(|id| format!("device:{id}"))
                .unwrap_or_else(|| "local".to_string()),
            None,
        ),
        None => (COPILOT_ACTOR.to_string(), "local".to_string(), None),
    }
}

/// Map a non-`Answered` loop outcome to a control-end error (the message stays
/// content-free — no model output leaks).
fn outcome_error(outcome: &LoopOutcome) -> AgentError {
    let (kind, message) = match outcome {
        LoopOutcome::CircuitBreak(_) => (
            AgentErrorKind::OutputLimitExceeded,
            "the copilot reached its step limit before answering",
        ),
        LoopOutcome::Truncated => (
            AgentErrorKind::OutputLimitExceeded,
            "the copilot response was truncated",
        ),
        LoopOutcome::ProtocolError(_) => (
            AgentErrorKind::Internal,
            "the model violated the response contract",
        ),
        LoopOutcome::TurnBusy => (
            AgentErrorKind::SessionUnavailable,
            "another copilot turn is already in progress",
        ),
        LoopOutcome::SubjectRejected(_) => (
            AgentErrorKind::PermissionDenied,
            "this conversation belongs to a different session",
        ),
        LoopOutcome::Answered(_) => (AgentErrorKind::Internal, "internal error"),
    };
    AgentError {
        kind,
        message: message.to_string(),
        retryable: false,
        safe_for_model: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnose::agent::InMemorySessionSeam;
    use desk_agent_protocol::exec::ExecDecision;
    use desk_agent_protocol::terminal_copilot::{
        TerminalContext, TerminalCopilotEventKind, TerminalCopilotMode,
    };
    use desk_diagnose_core::chat::{ChatRole, ModelTurn, StopReason, ToolCall};
    use desk_diagnose_core::seam::{ModelRequest, ToolRunOutput, TurnSink};
    use std::sync::Mutex;

    /// A model that captures the last user message it was sent (to assert
    /// redaction happened before the dial) and answers with a fixed JSON.
    struct CapturingModel {
        answer_json: String,
        captured_user: Mutex<Option<String>>,
    }

    #[async_trait::async_trait(?Send)]
    impl ModelSeam for CapturingModel {
        async fn call(
            &self,
            request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            let user = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, ChatRole::User))
                .map(|m| m.text.clone());
            *self.captured_user.lock().unwrap() = user;
            Ok(ModelTurn {
                text: self.answer_json.clone(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            })
        }
    }

    struct NoTools;

    #[async_trait::async_trait(?Send)]
    impl ToolSeam for NoTools {
        async fn run_read(&self, _call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
            unreachable!("the copilot answered immediately without calling a tool")
        }
    }

    fn ask_with_secret() -> TerminalCopilotAsk {
        TerminalCopilotAsk {
            conversation_id: None,
            mode: TerminalCopilotMode::HowTo,
            question: Some("how do I inspect the shadow file".into()),
            context: TerminalContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: None,
                recent_output: "export AWS_KEY=AKIAIOSFODNN7EXAMPLE".into(),
                last_command: None,
                error_text: None,
            },
        }
    }

    fn drain_events(rx: &mut broadcast::Receiver<String>) -> Vec<TerminalCopilotEvent> {
        let mut out = Vec::new();
        while let Ok(text) = rx.try_recv() {
            let model: SignalingModel = serde_json::from_str(&text).unwrap();
            if let Ok(Some(ev)) = model.get_data_with_type::<TerminalCopilotEvent>() {
                out.push(ev);
            }
        }
        out
    }

    /// End-to-end daemon path: redaction runs before the model dial, and the
    /// streamed terminal `Final` carries the server-computed `ExecDecision` (the
    /// model never reports it) — a blocklisted command classifies as `Blocked`.
    #[tokio::test]
    async fn daemon_copilot_redacts_input_and_streams_server_decision() {
        let answer = r#"{"explanation_md":"Reading it is restricted.",
            "suggestions":[{"command":"cat /etc/shadow","shell":"bash","note":"read shadow"}]}"#;
        let model = CapturingModel {
            answer_json: answer.into(),
            captured_user: Mutex::new(None),
        };
        let tools = NoTools;
        let session = InMemorySessionSeam::new();
        let (tx, mut rx) = broadcast::channel(32);
        let mut sink = copilot_signaling_sink(tx, Some("conn-1".into()), "req-1".into());

        run_copilot_turn(
            &session,
            &model,
            &tools,
            "req-1",
            ask_with_secret(),
            None,
            Some("conn-1".into()),
            &mut sink,
        )
        .await;

        // The model received a redacted prompt (the AWS key was scrubbed before
        // the dial), proving redaction is fail-closed and runs server-side.
        let captured = model
            .captured_user
            .lock()
            .unwrap()
            .clone()
            .expect("the model was called");
        assert!(
            !captured.contains("AKIAIOSFODNN7EXAMPLE"),
            "secret leaked into the model prompt: {captured}"
        );

        // The daemon streamed a terminal Final whose suggestion decision is the
        // server-computed Blocked, not anything the model self-reported.
        let events = drain_events(&mut rx);
        let final_ev = events
            .iter()
            .find(|e| matches!(e.kind, TerminalCopilotEventKind::Final))
            .expect("a terminal Final frame was streamed");
        let answer = final_ev.answer.as_ref().expect("Final carries an answer");
        assert_eq!(answer.suggestions.len(), 1);
        assert_eq!(answer.suggestions[0].decision, ExecDecision::Blocked);
    }
}
