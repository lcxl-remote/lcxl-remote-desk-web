//! Daemon-side terminal copilot frame sink.
//!
//! Terminal copilot is orchestrated by the central signaling brain; the daemon
//! only relays the resulting [`TerminalCopilotEvent`] frames back to the asking
//! control end. This module wires the shared [`CopilotStreamSink`] lifecycle→frame
//! mapping to the daemon's outbound signaling lane so a neutralized edge handler
//! can report a content-free error to the browser without re-deriving frame
//! serialization.

use desk_agent_protocol::terminal_copilot::TerminalCopilotEvent;
use desk_diagnose_core::terminal_copilot::{CopilotFrameSink, CopilotStreamSink};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use tokio::sync::broadcast;

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

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentError, AgentErrorKind};

    /// An error emitted through the sink is serialized as a `TerminalCopilotEvent`
    /// frame routed to the asking control end — the path a neutralized edge handler
    /// uses to report that copilot is centralized.
    #[tokio::test]
    async fn sink_emits_error_frame_to_control_end() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut sink = copilot_signaling_sink(tx, Some("conn-1".into()), "req-1".into());
        sink.emit_error(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: "centralized".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });

        let text = rx.try_recv().expect("a frame was broadcast");
        let model: SignalingModel = serde_json::from_str(&text).unwrap();
        assert_eq!(model.signaling_type, SignalingType::TerminalCopilotEvent);
        let event = model
            .get_data_with_type::<TerminalCopilotEvent>()
            .unwrap()
            .expect("the frame carries a TerminalCopilotEvent");
        assert_eq!(event.request_id, "req-1");
    }
}
