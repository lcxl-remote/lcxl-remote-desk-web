//! Forwarder upstream — connects the worker hub to the daemon's `/ws/host_upstream`
//! endpoint. Outbound messages from the hub are queued here; a background ws task
//! drains the queue and pushes them to the daemon. Inbound messages from the daemon
//! are published on a broadcast channel that the hub subscribes to.
//!
//! In unit tests the ws task is replaced by direct injection so the forwarder
//! can be exercised without a running daemon.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::{broadcast, mpsc, watch};

use super::protocol::HostControlMessage;

const INBOUND_BROADCAST_CAPACITY: usize = 128;
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

/// Bridge between the Forwarder hub (worker process) and the daemon's
/// `/ws/host_upstream` endpoint.
pub struct UpstreamForwarder {
    outbound_tx: mpsc::UnboundedSender<HostControlMessage>,
    /// Held until production code consumes it via `take_outbound_rx()` (or until
    /// tests assert on it via `test_outbound_rx()`).
    outbound_rx: Mutex<Option<mpsc::UnboundedReceiver<HostControlMessage>>>,
    inbound_tx: broadcast::Sender<HostControlMessage>,
    connected: AtomicBool,
    /// Watch channel publishing connection-state transitions. Forwarder hubs
    /// subscribe so they can deny in-flight approvals the moment upstream drops.
    connection_state_tx: watch::Sender<bool>,
}

impl UpstreamForwarder {
    /// Create a new forwarder in the disconnected state. Production callers should
    /// then call `take_outbound_rx()` and spawn a ws-client task that drains it
    /// and pushes inbound messages via `publish_inbound`.
    pub fn new() -> Arc<Self> {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (inbound_tx, _) = broadcast::channel(INBOUND_BROADCAST_CAPACITY);
        let (connection_state_tx, _) = watch::channel(false);
        Arc::new(Self {
            outbound_tx,
            outbound_rx: Mutex::new(Some(outbound_rx)),
            inbound_tx,
            connected: AtomicBool::new(false),
            connection_state_tx,
        })
    }

    /// True if the upstream ws is currently connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Mark the upstream as connected (called by the ws-client task on handshake).
    pub fn mark_connected(&self) {
        let was = self.connected.swap(true, Ordering::AcqRel);
        if !was {
            // `send_replace` is used because `send` drops the value when no
            // receivers are alive yet (e.g. before a hub subscribes).
            self.connection_state_tx.send_replace(true);
        }
    }

    /// Mark the upstream as disconnected (called by the ws-client task on close).
    pub fn mark_disconnected(&self) {
        let was = self.connected.swap(false, Ordering::AcqRel);
        if was {
            self.connection_state_tx.send_replace(false);
        }
    }

    /// Subscribe to connection-state transitions. The first borrow on the
    /// returned receiver yields the current value; subsequent `changed().await`
    /// resolutions yield each transition in order.
    pub fn subscribe_connection_state(&self) -> watch::Receiver<bool> {
        self.connection_state_tx.subscribe()
    }

    /// Enqueue an outbound message to be sent to the daemon. Drops silently if
    /// the receiver has been taken and dropped (i.e. the ws task exited).
    pub fn send(&self, msg: HostControlMessage) {
        let _ = self.outbound_tx.send(msg);
    }

    /// Subscribe to inbound messages received from the daemon.
    pub fn subscribe_inbound(&self) -> broadcast::Receiver<HostControlMessage> {
        self.inbound_tx.subscribe()
    }

    /// Take ownership of the outbound receiver. Returns `None` after the first
    /// call. Production callers consume this and feed the ws sink.
    pub fn take_outbound_rx(&self) -> Option<mpsc::UnboundedReceiver<HostControlMessage>> {
        self.outbound_rx.lock().unwrap().take()
    }

    /// Publish an inbound message to all hub subscribers. Called by the ws-client
    /// task whenever the daemon sends a frame.
    pub fn publish_inbound(&self, msg: HostControlMessage) {
        let _ = self.inbound_tx.send(msg);
    }

    // ===== Test helpers =====

    /// Construct a forwarder pre-configured for unit testing.
    /// `connected_initially` controls the initial value of `is_connected()`.
    pub fn new_for_test(connected_initially: bool) -> Arc<Self> {
        let f = Self::new();
        if connected_initially {
            f.mark_connected();
        }
        f
    }

    /// Test-only: take the outbound receiver so a test can assert on traffic.
    pub fn test_outbound_rx(&self) -> mpsc::UnboundedReceiver<HostControlMessage> {
        self.take_outbound_rx()
            .expect("test_outbound_rx already taken")
    }

    /// Test-only: simulate the daemon pushing an inbound message.
    pub fn test_inject_inbound(&self, msg: HostControlMessage) {
        self.publish_inbound(msg);
    }
}

/// Compute the next reconnect delay using exponential backoff capped at
/// `RECONNECT_MAX_DELAY`. Returns the new delay and whether the cap was reached.
pub(crate) fn next_backoff(current: Duration) -> Duration {
    let next = current.saturating_mul(2);
    if next >= RECONNECT_MAX_DELAY {
        RECONNECT_MAX_DELAY
    } else {
        next
    }
}

/// Spawn a background ws-client task that connects to the daemon's
/// `/ws/host_upstream` endpoint and bridges traffic to/from the supplied
/// forwarder. Implementation detail of production setup; tests never run this.
///
/// The task reconnects indefinitely with exponential backoff
/// (`RECONNECT_INITIAL_DELAY` → `RECONNECT_MAX_DELAY`).
pub fn spawn_upstream_ws_task(
    forwarder: Arc<UpstreamForwarder>,
    daemon_ws_url: String,
    ipc_token: String,
) {
    actix_web::rt::spawn(async move {
        let url_with_token = format!("{daemon_ws_url}?token={ipc_token}");
        let mut backoff = RECONNECT_INITIAL_DELAY;

        // Take the outbound receiver once at startup. If it's already gone we're
        // misconfigured — log and bail.
        let mut outbound_rx = match forwarder.take_outbound_rx() {
            Some(rx) => rx,
            None => {
                warn!("[Upstream] outbound_rx already taken; aborting ws task");
                return;
            }
        };

        loop {
            info!("[Upstream] Connecting to {daemon_ws_url}");
            match awc::Client::default().ws(&url_with_token).connect().await {
                Ok((_resp, framed)) => {
                    info!("[Upstream] Connected");
                    forwarder.mark_connected();
                    backoff = RECONNECT_INITIAL_DELAY;

                    use futures_util::{SinkExt, StreamExt};
                    let (mut sink, mut stream) = framed.split();

                    // Announce role so the aggregator routes correctly.
                    let ready = HostControlMessage::Ready {
                        role: super::protocol::ClientRole::Forwarder,
                        is_admin: None,
                    };
                    if let Ok(json) = serde_json::to_string(&ready)
                        && sink
                            .send(awc::ws::Message::Text(json.into()))
                            .await
                            .is_err()
                    {
                        warn!("[Upstream] failed to send Ready");
                        forwarder.mark_disconnected();
                        tokio::time::sleep(backoff).await;
                        backoff = next_backoff(backoff);
                        continue;
                    }

                    'session: loop {
                        tokio::select! {
                            outbound = outbound_rx.recv() => {
                                let Some(msg) = outbound else { break 'session };
                                let json = match serde_json::to_string(&msg) {
                                    Ok(j) => j,
                                    Err(e) => {
                                        warn!("[Upstream] serialize error: {e}");
                                        continue;
                                    }
                                };
                                if sink.send(awc::ws::Message::Text(json.into())).await.is_err() {
                                    debug!("[Upstream] sink closed");
                                    break 'session;
                                }
                            }
                            ws = stream.next() => {
                                match ws {
                                    Some(Ok(awc::ws::Frame::Text(bytes))) => {
                                        let text = String::from_utf8_lossy(&bytes);
                                        match serde_json::from_str::<HostControlMessage>(&text) {
                                            Ok(msg) => forwarder.publish_inbound(msg),
                                            Err(e) => warn!("[Upstream] parse: {e} ({text})"),
                                        }
                                    }
                                    Some(Ok(awc::ws::Frame::Ping(data))) => {
                                        let _ = sink.send(awc::ws::Message::Pong(data)).await;
                                    }
                                    Some(Ok(awc::ws::Frame::Close(_))) | None => break 'session,
                                    Some(Err(e)) => {
                                        warn!("[Upstream] ws err: {e}");
                                        break 'session;
                                    }
                                    Some(Ok(_)) => {}
                                }
                            }
                        }
                    }

                    info!("[Upstream] disconnected");
                    forwarder.mark_disconnected();
                }
                Err(e) => {
                    warn!("[Upstream] connect failed: {e:?} (next retry in {backoff:?})");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // U-17: exponential backoff sequence reaches the cap and stays there.
    #[test]
    fn u17_backoff_progression() {
        let mut d = RECONNECT_INITIAL_DELAY;
        let mut seen = vec![d];
        for _ in 0..10 {
            d = next_backoff(d);
            seen.push(d);
        }
        // Initial 1s; doubles to 2, 4, 8, 16; then capped at 30 forever.
        assert_eq!(seen[0], Duration::from_secs(1));
        assert_eq!(seen[1], Duration::from_secs(2));
        assert_eq!(seen[2], Duration::from_secs(4));
        assert_eq!(seen[3], Duration::from_secs(8));
        assert_eq!(seen[4], Duration::from_secs(16));
        // From here on, capped.
        for v in &seen[5..] {
            assert_eq!(*v, RECONNECT_MAX_DELAY);
        }
    }

    #[tokio::test]
    async fn outbound_rx_can_be_taken_once() {
        let f = UpstreamForwarder::new();
        assert!(f.take_outbound_rx().is_some());
        assert!(f.take_outbound_rx().is_none());
    }

    #[tokio::test]
    async fn send_and_take_outbound_round_trip() {
        let f = UpstreamForwarder::new();
        let mut rx = f.take_outbound_rx().unwrap();
        f.send(HostControlMessage::PrivateScreenHide {
            connection_id: "c1".into(),
        });
        let got = tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            got,
            HostControlMessage::PrivateScreenHide {
                connection_id: "c1".into()
            }
        );
    }

    #[tokio::test]
    async fn inbound_broadcast_to_subscribers() {
        let f = UpstreamForwarder::new();
        let mut rx_a = f.subscribe_inbound();
        let mut rx_b = f.subscribe_inbound();
        f.publish_inbound(HostControlMessage::SecurityApprovalCancel {
            req_id: "r1".into(),
        });
        for rx in [&mut rx_a, &mut rx_b] {
            let got = tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                got,
                HostControlMessage::SecurityApprovalCancel { ref req_id } if req_id == "r1"
            ));
        }
    }

    #[test]
    fn connected_flag_round_trip() {
        let f = UpstreamForwarder::new();
        assert!(!f.is_connected());
        f.mark_connected();
        assert!(f.is_connected());
        f.mark_disconnected();
        assert!(!f.is_connected());
    }

    // Connection-state watch publishes only on real transitions: repeated
    // mark_connected calls collapse, and the same is true for mark_disconnected.
    #[tokio::test]
    async fn connection_state_watch_emits_transitions_only() {
        let f = UpstreamForwarder::new();
        let mut rx = f.subscribe_connection_state();
        // Initial value is false; .changed() must not fire spuriously.
        assert!(!*rx.borrow());

        f.mark_connected();
        rx.changed().await.expect("connect transition");
        assert!(*rx.borrow());

        // Idempotent: a second mark_connected must not re-fire.
        f.mark_connected();
        let none = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        assert!(none.is_err(), "no spurious change for repeat connect");

        f.mark_disconnected();
        rx.changed().await.expect("disconnect transition");
        assert!(!*rx.borrow());

        f.mark_disconnected();
        let none = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        assert!(none.is_err(), "no spurious change for repeat disconnect");
    }
}
