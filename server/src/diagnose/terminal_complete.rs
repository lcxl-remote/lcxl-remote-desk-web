//! Daemon-side terminal command-completion result frame.
//!
//! Completion is orchestrated by the central signaling brain; the daemon only
//! relays the resulting [`TerminalCompleteResult`] back to the asking control
//! end. This helper serializes that result into its notification-style signaling
//! frame so a neutralized edge handler can answer the browser without re-deriving
//! frame serialization.

use desk_agent_protocol::terminal_complete::TerminalCompleteResult;
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
        SignalingType::TerminalCompletionsGenerated,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A result sent through the helper is serialized as a `TerminalCompleteResult`
    /// frame routed to the asking control end — the path a neutralized edge handler
    /// uses to answer the browser.
    #[test]
    fn send_result_broadcasts_frame_to_control_end() {
        let (tx, mut rx) = broadcast::channel(8);
        let result = TerminalCompleteResult::ok("req-1", Vec::new());
        send_completion_result(&tx, Some("conn-1".into()), &result);

        let text = rx.try_recv().expect("a frame was broadcast");
        let model: SignalingModel = serde_json::from_str(&text).unwrap();
        assert_eq!(
            model.signaling_type,
            SignalingType::TerminalCompletionsGenerated
        );
        let got = model
            .get_data_with_type::<TerminalCompleteResult>()
            .unwrap()
            .expect("the frame carries a TerminalCompleteResult");
        assert_eq!(got.request_id, "req-1");
    }
}
