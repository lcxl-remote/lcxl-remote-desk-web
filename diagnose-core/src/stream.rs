//! Streaming bridge: maps the agentic loop's [`TurnSink`] lifecycle onto
//! [`DiagnoseEvent`] wire frames.
//!
//! Both runtimes drive the same loop and both surface progress to the control end
//! as notification-style `DiagnoseEvent` frames, so the mapping lives here once —
//! the Direct runtime and the manager wrap it over their own outbound channels and
//! can never drift on frame shape, sequencing, or terminal semantics.
//!
//! A [`StreamingTurnSink`] owns a monotonic `seq` and a `terminated` latch: the
//! loop's `on_answer_committed` emits the terminal [`DiagnoseEventKind::Answer`],
//! and the runtime maps any non-answered outcome to a terminal
//! [`DiagnoseEventKind::Error`] via [`StreamingTurnSink::finish_outcome`]. The
//! latch guarantees exactly one terminal frame even if a late save error follows a
//! committed answer. The loop does not push the turn's start through `TurnSink`
//! (it has no turn id), so the runtime emits it via
//! [`StreamingTurnSink::turn_started`] before running the turn.

use desk_agent_protocol::content_safety::StreamRetractionReason;
use desk_agent_protocol::diagnose::DiagnoseEvent;
use desk_agent_protocol::provenance::AiProvenance;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_utils::error::DeskErrorCode;

use crate::agent_loop::{CircuitBreakReason, LoopOutcome};
use crate::content_safety::{content_blocked_error, stream_retraction_reason_for};
use crate::seam::TurnSink;

/// Emits assembled [`DiagnoseEvent`] frames over a runtime's outbound channel. The
/// Direct runtime implements it over the connection's frame sender; the manager
/// over its cross-instance stream. A blanket impl covers any `Fn(DiagnoseEvent)`,
/// so a runtime can pass a closure that forwards to its existing sink.
pub trait DiagnoseFrameSink {
    fn emit(&self, event: DiagnoseEvent);
}

impl<F: Fn(DiagnoseEvent)> DiagnoseFrameSink for F {
    fn emit(&self, event: DiagnoseEvent) {
        self(event)
    }
}

/// A [`TurnSink`] that turns the loop's streaming + lifecycle callbacks into
/// `DiagnoseEvent` frames with a monotonic `seq`, forwarding each to a
/// [`DiagnoseFrameSink`]. Holds a `terminated` latch so at most one terminal frame
/// (an `Answer` or an `Error`) is ever emitted for the request.
pub struct StreamingTurnSink<S> {
    sink: S,
    request_id: String,
    seq: u32,
    terminated: bool,
    uncommitted_partial: bool,
    context_trimmed_turn_id: Option<String>,
    /// Machine-readable AI marking stamped onto the terminal `Answer` frame, when
    /// the upper layer (which knows the model and has a clock) injected one before
    /// running the turn. This crate has neither, so it carries the pre-built stamp
    /// rather than building it here.
    provenance: Option<AiProvenance>,
}

impl<S: DiagnoseFrameSink> StreamingTurnSink<S> {
    /// Build a sink streaming a single diagnose request's frames to `sink`.
    pub fn new(sink: S, request_id: impl Into<String>) -> Self {
        Self::starting_at(sink, request_id, 0)
    }

    /// Build a sink whose first emitted frame uses `initial_seq`. Runtimes that
    /// emitted collection/modeling status frames before entering the shared loop
    /// use this to preserve one monotonic request stream.
    pub fn starting_at(sink: S, request_id: impl Into<String>, initial_seq: u32) -> Self {
        Self {
            sink,
            request_id: request_id.into(),
            seq: initial_seq,
            terminated: false,
            uncommitted_partial: false,
            context_trimmed_turn_id: None,
            provenance: None,
        }
    }

    /// Inject the AI provenance to stamp onto the terminal `Answer` frame. The
    /// upper layer builds it (it knows the marking scheme and has a clock, which
    /// this crate lacks) and sets it before the turn runs; this crate only carries
    /// it through. The answer is marked AI by its frame kind regardless, so a sink
    /// with no injected provenance still emits a valid (unenriched) answer frame.
    pub fn set_provenance(&mut self, provenance: AiProvenance) {
        self.provenance = Some(provenance);
    }

    fn next_seq(&mut self) -> u32 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// Emit a `TurnStarted` frame. The loop does not push the turn start through
    /// `TurnSink`; the runtime (which owns the turn id) emits it before running the
    /// turn so the control end can show the turn beginning.
    pub fn turn_started(&mut self, turn_id: &str) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink
            .emit(DiagnoseEvent::turn_started(&self.request_id, seq, turn_id));
    }

    /// Emit a terminal `Error` frame, unless a terminal frame was already sent
    /// (so a save error following a committed answer cannot double-terminate the
    /// stream).
    pub fn error(&mut self, error: AgentError) {
        self.emit_error(error, None);
    }

    fn emit_error(&mut self, error: AgentError, reason: Option<StreamRetractionReason>) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        let event = match reason {
            Some(reason) => {
                DiagnoseEvent::error_with_retraction_reason(&self.request_id, seq, error, reason)
            }
            None => DiagnoseEvent::error(&self.request_id, seq, error),
        };
        self.sink.emit(event);
        self.uncommitted_partial = false;
        self.terminated = true;
    }

    /// Map a loop outcome to its terminal frame: a successful `Answered` already
    /// emitted its `Answer` via [`TurnSink::on_answer_committed`], so nothing more
    /// is sent; every other outcome becomes a terminal `Error`.
    pub fn finish_outcome(&mut self, outcome: &LoopOutcome) {
        if let LoopOutcome::ContentRejected(decision) = outcome
            && let Some(reason) = stream_retraction_reason_for(*decision)
        {
            self.emit_error(content_blocked_error(), Some(reason));
            return;
        }
        if let Some(err) = terminal_error_for(outcome) {
            self.error(err);
        }
    }

    /// Whether a terminal frame (answer or error) has been emitted.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<S: DiagnoseFrameSink> TurnSink for StreamingTurnSink<S> {
    fn on_text_delta(&mut self, delta: &str) {
        // Provisional streaming text; the committed answer is authoritative. Skip
        // empty deltas to avoid emitting no-op partial frames.
        if self.terminated || delta.is_empty() {
            return;
        }
        let seq = self.next_seq();
        self.sink
            .emit(DiagnoseEvent::partial(&self.request_id, seq, delta));
        self.uncommitted_partial = true;
    }

    fn on_partial_committed(&mut self) {
        if self.terminated || !self.uncommitted_partial {
            return;
        }
        let seq = self.next_seq();
        self.sink
            .emit(DiagnoseEvent::partial_committed(&self.request_id, seq));
        self.uncommitted_partial = false;
    }

    fn on_turn_retracted(&mut self, reason: StreamRetractionReason, error: Option<AgentError>) {
        if self.terminated {
            return;
        }
        if self.uncommitted_partial {
            let seq = self.next_seq();
            self.sink.emit(DiagnoseEvent::retracted(
                &self.request_id,
                seq,
                reason,
                error,
            ));
            self.uncommitted_partial = false;
            self.terminated = true;
        } else if let Some(error) = error {
            self.emit_error(error, Some(reason));
        }
    }

    fn on_tool_started(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink.emit(DiagnoseEvent::tool_started(
            &self.request_id,
            seq,
            tool_name,
            call_id,
            false,
            arguments_json,
        ));
    }

    fn on_awaiting_approval(&mut self, tool_name: &str, call_id: &str, arguments_json: &str) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink.emit(DiagnoseEvent::tool_started(
            &self.request_id,
            seq,
            tool_name,
            call_id,
            true,
            arguments_json,
        ));
    }

    fn on_tool_finished(
        &mut self,
        call_id: &str,
        ok: bool,
        output: &str,
        background_task_id: Option<&str>,
    ) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        let mut event = DiagnoseEvent::tool_finished(&self.request_id, seq, call_id, ok, output);
        if let Some(background_task_id) = background_task_id {
            event = event.with_background_task_id(background_task_id);
        }
        self.sink.emit(event);
    }

    fn on_answer_committed(&mut self, text: &str) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        let mut frame = DiagnoseEvent::answer(&self.request_id, seq, text);
        if let Some(provenance) = &self.provenance {
            frame = frame.with_provenance(provenance.clone());
        }
        self.sink.emit(frame);
        self.uncommitted_partial = false;
        self.terminated = true;
    }

    fn on_context_trimmed(&mut self, turn_id: &str) {
        if self.terminated || self.context_trimmed_turn_id.as_deref() == Some(turn_id) {
            return;
        }
        self.context_trimmed_turn_id = Some(turn_id.to_string());
        let seq = self.next_seq();
        self.sink.emit(DiagnoseEvent::status_for_turn(
            &self.request_id,
            seq,
            "context_trimmed",
            turn_id,
        ));
    }

    fn on_turn_discarded(&mut self) {
        // No frame: the provisional text is simply superseded. A truncated turn's
        // terminal `Error` is emitted by the runtime via `finish_outcome`.
    }
}

/// The terminal [`AgentError`] for a non-answered loop outcome, or `None` for a
/// successful `Answered` turn (whose `Answer` frame already closed the stream).
/// Messages are deliberately generic (model-safe): they carry no internal turn
/// details that could be fed back to a model.
pub fn terminal_error_for(outcome: &LoopOutcome) -> Option<AgentError> {
    let err = match outcome {
        LoopOutcome::Answered(_) => return None,
        LoopOutcome::ContentRejected(_) => crate::content_safety::content_blocked_error(),
        LoopOutcome::ContentSafetyUnavailable(error) => error.clone(),
        LoopOutcome::Truncated => AgentError {
            kind: AgentErrorKind::OutputLimitExceeded,
            message: "the model response was truncated before it finished; please retry".into(),
            retryable: true,
            safe_for_model: true,
            error_code: Some(DeskErrorCode::COPILOT_RESPONSE_TRUNCATED.code()),
        },
        LoopOutcome::CircuitBreak(reason) => {
            let (message, error_code) = match reason {
                CircuitBreakReason::StepBudget => (
                    "the assistant stopped after too many steps without reaching an answer",
                    DeskErrorCode::COPILOT_STEP_LIMIT_EXCEEDED,
                ),
                CircuitBreakReason::SameToolRepeat => (
                    "the assistant stopped after repeating the same action too many times",
                    DeskErrorCode::AGENT_SAME_TOOL_REPEAT_LIMIT,
                ),
            };
            AgentError {
                kind: AgentErrorKind::Internal,
                message: message.into(),
                retryable: false,
                safe_for_model: true,
                error_code: Some(error_code.code()),
            }
        }
        LoopOutcome::ProtocolError(_) => AgentError {
            kind: AgentErrorKind::Internal,
            message: "the model returned an invalid response".into(),
            retryable: true,
            safe_for_model: true,
            error_code: Some(DeskErrorCode::COPILOT_PROTOCOL_VIOLATION.code()),
        },
        LoopOutcome::TurnBusy => AgentError {
            kind: AgentErrorKind::SessionUnavailable,
            message: "a diagnosis is already running for this conversation".into(),
            retryable: true,
            safe_for_model: true,
            error_code: Some(DeskErrorCode::COPILOT_TURN_BUSY.code()),
        },
        LoopOutcome::SubjectRejected(_) => AgentError {
            kind: AgentErrorKind::PermissionDenied,
            message: "this conversation belongs to a different user".into(),
            retryable: false,
            safe_for_model: true,
            error_code: Some(DeskErrorCode::COPILOT_SUBJECT_MISMATCH.code()),
        },
    };
    Some(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ModelTurnError, StopReason};
    use crate::session::SubjectMismatch;
    use desk_agent_protocol::diagnose::DiagnoseEventKind;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A recording frame sink: a closure pushing each frame into a shared buffer.
    fn recorder() -> (Rc<RefCell<Vec<DiagnoseEvent>>>, impl Fn(DiagnoseEvent)) {
        let store = Rc::new(RefCell::new(Vec::new()));
        let s = store.clone();
        (store, move |e| s.borrow_mut().push(e))
    }

    /// A read-tool turn maps to TurnStarted → ToolStarted → ToolFinished → Answer
    /// with a gapless monotonic seq and the right payload on each frame.
    #[test]
    fn maps_turn_tool_and_answer_frames_in_order() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "req-1");
        b.turn_started("turn-1");
        b.on_tool_started("sysinfo", "c1", r#"{"limit":5}"#);
        b.on_tool_finished("c1", true, "five processes", None);
        b.on_answer_committed("all good");

        let ev = store.borrow();
        let kinds: Vec<_> = ev.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiagnoseEventKind::TurnStarted,
                DiagnoseEventKind::ToolStarted,
                DiagnoseEventKind::ToolFinished,
                DiagnoseEventKind::Answer,
            ]
        );
        assert_eq!(
            ev.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(ev.iter().all(|e| e.request_id == "req-1"));
        assert_eq!(ev[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(ev[1].tool_name.as_deref(), Some("sysinfo"));
        assert_eq!(ev[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(ev[1].tool_arguments_json.as_deref(), Some(r#"{"limit":5}"#));
        assert!(!ev[1].awaiting_approval);
        assert_eq!(ev[2].tool_ok, Some(true));
        assert_eq!(ev[2].tool_output.as_deref(), Some("five processes"));
        assert!(ev[2].background_task_id.is_none());
        assert_eq!(ev[3].answer.as_deref(), Some("all good"));
        assert!(ev[3].is_terminal());
        assert!(b.is_terminated());
    }

    #[test]
    fn background_dispatch_frame_carries_structured_task_id() {
        let (store, sink) = recorder();
        let mut bridge = StreamingTurnSink::new(sink, "req-1");
        bridge.on_tool_finished(
            "c1",
            true,
            r#"{"status":"background_running","background_task_id":"exec_task_1"}"#,
            Some("exec_task_1"),
        );

        let events = store.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].background_task_id.as_deref(), Some("exec_task_1"));
    }

    /// An injected provenance is stamped onto the terminal `Answer` frame so the
    /// AI-generated diagnosis answer carries a machine-readable marking.
    #[test]
    fn answer_frame_carries_injected_provenance() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "req-1");
        b.set_provenance(AiProvenance::stamp(
            None,
            Some("2026-07-14T00:00:00Z".into()),
        ));
        b.on_answer_committed("all good");
        let ev = store.borrow();
        let prov = ev[0]
            .provenance
            .as_ref()
            .expect("answer frame carries the injected provenance");
        assert_eq!(
            prov.marking_scheme.as_deref(),
            Some(desk_agent_protocol::provenance::AI_MARKING_SCHEME_V1)
        );
    }

    /// With no provenance injected, the answer frame simply omits it (the frame
    /// kind still marks it AI; a missing marking never downgrades it).
    #[test]
    fn answer_frame_without_provenance_omits_it() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "req-1");
        b.on_answer_committed("all good");
        assert!(store.borrow()[0].provenance.is_none());
    }

    /// A mutating tool's approval wait flags `awaiting_approval` on its
    /// `ToolStarted` frame (a read start does not).
    #[test]
    fn awaiting_approval_sets_the_flag() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "r");
        b.on_awaiting_approval("exec_command", "c1", r#"{"command":"uptime"}"#);
        let ev = store.borrow();
        assert_eq!(ev[0].kind, DiagnoseEventKind::ToolStarted);
        assert_eq!(ev[0].tool_name.as_deref(), Some("exec_command"));
        assert_eq!(
            ev[0].tool_arguments_json.as_deref(),
            Some(r#"{"command":"uptime"}"#)
        );
        assert!(ev[0].awaiting_approval);
    }

    /// Text deltas become `Partial` frames; empty deltas are dropped.
    #[test]
    fn text_deltas_become_partials_and_skip_empty() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "r");
        b.on_text_delta("Port ");
        b.on_text_delta("");
        b.on_text_delta("8080 busy");
        let ev = store.borrow();
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().all(|e| e.kind == DiagnoseEventKind::Partial));
        assert_eq!(ev[0].partial_summary.as_deref(), Some("Port "));
        assert_eq!(ev[1].partial_summary.as_deref(), Some("8080 busy"));
    }

    /// Once an answer commits, every later frame (a save-error terminal, a stray
    /// tool event) is suppressed — exactly one terminal per request.
    #[test]
    fn no_frame_after_a_committed_answer() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "r");
        b.on_answer_committed("done");
        b.error(AgentError {
            kind: AgentErrorKind::Internal,
            message: "late save failure".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
        b.on_tool_started("t", "c", "{}");
        b.finish_outcome(&LoopOutcome::Answered("done".into()));
        let ev = store.borrow();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, DiagnoseEventKind::Answer);
    }

    /// `finish_outcome` on a successful answer emits nothing extra.
    #[test]
    fn finish_outcome_on_answered_is_a_noop() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "r");
        b.on_answer_committed("done");
        b.finish_outcome(&LoopOutcome::Answered("done".into()));
        assert_eq!(store.borrow().len(), 1);
    }

    /// Every non-answered outcome maps to a single terminal `Error` frame.
    #[test]
    fn finish_outcome_maps_non_answered_to_terminal_error() {
        let outcomes = [
            LoopOutcome::Truncated,
            LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget),
            LoopOutcome::CircuitBreak(CircuitBreakReason::SameToolRepeat),
            LoopOutcome::ProtocolError(ModelTurnError::InconsistentStopReason {
                stop_reason: StopReason::EndTurn,
                tool_calls: 1,
            }),
            LoopOutcome::TurnBusy,
            LoopOutcome::SubjectRejected(SubjectMismatch::Actor),
        ];
        for outcome in outcomes {
            let (store, sink) = recorder();
            let mut b = StreamingTurnSink::new(sink, "r");
            b.finish_outcome(&outcome);
            let ev = store.borrow();
            assert_eq!(ev.len(), 1, "outcome {outcome:?} should emit one frame");
            assert_eq!(ev[0].kind, DiagnoseEventKind::Error);
            assert!(ev[0].is_terminal());
            assert!(ev[0].error.is_some());
        }
    }

    /// The classified error kinds carry the right retryable / kind for the UI.
    #[test]
    fn terminal_error_kinds_are_classified() {
        assert!(terminal_error_for(&LoopOutcome::Answered("x".into())).is_none());

        let truncated = terminal_error_for(&LoopOutcome::Truncated).unwrap();
        assert_eq!(truncated.kind, AgentErrorKind::OutputLimitExceeded);
        assert!(truncated.retryable);
        assert_eq!(
            truncated.error_code,
            Some(DeskErrorCode::COPILOT_RESPONSE_TRUNCATED.code())
        );

        let busy = terminal_error_for(&LoopOutcome::TurnBusy).unwrap();
        assert_eq!(busy.kind, AgentErrorKind::SessionUnavailable);
        assert!(busy.retryable);
        assert_eq!(
            busy.error_code,
            Some(DeskErrorCode::COPILOT_TURN_BUSY.code())
        );

        let breaker =
            terminal_error_for(&LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)).unwrap();
        assert_eq!(breaker.kind, AgentErrorKind::Internal);
        assert!(!breaker.retryable);
        assert_eq!(
            breaker.error_code,
            Some(DeskErrorCode::COPILOT_STEP_LIMIT_EXCEEDED.code())
        );

        let repeat = terminal_error_for(&LoopOutcome::CircuitBreak(
            CircuitBreakReason::SameToolRepeat,
        ))
        .unwrap();
        assert_eq!(
            repeat.error_code,
            Some(DeskErrorCode::AGENT_SAME_TOOL_REPEAT_LIMIT.code())
        );

        let subject =
            terminal_error_for(&LoopOutcome::SubjectRejected(SubjectMismatch::Device)).unwrap();
        assert_eq!(subject.kind, AgentErrorKind::PermissionDenied);
        assert!(!subject.retryable);
        assert_eq!(
            subject.error_code,
            Some(DeskErrorCode::COPILOT_SUBJECT_MISMATCH.code())
        );
    }

    /// A turn that starts then truncates: TurnStarted, then the runtime maps the
    /// `Truncated` outcome to a terminal error (the loop's `on_turn_discarded`
    /// emitted no frame).
    #[test]
    fn truncated_turn_emits_start_then_terminal_error() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "r");
        b.turn_started("turn-1");
        b.on_text_delta("half ans");
        b.on_turn_discarded();
        b.finish_outcome(&LoopOutcome::Truncated);
        let ev = store.borrow();
        let kinds: Vec<_> = ev.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiagnoseEventKind::TurnStarted,
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Error,
            ]
        );
        assert!(ev.last().unwrap().is_terminal());
    }
    #[test]
    fn provisional_partial_is_retracted_once_and_late_frames_are_ignored() {
        use desk_agent_protocol::content_safety::{ContentSafetyDecision, StreamRetractionReason};

        let (store, sink) = recorder();
        let mut bridge = StreamingTurnSink::new(sink, "r");
        bridge.on_text_delta("unsafe provisional text");
        bridge.on_turn_retracted(
            StreamRetractionReason::PolicyBlocked,
            Some(crate::content_safety::content_blocked_error()),
        );
        bridge.on_text_delta("late text");
        bridge.finish_outcome(&LoopOutcome::ContentRejected(ContentSafetyDecision::Block));

        let events = store.borrow();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, DiagnoseEventKind::Partial);
        assert_eq!(events[1].kind, DiagnoseEventKind::Retracted);
        assert!(events[1].is_terminal());
        assert_eq!(
            events[1].retraction_reason,
            Some(StreamRetractionReason::PolicyBlocked)
        );
    }

    #[test]
    fn policy_failure_without_partial_uses_reasoned_error_not_retracted() {
        use desk_agent_protocol::content_safety::StreamRetractionReason;

        let (store, sink) = recorder();
        let mut bridge = StreamingTurnSink::new(sink, "r");
        bridge.on_turn_retracted(
            StreamRetractionReason::SafeRedirect,
            Some(crate::content_safety::content_blocked_error()),
        );

        let events = store.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, DiagnoseEventKind::Error);
        assert_eq!(
            events[0].retraction_reason,
            Some(StreamRetractionReason::SafeRedirect)
        );
        assert!(events[0].is_terminal());
    }

    #[test]
    fn input_safe_redirect_outcome_keeps_reason_without_sink_callback() {
        use desk_agent_protocol::content_safety::{ContentSafetyDecision, StreamRetractionReason};

        let (store, sink) = recorder();
        let mut bridge = StreamingTurnSink::new(sink, "r");
        bridge.finish_outcome(&LoopOutcome::ContentRejected(
            ContentSafetyDecision::SafeRedirect,
        ));

        let events = store.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, DiagnoseEventKind::Error);
        assert_eq!(
            events[0].retraction_reason,
            Some(StreamRetractionReason::SafeRedirect)
        );
    }

    #[test]
    fn context_trim_notice_is_non_terminal_turn_scoped_and_deduped() {
        let (store, sink) = recorder();
        let mut bridge = StreamingTurnSink::new(sink, "r");
        bridge.on_context_trimmed("turn-7");
        bridge.on_context_trimmed("turn-7");
        bridge.on_answer_committed("done");

        let events = store.borrow();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, DiagnoseEventKind::Status);
        assert_eq!(events[0].status.as_deref(), Some("context_trimmed"));
        assert_eq!(events[0].turn_id.as_deref(), Some("turn-7"));
        assert!(!events[0].is_terminal());
        assert_eq!(events[1].kind, DiagnoseEventKind::Answer);
    }

    #[test]
    fn committed_partial_precedes_tool_and_later_image_failure_is_error() {
        use desk_agent_protocol::content_safety::StreamRetractionReason;

        let (store, sink) = recorder();
        let mut bridge = StreamingTurnSink::new(sink, "r");
        bridge.on_text_delta("reviewed reasoning");
        bridge.on_partial_committed();
        bridge.on_tool_started("screenshot", "c1", "{}");
        bridge.on_turn_retracted(
            StreamRetractionReason::PolicyBlocked,
            Some(crate::content_safety::content_blocked_error()),
        );

        let events = store.borrow();
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::PartialCommitted,
                DiagnoseEventKind::ToolStarted,
                DiagnoseEventKind::Error,
            ]
        );
        assert!(events.last().unwrap().is_terminal());
    }
}
