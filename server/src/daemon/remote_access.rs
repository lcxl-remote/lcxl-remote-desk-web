use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::durable_file::{FileMode, durable_atomic_write};
use crate::host_activity::HostActivityRegistry;
use crate::host_control::{CentralSyncState, HostRemoteAccessMode, HostRemoteAccessStatus};

const FORMAT_VERSION: u32 = 1;
#[cfg(test)]
const STATE_FILE_NAME: &str = "remote-access-state.toml";
const WORKER_STATE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const RESTARTED_WORKER_STATE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAccessMode {
    Unlocked,
    Locked,
    RecoveryLocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAccessState {
    pub mode: RemoteAccessMode,
    pub state_version: u64,
    pub lock_id: Option<String>,
    /// Lock round mirrored centrally. Retained after local unlock so the central
    /// unlock can be fenced against the exact round it closes.
    pub central_lock_id: Option<String>,
    pub locked_at: Option<String>,
    pub durable: bool,
    pub central_sync_pending: bool,
}

impl RemoteAccessState {
    pub fn unlocked(state_version: u64) -> Self {
        Self {
            mode: RemoteAccessMode::Unlocked,
            state_version,
            lock_id: None,
            central_lock_id: None,
            locked_at: None,
            durable: true,
            central_sync_pending: false,
        }
    }

    pub fn locked(
        state_version: u64,
        lock_id: String,
        locked_at: String,
        central_sync_pending: bool,
    ) -> Self {
        Self {
            mode: RemoteAccessMode::Locked,
            state_version,
            lock_id: Some(lock_id.clone()),
            central_lock_id: Some(lock_id),
            locked_at: Some(locked_at),
            durable: true,
            central_sync_pending,
        }
    }

    fn recovery_locked() -> Self {
        Self {
            mode: RemoteAccessMode::RecoveryLocked,
            state_version: 0,
            lock_id: Some(uuid::Uuid::new_v4().to_string()),
            central_lock_id: None,
            locked_at: None,
            durable: false,
            central_sync_pending: true,
        }
    }

    pub fn is_locked(&self) -> bool {
        !matches!(self.mode, RemoteAccessMode::Unlocked)
    }
}

impl From<&RemoteAccessState> for HostRemoteAccessStatus {
    fn from(state: &RemoteAccessState) -> Self {
        let mode = match state.mode {
            RemoteAccessMode::Unlocked => HostRemoteAccessMode::Unlocked,
            RemoteAccessMode::Locked => HostRemoteAccessMode::Locked,
            RemoteAccessMode::RecoveryLocked => HostRemoteAccessMode::RecoveryLocked,
        };
        let central_sync = if state.central_sync_pending {
            CentralSyncState::Pending
        } else if state.is_locked() {
            CentralSyncState::Synced
        } else {
            CentralSyncState::NotRequired
        };
        Self {
            mode,
            state_version: state.state_version,
            locked_at: state.locked_at.clone(),
            durable: state.durable,
            central_sync,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRemoteAccessState {
    format_version: u32,
    state_version: u64,
    locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    central_lock_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locked_at: Option<String>,
    central_sync_pending: bool,
}

impl PersistedRemoteAccessState {
    fn from_runtime(state: &RemoteAccessState) -> Result<Self> {
        if state.mode == RemoteAccessMode::RecoveryLocked {
            bail!("recovery-locked state must be repaired before it can be persisted");
        }
        if state.mode == RemoteAccessMode::Locked
            && (state.lock_id.as_deref().is_none_or(str::is_empty)
                || state.locked_at.as_deref().is_none_or(str::is_empty))
        {
            bail!("locked state requires lock_id and locked_at");
        }
        if state.mode == RemoteAccessMode::Unlocked
            && (state.lock_id.is_some() || state.locked_at.is_some())
        {
            bail!("unlocked state cannot retain lock metadata");
        }
        Ok(Self {
            format_version: FORMAT_VERSION,
            state_version: state.state_version,
            locked: state.mode == RemoteAccessMode::Locked,
            lock_id: state.lock_id.clone(),
            central_lock_id: state.central_lock_id.clone(),
            locked_at: state.locked_at.clone(),
            central_sync_pending: state.central_sync_pending,
        })
    }

    fn into_runtime(self) -> Result<RemoteAccessState> {
        if self.format_version != FORMAT_VERSION {
            bail!("unsupported remote-access state format");
        }
        if self.locked {
            let lock_id = self
                .lock_id
                .filter(|value| !value.is_empty())
                .context("locked state is missing lock_id")?;
            let locked_at = self
                .locked_at
                .filter(|value| !value.is_empty())
                .context("locked state is missing locked_at")?;
            let mut state = RemoteAccessState::locked(
                self.state_version,
                lock_id,
                locked_at,
                self.central_sync_pending,
            );
            state.central_lock_id = self.central_lock_id.or_else(|| state.lock_id.clone());
            Ok(state)
        } else {
            if self.lock_id.is_some() || self.locked_at.is_some() {
                bail!("unlocked state contains lock metadata");
            }
            Ok(RemoteAccessState {
                mode: RemoteAccessMode::Unlocked,
                state_version: self.state_version,
                lock_id: None,
                central_lock_id: self.central_lock_id,
                locked_at: None,
                durable: true,
                central_sync_pending: self.central_sync_pending,
            })
        }
    }
}

/// Whether a read error proves the state file is absent rather than merely
/// unreadable.
///
/// The distinction decides the failure direction: a file that might exist but
/// cannot be read could be holding a lock, so it fails closed; a file that
/// provably does not exist is an uninitialized install, and locking a fresh
/// device out of itself would be worse than starting unlocked.
///
/// A path whose parent component is not a directory is one such proof, but the
/// platforms disagree on how to say it: Unix reports `NotADirectory` while
/// Windows collapses it into `NotFound`. Matching only `NotFound` therefore
/// made the very same misconfiguration start unlocked on Windows and
/// recovery-locked on Unix.
fn proves_absence(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[derive(Debug, Clone)]
pub struct RemoteAccessStateStore {
    path: PathBuf,
}

impl RemoteAccessStateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the durable state. A missing file is a normal first-run condition;
    /// evidence of an invalid existing file fails closed.
    pub fn load_or_initialize(&self) -> RemoteAccessState {
        match fs::read_to_string(&self.path) {
            Ok(contents) => self.parse(&contents).unwrap_or_else(|error| {
                log::error!(
                    "Invalid remote-access state at {}: {error:#}",
                    self.path.display()
                );
                RemoteAccessState::recovery_locked()
            }),
            Err(error) if proves_absence(&error) => {
                let initial = RemoteAccessState::unlocked(1);
                if let Err(error) = self.persist(&initial) {
                    log::warn!(
                        "Could not initialize remote-access state at {}: {error:#}",
                        self.path.display()
                    );
                    RemoteAccessState::unlocked(0)
                } else {
                    initial
                }
            }
            Err(error) => {
                log::error!(
                    "Could not read remote-access state at {}: {error}",
                    self.path.display()
                );
                RemoteAccessState::recovery_locked()
            }
        }
    }

    /// Read-only status for the headless CLI when the daemon is offline. It
    /// never creates, repairs, or replaces the state file.
    pub fn load_read_only(&self) -> RemoteAccessState {
        match fs::read_to_string(&self.path) {
            Ok(contents) => self
                .parse(&contents)
                .unwrap_or_else(|_| RemoteAccessState::recovery_locked()),
            Err(error) if proves_absence(&error) => RemoteAccessState::unlocked(0),
            Err(_) => RemoteAccessState::recovery_locked(),
        }
    }

    pub fn persist(&self, state: &RemoteAccessState) -> Result<()> {
        let persisted = PersistedRemoteAccessState::from_runtime(state)?;
        let contents = toml::to_string_pretty(&persisted)
            .context("failed to serialize remote-access state")?;
        // The daemon owns this file outright, so each write puts it back to
        // owner-only rather than keeping whatever it happens to find.
        durable_atomic_write(&self.path, contents.as_bytes(), FileMode::OwnerOnly).with_context(
            || {
                format!(
                    "failed to persist remote-access state at {}",
                    self.path.display()
                )
            },
        )
    }

    fn parse(&self, contents: &str) -> Result<RemoteAccessState> {
        toml::from_str::<PersistedRemoteAccessState>(contents)
            .context("failed to parse remote-access state")?
            .into_runtime()
    }
}

#[derive(Clone)]
pub struct RemoteAccessGate {
    locked: Arc<AtomicBool>,
    state: Arc<RwLock<RemoteAccessState>>,
}

pub struct RemoteAccessCoordinator {
    transition: tokio::sync::Mutex<()>,
    store: RemoteAccessStateStore,
    gate: RemoteAccessGate,
    activity: HostActivityRegistry,
    runtime: std::sync::OnceLock<RemoteAccessRuntime>,
    central_commands: tokio::sync::broadcast::Sender<String>,
}

#[derive(Clone)]
struct RemoteAccessRuntime {
    pc_registry: crate::daemon::pc_manager::PcRegistry,
    worker_manager: crate::daemon::worker_manager::WorkerManager,
    virtual_display: Option<Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    host_control_hub: std::sync::Weak<crate::host_control::HostControlHub>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectOutcome {
    pub connection_id: String,
    pub already_disconnected: bool,
}

impl RemoteAccessCoordinator {
    pub fn new(
        store: RemoteAccessStateStore,
        gate: RemoteAccessGate,
        activity: HostActivityRegistry,
    ) -> Self {
        let (central_commands, _) = tokio::sync::broadcast::channel(64);
        Self {
            transition: tokio::sync::Mutex::new(()),
            store,
            gate,
            activity,
            runtime: std::sync::OnceLock::new(),
            central_commands,
        }
    }

    pub fn attach_runtime(
        &self,
        pc_registry: crate::daemon::pc_manager::PcRegistry,
        worker_manager: crate::daemon::worker_manager::WorkerManager,
        virtual_display: Option<Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
        host_control_hub: std::sync::Weak<crate::host_control::HostControlHub>,
    ) -> bool {
        self.runtime
            .set(RemoteAccessRuntime {
                pc_registry,
                worker_manager,
                virtual_display,
                host_control_hub,
            })
            .is_ok()
    }

    pub fn snapshot(&self) -> RemoteAccessState {
        self.gate.snapshot()
    }

    pub async fn disconnect_connection(&self, connection_id: &str) -> Result<DisconnectOutcome> {
        let Some(runtime) = self.runtime.get() else {
            bail!("remote-access runtime is not ready");
        };
        let mut existed = crate::daemon::pc_manager::force_disconnect_connection(
            &runtime.pc_registry,
            &runtime.worker_manager,
            runtime.virtual_display.as_ref(),
            connection_id,
            "host-disconnect",
        )
        .await;
        if let Some(hub) = runtime.host_control_hub.upgrade() {
            existed |= !hub.cancel_pending_for_connection(connection_id).is_empty();
        }
        if existed {
            self.enqueue_peer_eviction(connection_id);
        }
        Ok(DisconnectOutcome {
            connection_id: connection_id.to_string(),
            already_disconnected: !existed,
        })
    }

    /// Closes admission before touching disk. If persistence fails, this process
    /// remains locked so an emergency action never re-opens access by accident.
    pub async fn lock(&self) -> Result<RemoteAccessState> {
        let _transition = self.transition.lock().await;
        let current = self.gate.snapshot();
        if current.mode == RemoteAccessMode::Locked {
            let persist_result = self.store.persist(&current);
            let mut published = current.clone();
            if persist_result.is_ok() && !published.durable {
                published.durable = true;
                self.gate.replace_metadata(published.clone());
                self.publish(&published);
            }
            let runtime_result = self.apply_runtime(&published, true).await;
            persist_result?;
            runtime_result?;
            return Ok(published);
        }
        let mut next = RemoteAccessState::locked(
            current.state_version.saturating_add(1),
            uuid::Uuid::new_v4().to_string(),
            chrono::Utc::now().to_rfc3339(),
            true,
        );
        next.durable = false;
        self.gate.apply(next.clone())?;
        self.publish(&next);
        let persist_result = self.store.persist(&next);
        if persist_result.is_ok() {
            next.durable = true;
            self.gate.replace_metadata(next.clone());
            self.publish(&next);
        }
        let runtime_result = self.apply_runtime(&next, true).await;
        persist_result?;
        runtime_result?;
        Ok(next)
    }

    /// Persists the authenticated unlock before opening admission, so a crash
    /// cannot leave the running process open while the durable record is locked.
    pub async fn unlock(&self, expected_version: u64) -> Result<RemoteAccessState> {
        let _transition = self.transition.lock().await;
        let current = self.gate.snapshot();
        if current.state_version != expected_version {
            bail!(
                "stale remote-access state: expected version {expected_version}, current version {}",
                current.state_version
            );
        }
        if current.mode == RemoteAccessMode::Unlocked {
            return Ok(current);
        }
        let mut next = RemoteAccessState::unlocked(current.state_version.saturating_add(1));
        // Local OS authentication is the only unlock prerequisite. Central is a
        // defense-in-depth mirror and must never make the local safety control
        // unavailable. For a normal lock we retain its known fence; a recovery
        // lock may not know the central fence yet, so it sends an unlocked probe
        // and learns the fence asynchronously from the ack.
        next.central_lock_id = current.central_lock_id.clone().or_else(|| {
            (current.mode == RemoteAccessMode::Locked)
                .then(|| current.lock_id.clone())
                .flatten()
        });
        next.central_sync_pending = current.central_sync_pending
            || next.central_lock_id.is_some()
            || current.mode == RemoteAccessMode::RecoveryLocked;
        self.store.persist(&next)?;
        if let Err(error) = self.apply_runtime(&next, false).await {
            let rollback = RemoteAccessState::locked(
                next.state_version.saturating_add(1),
                uuid::Uuid::new_v4().to_string(),
                chrono::Utc::now().to_rfc3339(),
                true,
            );
            self.store.persist(&rollback)?;
            self.gate.apply(rollback.clone())?;
            self.publish(&rollback);
            let _ = self.apply_runtime(&rollback, true).await;
            bail!("worker could not safely apply unlock; host remains locked: {error:#}");
        }
        self.gate.apply(next.clone())?;
        self.publish(&next);
        Ok(next)
    }

    pub fn pending_central_request(
        &self,
    ) -> Option<desk_signal_facade::model::remote_access::HostRemoteAccessLockRequest> {
        let state = self.gate.snapshot();
        if !state.central_sync_pending
            || (!state.durable && state.mode != RemoteAccessMode::RecoveryLocked)
        {
            return None;
        }
        let lock_id = state
            .central_lock_id
            .clone()
            .or_else(|| state.lock_id.clone());
        if state.is_locked() && lock_id.is_none() {
            return None;
        }
        Some(
            desk_signal_facade::model::remote_access::HostRemoteAccessLockRequest {
                request_id: central_request_id(&state, lock_id.as_deref()),
                lock_id,
                state_version: state.state_version,
                locked: state.is_locked(),
            },
        )
    }

    pub async fn acknowledge_central(
        &self,
        ack: &desk_signal_facade::model::remote_access::HostRemoteAccessLockAck,
    ) -> Result<bool> {
        let _transition = self.transition.lock().await;
        let current = self.gate.snapshot();
        let Some(expected_request) = self.pending_central_request() else {
            return Ok(false);
        };
        if ack.request_id != expected_request.request_id {
            return Ok(false);
        }

        if current.mode == RemoteAccessMode::Unlocked && current.central_sync_pending {
            let mut reconciled = current.clone();
            if ack.locked {
                let Some(lock_id) = ack.lock_id.clone().filter(|value| !value.is_empty()) else {
                    return Ok(false);
                };
                // Central knows a lock fence that the local recovery state did
                // not. Learn it and retry an unlock above that version, while
                // deliberately keeping the authoritative local gate open.
                reconciled.state_version = current
                    .state_version
                    .max(ack.state_version)
                    .saturating_add(1);
                reconciled.central_lock_id = Some(lock_id);
                self.persist_reconciled_state(&current, &reconciled)?;
                return Ok(false);
            }

            // Desired state is already true centrally. This also covers an old
            // or freshly initialized central that never observed the preceding
            // lock round: no matching lock_id is needed merely to agree that the
            // device is unlocked.
            reconciled.state_version = current.state_version.max(ack.state_version);
            reconciled.central_lock_id = ack
                .lock_id
                .clone()
                .or_else(|| current.central_lock_id.clone());
            reconciled.central_sync_pending = false;
            self.persist_reconciled_state(&current, &reconciled)?;
            return Ok(true);
        }

        if current.mode == RemoteAccessMode::RecoveryLocked && current.central_sync_pending {
            if ack.locked {
                let Some(lock_id) = ack.lock_id.clone().filter(|value| !value.is_empty()) else {
                    return Ok(false);
                };
                // The central mirror already has a durable lock round. Adopting
                // it repairs the unreadable local record without ever opening
                // admission and gives a later authenticated unlock the exact
                // fence it must close.
                let repaired = RemoteAccessState::locked(
                    ack.state_version,
                    lock_id,
                    chrono::Utc::now().to_rfc3339(),
                    false,
                );
                self.store.persist(&repaired)?;
                self.gate.initialize_from_store(repaired.clone());
                self.publish(&repaired);
                return Ok(true);
            }

            // An equal/stale recovery version cannot overwrite a central row
            // that says unlocked (manager devices start at version zero). Move
            // the fail-closed lock round above the observed fence and retry.
            let lock_id = current
                .lock_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let retry = RemoteAccessState::locked(
                ack.state_version
                    .max(current.state_version)
                    .saturating_add(1),
                lock_id,
                chrono::Utc::now().to_rfc3339(),
                true,
            );
            self.store.persist(&retry)?;
            self.gate.initialize_from_store(retry.clone());
            self.publish(&retry);
            return Ok(false);
        }
        let expected_lock_id = current
            .central_lock_id
            .as_deref()
            .or(current.lock_id.as_deref());
        if !current.central_sync_pending
            || ack.state_version != current.state_version
            || ack.locked != current.is_locked()
            || ack.lock_id.as_deref() != expected_lock_id
        {
            return Ok(false);
        }
        let mut synced = current;
        synced.central_sync_pending = false;
        self.store.persist(&synced)?;
        self.gate.replace_metadata(synced.clone());
        self.publish(&synced);
        Ok(true)
    }

    fn persist_reconciled_state(
        &self,
        current: &RemoteAccessState,
        next: &RemoteAccessState,
    ) -> Result<()> {
        self.store.persist(next)?;
        if next.state_version == current.state_version {
            self.gate.replace_metadata(next.clone());
        } else {
            self.gate.apply(next.clone())?;
        }
        self.publish(next);
        Ok(())
    }

    fn publish(&self, state: &RemoteAccessState) {
        self.activity.set_remote_access_status(state.into());
    }

    async fn apply_runtime(
        &self,
        state: &RemoteAccessState,
        close_connections: bool,
    ) -> Result<()> {
        let Some(runtime) = self.runtime.get() else {
            return Ok(());
        };
        let operation_id = state
            .lock_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let payload = desk_ipc_protocol::message::RemoteAccessStatePayload {
            operation_id,
            state_version: state.state_version,
            locked: state.is_locked(),
        };
        if let Err(first_error) = runtime
            .worker_manager
            .apply_remote_access_state(payload.clone(), WORKER_STATE_ACK_TIMEOUT)
            .await
        {
            log::warn!(
                "Worker did not apply remote-access state; recycling it before retry: {first_error}"
            );
            runtime
                .worker_manager
                .recycle_for_remote_access_timeout()
                .await
                .map_err(anyhow::Error::msg)?;
            runtime
                .worker_manager
                .apply_remote_access_state(payload, RESTARTED_WORKER_STATE_ACK_TIMEOUT)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        if close_connections {
            for connection_id in runtime.pc_registry.all_connection_ids().await {
                crate::daemon::pc_manager::force_disconnect_connection(
                    &runtime.pc_registry,
                    &runtime.worker_manager,
                    runtime.virtual_display.as_ref(),
                    &connection_id,
                    "remote-access-lock",
                )
                .await;
                self.enqueue_peer_eviction(&connection_id);
                if let Some(hub) = runtime.host_control_hub.upgrade() {
                    hub.cancel_pending_for_connection(&connection_id);
                }
            }
            if let Some(hub) = runtime.host_control_hub.upgrade() {
                hub.cancel_all_pending_for_security_lock();
            }
        }
        Ok(())
    }

    pub fn subscribe_central_commands(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.central_commands.subscribe()
    }

    fn enqueue_peer_eviction(&self, target_connection_id: &str) {
        let operation_id = uuid::Uuid::new_v4().to_string();
        let payload = desk_signal_facade::model::remote_access::TerminateRemotePeerRequest {
            operation_id: operation_id.clone(),
            target_connection_id: target_connection_id.to_string(),
        };
        let frame = desk_signal_facade::model::signal::SignalingModel::new(
            &operation_id,
            desk_signal_facade::model::signal::SignalingType::TerminateRemotePeerRequest,
            None,
            None,
            serde_json::to_value(payload).ok(),
            None,
        );
        if let Ok(text) = serde_json::to_string(&frame) {
            let _ = self.central_commands.send(text);
        }
    }
}

fn central_request_id(state: &RemoteAccessState, lock_id: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"lcxl-remote-access-central-request-v1");
    digest.update(state.state_version.to_le_bytes());
    digest.update([u8::from(state.is_locked())]);
    digest.update(lock_id.unwrap_or_default().as_bytes());
    let digest = digest.finalize();
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("remote-access-{suffix}")
}

impl RemoteAccessGate {
    pub fn new(initial: RemoteAccessState) -> Self {
        Self {
            locked: Arc::new(AtomicBool::new(initial.is_locked())),
            state: Arc::new(RwLock::new(initial)),
        }
    }

    pub fn startup_locked() -> Self {
        Self::new(RemoteAccessState::recovery_locked())
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> RemoteAccessState {
        self.state.read().unwrap().clone()
    }

    pub fn apply(&self, next: RemoteAccessState) -> Result<bool> {
        let mut current = self.state.write().unwrap();
        if next.state_version < current.state_version {
            return Ok(false);
        }
        if next.state_version == current.state_version {
            if *current == next {
                return Ok(false);
            }
            self.locked.store(true, Ordering::Release);
            bail!("conflicting remote-access states use the same version");
        }
        self.locked.store(next.is_locked(), Ordering::Release);
        *current = next;
        Ok(true)
    }

    fn replace_metadata(&self, next: RemoteAccessState) {
        let mut current = self.state.write().unwrap();
        debug_assert_eq!(current.state_version, next.state_version);
        debug_assert_eq!(current.mode, next.mode);
        *current = next;
    }

    /// Replaces the temporary fail-closed startup state with the state loaded
    /// from the durable store. Callers must do this before accepting traffic.
    pub fn initialize_from_store(&self, loaded: RemoteAccessState) {
        let mut current = self.state.write().unwrap();
        self.locked.store(loaded.is_locked(), Ordering::Release);
        *current = loaded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_initializes_unlocked_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));

        let state = store.load_or_initialize();

        assert_eq!(state, RemoteAccessState::unlocked(1));
        assert_eq!(store.load_or_initialize(), state);
    }

    #[test]
    fn locked_state_survives_atomic_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));
        let _ = store.load_or_initialize();
        let locked = RemoteAccessState::locked(
            2,
            "lock-2".to_string(),
            "2026-07-22T12:00:00Z".to_string(),
            true,
        );

        store.persist(&locked).unwrap();

        assert_eq!(store.load_or_initialize(), locked);
        assert!(store.path().exists());
        assert_eq!(
            fs::read_dir(directory.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .count(),
            1
        );
    }

    #[test]
    fn malformed_existing_file_enters_recovery_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        fs::write(&path, "not valid = [").unwrap();
        let store = RemoteAccessStateStore::new(path);

        let state = store.load_or_initialize();

        assert_eq!(state.mode, RemoteAccessMode::RecoveryLocked);
        assert!(state.is_locked());
        assert!(state.lock_id.is_some());
    }

    #[test]
    fn unsupported_existing_format_enters_recovery_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        fs::write(
            &path,
            "format_version = 2\nstate_version = 7\nlocked = false\ncentral_sync_pending = false\n",
        )
        .unwrap();
        let store = RemoteAccessStateStore::new(path);

        assert_eq!(
            store.load_or_initialize().mode,
            RemoteAccessMode::RecoveryLocked
        );
    }

    #[test]
    fn gate_rejects_stale_and_conflicting_versions() {
        let gate = RemoteAccessGate::new(RemoteAccessState::unlocked(3));

        assert!(!gate.apply(RemoteAccessState::unlocked(2)).unwrap());
        assert!(
            gate.apply(RemoteAccessState::locked(
                4,
                "lock-4".to_string(),
                "2026-07-22T12:00:00Z".to_string(),
                false,
            ))
            .unwrap()
        );
        assert!(gate.is_locked());
        assert!(gate.apply(RemoteAccessState::unlocked(4)).is_err());
        assert!(gate.is_locked());
    }

    #[test]
    fn initialization_failure_does_not_default_to_locked() {
        let directory = tempfile::tempdir().unwrap();
        let blocking_file = directory.path().join("not-a-directory");
        fs::write(&blocking_file, "occupied").unwrap();
        let store = RemoteAccessStateStore::new(blocking_file.join(STATE_FILE_NAME));

        assert_eq!(store.load_or_initialize(), RemoteAccessState::unlocked(0));
        // The read-only CLI view reads the same condition the same way.
        assert_eq!(store.load_read_only(), RemoteAccessState::unlocked(0));
    }

    /// The failure direction hangs entirely on this classification, and the
    /// kind a blocked path yields differs per platform (`NotADirectory` on
    /// Unix, `NotFound` on Windows), so pin both spellings explicitly instead
    /// of relying on whichever one the host happens to produce.
    #[test]
    fn only_a_proven_absence_skips_the_recovery_lock() {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::NotADirectory] {
            assert!(
                proves_absence(&io::Error::new(kind, "test")),
                "{kind:?} proves the state file is absent",
            );
        }
        // Anything that leaves open the possibility of an existing locked
        // state must fail closed instead.
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Other,
        ] {
            assert!(
                !proves_absence(&io::Error::new(kind, "test")),
                "{kind:?} does not prove the state file is absent",
            );
        }
    }

    /// A state file that exists but cannot be read may be holding a lock, so
    /// the recovery lock still engages — the absence classification must not
    /// widen into "any read error is a fresh install".
    #[test]
    #[cfg(unix)]
    fn unreadable_existing_state_still_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STATE_FILE_NAME);
        fs::write(
            &path,
            "format_version = 1\nstate_version = 7\nlocked = false\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        // Root ignores the permission bits, so there is nothing to assert there.
        if fs::read_to_string(&path).is_ok() {
            return;
        }

        assert_eq!(
            store_at(&path).load_or_initialize().mode,
            RemoteAccessMode::RecoveryLocked
        );
        assert_eq!(
            store_at(&path).load_read_only().mode,
            RemoteAccessMode::RecoveryLocked
        );
    }

    #[cfg(unix)]
    fn store_at(path: &Path) -> RemoteAccessStateStore {
        RemoteAccessStateStore::new(path.to_path_buf())
    }

    fn coordinator(
        store: RemoteAccessStateStore,
        gate: RemoteAccessGate,
    ) -> RemoteAccessCoordinator {
        let (publisher, _) = tokio::sync::broadcast::channel(8);
        RemoteAccessCoordinator::new(store, gate, HostActivityRegistry::new(publisher))
    }

    #[tokio::test]
    async fn coordinator_lock_and_unlock_are_durable_and_versioned() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));
        let initial = store.load_or_initialize();
        let gate = RemoteAccessGate::new(initial);
        let coordinator = coordinator(store.clone(), gate.clone());

        let locked = coordinator.lock().await.unwrap();
        assert!(gate.is_locked());
        assert_eq!(store.load_or_initialize(), locked);

        let unlocked = coordinator.unlock(locked.state_version).await.unwrap();
        assert!(!gate.is_locked());
        assert!(unlocked.state_version > locked.state_version);
        assert!(unlocked.central_sync_pending);
        assert_eq!(store.load_or_initialize(), unlocked);

        let request = coordinator.pending_central_request().unwrap();
        let unlock_ack = desk_signal_facade::model::remote_access::HostRemoteAccessLockAck {
            request_id: request.request_id.clone(),
            lock_id: request.lock_id.clone(),
            state_version: request.state_version,
            locked: false,
            generation: 1,
        };
        let mut stale_ack = unlock_ack.clone();
        stale_ack.request_id = "stale-request".into();
        assert!(!coordinator.acknowledge_central(&stale_ack).await.unwrap());
        assert!(coordinator.snapshot().central_sync_pending);
        assert!(coordinator.acknowledge_central(&unlock_ack).await.unwrap());
        assert!(!coordinator.snapshot().central_sync_pending);
    }

    #[tokio::test]
    async fn failed_lock_persistence_keeps_process_gate_closed() {
        let directory = tempfile::tempdir().unwrap();
        let blocking_file = directory.path().join("not-a-directory");
        fs::write(&blocking_file, "occupied").unwrap();
        let store = RemoteAccessStateStore::new(blocking_file.join(STATE_FILE_NAME));
        let gate = RemoteAccessGate::new(RemoteAccessState::unlocked(0));
        let coordinator = coordinator(store.clone(), gate.clone());

        assert!(coordinator.lock().await.is_err());
        assert!(gate.is_locked());
        assert!(!coordinator.snapshot().durable);
        assert!(coordinator.pending_central_request().is_none());

        fs::remove_file(&blocking_file).unwrap();
        let retried = coordinator.lock().await.unwrap();
        assert!(retried.durable);
        assert_eq!(store.load_or_initialize(), retried);
    }

    #[tokio::test]
    async fn stale_unlock_does_not_open_gate() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));
        let locked = RemoteAccessState::locked(
            4,
            "lock-4".to_string(),
            "2026-07-22T12:00:00Z".to_string(),
            false,
        );
        store.persist(&locked).unwrap();
        let gate = RemoteAccessGate::new(locked);
        let coordinator = coordinator(store, gate.clone());

        assert!(coordinator.unlock(3).await.is_err());
        assert!(gate.is_locked());
    }

    #[tokio::test]
    async fn recovery_lock_can_be_locally_unlocked_before_central_replies() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));
        fs::write(store.path(), "invalid = [").unwrap();
        let recovery = store.load_or_initialize();
        let gate = RemoteAccessGate::new(recovery);
        let coordinator = coordinator(store.clone(), gate.clone());
        assert!(coordinator.pending_central_request().is_some());
        let unlocked = coordinator.unlock(0).await.unwrap();
        assert!(!gate.is_locked());
        assert_eq!(unlocked.mode, RemoteAccessMode::Unlocked);
        assert!(unlocked.durable);
        assert!(unlocked.central_sync_pending);
        let request = coordinator.pending_central_request().unwrap();
        assert!(!request.locked);
        assert!(request.lock_id.is_none());
        let ack = desk_signal_facade::model::remote_access::HostRemoteAccessLockAck {
            request_id: request.request_id,
            lock_id: None,
            state_version: 9,
            locked: false,
            generation: 3,
        };

        assert!(coordinator.acknowledge_central(&ack).await.unwrap());
        let synced = coordinator.snapshot();
        assert_eq!(synced.mode, RemoteAccessMode::Unlocked);
        assert_eq!(synced.state_version, 9);
        assert!(!synced.central_sync_pending);
        assert_eq!(store.load_or_initialize(), synced);
    }

    #[tokio::test]
    async fn locally_unlocked_recovery_learns_existing_central_fence_without_relocking() {
        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));
        fs::write(store.path(), "invalid = [").unwrap();
        let gate = RemoteAccessGate::new(store.load_or_initialize());
        let coordinator = coordinator(store.clone(), gate.clone());
        coordinator.unlock(0).await.unwrap();
        let first_request = coordinator.pending_central_request().unwrap();
        let ack = desk_signal_facade::model::remote_access::HostRemoteAccessLockAck {
            request_id: first_request.request_id,
            lock_id: Some("central-lock".into()),
            state_version: 12,
            locked: true,
            generation: 4,
        };

        assert!(!coordinator.acknowledge_central(&ack).await.unwrap());
        let retry = coordinator.snapshot();
        assert_eq!(retry.mode, RemoteAccessMode::Unlocked);
        assert!(!gate.is_locked());
        assert_eq!(retry.state_version, 13);
        assert_eq!(retry.central_lock_id.as_deref(), Some("central-lock"));
        assert!(retry.central_sync_pending);

        let second_request = coordinator.pending_central_request().unwrap();
        assert!(!second_request.locked);
        assert_eq!(second_request.lock_id.as_deref(), Some("central-lock"));
        let synced_ack = desk_signal_facade::model::remote_access::HostRemoteAccessLockAck {
            request_id: second_request.request_id,
            lock_id: Some("central-lock".into()),
            state_version: second_request.state_version,
            locked: false,
            generation: 4,
        };
        assert!(coordinator.acknowledge_central(&synced_ack).await.unwrap());
        assert!(!coordinator.snapshot().central_sync_pending);
        assert_eq!(store.load_or_initialize(), coordinator.snapshot());
    }

    #[tokio::test]
    async fn coordinator_disconnect_is_idempotent_and_covers_admission_only_connections() {
        use crate::daemon::pc_manager::{Admission, PcRegistry};
        use crate::daemon::worker_manager::WorkerManager;
        use crate::host_control::HostControlHub;
        use crate::model::settings::{Settings, SharedSettings};

        let directory = tempfile::tempdir().unwrap();
        let store = RemoteAccessStateStore::new(directory.path().join(STATE_FILE_NAME));
        let initial = store.load_or_initialize();
        let gate = RemoteAccessGate::new(initial);
        let hub = Arc::new(HostControlHub::new_local());
        let coordinator = Arc::new(RemoteAccessCoordinator::new(
            store,
            gate,
            hub.host_activity(),
        ));
        let registry = PcRegistry::new();
        let settings = actix_web::web::Data::new(SharedSettings::from(Settings::default()));
        let (worker_manager, _worker_rx) = WorkerManager::new(settings, registry.clone());
        let (ipc_tx, _ipc_rx) = tokio::sync::mpsc::unbounded_channel();
        worker_manager.install_active_for_test(ipc_tx).await;
        assert!(coordinator.attach_runtime(
            registry.clone(),
            worker_manager,
            None,
            Arc::downgrade(&hub),
        ));
        registry
            .record_admission("conn-only", Admission::OwnerFull)
            .await;

        let first = coordinator
            .disconnect_connection("conn-only")
            .await
            .unwrap();
        assert!(!first.already_disconnected);
        assert!(registry.admission("conn-only").await.is_none());
        assert!(registry.is_tombstoned("conn-only").await);

        let second = coordinator
            .disconnect_connection("conn-only")
            .await
            .unwrap();
        assert!(second.already_disconnected);

        let _outbound = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let approval_hub = hub.clone();
        let approval = tokio::spawn(async move {
            approval_hub
                .request_approval(
                    crate::host_control::ApprovalRequest {
                        req_id: "pending-only".to_string(),
                        permission_type:
                            crate::model::security_approval::SecurityPermissionType::RemoteControl,
                        from_connection_id: Some("conn-pending".to_string()),
                    },
                    None,
                )
                .await
        });
        for _ in 0..50 {
            if hub.pending_replay_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let pending_only = coordinator
            .disconnect_connection("conn-pending")
            .await
            .unwrap();
        assert!(!pending_only.already_disconnected);
        assert!(!approval.await.unwrap().approved);
    }
}
