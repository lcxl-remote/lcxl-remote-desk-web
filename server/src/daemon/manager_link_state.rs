//! Shared, in-memory state for the host's link to the central manager.
//!
//! When the manager fatally rejects this host's registration handshake (device
//! quota reached, or a missing device identity), the proxy must stop the 5-second
//! auto-reconnect storm: retrying changes nothing until the user frees a device
//! slot from a control end. This state records the last fatal rejection (so the
//! host UI can surface "device limit reached; remove an unused device") and exposes
//! a manual-retry trigger the user invokes once they have cleaned up — at which
//! point the proxy reconnects immediately, with no long backoff.
//!
//! This is genuinely node-local runtime state (one desk-server process, one outward
//! manager link), so it lives in process memory by design — it is not the kind of
//! cross-instance state the manager's multi-instance rule governs.

use serde::Serialize;
use tokio::sync::{Notify, RwLock};

/// A fatal manager-link registration rejection the user must act on.
#[derive(Debug, Clone, Serialize)]
pub struct FatalRejection {
    /// `DeskErrorCode` numeric value (`46` = device quota exceeded, `47` = missing
    /// client id).
    pub error_code: i32,
    /// Human-readable reason reported by the manager.
    pub message: String,
}

/// Runtime status of the host→manager link plus the manual-retry signal.
#[derive(Debug)]
pub struct ManagerLinkState {
    fatal: RwLock<Option<FatalRejection>>,
    retry: Notify,
}

impl Default for ManagerLinkState {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagerLinkState {
    pub fn new() -> Self {
        Self {
            fatal: RwLock::new(None),
            retry: Notify::new(),
        }
    }

    /// Record a fatal rejection; the proxy has stopped auto-reconnecting.
    pub async fn record_fatal(&self, error_code: i32, message: String) {
        *self.fatal.write().await = Some(FatalRejection {
            error_code,
            message,
        });
    }

    /// Clear any recorded rejection (on a successful reconnect / manual retry).
    pub async fn clear(&self) {
        *self.fatal.write().await = None;
    }

    /// The current fatal rejection, if the link is blocked.
    pub async fn snapshot(&self) -> Option<FatalRejection> {
        self.fatal.read().await.clone()
    }

    /// Trigger a manual reconnect (user action after freeing a device slot). Wakes a
    /// proxy loop parked in [`await_retry`]. A trigger sent while no loop is parked
    /// is coalesced by `Notify` and delivered to the next `await_retry`.
    pub fn request_retry(&self) {
        self.retry.notify_one();
    }

    /// Park until a manual retry is requested.
    pub async fn await_retry(&self) {
        self.retry.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn records_and_clears_fatal() {
        let st = ManagerLinkState::new();
        assert!(st.snapshot().await.is_none());
        st.record_fatal(46, "full".into()).await;
        let snap = st.snapshot().await.unwrap();
        assert_eq!(snap.error_code, 46);
        assert_eq!(snap.message, "full");
        st.clear().await;
        assert!(st.snapshot().await.is_none());
    }

    #[tokio::test]
    async fn manual_retry_wakes_parked_loop() {
        let st = Arc::new(ManagerLinkState::new());
        let waiter = {
            let st = st.clone();
            tokio::spawn(async move { st.await_retry().await })
        };
        // Give the waiter a moment to park, then signal.
        tokio::task::yield_now().await;
        st.request_retry();
        // The waiter must complete (no hang).
        waiter.await.unwrap();
    }
}
