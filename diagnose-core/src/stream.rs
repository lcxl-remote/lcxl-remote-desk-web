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

use desk_agent_protocol::diagnose::DiagnoseEvent;
use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::agent_loop::{CircuitBreakReason, LoopOutcome};
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
}

impl<S: DiagnoseFrameSink> StreamingTurnSink<S> {
    /// Build a sink streaming a single diagnose request's frames to `sink`.
    pub fn new(sink: S, request_id: impl Into<String>) -> Self {
        Self {
            sink,
            request_id: request_id.into(),
            seq: 0,
            terminated: false,
        }
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
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink
            .emit(DiagnoseEvent::error(&self.request_id, seq, error));
        self.terminated = true;
    }

    /// Map a loop outcome to its terminal frame: a successful `Answered` already
    /// emitted its `Answer` via [`TurnSink::on_answer_committed`], so nothing more
    /// is sent; every other outcome becomes a terminal `Error`.
    pub fn finish_outcome(&mut self, outcome: &LoopOutcome) {
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
    }

    fn on_tool_started(&mut self, tool_name: &str, call_id: &str) {
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
        ));
    }

    fn on_awaiting_approval(&mut self, tool_name: &str, call_id: &str) {
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
        ));
    }

    fn on_tool_finished(&mut self, call_id: &str, ok: bool) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink.emit(DiagnoseEvent::tool_finished(
            &self.request_id,
            seq,
            call_id,
            ok,
        ));
    }

    fn on_answer_committed(&mut self, text: &str) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink
            .emit(DiagnoseEvent::answer(&self.request_id, seq, text));
        self.terminated = true;
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
        LoopOutcome::Truncated => AgentError {
            kind: AgentErrorKind::OutputLimitExceeded,
            message: "the model response was truncated before it finished; please retry".into(),
            retryable: true,
            safe_for_model: true,
            error_code: None,
        },
        LoopOutcome::CircuitBreak(reason) => {
            let message = match reason {
                CircuitBreakReason::StepBudget => {
                    "the assistant stopped after too many steps without reaching an answer"
                }
                CircuitBreakReason::SameToolRepeat => {
                    "the assistant stopped after repeating the same action too many times"
                }
            };
            AgentError {
                kind: AgentErrorKind::Internal,
                message: message.into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }
        }
        LoopOutcome::ProtocolError(_) => AgentError {
            kind: AgentErrorKind::Internal,
            message: "the model returned an invalid response".into(),
            retryable: true,
            safe_for_model: true,
            error_code: None,
        },
        LoopOutcome::TurnBusy => AgentError {
            kind: AgentErrorKind::SessionUnavailable,
            message: "a diagnosis is already running for this conversation".into(),
            retryable: true,
            safe_for_model: true,
            error_code: None,
        },
        LoopOutcome::SubjectRejected(_) => AgentError {
            kind: AgentErrorKind::PermissionDenied,
            message: "this conversation belongs to a different user".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
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
        b.on_tool_started("sysinfo", "c1");
        b.on_tool_finished("c1", true);
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
        assert!(!ev[1].awaiting_approval);
        assert_eq!(ev[2].tool_ok, Some(true));
        assert_eq!(ev[3].answer.as_deref(), Some("all good"));
        assert!(ev[3].is_terminal());
        assert!(b.is_terminated());
    }

    /// A mutating tool's approval wait flags `awaiting_approval` on its
    /// `ToolStarted` frame (a read start does not).
    #[test]
    fn awaiting_approval_sets_the_flag() {
        let (store, sink) = recorder();
        let mut b = StreamingTurnSink::new(sink, "r");
        b.on_awaiting_approval("exec_command", "c1");
        let ev = store.borrow();
        assert_eq!(ev[0].kind, DiagnoseEventKind::ToolStarted);
        assert_eq!(ev[0].tool_name.as_deref(), Some("exec_command"));
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
        b.on_tool_started("t", "c");
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

        let busy = terminal_error_for(&LoopOutcome::TurnBusy).unwrap();
        assert_eq!(busy.kind, AgentErrorKind::SessionUnavailable);
        assert!(busy.retryable);

        let breaker =
            terminal_error_for(&LoopOutcome::CircuitBreak(CircuitBreakReason::StepBudget)).unwrap();
        assert_eq!(breaker.kind, AgentErrorKind::Internal);
        assert!(!breaker.retryable);

        let subject =
            terminal_error_for(&LoopOutcome::SubjectRejected(SubjectMismatch::Device)).unwrap();
        assert_eq!(subject.kind, AgentErrorKind::PermissionDenied);
        assert!(!subject.retryable);
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
}
