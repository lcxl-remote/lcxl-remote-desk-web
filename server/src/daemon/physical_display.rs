//! Host-local physical display mode correlation and best-effort restore state.

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use desk_ipc_protocol::message::{
    PhysicalDisplayAction, PhysicalDisplayModeOutcome, PhysicalDisplayModeResponsePayload,
    ServiceToWorker, SessionKey, SetPhysicalDisplayModePayload,
};
use desk_signal_facade::model::virtual_display::PhysicalDisplayCapability;
use tokio::sync::{Mutex, MutexGuard, Notify};

const AUTO_COOLDOWN: Duration = Duration::from_secs(30);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginPhysicalModeError {
    Busy,
    TimedOut,
    StaleViewport,
    CoolingDown,
}

impl BeginPhysicalModeError {
    pub fn retryable(self) -> bool {
        matches!(self, Self::Busy | Self::CoolingDown)
    }
}

impl std::fmt::Display for BeginPhysicalModeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "another display topology change is in progress",
            Self::TimedOut => "previous display topology change has not completed",
            Self::StaleViewport => "stale physical display viewport request",
            Self::CoolingDown => "physical display auto change is cooling down",
        })
    }
}

#[derive(Clone, Debug)]
pub struct AppliedPhysicalMode {
    pub original_selector: String,
    pub applied_selector: String,
    pub display_identity: String,
    pub connection_epoch: String,
    pub session_key: Option<SessionKey>,
}

#[derive(Debug)]
struct Pending {
    started: Instant,
    device_name: String,
    request_id: String,
    connection_epoch: String,
    session_key: Option<SessionKey>,
    is_restore: bool,
    auto: bool,
}

#[derive(Default)]
struct State {
    next_operation_id: u64,
    pending: Option<(u64, Pending)>,
    applied: HashMap<(Option<SessionKey>, String), AppliedPhysicalMode>,
    last_auto_change: HashMap<(Option<SessionKey>, String), Instant>,
    last_viewport_sequence: HashMap<(String, String), u64>,
}

/// One instance is shared by all signaling connections on this host.
/// The single pending slot serializes mode operations across the topology.
#[derive(Default)]
pub struct PhysicalDisplaySupervisor {
    state: Mutex<State>,
    changed: Notify,
    restore_lock: Mutex<()>,
}

impl PhysicalDisplaySupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn lock_restores(&self) -> MutexGuard<'_, ()> {
        self.restore_lock.lock().await
    }

    pub async fn begin(
        &self,
        device_name: &str,
        request_id: &str,
        connection_epoch: &str,
        session_key: Option<SessionKey>,
        action: &PhysicalDisplayAction,
    ) -> Result<u64, BeginPhysicalModeError> {
        self.begin_checked(
            device_name,
            request_id,
            connection_epoch,
            session_key,
            action,
            None,
        )
        .await
    }

    pub async fn begin_checked(
        &self,
        device_name: &str,
        request_id: &str,
        connection_epoch: &str,
        session_key: Option<SessionKey>,
        action: &PhysicalDisplayAction,
        virtual_display: Option<&super::virtual_display::VirtualDisplaySupervisor>,
    ) -> Result<u64, BeginPhysicalModeError> {
        let mut state = self.state.lock().await;
        if virtual_display.is_some_and(|supervisor| !supervisor.physical_modes_available()) {
            return Err(BeginPhysicalModeError::Busy);
        }
        if let Some((_, pending)) = state.pending.as_ref() {
            // A slow worker may still apply its mode and reply after the
            // browser's timeout. Keep the operation fence until that reply
            // arrives or the worker incarnation is replaced; opening a new
            // write here would lose the original-mode correlation.
            return Err(if pending.started.elapsed() >= OPERATION_TIMEOUT {
                BeginPhysicalModeError::TimedOut
            } else {
                BeginPhysicalModeError::Busy
            });
        }
        if let PhysicalDisplayAction::Auto {
            viewport_sequence, ..
        } = action
        {
            let viewport_key = (connection_epoch.to_string(), device_name.to_string());
            if state
                .last_viewport_sequence
                .get(&viewport_key)
                .is_some_and(|last| *viewport_sequence <= *last)
            {
                return Err(BeginPhysicalModeError::StaleViewport);
            }
            state
                .last_viewport_sequence
                .insert(viewport_key, *viewport_sequence);
            if state
                .last_auto_change
                .get(&(session_key.clone(), device_name.to_string()))
                .is_some_and(|last| last.elapsed() < AUTO_COOLDOWN)
            {
                return Err(BeginPhysicalModeError::CoolingDown);
            }
        }
        state.next_operation_id = state.next_operation_id.wrapping_add(1).max(1);
        let op_id = state.next_operation_id;
        state.pending = Some((
            op_id,
            Pending {
                started: Instant::now(),
                device_name: device_name.into(),
                request_id: request_id.into(),
                connection_epoch: connection_epoch.into(),
                session_key,
                is_restore: matches!(action, PhysicalDisplayAction::Restore { .. }),
                auto: matches!(action, PhysicalDisplayAction::Auto { .. }),
            },
        ));
        Ok(op_id)
    }

    /// Check physical work and publish the exclusive intent under the same
    /// lock used by `begin_checked`. This closes the gap between restoring
    /// physical modes and the virtual supervisor's desired-state update.
    pub async fn try_enter_exclusive(
        &self,
        virtual_display: &super::virtual_display::VirtualDisplaySupervisor,
        prompt_ms: u32,
    ) -> bool {
        let state = self.state.lock().await;
        if state.pending.is_some() || !state.applied.is_empty() {
            return false;
        }
        virtual_display.set_desired_exclusive(true, prompt_ms);
        true
    }

    pub async fn restore_action(
        &self,
        device_name: &str,
        session_key: Option<&SessionKey>,
    ) -> Option<PhysicalDisplayAction> {
        let state = self.state.lock().await;
        let mode = state
            .applied
            .get(&(session_key.cloned(), device_name.to_string()))?;
        Some(PhysicalDisplayAction::Restore {
            original_selector: mode.original_selector.clone(),
            expected_applied_selector: mode.applied_selector.clone(),
            expected_display_identity: mode.display_identity.clone(),
        })
    }

    pub async fn recorded_displays(&self) -> Vec<(String, AppliedPhysicalMode)> {
        self.state
            .lock()
            .await
            .applied
            .iter()
            .map(|((_, name), mode)| (name.clone(), mode.clone()))
            .collect()
    }

    pub async fn has_recorded_for_session(&self, session_key: Option<&SessionKey>) -> bool {
        self.state
            .lock()
            .await
            .applied
            .values()
            .any(|mode| mode.session_key.as_ref() == session_key)
    }

    pub async fn abandon(&self, operation_id: u64) {
        let mut state = self.state.lock().await;
        if state
            .pending
            .as_ref()
            .is_some_and(|(id, _)| *id == operation_id)
        {
            state.pending = None;
            self.changed.notify_waiters();
        }
    }

    /// Discard an unacknowledged operation only after its old worker process
    /// has terminated. An in-process worker replacement is not sufficient:
    /// an aborted `spawn_blocking` operation may still write the OS mode.
    pub async fn abandon_pending_for_session(&self, session_key: Option<&SessionKey>) -> bool {
        let mut state = self.state.lock().await;
        if state
            .pending
            .as_ref()
            .is_some_and(|(_, pending)| pending.session_key.as_ref() == session_key)
        {
            state.pending = None;
            self.changed.notify_waiters();
            return true;
        }
        false
    }

    pub async fn has_pending_for_session(&self, session_key: Option<&SessionKey>) -> bool {
        self.state
            .lock()
            .await
            .pending
            .as_ref()
            .is_some_and(|(_, pending)| pending.session_key.as_ref() == session_key)
    }

    pub async fn has_pending_operation(&self) -> bool {
        self.state.lock().await.pending.is_some()
    }

    pub async fn forget(&self, device_name: &str, session_key: Option<&SessionKey>) {
        self.state
            .lock()
            .await
            .applied
            .remove(&(session_key.cloned(), device_name.to_string()));
    }

    pub async fn forget_for_session(&self, session_key: Option<&SessionKey>) {
        self.state
            .lock()
            .await
            .applied
            .retain(|_, mode| mode.session_key.as_ref() != session_key);
    }

    /// A local user change or hot unplug supersedes our restore snapshot.
    /// Capabilities can arrive while our own operation is in flight; defer
    /// comparison then because the worker and daemon event lanes are async.
    pub async fn reconcile_capabilities(
        &self,
        session_key: Option<&SessionKey>,
        capabilities: &BTreeMap<String, BTreeMap<String, PhysicalDisplayCapability>>,
    ) -> Vec<String> {
        let mut state = self.state.lock().await;
        if state.pending.is_some() {
            return Vec::new();
        }
        let changed = state
            .applied
            .iter()
            .filter(|((owner_session, device_name), applied)| {
                if owner_session.as_ref() != session_key {
                    return false;
                }
                let current = capabilities
                    .values()
                    .filter_map(|devices| devices.get(device_name))
                    .find_map(|capability| capability.current_selector.as_deref());
                let identity = capabilities
                    .values()
                    .filter_map(|devices| devices.get(device_name))
                    .find_map(|capability| capability.display_identity.as_deref());
                current.is_some_and(|selector| selector != applied.applied_selector)
                    || identity.is_some_and(|identity| identity != applied.display_identity)
                    || !capabilities
                        .values()
                        .any(|devices| devices.contains_key(device_name))
            })
            .map(|((_, device_name), _)| device_name.clone())
            .collect::<Vec<_>>();
        for device_name in &changed {
            state
                .applied
                .remove(&(session_key.cloned(), device_name.clone()));
        }
        changed
    }

    pub async fn wait_for_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().await.pending.is_none() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return false;
            }
        }
    }

    /// Ignore stale results and keep the first original mode across resizes.
    /// Return whether the completed request was a restore; the caller uses
    /// this to avoid automatically retrying a failed restore forever.
    pub async fn complete_and_identify(
        &self,
        response: &PhysicalDisplayModeResponsePayload,
    ) -> Option<bool> {
        let mut state = self.state.lock().await;
        let Some((id, pending)) = state.pending.take() else {
            return None;
        };
        if id != response.operation_id
            || pending.request_id != response.request_id
            || pending.connection_epoch != response.connection_epoch
        {
            state.pending = Some((id, pending));
            return None;
        }
        if let PhysicalDisplayModeOutcome::Applied(data)
        | PhysicalDisplayModeOutcome::AppliedWithoutVideo { data, .. } = &response.outcome
        {
            if data.device_name != pending.device_name {
                state.pending = Some((id, pending));
                return None;
            }
            let display_key = (pending.session_key.clone(), data.device_name.clone());
            if pending.is_restore {
                state.applied.remove(&display_key);
            } else if data.changed {
                let existing = state
                    .applied
                    .get(&display_key)
                    .filter(|entry| entry.display_identity == data.display_identity);
                let original = existing
                    .map(|entry| entry.original_selector.clone())
                    .unwrap_or_else(|| data.previous_selector.clone());
                let session_key = existing
                    .map(|entry| entry.session_key.clone())
                    .unwrap_or_else(|| pending.session_key.clone());
                state.applied.insert(
                    display_key,
                    AppliedPhysicalMode {
                        original_selector: original,
                        applied_selector: data.selector.clone(),
                        display_identity: data.display_identity.clone(),
                        connection_epoch: response.connection_epoch.clone(),
                        session_key,
                    },
                );
                if pending.auto {
                    state.last_auto_change.insert(
                        (pending.session_key.clone(), data.device_name.clone()),
                        Instant::now(),
                    );
                }
            }
        }
        let was_restore = pending.is_restore;
        self.changed.notify_waiters();
        Some(was_restore)
    }

    #[cfg(test)]
    pub async fn complete(&self, response: &PhysicalDisplayModeResponsePayload) -> bool {
        self.complete_and_identify(response).await.is_some()
    }
}

/// Exclusive IDD mode detaches physical outputs. Restore every remembered
/// physical mode first; if any restore cannot be verified, skip entering
/// exclusive mode rather than hiding a display with a stranded snapshot.
pub async fn restore_before_exclusive(
    supervisor: &PhysicalDisplaySupervisor,
    registry: &super::pc_manager::PcRegistry,
    worker_mgr: &super::worker_manager::WorkerManager,
    session_filter: Option<&SessionKey>,
) -> bool {
    // Attach, control grant, late mode completion and worker recovery can
    // all request restoration. Only one may enumerate/send the sequence of
    // per-display restores at a time.
    let _restore_guard = supervisor.lock_restores().await;
    if !supervisor.wait_for_idle(OPERATION_TIMEOUT).await {
        return false;
    }
    let recorded = supervisor
        .recorded_displays()
        .await
        .into_iter()
        .filter(|(_, mode)| {
            session_filter.is_none_or(|session| mode.session_key.as_ref() == Some(session))
        })
        .collect::<Vec<_>>();
    if recorded.is_empty() {
        return true;
    }
    for (device_name, mode) in recorded {
        let mut route = None;
        for connection_id in registry.all_connection_ids().await {
            if worker_mgr.connection_target(&connection_id) != mode.session_key {
                continue;
            }
            if let Some(pc) = registry.get(&connection_id).await {
                route = Some((connection_id, pc.read().await.connection_epoch.clone()));
                break;
            }
        }
        let Some((connection_id, connection_epoch)) = route else {
            return false;
        };
        let Some(action) = supervisor
            .restore_action(&device_name, mode.session_key.as_ref())
            .await
        else {
            continue;
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let Ok(operation_id) = supervisor
            .begin(
                &device_name,
                &request_id,
                &connection_epoch,
                mode.session_key.clone(),
                &action,
            )
            .await
        else {
            return false;
        };
        let command = SetPhysicalDisplayModePayload {
            request_id,
            connection_id: connection_id.clone(),
            connection_epoch: connection_epoch.clone(),
            device_name: device_name.clone(),
            capture_backend: String::new(),
            operation_id,
            action,
        };
        if worker_mgr
            .send_to_connection_worker(
                &connection_id,
                ServiceToWorker::SetPhysicalDisplayMode(command),
            )
            .await
            .is_err()
        {
            supervisor.abandon(operation_id).await;
            return false;
        }
        if !supervisor.wait_for_idle(Duration::from_secs(20)).await {
            // A timed-out worker can still finish its OS write and reply.
            // Preserve the pending fence until the late result arrives or
            // the owning worker process is known to have exited.
            return false;
        }
        if supervisor
            .restore_action(&device_name, mode.session_key.as_ref())
            .await
            .is_some()
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        daemon::pc_manager::PcRegistry,
        model::settings::{Settings, SharedSettings},
    };
    use actix_web::web;
    use desk_ipc_protocol::message::PhysicalDisplayModeData;

    #[tokio::test]
    async fn exclusive_intent_and_physical_begin_share_one_gate() {
        let physical = PhysicalDisplaySupervisor::new();
        let settings = web::Data::new(SharedSettings::from(Settings::default()));
        let (workers, _worker_rx) =
            super::super::worker_manager::WorkerManager::new(settings, PcRegistry::new());
        let virtual_display = super::super::virtual_display::VirtualDisplaySupervisor::new(
            desk_virtual_display::lifecycle_provider(),
            workers,
        );
        let action = PhysicalDisplayAction::Select {
            selector: "mode".into(),
        };
        let operation_id = physical
            .begin_checked(
                "display",
                "first",
                "epoch",
                None,
                &action,
                Some(&virtual_display),
            )
            .await
            .unwrap();
        assert!(!physical.try_enter_exclusive(&virtual_display, 0).await);
        assert!(
            physical
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "first".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id,
                    outcome: PhysicalDisplayModeOutcome::Failed("simulated failure".into()),
                })
                .await
        );
        assert!(physical.try_enter_exclusive(&virtual_display, 0).await);
        assert!(matches!(
            physical
                .begin_checked(
                    "display",
                    "second",
                    "epoch",
                    None,
                    &action,
                    Some(&virtual_display)
                )
                .await,
            Err(BeginPhysicalModeError::Busy)
        ));
        virtual_display.set_desired_exclusive(false, 0);
        assert!(
            physical
                .begin_checked(
                    "display",
                    "third",
                    "epoch",
                    None,
                    &action,
                    Some(&virtual_display)
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn first_original_survives_multiple_mode_changes_and_restore_clears_it() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let action = PhysicalDisplayAction::Select {
            selector: "B".into(),
        };
        let first = supervisor
            .begin("display", "r1", "epoch", None, &action)
            .await
            .unwrap();
        let result = |op_id, request_id: &str, previous: &str, applied: &str, restored| {
            PhysicalDisplayModeResponsePayload {
                request_id: request_id.into(),
                connection_id: "connection".into(),
                connection_epoch: "epoch".into(),
                operation_id: op_id,
                outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                    device_name: "display".into(),
                    display_identity: "screen-1".into(),
                    previous_selector: previous.into(),
                    selector: applied.into(),
                    pixel_width: 1920,
                    pixel_height: 1080,
                    refresh_millihz: 60_000,
                    changed: true,
                    restored,
                }),
            }
        };
        assert_eq!(
            supervisor
                .complete_and_identify(&result(first, "r1", "A", "B", false))
                .await,
            Some(false)
        );
        let second = supervisor
            .begin("display", "r2", "epoch", None, &action)
            .await
            .unwrap();
        assert!(
            supervisor
                .complete(&result(second, "r2", "B", "C", false))
                .await
        );
        let restore = supervisor.restore_action("display", None).await.unwrap();
        assert!(matches!(&restore, PhysicalDisplayAction::Restore {
            original_selector,
            expected_applied_selector,
            expected_display_identity,
        } if original_selector == "A"
            && expected_applied_selector == "C"
            && expected_display_identity == "screen-1"));
        let third = supervisor
            .begin("display", "r3", "epoch", None, &restore)
            .await
            .unwrap();
        assert_eq!(
            supervisor
                .complete_and_identify(&result(third, "r3", "C", "A", true))
                .await,
            Some(true)
        );
        assert!(supervisor.restore_action("display", None).await.is_none());
    }

    #[tokio::test]
    async fn reused_display_name_does_not_inherit_previous_monitors_original_mode() {
        let supervisor = PhysicalDisplaySupervisor::new();
        for (request_id, identity, previous, selected) in [
            ("first", "monitor-one", "A", "B"),
            ("second", "monitor-two", "C", "D"),
        ] {
            let operation_id = supervisor
                .begin(
                    "display",
                    request_id,
                    "epoch",
                    None,
                    &PhysicalDisplayAction::Select {
                        selector: selected.into(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                supervisor
                    .complete_and_identify(&PhysicalDisplayModeResponsePayload {
                        request_id: request_id.into(),
                        connection_id: "connection".into(),
                        connection_epoch: "epoch".into(),
                        operation_id,
                        outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                            device_name: "display".into(),
                            display_identity: identity.into(),
                            previous_selector: previous.into(),
                            selector: selected.into(),
                            pixel_width: 1920,
                            pixel_height: 1080,
                            refresh_millihz: 60_000,
                            changed: true,
                            restored: false,
                        }),
                    })
                    .await,
                Some(false)
            );
        }
        assert!(matches!(
            supervisor.restore_action("display", None).await,
            Some(PhysicalDisplayAction::Restore {
                original_selector,
                expected_applied_selector,
                expected_display_identity,
            }) if original_selector == "C"
                && expected_applied_selector == "D"
                && expected_display_identity == "monitor-two"
        ));
    }

    #[tokio::test]
    async fn local_mode_change_forgets_snapshot_after_pending_operation() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let action = PhysicalDisplayAction::Select {
            selector: "B".into(),
        };
        let id = supervisor
            .begin("display", "request", "epoch", None, &action)
            .await
            .unwrap();
        let capabilities = BTreeMap::from([(
            "SCK".into(),
            BTreeMap::from([(
                "display".into(),
                PhysicalDisplayCapability {
                    available: true,
                    reason: None,
                    current_selector: Some("C".into()),
                    display_identity: Some("screen-1".into()),
                },
            )]),
        )]);
        assert!(
            supervisor
                .reconcile_capabilities(None, &capabilities)
                .await
                .is_empty()
        );
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "request".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id: id,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "screen-1".into(),
                        previous_selector: "A".into(),
                        selector: "B".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        assert_eq!(
            supervisor.reconcile_capabilities(None, &capabilities).await,
            vec!["display"]
        );
        assert!(supervisor.restore_action("display", None).await.is_none());
    }

    #[tokio::test]
    async fn failed_remote_operation_still_reconciles_local_change_after_pending() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let first_id = supervisor
            .begin(
                "display",
                "first",
                "epoch",
                None,
                &PhysicalDisplayAction::Select {
                    selector: "B".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "first".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id: first_id,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "screen-1".into(),
                        previous_selector: "A".into(),
                        selector: "B".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        let second_id = supervisor
            .begin(
                "display",
                "second",
                "epoch",
                None,
                &PhysicalDisplayAction::Select {
                    selector: "D".into(),
                },
            )
            .await
            .unwrap();
        let local_capabilities = BTreeMap::from([(
            "SCK".into(),
            BTreeMap::from([(
                "display".into(),
                PhysicalDisplayCapability {
                    available: true,
                    reason: None,
                    current_selector: Some("C".into()),
                    display_identity: Some("screen-1".into()),
                },
            )]),
        )]);
        // The display callback can race with the pending worker response.
        assert!(
            supervisor
                .reconcile_capabilities(None, &local_capabilities)
                .await
                .is_empty()
        );
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "second".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id: second_id,
                    outcome: PhysicalDisplayModeOutcome::Failed(
                        "physical display changed locally before mode apply".into(),
                    ),
                })
                .await
        );
        // The worker's post-result capability snapshot must reach this path.
        assert_eq!(
            supervisor
                .reconcile_capabilities(None, &local_capabilities)
                .await,
            vec!["display"]
        );
        assert!(supervisor.restore_action("display", None).await.is_none());
    }

    #[tokio::test]
    async fn same_mode_on_replaced_monitor_cancels_old_restore_snapshot() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let operation_id = supervisor
            .begin(
                "display",
                "request",
                "epoch",
                None,
                &PhysicalDisplayAction::Select {
                    selector: "B".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "request".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "monitor-one".into(),
                        previous_selector: "A".into(),
                        selector: "B".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        let capabilities = BTreeMap::from([(
            "SCK".into(),
            BTreeMap::from([(
                "display".into(),
                PhysicalDisplayCapability {
                    available: true,
                    reason: None,
                    current_selector: Some("B".into()),
                    display_identity: Some("monitor-two".into()),
                },
            )]),
        )]);
        assert_eq!(
            supervisor.reconcile_capabilities(None, &capabilities).await,
            vec!["display"]
        );
        assert!(supervisor.restore_action("display", None).await.is_none());
    }

    #[tokio::test]
    async fn capabilities_from_another_session_cannot_forget_restore_snapshot() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let owner = SessionKey {
            platform_session_id: "owner".into(),
            session_generation: 1,
        };
        let other = SessionKey {
            platform_session_id: "other".into(),
            session_generation: 1,
        };
        let operation_id = supervisor
            .begin(
                "display",
                "request",
                "epoch",
                Some(owner.clone()),
                &PhysicalDisplayAction::Select {
                    selector: "B".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "request".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "screen-1".into(),
                        previous_selector: "A".into(),
                        selector: "B".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        let other_operation = supervisor
            .begin(
                "display",
                "other-request",
                "other-epoch",
                Some(other.clone()),
                &PhysicalDisplayAction::Select {
                    selector: "Y".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "other-request".into(),
                    connection_id: "other-connection".into(),
                    connection_epoch: "other-epoch".into(),
                    operation_id: other_operation,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "screen-1".into(),
                        previous_selector: "X".into(),
                        selector: "Y".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        let missing = BTreeMap::new();
        assert_eq!(
            supervisor
                .reconcile_capabilities(Some(&other), &missing)
                .await,
            vec!["display"]
        );
        assert!(
            supervisor
                .restore_action("display", Some(&other))
                .await
                .is_none()
        );
        assert!(
            supervisor
                .restore_action("display", Some(&owner))
                .await
                .is_some()
        );
        assert_eq!(
            supervisor
                .reconcile_capabilities(Some(&owner), &missing)
                .await,
            vec!["display"]
        );
    }

    #[tokio::test]
    async fn auto_cooldown_is_retryable_after_a_real_change() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let auto = |viewport_sequence| PhysicalDisplayAction::Auto {
            viewport_width: 1600,
            viewport_height: 900,
            viewport_sequence,
            max_capture_width: 4096,
            max_capture_height: 4096,
        };
        let first = supervisor
            .begin("display", "r1", "epoch", None, &auto(1))
            .await
            .unwrap();
        assert_eq!(
            supervisor
                .begin("display", "r2", "epoch", None, &auto(2))
                .await,
            Err(BeginPhysicalModeError::Busy)
        );
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "r1".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id: first,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "screen-1".into(),
                        previous_selector: "A".into(),
                        selector: "B".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        assert_eq!(
            supervisor
                .begin("display", "r3", "epoch", None, &auto(3))
                .await,
            Err(BeginPhysicalModeError::CoolingDown)
        );
        assert!(BeginPhysicalModeError::Busy.retryable());
        assert!(BeginPhysicalModeError::CoolingDown.retryable());
        assert!(!BeginPhysicalModeError::StaleViewport.retryable());
    }

    #[tokio::test]
    async fn timed_out_operation_keeps_its_fence_until_owning_worker_is_replaced() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let owner = SessionKey {
            platform_session_id: "owner".into(),
            session_generation: 1,
        };
        let other = SessionKey {
            platform_session_id: "other".into(),
            session_generation: 1,
        };
        let action = PhysicalDisplayAction::Select {
            selector: "B".into(),
        };
        let first = supervisor
            .begin("display", "r1", "epoch", Some(owner.clone()), &action)
            .await
            .unwrap();
        {
            let mut state = supervisor.state.lock().await;
            state.pending.as_mut().unwrap().1.started =
                Instant::now() - OPERATION_TIMEOUT - Duration::from_secs(1);
        }
        assert_eq!(
            supervisor
                .begin("display", "r2", "epoch", Some(owner.clone()), &action)
                .await,
            Err(BeginPhysicalModeError::TimedOut)
        );
        assert!(!BeginPhysicalModeError::TimedOut.retryable());
        assert!(!supervisor.abandon_pending_for_session(Some(&other)).await);
        assert!(supervisor.has_pending_for_session(Some(&owner)).await);
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "r1".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id: first,
                    outcome: PhysicalDisplayModeOutcome::Applied(PhysicalDisplayModeData {
                        device_name: "display".into(),
                        display_identity: "screen-1".into(),
                        previous_selector: "A".into(),
                        selector: "B".into(),
                        pixel_width: 1920,
                        pixel_height: 1080,
                        refresh_millihz: 60_000,
                        changed: true,
                        restored: false,
                    }),
                })
                .await
        );
        assert!(
            supervisor
                .restore_action("display", Some(&owner))
                .await
                .is_some()
        );
        assert!(
            supervisor
                .begin("display", "r3", "epoch", Some(owner.clone()), &action)
                .await
                .is_ok()
        );
        assert!(supervisor.abandon_pending_for_session(Some(&owner)).await);
    }

    #[tokio::test]
    async fn all_exclusive_waiters_observe_a_completed_physical_operation() {
        let supervisor = std::sync::Arc::new(PhysicalDisplaySupervisor::new());
        let operation_id = supervisor
            .begin(
                "display",
                "request",
                "epoch",
                None,
                &PhysicalDisplayAction::Select {
                    selector: "B".into(),
                },
            )
            .await
            .unwrap();
        let waiters = (0..2)
            .map(|_| {
                let supervisor = std::sync::Arc::clone(&supervisor);
                tokio::spawn(async move { supervisor.wait_for_idle(Duration::from_secs(1)).await })
            })
            .collect::<Vec<_>>();
        tokio::task::yield_now().await;
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "request".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id,
                    outcome: PhysicalDisplayModeOutcome::Failed("simulated failure".into()),
                })
                .await
        );
        for waiter in waiters {
            assert!(waiter.await.unwrap());
        }
    }

    #[tokio::test]
    async fn failed_video_with_applied_mode_keeps_original_for_later_restore() {
        let supervisor = PhysicalDisplaySupervisor::new();
        let operation_id = supervisor
            .begin(
                "display",
                "request",
                "epoch",
                None,
                &PhysicalDisplayAction::Select {
                    selector: "applied".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            supervisor
                .complete(&PhysicalDisplayModeResponsePayload {
                    request_id: "request".into(),
                    connection_id: "connection".into(),
                    connection_epoch: "epoch".into(),
                    operation_id,
                    outcome: PhysicalDisplayModeOutcome::AppliedWithoutVideo {
                        data: PhysicalDisplayModeData {
                            device_name: "display".into(),
                            display_identity: "screen-1".into(),
                            previous_selector: "original".into(),
                            selector: "applied".into(),
                            pixel_width: 1920,
                            pixel_height: 1080,
                            refresh_millihz: 60_000,
                            changed: true,
                            restored: false,
                        },
                        reason: "capture failed".into(),
                    },
                })
                .await
        );
        assert!(matches!(
            supervisor.restore_action("display", None).await,
            Some(PhysicalDisplayAction::Restore { original_selector, expected_applied_selector, .. })
                if original_selector == "original" && expected_applied_selector == "applied"
        ));
    }
}
