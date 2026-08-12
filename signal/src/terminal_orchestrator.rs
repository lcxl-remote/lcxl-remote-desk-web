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
use desk_agent_protocol::provenance::AiProvenance;
use desk_agent_protocol::terminal_complete::{TerminalCompleteAsk, TerminalCompleteResult};
use desk_agent_protocol::terminal_copilot::{TerminalCopilotAsk, TerminalCopilotEvent};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::chat::{ChatMessage, ModelTurn};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::redaction::{Redactor, RegexRedactor};
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink, TurnSink};
use desk_diagnose_core::terminal_complete::{
    CompletionRedaction, build_completion_system_message, build_completion_user_message,
    parse_completions, redact_completion_ask,
};
use desk_diagnose_core::terminal_copilot::{
    CopilotFrameSink, CopilotStreamSink, build_copilot_history_messages,
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
        error_code: None,
    }
}

fn redaction_failed_error() -> AgentError {
    AgentError {
        kind: AgentErrorKind::RedactionFailed,
        message: "failed to redact terminal context".to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
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
    sink: &mut dyn TurnSink,
) -> Result<(ModelTurn, Option<String>), AgentError> {
    let config = model_provider::load(db)
        .await
        .map_err(|e| transport_error(format!("failed to load model provider config: {e}")))?;
    let seam = SignalModelSeam::from_config(&config)?;
    let request = ModelRequest::text_only(messages, ResponseFormatSpec::None);
    let turn = seam.call(request, sink).await?;
    record_usage(db, config.model.as_deref().unwrap_or_default(), &turn.usage).await;
    // Return the model name so the caller can stamp AI provenance on the answer.
    Ok((turn, config.model))
}

/// Redact every browser-supplied free-text field of a copilot ask fail-closed. Any
/// redactor error aborts the whole turn (the content-free reason is returned for
/// logging). Mirrors the edge policy so a secret in the scrollback never reaches
/// the model.
fn redact_copilot_context(
    redactor: &dyn Redactor,
    ask: &mut TerminalCopilotAsk,
) -> Result<(), String> {
    // The control-end-supplied conversation history is replayed into the prompt on
    // the stateless path, so it is redacted fail-closed exactly like the live
    // context — a secret echoed into an earlier turn never reaches the model.
    for turn in &mut ask.history {
        turn.user = redactor.redact(&turn.user).map_err(|e| e.reason)?.text;
        turn.assistant = redactor.redact(&turn.assistant).map_err(|e| e.reason)?.text;
    }
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
    // Completion has no progressive UI: the candidates render together, so the
    // model text is not streamed.
    match dial(db, messages, &mut NullTurnSink).await {
        Ok((turn, model)) => {
            let completions = parse_completions(&turn.text, &prefix, &default_shell);
            mark_completions(TerminalCompleteResult::ok(request_id, completions), model)
        }
        Err(transport) => TerminalCompleteResult::failed(request_id, transport),
    }
}

/// Stamp the AI marking onto a completion result that carries candidates
/// (Art.50(2)). The completions are novel model-generated command suggestions, not
/// an assistive edit of the operator's input, so they are marked; an empty result
/// shows nothing AI-generated and is left unmarked. `model` is the resolved model
/// name when known (the desk-server config may leave it unset).
fn mark_completions(
    result: TerminalCompleteResult,
    model: Option<String>,
) -> TerminalCompleteResult {
    if result.completions.is_empty() {
        return result;
    }
    result.with_provenance(AiProvenance::stamp(
        model,
        Some(chrono::Utc::now().to_rfc3339()),
    ))
}

/// Run one copilot turn: redact fail-closed, make a single tool-free model call,
/// and parse the structured answer (each proposed command stamped with the
/// server-authoritative risk / decision). Streams the explanation prose as
/// `Partial` frames through `sink` as the model writes it, then emits the terminal
/// `Final` answer (or an `Error`). The trailing ```json suggestions block is
/// withheld from the prose stream; the `Final` frame carries the parsed answer.
async fn run_copilot_turn(
    db: &DatabaseConnection,
    mut ask: TerminalCopilotAsk,
    sink: &mut CopilotStreamSink<impl CopilotFrameSink>,
) {
    let redactor = RegexRedactor::new();
    if let Err(reason) = redact_copilot_context(&redactor, &mut ask) {
        log::warn!("[copilot] redaction failed, aborting before model dial: {reason}");
        sink.emit_error(redaction_failed_error());
        return;
    }

    let default_shell = ask.context.shell.clone();
    // Stateless multi-turn: the control end replays the conversation, so the prompt
    // is [system, ...prior turns (capped + redacted), current user]. The signal
    // central brain keeps no session of its own.
    let mut messages = vec![build_copilot_system_message(
        ask.mode,
        ask.locale.as_deref(),
    )];
    messages.extend(build_copilot_history_messages(&ask.history));
    messages.push(build_copilot_user_message(&ask));
    match dial(db, messages, sink).await {
        Ok((turn, model)) => {
            let (answer, _outcome) = parse_copilot_answer(&turn.text, &default_shell);
            // Mark the AI-generated answer with machine-readable provenance (Art.50(2)).
            sink.set_provenance(AiProvenance::stamp(
                model,
                Some(chrono::Utc::now().to_rfc3339()),
            ));
            sink.emit_final(answer);
        }
        Err(transport) => sink.emit_error(transport),
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
        SignalingType::TerminalCompletionsGenerated,
        &result,
    )
    .await;
}

/// Drive a terminal copilot turn centrally and stream its events to the browser.
/// Spawned by the control-frame authorizer (the model dial is `!Send`).
///
/// An ordered async forwarder (mirroring the manager copilot entry) decouples the
/// synchronous stream sink from the async WebSocket send: the sink enqueues each
/// frame and this drains them in order, so partial explanation frames stream to the
/// browser as the model writes them without blocking the dial.
pub async fn run_copilot(
    connection_map: web::Data<SharedConnectionMap>,
    db: DatabaseConnection,
    request_id: String,
    browser_connection_id: String,
    ask: TerminalCopilotAsk,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TerminalCopilotEvent>();
    let forward_map = connection_map.clone();
    let forward_browser = browser_connection_id.clone();
    let forwarder = actix_web::rt::spawn(async move {
        while let Some(event) = rx.recv().await {
            send_to_browser(
                forward_map.as_ref(),
                &forward_browser,
                &event.request_id,
                SignalingType::TerminalCopilotUpdated,
                &event,
            )
            .await;
        }
    });
    let frame_sink = move |event: TerminalCopilotEvent| {
        // The forwarder owns delivery; a closed channel only means the browser is
        // gone, which the turn need not react to.
        let _ = tx.send(event);
    };
    let mut sink = CopilotStreamSink::new(frame_sink, request_id).streaming_text();

    run_copilot_turn(&db, ask, &mut sink).await;

    // Dropping the sink drops `tx`, ending the forwarder once it has flushed every
    // queued frame.
    drop(sink);
    let _ = forwarder.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::terminal_complete::TerminalCompletionContext;
    use desk_agent_protocol::terminal_copilot::{
        TerminalContext, TerminalCopilotEventKind, TerminalCopilotMode,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A sqlite memory DB with just the model-provider table (empty → the default
    /// config has no api_key, so the seam build fails closed).
    async fn provider_db() -> DatabaseConnection {
        use crate::entity::model_provider;
        use sea_orm::{ConnectionTrait, Database, Schema};
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(model_provider::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    /// A recording frame sink: a closure pushing each emitted frame into a shared
    /// buffer.
    fn recorder() -> (
        Rc<RefCell<Vec<TerminalCopilotEvent>>>,
        impl Fn(TerminalCopilotEvent),
    ) {
        let store = Rc::new(RefCell::new(Vec::new()));
        let s = store.clone();
        (store, move |e| s.borrow_mut().push(e))
    }

    fn complete_ask(prefix: &str, recent: &str) -> TerminalCompleteAsk {
        TerminalCompleteAsk {
            prefix: prefix.into(),
            context: TerminalCompletionContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: recent.into(),
            },
            model_id: None,
            org_id: None,
        }
    }

    fn copilot_ask(recent: &str, err: Option<&str>) -> TerminalCopilotAsk {
        TerminalCopilotAsk {
            conversation_id: None,
            mode: TerminalCopilotMode::HowTo,
            question: Some("how do I check the service?".into()),
            locale: None,
            history: Vec::new(),
            context: TerminalContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: None,
                recent_output: recent.into(),
                last_command: None,
                error_text: err.map(|e| e.into()),
            },
            model_id: None,
            org_id: None,
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

    fn sample_completion() -> desk_agent_protocol::terminal_complete::CommandCompletion {
        use desk_agent_protocol::RiskLevel;
        use desk_agent_protocol::exec::ExecDecision;
        desk_agent_protocol::terminal_complete::CommandCompletion {
            completion: "ctl status nginx".into(),
            note: "Show the nginx service status.".into(),
            risk: RiskLevel::Low,
            decision: ExecDecision::NotExecutable,
        }
    }

    /// A result carrying candidates is marked AI-generated with the resolved model;
    /// an empty result shows nothing AI-generated and stays unmarked.
    #[test]
    fn mark_completions_marks_only_nonempty_results() {
        let marked = mark_completions(
            TerminalCompleteResult::ok("r", vec![sample_completion()]),
            Some("gpt-4o".into()),
        );
        let prov = marked.provenance.expect("candidates carry a marking");
        assert_eq!(prov.model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(
            prov.marking_scheme.as_deref(),
            Some(desk_agent_protocol::provenance::AI_MARKING_SCHEME_V1)
        );

        let empty = mark_completions(
            TerminalCompleteResult::ok("r", Vec::new()),
            Some("gpt-4o".into()),
        );
        assert!(empty.provenance.is_none());
    }

    /// The desk-server config may not surface a model name; the candidates are still
    /// marked (the marking scheme establishes AI-generation; the model is optional).
    #[test]
    fn mark_completions_marks_even_without_a_model_name() {
        let marked = mark_completions(
            TerminalCompleteResult::ok("r", vec![sample_completion()]),
            None,
        );
        let prov = marked.provenance.expect("candidates carry a marking");
        assert!(prov.model_id.is_none());
        assert_eq!(
            prov.marking_scheme.as_deref(),
            Some(desk_agent_protocol::provenance::AI_MARKING_SCHEME_V1)
        );
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

    /// The replayed conversation history is redacted fail-closed too, so a secret
    /// echoed into an earlier turn never reaches the model on a follow-up.
    #[test]
    fn copilot_history_is_redacted_before_dial() {
        use desk_agent_protocol::terminal_copilot::CopilotHistoryTurn;
        let redactor = RegexRedactor::new();
        let mut ask = copilot_ask("clean", None);
        ask.history = vec![CopilotHistoryTurn {
            user: "export AWS_KEY=AKIAIOSFODNN7EXAMPLE".into(),
            assistant: "token=ghp_secretsecretsecretsecretsecret1234 leaked".into(),
        }];
        redact_copilot_context(&redactor, &mut ask).expect("redaction succeeds");
        assert!(!ask.history[0].user.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!ask.history[0].assistant.contains("ghp_secret"));
    }

    /// With no provider configured the seam build fails closed, so the streaming
    /// copilot turn emits exactly one terminal `Error` frame (no half-stream).
    #[actix_web::test]
    async fn copilot_turn_without_provider_emits_single_error_frame() {
        let db = provider_db().await;
        let (store, frame_sink) = recorder();
        let mut sink = CopilotStreamSink::new(frame_sink, "req-1").streaming_text();
        run_copilot_turn(
            &db,
            copilot_ask("bind: address already in use", None),
            &mut sink,
        )
        .await;
        drop(sink);

        let ev = store.borrow();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, TerminalCopilotEventKind::Error);
        assert_eq!(ev[0].request_id, "req-1");
    }

    /// The full entry runs to completion even with no live browser: the forwarder
    /// drains the (no-op) sends and joins, exercising the channel + forwarder wiring
    /// without hanging.
    #[actix_web::test]
    async fn run_copilot_completes_without_a_live_browser() {
        let db = provider_db().await;
        let connection_map = web::Data::new(SharedConnectionMap::new());
        run_copilot(
            connection_map,
            db,
            "req-2".into(),
            "browser-gone".into(),
            copilot_ask("recent", None),
        )
        .await;
    }
}
