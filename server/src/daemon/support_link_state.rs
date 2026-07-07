//! Shared, in-memory lifecycle state for the host's on-demand temporary-support
//! upstream.
//!
//! A local user clicks "get a support code"; the host REST layer flips this state
//! to active, which wakes the signaling proxy's support loop. That loop opens a
//! dedicated `Support` upstream to the manager (see [`RemoteDeskTypeEnum::Support`]),
//! which mints a temporary code and pushes it back as `SupportCodeIssued`. The
//! host records the code snapshot here for its local UI and arms a teardown at the
//! code's expiry. Stopping — a manual "end support", the code's TTL, or the
//! upstream closing — flips the state back to inactive, and the proxy tears the
//! session down.
//!
//! Like [`super::manager_link_state::ManagerLinkState`] this is genuinely
//! node-local runtime state (one desk-server process, at most one live support
//! session), so it lives in process memory by design — it is not the kind of
//! cross-instance state the manager's multi-instance rule governs.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::{RwLock, watch};

/// The temporary code the manager issued for the current support session, held
/// for the host's local UI (shown to the on-site user to read out to a supporter).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SupportCodeSnapshot {
    /// Human-readable support code (uppercase, unambiguous alphabet).
    pub code: String,
    /// Unix seconds at which the code / session expires.
    pub expires_at: i64,
}

/// Lifecycle state of the host's on-demand support upstream plus the currently
/// issued code snapshot. `active` is driven through a `watch` channel so the
/// proxy loop can await either transition without missing edges or leaking
/// notification permits.
#[derive(Debug)]
pub struct SupportLinkState {
    active_tx: watch::Sender<bool>,
    /// Bumped on every start so a stale expiry timer armed for an earlier session
    /// cannot tear down a newer one.
    epoch: AtomicU64,
    snapshot: RwLock<Option<SupportCodeSnapshot>>,
}

impl Default for SupportLinkState {
    fn default() -> Self {
        Self::new()
    }
}

impl SupportLinkState {
    pub fn new() -> Self {
        let (active_tx, _) = watch::channel(false);
        Self {
            active_tx,
            epoch: AtomicU64::new(0),
            snapshot: RwLock::new(None),
        }
    }

    /// Request a new support session. Returns `true` if this flipped the state
    /// from inactive to active (waking the proxy loop) and `false` if a session
    /// was already active (idempotent — the caller should surface the existing
    /// session rather than opening a second one).
    pub fn request_start(&self) -> bool {
        let mut started = false;
        self.active_tx.send_if_modified(|active| {
            if *active {
                false
            } else {
                *active = true;
                started = true;
                true
            }
        });
        if started {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
        started
    }

    /// Request the current support session to stop (manual "end support" or TTL
    /// expiry). A no-op if no session is active.
    pub fn request_stop(&self) {
        self.active_tx
            .send_if_modified(|active| if *active { *active = false; true } else { false });
    }

    /// Whether a support session is currently active.
    pub fn is_active(&self) -> bool {
        *self.active_tx.borrow()
    }

    /// The generation of the current/most-recent session. An expiry timer captures
    /// this at arm time and only tears down if it is unchanged when it fires.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Record the manager-issued code for the local UI.
    pub async fn set_snapshot(&self, code: String, expires_at: i64) {
        *self.snapshot.write().await = Some(SupportCodeSnapshot { code, expires_at });
    }

    /// The current code snapshot, if a session is live and the code has arrived.
    pub async fn snapshot(&self) -> Option<SupportCodeSnapshot> {
        self.snapshot.read().await.clone()
    }

    /// Park until a support session is requested (active becomes `true`). Returns
    /// immediately if already active.
    pub async fn wait_for_start(&self) {
        let mut rx = self.active_tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        // The sender lives for the state's lifetime (held in an `Arc`), so this
        // never errors with `RecvError`.
        let _ = rx.wait_for(|active| *active).await;
    }

    /// Park until the current support session is stopped (active becomes `false`).
    /// Returns immediately if already inactive.
    pub async fn wait_for_stop(&self) {
        let mut rx = self.active_tx.subscribe();
        if !*rx.borrow_and_update() {
            return;
        }
        let _ = rx.wait_for(|active| !*active).await;
    }

    /// Reset after a session ends (upstream closed, stop, or TTL): mark inactive
    /// and drop the code snapshot so the local UI reflects "no active session".
    pub async fn finish(&self) {
        self.request_stop();
        *self.snapshot.write().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn start_is_idempotent_and_bumps_epoch() {
        let st = SupportLinkState::new();
        assert!(!st.is_active());
        let e0 = st.epoch();
        assert!(st.request_start());
        assert!(st.is_active());
        assert_eq!(st.epoch(), e0 + 1);
        // Second start while active is a no-op (no double session, epoch steady).
        assert!(!st.request_start());
        assert_eq!(st.epoch(), e0 + 1);
    }

    #[tokio::test]
    async fn start_wakes_parked_loop_then_stop_wakes_it() {
        let st = Arc::new(SupportLinkState::new());
        let waiter = {
            let st = st.clone();
            tokio::spawn(async move { st.wait_for_start().await })
        };
        tokio::task::yield_now().await;
        assert!(st.request_start());
        waiter.await.unwrap();

        // Now a serving loop parked on stop is woken by request_stop.
        let stopper = {
            let st = st.clone();
            tokio::spawn(async move { st.wait_for_stop().await })
        };
        tokio::task::yield_now().await;
        st.request_stop();
        stopper.await.unwrap();
        assert!(!st.is_active());
    }

    #[tokio::test]
    async fn finish_clears_snapshot_and_deactivates() {
        let st = SupportLinkState::new();
        st.request_start();
        st.set_snapshot("ABCDEFGHJK".into(), 1_900_000_000).await;
        assert!(st.snapshot().await.is_some());
        st.finish().await;
        assert!(!st.is_active());
        assert!(st.snapshot().await.is_none());
    }
}
