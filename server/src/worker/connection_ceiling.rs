//! Worker-side per-connection capability ceiling map.
//!
//! The daemon registers a connection's validated ceiling via
//! [`desk_ipc_protocol::message::ServiceToWorker::SetConnectionCeiling`] the
//! moment it admits a redeemed-grant session, ahead of any worker-bound frame
//! for that connection (the never-drop event pipe keeps the registration
//! FIFO-ordered before the connection's first file-list / terminal / media
//! request). The worker-side `meet(ceiling, global)` permission gates read this
//! map before allowing a capability.
//!
//! A missing entry means "no grant cap" — an owner / unrestricted connection is
//! never registered and falls back to global-only gating. The entry is cleared
//! when the connection tears down (`StopMedia`).

use std::collections::HashMap;
use std::sync::Arc;

use desk_signal_facade::model::security_settings::SecuritySettings;
use tokio::sync::RwLock;

/// Shared, cheap-to-clone view of the per-connection ceiling map. The session
/// loop mutates it on `SetConnectionCeiling` / `StopMedia`; each worker-side
/// permission gate holds a clone and reads the same view.
#[derive(Clone, Default)]
pub struct ConnectionCeilingStore {
    inner: Arc<RwLock<HashMap<String, SecuritySettings>>>,
}

impl ConnectionCeilingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or overwrite) `connection_id`'s validated ceiling. A `None`
    /// ceiling clears any entry — an owner / unrestricted registration carries no
    /// cap, so the connection reverts to global-only gating.
    pub async fn set(&self, connection_id: &str, ceiling: Option<SecuritySettings>) {
        let mut map = self.inner.write().await;
        match ceiling {
            Some(c) => {
                map.insert(connection_id.to_string(), c);
            }
            None => {
                map.remove(connection_id);
            }
        }
    }

    /// Drop `connection_id`'s ceiling on teardown. Idempotent — a no-op for a
    /// connection that was never registered (owner / unrestricted).
    pub async fn clear(&self, connection_id: &str) {
        self.inner.write().await.remove(connection_id);
    }

    pub async fn clear_all(&self) {
        self.inner.write().await.clear();
    }

    /// The validated ceiling for `connection_id`, if it was admitted under a
    /// grant. `None` means owner / unrestricted (global-only gating).
    pub async fn get(&self, connection_id: &str) -> Option<SecuritySettings> {
        self.inner.read().await.get(connection_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ceiling_with_file_transfer(v: bool) -> SecuritySettings {
        SecuritySettings {
            allow_file_transfer: Some(v),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn set_then_get_returns_registered_ceiling() {
        let store = ConnectionCeilingStore::new();
        assert!(store.get("conn-a").await.is_none());

        let ceiling = ceiling_with_file_transfer(false);
        store.set("conn-a", Some(ceiling.clone())).await;
        assert_eq!(store.get("conn-a").await, Some(ceiling));
        // Unrelated connections stay unregistered (global-only gating).
        assert!(store.get("conn-b").await.is_none());
    }

    #[tokio::test]
    async fn set_none_clears_any_entry() {
        let store = ConnectionCeilingStore::new();
        store
            .set("conn-a", Some(ceiling_with_file_transfer(false)))
            .await;
        assert!(store.get("conn-a").await.is_some());

        // An owner/unrestricted registration (ceiling = None) removes the cap.
        store.set("conn-a", None).await;
        assert!(store.get("conn-a").await.is_none());
    }

    #[tokio::test]
    async fn clear_removes_entry_and_is_idempotent() {
        let store = ConnectionCeilingStore::new();
        store
            .set("conn-a", Some(ceiling_with_file_transfer(true)))
            .await;
        store.clear("conn-a").await;
        assert!(store.get("conn-a").await.is_none());
        // Clearing an unknown connection is a silent no-op.
        store.clear("conn-a").await;
        store.clear("conn-unknown").await;
    }
}
