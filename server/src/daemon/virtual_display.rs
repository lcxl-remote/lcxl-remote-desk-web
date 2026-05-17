//! Daemon-side owner of the Windows virtual display.
//!
//! The supervisor holds the `SwDevice` handle returned by
//! [`desk_virtual_display::VirtualDisplayLifecycle::create`]. In phase 1
//! commit 4 (this file's introduction) only the lifecycle skeleton +
//! `is_active()` predicate exist so the signaling router can reject
//! inbound `ChangeDisplaySettings` requests with the correct
//! `DeskErrorCode` when the daemon is not in service mode or the
//! supervisor has not yet attached. Commit 5 fills in `apply`,
//! `on_worker_capabilities`, and `shutdown` with the full state
//! machine that drives lifecycle + worker reattach.
//!
//! Service-daemon mode only. The Default / Signaling / DeskServer
//! startup paths never construct a supervisor, so `RouterContext::
//! virtual_display` is `None` everywhere outside service mode and
//! the router short-circuits with `FEATURE_UNAVAILABLE`.

use tokio::sync::RwLock;

/// Internal lifecycle state. Phase 1 commit 4 only models the
/// `Disabled` state — commit 5 extends this with `Attaching`,
/// `Attached`, and `Detaching` plus the transitions between them.
#[allow(dead_code)]
enum SupervisorState {
    /// The supervisor exists but has not been activated. Either the
    /// `DeskSettings::enable_virtual_display` toggle is off, or the
    /// `VirtualDisplayLifecycle::create` call returned an error.
    Disabled,
}

/// Holds the `SwDevice` handle for the daemon-side virtual display.
/// Service-daemon mode only — instantiated by the service-daemon
/// startup path and stored as `RouterContext::virtual_display =
/// Some(Arc::new(supervisor))`. All other startup modes leave the
/// router context's field as `None`.
pub struct VirtualDisplaySupervisor {
    state: RwLock<SupervisorState>,
}

impl VirtualDisplaySupervisor {
    /// Construct a supervisor in the `Disabled` state. Commit 5
    /// extends this constructor to take the `VirtualDisplayLifecycle`
    /// provider and `WorkerManager`; for now the stub has no
    /// dependencies because nothing yet calls `apply` /
    /// `on_worker_capabilities`.
    pub fn new() -> Self {
        Self {
            state: RwLock::new(SupervisorState::Disabled),
        }
    }

    /// Whether the supervisor currently holds a live virtual display
    /// handle. The router uses this to decide between
    /// `FEATURE_UNAVAILABLE` ("not enabled" vs "unavailable") error
    /// responses for inbound `ChangeDisplaySettings`. In phase 1
    /// commit 4 this is always `false` — the `Attached` state lands
    /// in commit 5.
    pub async fn is_active(&self) -> bool {
        // The match is intentionally exhaustive on `SupervisorState`
        // so adding `Attached` in commit 5 will require updating this
        // arm rather than silently flipping behaviour.
        match *self.state.read().await {
            SupervisorState::Disabled => false,
        }
    }
}

impl Default for VirtualDisplaySupervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn newly_constructed_supervisor_is_inactive() {
        let supervisor = VirtualDisplaySupervisor::new();
        assert!(!supervisor.is_active().await);
    }
}
