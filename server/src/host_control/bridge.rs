//! Bridge senders that adapt the legacy `std::sync::mpsc` interfaces of
//! `desk_input_injection`'s host-control helper / whiteboard manager onto the
//! unified [`HostControlHub`](super::HostControlHub).
//!
//! `desk_input_injection` exposes its overlay control points via two
//! `std::sync::mpsc::Sender<_>` channel kinds:
//! - [`PrivateScreenCommand`] for the privacy-screen overlay.
//! - [`WhiteboardCommand`] for the whiteboard overlay.
//!
//! After the host-control unification (Step 6) all overlay traffic must travel
//! through the hub's broadcast channel so a single ws bridge to Tauri carries
//! both portable and daemon scenarios. The functions in this module produce
//! sender handles with the legacy signatures while internally translating each
//! command into a [`HostControlMessage`] and pushing it through the hub.
//!
//! The translation runs on a dedicated `std::thread` (the channels are
//! synchronous and `recv()` is blocking). The thread exits cleanly once all
//! senders are dropped (i.e. the underlying `desk_session`/`peer_connection`
//! has been torn down).

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use desk_input_injection::model::host_control::{PrivateScreenCommand, WhiteboardCommand};
use log::{debug, trace, warn};

use super::{HostControlHub, HostControlMessage};

/// Build a private-screen command sender that fans into the hub's broadcast
/// channel as `PrivateScreenShow` / `PrivateScreenHide` messages.
///
/// `Quit` terminates the bridge thread without producing a wire message —
/// matching the legacy `desk_input_injection` semantics where Quit is purely
/// a thread-shutdown signal.
pub fn bridge_private_screen_to_hub(
    hub: Arc<HostControlHub>,
) -> mpsc::Sender<PrivateScreenCommand> {
    let (tx, rx) = mpsc::channel::<PrivateScreenCommand>();
    thread::Builder::new()
        .name("hostctrl-bridge-privscr".to_string())
        .spawn(move || run_private_screen_bridge(hub, rx))
        .expect("spawn private-screen bridge thread");
    tx
}

/// Build a whiteboard command sender that fans into the hub's broadcast channel
/// as `WhiteboardShow` / `WhiteboardDraw` / `WhiteboardHide` messages.
pub fn bridge_whiteboard_to_hub(hub: Arc<HostControlHub>) -> mpsc::Sender<WhiteboardCommand> {
    let (tx, rx) = mpsc::channel::<WhiteboardCommand>();
    thread::Builder::new()
        .name("hostctrl-bridge-whiteboard".to_string())
        .spawn(move || run_whiteboard_bridge(hub, rx))
        .expect("spawn whiteboard bridge thread");
    tx
}

fn run_private_screen_bridge(hub: Arc<HostControlHub>, rx: mpsc::Receiver<PrivateScreenCommand>) {
    while let Ok(cmd) = rx.recv() {
        let msg = match cmd {
            PrivateScreenCommand::Show(connection_id) => {
                HostControlMessage::PrivateScreenShow { connection_id }
            }
            PrivateScreenCommand::Hide(connection_id) => {
                HostControlMessage::PrivateScreenHide { connection_id }
            }
            PrivateScreenCommand::Quit => {
                debug!("[hostctrl/bridge/privscr] received Quit; exiting");
                break;
            }
        };
        if let Err(e) = hub.send_command(msg) {
            warn!("[hostctrl/bridge/privscr] hub send failed: {e}");
        } else {
            trace!("[hostctrl/bridge/privscr] forwarded one command");
        }
    }
}

fn run_whiteboard_bridge(hub: Arc<HostControlHub>, rx: mpsc::Receiver<WhiteboardCommand>) {
    while let Ok(cmd) = rx.recv() {
        let msg = match cmd {
            WhiteboardCommand::Show(connection_id) => {
                HostControlMessage::WhiteboardShow { connection_id }
            }
            WhiteboardCommand::Hide(connection_id) => {
                HostControlMessage::WhiteboardHide { connection_id }
            }
            WhiteboardCommand::DrawMessage(json_str) => {
                let message = serde_json::from_str::<serde_json::Value>(&json_str)
                    .unwrap_or(serde_json::Value::String(json_str));
                HostControlMessage::WhiteboardDraw {
                    connection_id: String::new(),
                    message,
                }
            }
            WhiteboardCommand::Quit => {
                debug!("[hostctrl/bridge/whiteboard] received Quit; exiting");
                break;
            }
        };
        if let Err(e) = hub.send_command(msg) {
            warn!("[hostctrl/bridge/whiteboard] hub send failed: {e}");
        } else {
            trace!("[hostctrl/bridge/whiteboard] forwarded one command");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_control::HostControlHub;
    use std::time::Duration;

    /// Show / Hide commands round-trip through the bridge and arrive as the
    /// matching `HostControlMessage` variants on the hub's broadcast channel.
    #[tokio::test]
    async fn private_screen_bridge_forwards_show_hide() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        let tx = bridge_private_screen_to_hub(Arc::clone(&hub));

        tx.send(PrivateScreenCommand::Show("c1".to_string()))
            .expect("send show");
        let got = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        match got {
            HostControlMessage::PrivateScreenShow { connection_id } => {
                assert_eq!(connection_id, "c1");
            }
            other => panic!("unexpected: {other:?}"),
        }

        tx.send(PrivateScreenCommand::Hide("c1".to_string()))
            .expect("send hide");
        let got = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        assert!(matches!(
            got,
            HostControlMessage::PrivateScreenHide { connection_id } if connection_id == "c1"
        ));
    }

    /// `Quit` does NOT produce a wire message — it just terminates the bridge.
    #[tokio::test]
    async fn private_screen_bridge_quit_terminates_silently() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        let tx = bridge_private_screen_to_hub(Arc::clone(&hub));

        tx.send(PrivateScreenCommand::Quit).expect("send quit");
        let attempt = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        assert!(attempt.is_err(), "Quit must not yield a hub message");
    }

    /// Whiteboard Show / DrawMessage / Hide round-trip through the bridge.
    #[tokio::test]
    async fn whiteboard_bridge_forwards_all_three_commands() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        let tx = bridge_whiteboard_to_hub(Arc::clone(&hub));

        tx.send(WhiteboardCommand::Show("c1".to_string()))
            .expect("send show");
        let show = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        assert!(matches!(
            show,
            HostControlMessage::WhiteboardShow { connection_id } if connection_id == "c1"
        ));

        tx.send(WhiteboardCommand::DrawMessage(
            r#"{"action":"stroke"}"#.to_string(),
        ))
        .expect("send draw");
        let draw = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        match draw {
            HostControlMessage::WhiteboardDraw { message, .. } => {
                assert_eq!(message["action"], "stroke");
            }
            other => panic!("unexpected: {other:?}"),
        }

        tx.send(WhiteboardCommand::Hide("c2".to_string()))
            .expect("send hide");
        let hide = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        assert!(matches!(
            hide,
            HostControlMessage::WhiteboardHide { connection_id } if connection_id == "c2"
        ));
    }

    /// Sending after Quit returns Err (the receiver-side thread has exited and
    /// dropped the receiver). Kept as documentation of the bridge lifecycle.
    #[tokio::test]
    async fn whiteboard_bridge_quit_terminates() {
        let hub = Arc::new(HostControlHub::new_local());
        let _rx = hub.subscribe_outbound();
        let tx = bridge_whiteboard_to_hub(Arc::clone(&hub));
        tx.send(WhiteboardCommand::Quit).expect("send quit");

        // Give the thread time to consume Quit and drop the receiver.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(tx.send(WhiteboardCommand::Show("c".to_string())).is_err());
    }
}
