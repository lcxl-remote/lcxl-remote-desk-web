//! Daemon-side helper struct shared between the local HTTP API and the host
//! control hub endpoint.
//!
//! The legacy `TauriIpcBridge` once owned a private WebSocket endpoint
//! (`/ws/tauri_ipc`) and a fleet of background `std::thread::spawn` forwarders
//! that proxied mpsc commands across the link. After the host-control-hub
//! unification (Steps 4 and 6) all of that is gone — `/ws/tauri_ipc` and
//! `/ws/host_upstream` are now served by [`crate::host_control::endpoint`] and
//! every command travels through [`crate::host_control::HostControlHub`].
//!
//! What remains is two small pieces of shared state:
//! - `tauri_is_admin`: the elevation flag reported by the Tauri shell on
//!   `Ready`. Read by `/api/sysinfo` and `/api/server_info` to override the
//!   ServiceDaemon's SYSTEM-account `is_admin()` result.
//! - `tauri_login_token`: the auto-login token written by the host control
//!   endpoint on every Tauri ws connect, consumed by the HTTP `/login_tauri`
//!   route.

use std::sync::{Arc, Mutex};

use crate::{TauriIsAdminOverride, TauriLoginToken};

/// Daemon-side host-state holder.
///
/// Construct once per daemon process. The values are intentionally cheaply
/// cloneable: `Arc<Mutex<...>>` for `tauri_is_admin` and the `Clone`able
/// `TauriLoginToken` (with shared inner `Arc<Mutex<>>`).
pub struct TauriIpcBridge {
    pub tauri_is_admin: TauriIsAdminOverride,
    pub tauri_login_token: TauriLoginToken,
}

impl TauriIpcBridge {
    /// Create a new bridge with a fresh `tauri_is_admin` slot (initially `None`)
    /// and an empty `TauriLoginToken` (the hub endpoint refreshes it on every
    /// Tauri ws connect).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tauri_is_admin: Arc::new(Mutex::new(None)),
            tauri_login_token: TauriLoginToken::empty(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructor produces a fresh bridge with `tauri_is_admin = None` and an
    /// empty token (verifies always returns false until refreshed).
    #[test]
    fn new_bridge_starts_unset() {
        let bridge = TauriIpcBridge::new();
        assert!(bridge.tauri_is_admin.lock().unwrap().is_none());
        assert!(!bridge.tauri_login_token.verify_and_consume("anything"));
    }

    /// `tauri_is_admin` is shared via Arc — mutating from one clone is visible
    /// through any other clone (HTTP endpoints + ws handler must observe the
    /// same value).
    #[test]
    fn tauri_is_admin_shared_across_clones() {
        let bridge = TauriIpcBridge::new();
        let other = Arc::clone(&bridge.tauri_is_admin);
        *other.lock().unwrap() = Some(true);
        assert_eq!(*bridge.tauri_is_admin.lock().unwrap(), Some(true));
    }

    /// The `TauriLoginToken` shares its inner state across clones so the HTTP
    /// route's view stays in sync with refreshes performed inside the host
    /// control endpoint.
    #[test]
    fn tauri_login_token_shared_across_clones() {
        let bridge = TauriIpcBridge::new();
        let cloned = bridge.tauri_login_token.clone();
        cloned.refresh("tok-1".to_string());
        assert!(bridge.tauri_login_token.verify_and_consume("tok-1"));
    }
}
