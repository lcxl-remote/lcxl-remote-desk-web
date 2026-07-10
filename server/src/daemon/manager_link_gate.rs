//! Shared, in-memory gate for whether the host's manager link should currently
//! be connected.
//!
//! The value carried here is the full "should the manager link be up right now"
//! predicate — configuration present (`manager_url` + `manager_api_token`
//! non-empty) **and** the host-local `manager_enabled` toggle not turned off (see
//! [`super::signaling_proxy::manager_link_should_connect`]). It is driven through
//! a [`watch`] channel — not a `Notify` — so a consumer can observe the current
//! value and await the "disabled" edge without missing it: `Notify::notify_waiters`
//! only wakes tasks already parked at notify time, so a disable that lands after a
//! link connects but before its read loop parks would be lost, leaving a stale
//! connection up. `watch::Receiver::wait_for` re-checks the current value, so the
//! edge is never missed.
//!
//! Two consumers share it:
//!   - the always-on manager upstream and the on-demand support upstream tear the
//!     current WebSocket down the moment this flips to `false`;
//!   - the fleet audit sink skips its best-effort manager report (and stays purely
//!     local) whenever this is `false`, so a host with the manager link disabled
//!     does not emit audit frames onto the outbound lane.
//!
//! Like [`super::manager_link_state::ManagerLinkState`] and
//! [`super::support_link_state::SupportLinkState`] this is genuinely node-local
//! runtime state (one desk-server process), so it lives in process memory by
//! design and is not the cross-instance state the manager's multi-instance rule
//! governs.

use tokio::sync::watch;

/// Shared gate reflecting whether the manager link should be connected.
#[derive(Debug)]
pub struct ManagerLinkGate {
    should_connect_tx: watch::Sender<bool>,
}

impl ManagerLinkGate {
    /// Create the gate with its initial "should connect" value, derived from the
    /// persisted settings at startup.
    pub fn new(should_connect: bool) -> Self {
        let (should_connect_tx, _) = watch::channel(should_connect);
        Self { should_connect_tx }
    }

    /// Update the gate. Sends only on an actual change so idle consumers are not
    /// woken spuriously.
    pub fn set(&self, should_connect: bool) {
        self.should_connect_tx.send_if_modified(|current| {
            if *current != should_connect {
                *current = should_connect;
                true
            } else {
                false
            }
        });
    }

    /// Whether the manager link should currently be connected.
    pub fn should_connect(&self) -> bool {
        *self.should_connect_tx.borrow()
    }

    /// A receiver for the manager upstream / support upstream to await the
    /// "disabled" edge on.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.should_connect_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reflects_initial_and_updated_value() {
        let gate = ManagerLinkGate::new(true);
        assert!(gate.should_connect());
        gate.set(false);
        assert!(!gate.should_connect());
        gate.set(true);
        assert!(gate.should_connect());
    }

    #[tokio::test]
    async fn wait_for_disabled_fires_even_when_disabled_before_park() {
        // The disable lands before the consumer parks on the edge; `wait_for`
        // must still return immediately (no missed edge), which is the whole
        // reason this uses `watch` rather than `Notify`.
        let gate = Arc::new(ManagerLinkGate::new(true));
        let mut rx = gate.subscribe();
        gate.set(false);
        // Parks only now, after the edge already happened.
        let disabled = rx.wait_for(|c| !*c).await;
        assert!(disabled.is_ok());
        assert!(!*disabled.unwrap());
    }

    #[tokio::test]
    async fn wait_for_disabled_wakes_a_parked_consumer() {
        let gate = Arc::new(ManagerLinkGate::new(true));
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let mut rx = gate.subscribe();
                let _ = rx.wait_for(|c| !*c).await;
            })
        };
        tokio::task::yield_now().await;
        gate.set(false);
        waiter.await.unwrap();
    }
}
