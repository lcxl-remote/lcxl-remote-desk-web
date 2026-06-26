//! The signal central brain's terminal copilot / completion orchestration.
//!
//! In the thin-edge model signal — not the edge — runs the AI behind the terminal
//! copilot ("how-to" / "explain error" suggestions) and inline command completion.
//! Both are driven entirely from the control-end ask: the browser sends the
//! terminal context inline (recent scrollback / prefix / error passage), so the
//! central brain needs no round-trip to the edge. signal redacts that context
//! fail-closed, dials its configured model once (tool-free, non-agentic — these
//! are latency-sensitive single shots), classifies any proposed command through
//! the shared baseline classifier, and streams the result back to the browser.
//!
//! This is signal's own implementation (single-account, single-turn), distinct
//! from the edge's agentic copilot: signal makes one direct model call rather than
//! running the read-tool loop, because the OSS single-account central brain serves
//! one operator and the inline context is sufficient. The model dial is `!Send`
//! (`awc`), so callers spawn these on actix's single-threaded runtime.

use actix_web::web;
use desk_agent_protocol::terminal_complete::{TerminalCompleteAsk, TerminalCompleteResult};
use desk_agent_protocol::terminal_copilot::{TerminalCopilotAsk, TerminalCopilotEvent};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::chat::{ChatMessage, ModelTurn};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::redaction::{Redactor, RegexRedactor};
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
use desk_diagnose_core::terminal_complete::{
    CompletionRedaction, build_completion_system_message, build_completion_user_message,
    parse_completions, redact_completion_ask,
};
use desk_diagnose_core::terminal_copilot::{
    build_copilot_system_message, build_copilot_user_message, parse_copilot_answer,
};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use sea_orm::DatabaseConnection;

use crate::diagnose_orchestrator::record_usage;
use crate::model_dial::SignalModelSeam;
use crate::model_provider;

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
    }
}

fn redaction_failed_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::RedactionFailed,
        message: "failed to redact terminal context".to_string(),
        retryable: false,
        safe_for_model: true,
    }
}

/// Send one notification-style signaling frame to a connection, if it is still in
/// the map. Best-effort: a gone connection or a write error is logged, not
/// surfaced.
async fn send_to_browser<T: serde::Serialize>(
    connection_map: &SharedConnectionMap,
    browser_connection_id: &str,
    request_id: &str,
    signaling_type: SignalingType,
    payload: &T,
) {
    let conn: Option<ConnectionState> = {
        let map = connection_map.read().await;
        map.get(browser_connection_id).cloned()
    };
    let Some(conn) = conn else {
        log::warn!("[terminal] browser {browser_connection_id} gone; dropping frame");
        return;
    };
    let frame = SignalingModel::new(
        request_id,
        signaling_type,
        None,
        Some(browser_connection_id.to_string()),
        serde_json::to_value(payload).ok(),
        None,
    );
    let text = match serde_json::to_string(&frame) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("[terminal] failed to encode frame for {browser_connection_id}: {e}");
            return;
        }
    };
    if let Err(e) = conn.session.write().await.text(text).await {
        log::warn!("[terminal] failed to send frame to {browser_connection_id}: {e}");
    }
}

/// Load the provider config, build the model seam, dial once over `messages`, and
/// record the call into the usage rollup. The single place every terminal model
/// dial flows through, so the seam construction and usage accounting stay shared
/// with the diagnose path.
async fn dial(
    db: &DatabaseConnection,
    messages: Vec<ChatMessage>,
) -> Result<ModelTurn, AgentError> {
    let config = model_provider::load(db)
        .await
        .map_err(|e| transport_error(format!("failed to load model provider config: {e}")))?;
    let seam = SignalModelSeam::from_config(&config)?;
    let request = ModelRequest::text_only(messages, ResponseFormatSpec::None);
    let mut sink = NullTurnSink;
    let turn = seam.call(request, &mut sink).await?;
    record_usage(db, config.model.as_deref().unwrap_or_default(), &turn.usage).await;
    Ok(turn)
}

/// Redact every browser-supplied free-text field of a copilot ask fail-closed. Any
/// redactor error aborts the whole turn (the content-free reason is returned for
/// logging). Mirrors the edge policy so a secret in the scrollback never reaches
/// the model.
fn redact_copilot_context(
    redactor: &dyn Redactor,
    ask: &mut TerminalCopilotAsk,
) -> Result<(), String> {
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

/// Run one completion turn: redact fail-closed, make a single tool-free model call,
/// and parse the answer into server-classified candidates. Returns the
/// [`TerminalCompleteResult`] streamed back (one frame per ask). A sensitive prefix
/// declines cleanly (empty, non-error); a redactor failure is a terminal error.
async fn run_completion_turn(
    db: &DatabaseConnection,
    request_id: &str,
    mut ask: TerminalCompleteAsk,
) -> TerminalCompleteResult {
    let redactor = RegexRedactor::new();
    match redact_completion_ask(&redactor, &mut ask) {
        CompletionRedaction::Ready => {}
        CompletionRedaction::DeclineSensitivePrefix => {
            // Never dial the model with a prefix that carries a secret: decline
            // silently with no candidates.
            return TerminalCompleteResult::ok(request_id, Vec::new());
        }
        CompletionRedaction::Failed(reason) => {
            log::warn!("[complete] redaction failed, aborting before model dial: {reason}");
            return TerminalCompleteResult::failed(request_id, redaction_failed_error());
        }
    }

    let default_shell = ask.context.shell.clone();
    let prefix = ask.prefix.clone();
    let messages = vec![
        build_completion_system_message(),
        build_completion_user_message(&ask),
    ];
    match dial(db, messages).await {
        Ok(turn) => {
            let completions = parse_completions(&turn.text, &prefix, &default_shell);
            TerminalCompleteResult::ok(request_id, completions)
        }
        Err(transport) => TerminalCompleteResult::failed(request_id, transport),
    }
}

/// Run one copilot turn: redact fail-closed, make a single tool-free model call,
/// and parse the structured answer (each proposed command stamped with the
/// server-authoritative risk / decision). Returns the single terminal
/// [`TerminalCopilotEvent`] — a `Final` answer or an `Error`.
async fn run_copilot_turn(
    db: &DatabaseConnection,
    request_id: &str,
    mut ask: TerminalCopilotAsk,
) -> TerminalCopilotEvent {
    let redactor = RegexRedactor::new();
    if let Err(reason) = redact_copilot_context(&redactor, &mut ask) {
        log::warn!("[copilot] redaction failed, aborting before model dial: {reason}");
        return TerminalCopilotEvent::error(request_id, 0, redaction_failed_error());
    }

    let default_shell = ask.context.shell.clone();
    let messages = vec![
        build_copilot_system_message(ask.mode),
        build_copilot_user_message(&ask),
    ];
    match dial(db, messages).await {
        Ok(turn) => {
            let (answer, _outcome) = parse_copilot_answer(&turn.text, &default_shell);
            TerminalCopilotEvent::final_answer(request_id, 0, answer)
        }
        Err(transport) => TerminalCopilotEvent::error(request_id, 0, transport),
    }
}

/// Drive a terminal completion centrally and stream the result to the browser.
/// Spawned by the control-frame authorizer (the model dial is `!Send`).
pub async fn run_completion(
    connection_map: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    ask: TerminalCompleteAsk,
) {
    let result = run_completion_turn(&db, &request_id, ask).await;
    send_to_browser(
        connection_map.as_ref(),
        &browser_connection_id,
        &request_id,
        SignalingType::TerminalCompleteResult,
        &result,
    )
    .await;
}

/// Drive a terminal copilot turn centrally and stream the terminal event to the
/// browser. Spawned by the control-frame authorizer (the model dial is `!Send`).
pub async fn run_copilot(
    connection_map: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    ask: TerminalCopilotAsk,
) {
    let event = run_copilot_turn(&db, &request_id, ask).await;
    send_to_browser(
        connection_map.as_ref(),
        &browser_connection_id,
        &request_id,
        SignalingType::TerminalCopilotEvent,
        &event,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::terminal_complete::TerminalCompletionContext;
    use desk_agent_protocol::terminal_copilot::{TerminalContext, TerminalCopilotMode};

    fn complete_ask(prefix: &str, recent: &str) -> TerminalCompleteAsk {
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

    fn copilot_ask(recent: &str, err: Option<&str>) -> TerminalCopilotAsk {
        TerminalCopilotAsk {
            conversation_id: None,
            mode: TerminalCopilotMode::HowTo,
            question: Some("how do I check the service?".into()),
            context: TerminalContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: None,
                recent_output: recent.into(),
                last_command: None,
                error_text: err.map(|e| e.into()),
            },
        }
    }

    /// The completion redactor leaves a clean prefix `Ready`, so the turn would
    /// proceed to dial; the helper itself is pure, so we assert the classification
    /// branch directly (a secret in the prefix declines without dialling).
    #[test]
    fn completion_redaction_declines_sensitive_prefix() {
        let redactor = RegexRedactor::new();
        let mut ask = complete_ask("aws configure set secret AKIAIOSFODNN7EXAMPLE", "");
        assert!(matches!(
            redact_completion_ask(&redactor, &mut ask),
            CompletionRedaction::DeclineSensitivePrefix
        ));
    }

    /// Copilot context redaction scrubs a secret out of the recent scrollback and
    /// the error passage before any model dial (fail-closed input).
    #[test]
    fn copilot_context_is_redacted_before_dial() {
        let redactor = RegexRedactor::new();
        let mut ask = copilot_ask(
            "export AWS_KEY=AKIAIOSFODNN7EXAMPLE",
            Some("token=ghp_secretsecretsecretsecretsecret1234 failed"),
        );
        redact_copilot_context(&redactor, &mut ask).expect("redaction succeeds");
        assert!(!ask.context.recent_output.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(
            !ask.context
                .error_text
                .as_deref()
                .unwrap_or_default()
                .contains("ghp_secret")
        );
    }
}
