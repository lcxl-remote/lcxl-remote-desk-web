//! Daemon-side terminal command completion (Default / DeskServer).
//!
//! Completion is non-agentic and latency-sensitive: it makes a single tool-free
//! model call that turns the operator's command prefix into a short list of full
//! command lines, parses them with the shared logic (keeping only true
//! prefix-extending candidates, each stamped with the server-authoritative
//! `risk` / `decision`), and answers with one [`TerminalCompleteResult`] frame.
//!
//! The browser-supplied context is redacted fail-closed *before* the dial via the
//! shared [`redact_completion_ask`] policy; a secret typed into the prefix itself
//! makes the turn decline (empty result) rather than ever reach the model. No
//! command is executed here — accepting a completion only fills the input.

use desk_agent_protocol::terminal_complete::{TerminalCompleteAsk, TerminalCompleteResult};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::redaction::RegexRedactor;
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
use desk_diagnose_core::terminal_complete::{
    CompletionRedaction, build_completion_system_message, build_completion_user_message,
    parse_completions, redact_completion_ask,
};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use tokio::sync::broadcast;

/// Serialize a [`TerminalCompleteResult`] into its notification-style signaling
/// frame (`response_state = None`) and broadcast it to the asking control end over
/// the daemon's outbound lane.
pub fn send_completion_result(
    outbound_tx: &broadcast::Sender<String>,
    to_connection_id: Option<String>,
    result: &TerminalCompleteResult,
) {
    let data = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[complete] failed to serialise TerminalCompleteResult: {e}");
            return;
        }
    };
    let frame = SignalingModel::new(
        &result.request_id,
        SignalingType::TerminalCompleteResult,
        None,
        to_connection_id,
        Some(data),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(e) => log::warn!("[complete] failed to serialise TerminalCompleteResult frame: {e}"),
    }
}

/// Run one completion turn: redact fail-closed, make a single tool-free model
/// call, and parse the answer into server-classified candidates. Returns the
/// [`TerminalCompleteResult`] the caller streams back (one frame per ask).
///
/// On a redactor failure the result is a terminal error; on a sensitive prefix it
/// is a clean empty result (the operator simply gets no ghost text); otherwise it
/// is the best-first candidate list.
pub async fn run_completion_turn(
    model: &dyn ModelSeam,
    request_id: &str,
    mut ask: TerminalCompleteAsk,
) -> TerminalCompleteResult {
    let redactor = RegexRedactor::new();
    match redact_completion_ask(&redactor, &mut ask) {
        CompletionRedaction::Ready => {}
        CompletionRedaction::DeclineSensitivePrefix => {
            // Decline silently with no candidates: never dial the model with a
            // prefix that carries a secret.
            return TerminalCompleteResult::ok(request_id, Vec::new());
        }
        CompletionRedaction::Failed(reason) => {
            log::warn!("[complete] redaction failed, aborting before model dial: {reason}");
            return TerminalCompleteResult::failed(
                request_id,
                AgentError {
                    kind: AgentErrorKind::RedactionFailed,
                    message: "failed to redact terminal context".to_string(),
                    retryable: false,
                    safe_for_model: true,
                },
            );
        }
    }

    let default_shell = ask.context.shell.clone();
    let prefix = ask.prefix.clone();
    let system = build_completion_system_message();
    let user = build_completion_user_message(&ask);
    let request = ModelRequest::text_only(vec![system, user], ResponseFormatSpec::None);
    let mut sink = NullTurnSink;
    match model.call(request, &mut sink).await {
        Ok(turn) => {
            let completions = parse_completions(&turn.text, &prefix, &default_shell);
            TerminalCompleteResult::ok(request_id, completions)
        }
        Err(transport) => TerminalCompleteResult::failed(request_id, transport),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::terminal_complete::TerminalCompletionContext;
    use desk_diagnose_core::chat::{ChatRole, ModelTurn, StopReason};
    use desk_diagnose_core::seam::TurnSink;
    use std::sync::Mutex;

    /// A model capturing the last user prompt and answering with fixed JSON.
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

    fn ask(prefix: &str, recent: &str) -> TerminalCompleteAsk {
        TerminalCompleteAsk {
            prefix: prefix.into(),
            context: TerminalCompletionContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: recent.into(),
            },
        }
    }

    /// End-to-end daemon path: the model prompt is redacted before the dial, and
    /// the returned candidate carries the server-computed decision (not anything
    /// the model self-reports) plus a true prefix-extending suffix.
    #[tokio::test]
    async fn redacts_then_returns_server_classified_candidate() {
        let answer = r#"{"completions":[{"command":"systemctl status nginx","note":"status"}]}"#;
        let model = CapturingModel {
            answer_json: answer.into(),
            captured_user: Mutex::new(None),
        };
        let result = run_completion_turn(
            &model,
            "req-1",
            ask("systemctl ", "export AWS_KEY=AKIAIOSFODNN7EXAMPLE"),
        )
        .await;

        let captured = model.captured_user.lock().unwrap().clone().unwrap();
        assert!(
            !captured.contains("AKIAIOSFODNN7EXAMPLE"),
            "secret leaked into the model prompt: {captured}"
        );
        assert!(!result.is_error());
        assert_eq!(result.completions.len(), 1);
        assert_eq!(result.completions[0].completion, "status nginx");
    }

    /// A secret typed into the prefix itself makes the turn decline without ever
    /// dialling the model — an empty (non-error) result.
    #[tokio::test]
    async fn sensitive_prefix_declines_without_dialling() {
        let model = CapturingModel {
            answer_json: "{}".into(),
            captured_user: Mutex::new(None),
        };
        let result = run_completion_turn(
            &model,
            "req-2",
            ask("aws configure set secret AKIAIOSFODNN7EXAMPLE", ""),
        )
        .await;
        assert!(!result.is_error());
        assert!(result.completions.is_empty());
        assert!(
            model.captured_user.lock().unwrap().is_none(),
            "the model must not be dialled for a sensitive prefix"
        );
    }
}
