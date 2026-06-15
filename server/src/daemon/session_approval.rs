//! Session-scoped approval store for the `SessionApproved` execution mode.
//!
//! In [`ExecutionMode::SessionApproved`], the first explicit confirmation of a
//! command template grants that template for the rest of the control-end
//! session: subsequent `ConfirmExec` frames whose command matches the same
//! template — and that still classify as executable — run without re-prompting.
//!
//! Grants are:
//! - **bound to a single control-end connection** (keyed by `connection_id`), so
//!   one session's approval never leaks to another connection;
//! - **intersected with the command whitelist**: only a command that matched a
//!   template (i.e. is classified executable) is ever granted, so an approval can
//!   never widen what the classifier already permits;
//! - **in-memory and short-lived**: releasing control (`CloseControl`), the
//!   connection ending (`ConnectionRemoved`), or a daemon restart revokes them.
//!
//! [`ExecutionMode::SessionApproved`]: desk_agent_protocol::ExecutionMode::SessionApproved

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// In-memory map of `connection_id` → set of session-approved `template_id`s.
/// Locks are brief and never held across an `.await`.
#[derive(Default)]
pub struct SessionApprovalStore {
    inner: Mutex<HashMap<String, HashSet<String>>>,
}

impl SessionApprovalStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant `template_id` for `connection_id` for the rest of the session.
    /// Idempotent: re-granting an already-granted template is a no-op.
    pub fn grant(&self, connection_id: &str, template_id: &str) {
        let mut map = self.inner.lock().expect("session approvals lock");
        map.entry(connection_id.to_string())
            .or_default()
            .insert(template_id.to_string());
    }

    /// Whether `template_id` is already session-approved for `connection_id`.
    pub fn is_granted(&self, connection_id: &str, template_id: &str) -> bool {
        let map = self.inner.lock().expect("session approvals lock");
        map.get(connection_id)
            .is_some_and(|set| set.contains(template_id))
    }

    /// Revoke every grant for a connection (release control / connection end).
    /// Returns the number of templates that were revoked.
    pub fn revoke_connection(&self, connection_id: &str) -> usize {
        let mut map = self.inner.lock().expect("session approvals lock");
        map.remove(connection_id).map(|set| set.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn granted_count(&self, connection_id: &str) -> usize {
        self.inner
            .lock()
            .expect("session approvals lock")
            .get(connection_id)
            .map(|set| set.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_then_is_granted_for_same_connection_and_template() {
        let store = SessionApprovalStore::new();
        assert!(!store.is_granted("conn1", "docker_restart"));
        store.grant("conn1", "docker_restart");
        assert!(store.is_granted("conn1", "docker_restart"));
        // A different template under the same connection is not granted.
        assert!(!store.is_granted("conn1", "systemctl_restart"));
    }

    #[test]
    fn grant_does_not_leak_across_connections() {
        let store = SessionApprovalStore::new();
        store.grant("conn1", "docker_restart");
        assert!(!store.is_granted("conn2", "docker_restart"));
    }

    #[test]
    fn grant_is_idempotent() {
        let store = SessionApprovalStore::new();
        store.grant("conn1", "docker_restart");
        store.grant("conn1", "docker_restart");
        assert_eq!(store.granted_count("conn1"), 1);
    }

    #[test]
    fn revoke_connection_clears_all_its_grants_only() {
        let store = SessionApprovalStore::new();
        store.grant("conn1", "docker_restart");
        store.grant("conn1", "systemctl_restart");
        store.grant("conn2", "docker_restart");

        assert_eq!(store.revoke_connection("conn1"), 2);
        assert!(!store.is_granted("conn1", "docker_restart"));
        assert!(!store.is_granted("conn1", "systemctl_restart"));
        // The other connection is untouched.
        assert!(store.is_granted("conn2", "docker_restart"));
    }

    #[test]
    fn revoke_unknown_connection_is_zero() {
        let store = SessionApprovalStore::new();
        assert_eq!(store.revoke_connection("nope"), 0);
    }
}
