use crate::daemon::pc_manager::PcRegistry;
use crate::daemon::session_target::SessionTargetCatalog;
use crate::host_control::HostControlHub;
use crate::model::settings::{Args, SharedSettings};
use actix_web::web;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaReceiver, framed, inprocess},
    message::{
        DesktopTarget, FileTransferPayload, InteractiveRouteAppliedPayload,
        InteractiveRouteCommandPayload, MediaCapabilities, PolicyApplyOutcome,
        SecurityPolicyAppliedPayload, ServiceToWorker, UpdateSecurityPolicyPayload, WorkerIdentity,
        WorkerInitPayload, WorkerKey, WorkerProfile, WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_signal_facade::model::policy_snapshot::PolicySnapshot;
#[cfg(target_os = "linux")]
use desk_utils::linux_display::{LinuxDisplayServer, detect_linux_display_environment};
use desk_wayland_portal::PortalSnapshot;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};

/// Default heartbeat-watchdog grace period when settings don't override
/// it. Worker heartbeats every 5s, so 30s ≈ 6 missed beats — wide
/// enough that transient stalls don't trigger restarts but tight
/// enough that a real hang gets cleared in well under a minute.
const DEFAULT_WORKER_HEARTBEAT_TIMEOUT_SECS: u64 = 30;
/// How often the watchdog re-checks staleness. Independent of the
/// timeout itself — finer granularity costs nothing meaningful and
/// keeps recovery latency bounded.
const WORKER_HEARTBEAT_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Cross-platform hard ceiling for simultaneously resident user-session workers.
/// Platform registration sources must enforce this before spawning a worker.
pub const MAX_RESIDENT_SESSION_WORKERS: usize = 32;
const INTERACTIVE_ROUTE_ACK_TIMEOUT: Duration = Duration::from_secs(1);

/// Identifies one worker the daemon started, so what that worker says can be
/// told apart from what the worker that replaced it says.
///
/// Replacing a worker does not silence it instantly: messages it already put on
/// the wire, and messages its bridge already queued, arrive after the
/// replacement is installed. Without a name on them the daemon reads a dead
/// worker's report as the living one's — an old capability snapshot overwrites
/// the new worker's devices, an old desktop-drift notice restarts a worker that
/// just started, and an old message counts as the replacement's sign of life
/// even if the replacement has never spoken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerIncarnation(u64);

impl std::fmt::Display for WorkerIncarnation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

impl WorkerIncarnation {
    pub fn get(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveInteractiveRoute {
    worker_key: WorkerKey,
    incarnation: WorkerIncarnation,
    route_epoch: u64,
    activated_at: Instant,
    activated_at_unix_ms: u64,
    accepting_interactive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InteractiveRouteAckKey {
    worker_key: WorkerKey,
    incarnation: WorkerIncarnation,
    route_epoch: u64,
    active: bool,
}

/// A message from a worker, carrying which worker sent it.
#[derive(Debug, Clone)]
pub struct WorkerMessage {
    pub incarnation: WorkerIncarnation,
    /// Present for a ServiceDaemon resident-pool worker. The legacy portable
    /// adapter deliberately remains anonymous and single-worker.
    pub worker_key: Option<WorkerKey>,
    pub message: WorkerToService,
}

#[derive(Debug, Clone)]
pub struct ExecWorkerTarget {
    pub worker_key: Option<WorkerKey>,
    pub source_incarnation: WorkerIncarnation,
    pub session_target_id: String,
    pub registration_generation: u64,
    /// Anonymous portable workers predate keyed identities and use zero on the
    /// PTY wire; resident workers echo their daemon-issued incarnation.
    pub wire_worker_incarnation: u64,
}

/// Whether the worker a daemon-side task is working for is still the one the
/// daemon is running.
///
/// The event lane answers this under the manager's lock, because it is
/// refreshing the heartbeat in the same breath. The media and file lanes cannot:
/// media asks once per frame, and taking an async mutex per frame to decide
/// whether to write it would put the whole capture pipeline behind whatever else
/// wants that lock. So they read it here instead, from a value the manager
/// maintains under that same lock.
#[derive(Clone)]
pub(super) struct IncarnationGate {
    mine: WorkerIncarnation,
    worker_key: Option<WorkerKey>,
    current: Arc<AtomicU64>,
    connection_targets: Arc<StdMutex<HashMap<String, desk_ipc_protocol::message::SessionKey>>>,
    active_interactive_routes:
        Arc<StdMutex<HashMap<desk_ipc_protocol::message::SessionKey, ActiveInteractiveRoute>>>,
}

impl IncarnationGate {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::Relaxed) == self.mine.0
    }

    fn owns_session_connection(&self, connection_id: &str) -> bool {
        let Some(key) = self.worker_key.as_ref() else {
            return true;
        };
        self.connection_targets
            .lock()
            .unwrap()
            .get(connection_id)
            .is_some_and(|session| session == &key.session)
    }

    fn owns_interactive_connection(&self, connection_id: &str) -> bool {
        let Some(key) = self.worker_key.as_ref() else {
            return true;
        };
        let Some(session) = self
            .connection_targets
            .lock()
            .unwrap()
            .get(connection_id)
            .cloned()
        else {
            return false;
        };
        self.active_interactive_routes
            .lock()
            .unwrap()
            .get(&session)
            .is_some_and(|route| {
                route.worker_key == *key
                    && route.incarnation == self.mine
                    && route.accepting_interactive
            })
    }

    /// Report the frames or payloads this lane must not deliver, once, when it
    /// first finds itself superseded. A lane can be holding thousands of queued
    /// frames; one line says as much as all of them.
    fn superseded_once(&self, lane: &str, already_logged: &mut bool) {
        if !*already_logged {
            *already_logged = true;
            info!(
                "[{lane}] worker {} has been replaced; dropping what it had queued",
                self.mine
            );
        }
    }
}

/// The daemon end of one worker's event lane. Stamps every message with the
/// worker it came from, which is what lets the reader tell a live report from a
/// late one.
#[derive(Clone)]
pub(super) struct WorkerMessageSink {
    incarnation: WorkerIncarnation,
    worker_key: Option<WorkerKey>,
    current: Arc<AtomicU64>,
    connection_targets: Arc<StdMutex<HashMap<String, desk_ipc_protocol::message::SessionKey>>>,
    active_interactive_routes:
        Arc<StdMutex<HashMap<desk_ipc_protocol::message::SessionKey, ActiveInteractiveRoute>>>,
    tx: mpsc::UnboundedSender<WorkerMessage>,
}

impl WorkerMessageSink {
    pub(super) fn incarnation(&self) -> WorkerIncarnation {
        self.incarnation
    }

    pub(super) fn worker_key(&self) -> Option<&WorkerKey> {
        self.worker_key.as_ref()
    }

    /// A gate for this worker's other lanes, which carry no messages to stamp.
    pub(super) fn gate(&self) -> IncarnationGate {
        IncarnationGate {
            mine: self.incarnation,
            worker_key: self.worker_key.clone(),
            current: Arc::clone(&self.current),
            connection_targets: Arc::clone(&self.connection_targets),
            active_interactive_routes: Arc::clone(&self.active_interactive_routes),
        }
    }

    /// Hand a message to the daemon, stamped with this worker. `false` means
    /// the daemon has stopped reading altogether, which is the bridge's cue to
    /// give up rather than keep draining a worker nobody is listening to.
    #[must_use]
    pub(super) fn send(&self, message: WorkerToService) -> bool {
        self.tx
            .send(WorkerMessage {
                incarnation: self.incarnation,
                worker_key: self.worker_key.clone(),
                message,
            })
            .is_ok()
    }

    #[cfg(test)]
    pub(super) fn for_test(incarnation: u64, tx: mpsc::UnboundedSender<WorkerMessage>) -> Self {
        Self {
            incarnation: WorkerIncarnation(incarnation),
            worker_key: None,
            current: Arc::new(AtomicU64::new(incarnation)),
            connection_targets: Arc::new(StdMutex::new(HashMap::new())),
            active_interactive_routes: Arc::new(StdMutex::new(HashMap::new())),
            tx,
        }
    }
}

#[derive(Clone)]
pub struct WorkerManager {
    settings: web::Data<SharedSettings>,
    inner: Arc<Mutex<WorkerManagerInner>>,
    worker_msg_tx: Arc<mpsc::UnboundedSender<WorkerMessage>>,
    /// Source of [`WorkerIncarnation`]s. Monotonic for the life of the daemon,
    /// so a number is never reused and a late message can never be mistaken for
    /// a message from the worker occupying that slot now.
    next_incarnation: Arc<AtomicU64>,
    /// The worker the daemon most recently started, for readers that cannot take
    /// the manager's lock to ask. Maintained alongside `active_worker`: set when
    /// a worker is minted, cleared to zero when one is taken away and nothing
    /// replaces it. See [`IncarnationGate`].
    current_incarnation: Arc<AtomicU64>,
    /// Per-key lane fences for resident workers. Replacing one key updates only
    /// that key's atomic, so another session's media/file lanes stay current.
    resident_incarnation_gates: Arc<StdMutex<HashMap<WorkerKey, Arc<AtomicU64>>>>,
    /// Daemon-side per-`connection_id` PeerConnection registry.
    /// Held as a clonable handle so the media-pipe receiver task can
    /// look up `video_track`s and call `write_sample` without going back
    /// through `signaling_proxy`. The registry itself is shared with
    /// `signaling_proxy`'s `RouterContext` — they refer to the same
    /// underlying map.
    pc_registry: PcRegistry,
    /// Latest [`MediaCapabilities`] reported by the worker on Init
    /// (`WorkerToService::Capabilities`). Cleared when the worker is
    /// replaced; fresh capabilities arrive from the new worker as part
    /// of its Init handshake. Read by `pc_manager::handle_request_remote`
    /// to populate the daemon's `Init` reply with codec / device data.
    worker_capabilities: Arc<StdMutex<Option<MediaCapabilities>>>,
    wayland_portal_snapshot: Arc<StdMutex<Option<PortalSnapshot>>>,
    #[cfg(target_os = "linux")]
    linux_display_server: Arc<StdMutex<LinuxDisplayServer>>,
    #[cfg(target_os = "linux")]
    session_shell_registry:
        Arc<StdRwLock<Option<crate::host_control::session_shell::SessionShellRegistry>>>,
    session_targets: SessionTargetCatalog,
    session_targeting_enabled: Arc<AtomicBool>,
    /// Immutable connection/task anchor chosen at admission. Every resident
    /// business dispatch resolves through this map; no foreground-session
    /// fallback exists when targeting is enabled.
    connection_targets: Arc<StdMutex<HashMap<String, desk_ipc_protocol::message::SessionKey>>>,
    /// Per-session desktop selected for capture and human input. Session-user
    /// resources intentionally do not consult this map, so UAC can never move
    /// terminal/file/AI work into the Winlogon SYSTEM worker.
    active_interactive_routes:
        Arc<StdMutex<HashMap<desk_ipc_protocol::message::SessionKey, ActiveInteractiveRoute>>>,
    desired_interactive_desktops:
        Arc<StdMutex<HashMap<desk_ipc_protocol::message::SessionKey, DesktopTarget>>>,
    interactive_switch_locks:
        Arc<StdMutex<HashMap<desk_ipc_protocol::message::SessionKey, Arc<Mutex<()>>>>>,
    interactive_route_epochs: Arc<StdMutex<HashMap<desk_ipc_protocol::message::SessionKey, u64>>>,
    interactive_route_acks: Arc<
        StdMutex<HashMap<InteractiveRouteAckKey, oneshot::Sender<InteractiveRouteAppliedPayload>>>,
    >,
    /// Monotonic counter bumped every time [`Self::set_worker_capabilities`]
    /// installs a fresh snapshot. Paired with [`Self::capabilities_version_tx`]
    /// so async callers can wait until the cache reflects a known-newer
    /// snapshot (e.g. `VirtualDisplaySupervisor::ensure_attached` waits
    /// for the post-attach `RefreshCapabilities` round-trip to update
    /// the cache before letting `RequestRemoteAccess` assemble the RemoteAccessInitialized response).
    capabilities_version: Arc<AtomicU64>,
    /// The security policy sequence the worker last confirmed holding. Compared
    /// against what the daemon published to tell a converged worker from one
    /// that is lagging or has asked to be resynchronized.
    policy_applied_seq: Arc<AtomicU64>,
    /// Watch channel mirroring [`Self::capabilities_version`] so awaiters
    /// can use `recv.changed().await` instead of polling. The sender side
    /// is wrapped in `Arc` because `WorkerManager` is `Clone` and
    /// `watch::Sender` is not.
    capabilities_version_tx: Arc<watch::Sender<u64>>,
    /// `true` once [`Self::start_inprocess_worker`] has been called.
    /// Portable / Default mode runs the worker as an `actix_web::rt::spawn`
    /// task in the same process, so the daemon must NOT fall back to
    /// `start_worker` (which spawns an external process via
    /// `CreateProcessAsUserW`) on desktop drift or crash recovery —
    /// in-process mode has nothing to swap to and no SYSTEM token to
    /// launch under. The signaling proxy and crash-recovery paths read
    /// this flag and skip the swap, leaving the existing in-process
    /// worker in place.
    is_inprocess: Arc<AtomicBool>,
    /// Permanent fail-safe latch for this host process. If an in-process worker
    /// does not exit after a cooperative Shutdown within the hard deadline, its
    /// native capture threads may still be alive; no replacement worker may be
    /// started beside it until the application/service process is restarted.
    media_worker_restart_required: Arc<AtomicBool>,
    remote_access_gate: Arc<StdRwLock<crate::daemon::remote_access::RemoteAccessGate>>,
    remote_access_acks: Arc<
        StdMutex<
            HashMap<
                String,
                oneshot::Sender<desk_ipc_protocol::message::RemoteAccessStateAppliedPayload>,
            >,
        >,
    >,
    /// Publications awaiting the worker's confirmation, keyed by operation id.
    policy_acks: Arc<StdMutex<HashMap<String, oneshot::Sender<SecurityPolicyAppliedPayload>>>>,
    application_policy_acks: Arc<
        StdMutex<
            HashMap<
                String,
                oneshot::Sender<desk_ipc_protocol::message::ComputerUseApplicationPolicyPayload>,
            >,
        >,
    >,
}

struct WorkerManagerInner {
    /// Legacy single-worker adapter used by portable/in-process mode and by
    /// the Windows ServiceDaemon until its keyed migration lands.
    active_worker: Option<WorkerHandle>,
    /// Keyed ServiceDaemon workers. Linux registrations enter here; business
    /// traffic is not allowed to use them through the legacy global sender.
    resident_workers: HashMap<WorkerKey, WorkerHandle>,
}

struct WorkerHandle {
    /// Which worker this is. Every message the daemon reads carries the same
    /// value, and only a match means the sender is still the worker in charge.
    incarnation: WorkerIncarnation,
    pipe_name: String,
    ipc_tx: mpsc::UnboundedSender<ServiceToWorker>,
    process_handle: Option<ProcessHandle>,
    /// Last instant the daemon received any IPC message from this
    /// worker (initialised to spawn time). Used by the heartbeat
    /// watchdog — if no heartbeat (or any other message) shows up
    /// within the configured timeout the worker is presumed stuck.
    last_heartbeat_at: Instant,
    capabilities: Option<MediaCapabilities>,
    /// Stored so the heartbeat watchdog can hand them back to
    /// `handle_crash_recovery` when it triggers a restart.
    session_id: u32,
    desktop_name: Option<String>,
    /// Late-publish slot for the daemon→worker file-lane sender.
    ///
    /// Populated:
    /// - in named-pipe mode: by `run_pipe_server` after the worker
    ///   dials in on the dedicated file pipe and the framed sender
    ///   is constructed.
    /// - in in-process mode: by `start_inprocess_worker` immediately
    ///   after constructing the `make_file_inprocess` pair.
    ///
    /// Readers ([`WorkerManager::send_file_to_worker`]) MUST clone the
    /// `Arc` and drop the manager-level guard before awaiting the
    /// nested `RwLock`: a bounded `send().await` on the sender can
    /// pause for SCTP backpressure, and holding `WorkerManagerInner`
    /// across that wait would block worker-recovery /
    /// heartbeat / `send_to_worker` for the duration of the stall.
    file_sender_tx: Arc<RwLock<Option<Arc<dyn EventSender<FileTransferPayload>>>>>,
    inprocess_task: Option<tokio::task::JoinHandle<()>>,
    inprocess_restart: Option<InprocessRestart>,
    /// Daemon-side tasks reading this worker's media and file lanes.
    ///
    /// Held so a replacement can stop them. Only the in-process topology puts
    /// them here: in named-pipe mode the pipe server owns them and aborts them
    /// on its own way out, and reaching in to abort that task instead would skip
    /// the socket files and sender slot it cleans up.
    lane_tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
struct InprocessRestart {
    args: Args,
    host_control_hub: Arc<HostControlHub>,
    computer_use_broker: Arc<crate::worker::agent::computer_use_broker::ComputerUseBroker>,
}

enum ProcessHandle {
    Tokio(tokio::process::Child),
    #[cfg(target_os = "windows")]
    WindowsNative(NativeWindowsChild),
}

impl ProcessHandle {
    async fn kill(&mut self) -> std::io::Result<()> {
        match self {
            ProcessHandle::Tokio(c) => c.kill().await,
            #[cfg(target_os = "windows")]
            ProcessHandle::WindowsNative(h) => h.kill(),
        }
    }

    async fn wait(&mut self) {
        match self {
            ProcessHandle::Tokio(c) => {
                let _ = c.wait().await;
            }
            #[cfg(target_os = "windows")]
            ProcessHandle::WindowsNative(h) => {
                let _ = h.wait().await;
            }
        }
    }
}

#[cfg(target_os = "windows")]
struct NativeWindowsChild {
    handle: usize,
    pid: u32,
}

#[cfg(target_os = "windows")]
unsafe impl Send for NativeWindowsChild {}
#[cfg(target_os = "windows")]
unsafe impl Sync for NativeWindowsChild {}

#[cfg(target_os = "windows")]
impl NativeWindowsChild {
    fn new(handle: windows::Win32::Foundation::HANDLE, pid: u32) -> Self {
        Self {
            handle: handle.0 as usize,
            pid,
        }
    }

    fn raw_handle(&self) -> windows::Win32::Foundation::HANDLE {
        use windows::Win32::Foundation::HANDLE;
        HANDLE(self.handle as *mut std::ffi::c_void)
    }

    fn kill(&self) -> std::io::Result<()> {
        use windows::Win32::System::Threading::TerminateProcess;
        unsafe {
            TerminateProcess(self.raw_handle(), 1)
                .map_err(|e| std::io::Error::other(format!("TerminateProcess: {e}")))
        }
    }

    async fn wait(&self) -> std::io::Result<()> {
        let raw = self.handle;
        tokio::task::spawn_blocking(move || {
            use windows::Win32::{
                Foundation::HANDLE,
                System::Threading::{INFINITE, WaitForSingleObject},
            };
            let h = HANDLE(raw as *mut std::ffi::c_void);
            unsafe { WaitForSingleObject(h, INFINITE) };
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[cfg(target_os = "windows")]
impl Drop for NativeWindowsChild {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.raw_handle());
        }
    }
}

pub type WorkerMessageReceiver = mpsc::UnboundedReceiver<WorkerMessage>;

#[cfg(target_os = "linux")]
fn linux_session_key(
    logical: &crate::host_control::session_shell::LogicalSessionKey,
    generation: u64,
) -> desk_ipc_protocol::message::SessionKey {
    // JSON keeps arbitrary session/seat labels unambiguous without exposing
    // the daemon-issued registration UUID as a platform session identifier.
    let platform_session_id = format!(
        "linux:{}",
        serde_json::to_string(&(logical.uid, &logical.session_id, &logical.seat))
            .expect("serializing string session labels cannot fail")
    );
    desk_ipc_protocol::message::SessionKey {
        platform_session_id,
        session_generation: generation,
    }
}

#[cfg(target_os = "linux")]
fn linux_worker_key(
    registration: &crate::host_control::session_shell::RegisteredSessionShell,
) -> WorkerKey {
    WorkerKey {
        session: linux_session_key(
            &registration.logical_session,
            registration.registration_generation,
        ),
        desktop: DesktopTarget::LinuxSession,
    }
}

#[cfg(target_os = "linux")]
fn linux_session_candidate(
    registration: &crate::host_control::session_shell::RegisteredSessionShell,
) -> crate::daemon::session_target::SessionCandidate {
    let label = registration
        .logical_session
        .session_id
        .as_deref()
        .unwrap_or("unknown");
    crate::daemon::session_target::SessionCandidate {
        session: linux_session_key(
            &registration.logical_session,
            registration.registration_generation,
        ),
        display_name: format!(
            "Linux session {label} (uid {})",
            registration.logical_session.uid
        ),
        session_type: registration.session_type.clone(),
        seat: registration.logical_session.seat.clone(),
        foreground: false,
        // Registration proves the session context, not that a worker has
        // connected and completed its capability handshake.
        remote_desktop_ready: false,
        terminal_ready: false,
        file_ready: false,
        assistant_ready: false,
    }
}

#[cfg(target_os = "linux")]
fn reconcile_linux_session_targets(
    targets: &SessionTargetCatalog,
    registry: &crate::host_control::session_shell::SessionShellRegistry,
) {
    targets.replace_all(
        registry
            .snapshot()
            .iter()
            .map(|registration| linux_session_candidate(registration))
            .collect(),
    );
}

#[cfg(target_os = "linux")]
fn linux_worker_runtime_dirs(
    registration: &crate::host_control::session_shell::RegisteredSessionShell,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    use std::os::unix::ffi::OsStrExt;

    fn environment_path(
        registration: &crate::host_control::session_shell::RegisteredSessionShell,
        name: &[u8],
    ) -> Option<std::path::PathBuf> {
        registration
            .environment
            .iter()
            .find(|(key, _)| key.as_os_str().as_bytes() == name)
            .map(|(_, value)| std::path::PathBuf::from(value))
    }

    let home = environment_path(registration, b"HOME")
        .filter(|path| path.is_absolute())
        .ok_or("registered session has no absolute HOME")?;
    let state_root = environment_path(registration, b"XDG_STATE_HOME")
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"));
    let data_root = environment_path(registration, b"XDG_DATA_HOME")
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    let log_dir = state_root.join("lcxl-remote-desk/logs");
    let data_dir = data_root.join("lcxl-remote-desk");
    Ok((
        log_dir
            .into_os_string()
            .into_string()
            .map_err(|_| "worker log directory is not valid UTF-8")?,
        data_dir
            .into_os_string()
            .into_string()
            .map_err(|_| "worker data directory is not valid UTF-8")?,
    ))
}

impl WorkerManager {
    pub fn new(
        settings: web::Data<SharedSettings>,
        pc_registry: PcRegistry,
    ) -> (Self, WorkerMessageReceiver) {
        let (tx, rx) = mpsc::unbounded_channel::<WorkerMessage>();
        let (cap_version_tx, _cap_version_rx) = watch::channel::<u64>(0);
        let mgr = WorkerManager {
            settings,
            inner: Arc::new(Mutex::new(WorkerManagerInner {
                active_worker: None,
                resident_workers: HashMap::new(),
            })),
            worker_msg_tx: Arc::new(tx),
            next_incarnation: Arc::new(AtomicU64::new(1)),
            current_incarnation: Arc::new(AtomicU64::new(0)),
            resident_incarnation_gates: Arc::new(StdMutex::new(HashMap::new())),
            pc_registry,
            worker_capabilities: Arc::new(StdMutex::new(None)),
            wayland_portal_snapshot: Arc::new(StdMutex::new(None)),
            #[cfg(target_os = "linux")]
            linux_display_server: Arc::new(StdMutex::new(
                detect_linux_display_environment().active_server(),
            )),
            #[cfg(target_os = "linux")]
            session_shell_registry: Arc::new(StdRwLock::new(None)),
            session_targets: SessionTargetCatalog::default(),
            session_targeting_enabled: Arc::new(AtomicBool::new(false)),
            connection_targets: Arc::new(StdMutex::new(HashMap::new())),
            active_interactive_routes: Arc::new(StdMutex::new(HashMap::new())),
            desired_interactive_desktops: Arc::new(StdMutex::new(HashMap::new())),
            interactive_switch_locks: Arc::new(StdMutex::new(HashMap::new())),
            interactive_route_epochs: Arc::new(StdMutex::new(HashMap::new())),
            interactive_route_acks: Arc::new(StdMutex::new(HashMap::new())),
            capabilities_version: Arc::new(AtomicU64::new(0)),
            policy_applied_seq: Arc::new(AtomicU64::new(0)),
            capabilities_version_tx: Arc::new(cap_version_tx),
            is_inprocess: Arc::new(AtomicBool::new(false)),
            media_worker_restart_required: Arc::new(AtomicBool::new(false)),
            remote_access_gate: Arc::new(StdRwLock::new(
                crate::daemon::remote_access::RemoteAccessGate::startup_locked(),
            )),
            remote_access_acks: Arc::new(StdMutex::new(HashMap::new())),
            policy_acks: Arc::new(StdMutex::new(HashMap::new())),
            application_policy_acks: Arc::new(StdMutex::new(HashMap::new())),
        };
        (mgr, rx)
    }

    /// Returns `true` when this manager is driving an in-process (portable
    /// / Default-mode) worker. Set by [`Self::start_inprocess_worker`] and
    /// read by `signaling_proxy` to gate worker-restart actions that are
    /// only meaningful in the daemon-spawned (named-pipe) topology.
    pub fn is_inprocess(&self) -> bool {
        self.is_inprocess.load(Ordering::Relaxed)
    }

    pub fn media_worker_restart_required(&self) -> bool {
        self.media_worker_restart_required.load(Ordering::Acquire)
    }

    /// Name the worker that is about to start, and hand back the sink its
    /// bridge posts to. Called before the worker exists so everything the
    /// worker ever says is already stamped.
    fn mint_worker(&self) -> WorkerMessageSink {
        let incarnation = WorkerIncarnation(self.next_incarnation.fetch_add(1, Ordering::Relaxed));
        // Current from the moment it is named, not from the moment its handle is
        // installed. Its own lanes are spawned in between, and a frame arriving
        // in that window belongs to it.
        self.current_incarnation
            .store(incarnation.0, Ordering::Relaxed);
        WorkerMessageSink {
            incarnation,
            worker_key: None,
            current: Arc::clone(&self.current_incarnation),
            connection_targets: Arc::clone(&self.connection_targets),
            active_interactive_routes: Arc::clone(&self.active_interactive_routes),
            tx: (*self.worker_msg_tx).clone(),
        }
    }

    /// Mint an independently fenced identity for one keyed resident worker.
    /// Unlike the legacy adapter, starting one resident must not supersede any
    /// other session's lanes.
    fn mint_resident_worker(&self, key: WorkerKey) -> WorkerMessageSink {
        let incarnation = WorkerIncarnation(self.next_incarnation.fetch_add(1, Ordering::Relaxed));
        let current = self
            .resident_incarnation_gates
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        current.store(incarnation.0, Ordering::Release);
        WorkerMessageSink {
            incarnation,
            worker_key: Some(key),
            current,
            connection_targets: Arc::clone(&self.connection_targets),
            active_interactive_routes: Arc::clone(&self.active_interactive_routes),
            tx: (*self.worker_msg_tx).clone(),
        }
    }

    fn fence_resident_worker(&self, key: &WorkerKey, incarnation: WorkerIncarnation) {
        if let Some(current) = self.resident_incarnation_gates.lock().unwrap().get(key) {
            let _ = current.compare_exchange(incarnation.0, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    /// Stop the daemon-side tasks reading a departing worker's media and file
    /// lanes, and record that no worker is current.
    ///
    /// Aborted rather than awaited: a lane task can be parked on a write to a
    /// browser that has stopped reading, and worker teardown holds the manager's
    /// lock. Whatever a task manages to deliver between here and its next await
    /// point is refused by its own [`IncarnationGate`], so stopping them is
    /// tidiness rather than the guarantee.
    fn retire_worker(&self, worker: &mut WorkerHandle) {
        self.current_incarnation.store(0, Ordering::Relaxed);
        for task in worker.lane_tasks.drain(..) {
            task.abort();
        }
        if let Some(task) = worker.inprocess_task.take() {
            task.abort();
        }
    }

    fn retire_resident_worker(&self, worker: &mut WorkerHandle) {
        for task in worker.lane_tasks.drain(..) {
            task.abort();
        }
        if let Some(task) = worker.inprocess_task.take() {
            task.abort();
        }
    }

    pub fn bind_remote_access_gate(&self, gate: crate::daemon::remote_access::RemoteAccessGate) {
        *self.remote_access_gate.write().unwrap() = gate;
    }

    #[cfg(target_os = "linux")]
    pub fn bind_session_shell_registry(
        &self,
        registry: crate::host_control::session_shell::SessionShellRegistry,
    ) {
        let mut slot = self.session_shell_registry.write().unwrap();
        if slot.is_some() {
            warn!("session-shell registry is already bound; ignoring duplicate binding");
            return;
        }

        let mut events = registry.subscribe();
        self.session_targeting_enabled
            .store(true, Ordering::Release);
        reconcile_linux_session_targets(&self.session_targets, &registry);
        let initial_registrations = registry.snapshot();
        *slot = Some(registry.clone());
        drop(slot);

        let targets = self.session_targets.clone();
        let mgr = self.clone();
        tokio::spawn(async move {
            for registration in initial_registrations {
                if let Err(error) = mgr.start_linux_resident_worker(registration).await {
                    error!("failed to start registered Linux session worker: {error}");
                }
            }
            loop {
                match events.recv().await {
                    Ok(crate::host_control::session_shell::SessionShellRegistryEvent::Registered(
                        registration,
                    )) => {
                        targets.upsert(linux_session_candidate(&registration));
                        if let Err(error) = mgr.start_linux_resident_worker(registration).await {
                            error!("failed to start registered Linux session worker: {error}");
                        }
                    }
                    Ok(
                        crate::host_control::session_shell::SessionShellRegistryEvent::Disconnected {
                            registration_generation,
                            logical_session,
                            ..
                        },
                    ) => {
                        let session = linux_session_key(&logical_session, registration_generation);
                        targets.remove(&session);
                        for connection_id in mgr.connection_ids_for_session(&session) {
                            crate::daemon::pc_manager::force_disconnect_connection(
                                &mgr.pc_registry,
                                &mgr,
                                None,
                                &connection_id,
                                "desktop-session-disconnected",
                            )
                            .await;
                        }
                        mgr.stop_resident_worker(&WorkerKey {
                            session,
                            desktop: DesktopTarget::LinuxSession,
                        })
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            "session-shell registry event consumer lagged by {skipped}; reconciling from snapshot"
                        );
                        reconcile_linux_session_targets(&targets, &registry);
                        mgr.reconcile_linux_resident_workers(&registry).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn session_shell_registry(
        &self,
    ) -> Option<crate::host_control::session_shell::SessionShellRegistry> {
        self.session_shell_registry.read().unwrap().clone()
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn bind_session_shell_registry_for_test(
        &self,
        registry: crate::host_control::session_shell::SessionShellRegistry,
    ) {
        *self.session_shell_registry.write().unwrap() = Some(registry);
        self.session_targeting_enabled
            .store(true, Ordering::Release);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn session_shell_registration(
        &self,
        session: &desk_ipc_protocol::message::SessionKey,
    ) -> Option<Arc<crate::host_control::session_shell::RegisteredSessionShell>> {
        self.session_shell_registry
            .read()
            .unwrap()
            .as_ref()?
            .snapshot()
            .into_iter()
            .find(|registration| {
                linux_session_key(
                    &registration.logical_session,
                    registration.registration_generation,
                ) == *session
            })
    }

    pub fn session_targets(&self) -> SessionTargetCatalog {
        self.session_targets.clone()
    }

    pub fn uses_session_targeting(&self) -> bool {
        self.session_targeting_enabled.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn enable_session_targeting_for_test(&self) {
        self.session_targeting_enabled
            .store(true, Ordering::Release);
    }

    pub fn resolve_session_target(
        &self,
        capability: crate::daemon::session_target::SessionCapability,
        requested_target_id: Option<&str>,
    ) -> Result<
        Option<desk_ipc_protocol::message::SessionKey>,
        crate::daemon::session_target::SessionTargetSelectionError,
    > {
        if !self.session_targeting_enabled.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.session_targets
            .select(capability, requested_target_id)
            .map(Some)
    }

    /// Resolve an Assistant operation against an already-frozen interactive
    /// connection when one is supplied; otherwise apply the normal 0/1/N target
    /// rule. The host owns both maps, so a central brain never interprets opaque
    /// session ids or silently moves a task to a different login session.
    pub fn resolve_session_target_for_connection(
        &self,
        capability: crate::daemon::session_target::SessionCapability,
        session_connection_id: Option<&str>,
    ) -> Result<
        Option<desk_ipc_protocol::message::SessionKey>,
        crate::daemon::session_target::SessionTargetSelectionError,
    > {
        if !self.session_targeting_enabled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let Some(connection_id) = session_connection_id else {
            return self.resolve_session_target(capability, None);
        };
        let session = self
            .connection_target(connection_id)
            .ok_or(crate::daemon::session_target::SessionTargetSelectionError::Stale)?;
        self.session_targets
            .validate_bound_session(capability, &session)
            .map(Some)
    }

    pub fn bind_connection_target(
        &self,
        connection_id: &str,
        session: &desk_ipc_protocol::message::SessionKey,
    ) -> Result<(), String> {
        let mut bindings = self.connection_targets.lock().unwrap();
        match bindings.get(connection_id) {
            Some(existing) if existing != session => Err(format!(
                "connection {connection_id} is already bound to another session target"
            )),
            Some(_) => Ok(()),
            None => {
                bindings.insert(connection_id.to_string(), session.clone());
                Ok(())
            }
        }
    }

    pub fn clear_connection_target(&self, connection_id: &str) {
        self.connection_targets
            .lock()
            .unwrap()
            .remove(connection_id);
    }

    pub fn connection_target(
        &self,
        connection_id: &str,
    ) -> Option<desk_ipc_protocol::message::SessionKey> {
        self.connection_targets
            .lock()
            .unwrap()
            .get(connection_id)
            .cloned()
    }

    pub fn connection_ids_for_session(
        &self,
        session: &desk_ipc_protocol::message::SessionKey,
    ) -> Vec<String> {
        self.connection_targets
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, bound)| *bound == session)
            .map(|(connection_id, _)| connection_id.clone())
            .collect()
    }

    pub fn resident_worker_owns_connection(&self, key: &WorkerKey, connection_id: &str) -> bool {
        self.connection_target(connection_id)
            .is_some_and(|session| session == key.session)
    }

    #[cfg(target_os = "linux")]
    async fn start_linux_resident_worker(
        &self,
        registration: Arc<crate::host_control::session_shell::RegisteredSessionShell>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let key = linux_worker_key(&registration);
        let mut inner = self.inner.lock().await;
        if inner.resident_workers.contains_key(&key) {
            return Ok(());
        }

        let owner = if unsafe { libc::geteuid() } == 0 {
            Some((
                registration.process_identity.uid,
                registration.process_identity.gid,
            ))
        } else {
            None
        };
        let pipe_name = allocate_worker_socket_path_for(registration.process_identity.uid, owner)?;
        let (ipc_cmd_tx, ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
        let (config_json, ipc_token) = {
            let settings = self.settings.read().await;
            (
                serde_json::to_string(&*settings)
                    .map_err(|error| format!("failed to serialize worker settings: {error}"))?,
                settings.system.tauri_ipc_token.clone(),
            )
        };
        let (log_dir, data_dir) = linux_worker_runtime_dirs(&registration)?;
        let host_upstream_url = format!(
            "ws://127.0.0.1:{}/ws/host_upstream",
            crate::daemon::local_api::SERVICE_API_PORT
        );
        let sink = self.mint_resident_worker(key.clone());
        let incarnation = sink.incarnation();
        let identity = WorkerIdentity {
            key: key.clone(),
            profile: WorkerProfile::SessionUser,
            incarnation: incarnation.0,
        };
        let file_sender_slot: Arc<RwLock<Option<Arc<dyn EventSender<FileTransferPayload>>>>> =
            Arc::new(RwLock::new(None));
        let (transport_ready_tx, transport_ready_rx) = oneshot::channel();

        let mgr = self.clone();
        let pipe_name_for_server = pipe_name.clone();
        let file_sender_for_server = Arc::clone(&file_sender_slot);
        let worker_uid = registration.process_identity.uid;
        let identity_for_server = identity.clone();
        let key_for_server = key.clone();
        let pipe_task = tokio::spawn(async move {
            if let Err(error) = run_pipe_server(
                &pipe_name_for_server,
                worker_uid,
                None,
                config_json,
                log_dir,
                data_dir,
                ipc_cmd_rx,
                sink,
                mgr.clone(),
                host_upstream_url,
                ipc_token,
                mgr.pc_registry.clone(),
                file_sender_for_server,
                Some(identity_for_server),
                owner,
                transport_ready_tx,
            )
            .await
            {
                error!(
                    "resident worker pipe server failed for {:?}: {error}",
                    key_for_server
                );
                mgr.handle_resident_crash_recovery(key_for_server, incarnation);
            }
        });

        match tokio::time::timeout(Duration::from_secs(5), transport_ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                pipe_task.abort();
                cleanup_worker_socket_paths(&pipe_name);
                return Err("resident worker IPC listener exited before readiness".into());
            }
            Err(_) => {
                pipe_task.abort();
                cleanup_worker_socket_paths(&pipe_name);
                return Err("timed out preparing resident worker IPC listeners".into());
            }
        }

        let executable = std::env::current_exe()?;
        let process = match launch_linux_session_worker(&executable, &pipe_name, &registration) {
            Ok(process) => process,
            Err(error) => {
                pipe_task.abort();
                cleanup_worker_socket_paths(&pipe_name);
                return Err(error);
            }
        };
        inner.resident_workers.insert(
            identity.key.clone(),
            WorkerHandle {
                incarnation,
                pipe_name,
                ipc_tx: ipc_cmd_tx,
                process_handle: Some(ProcessHandle::Tokio(process)),
                last_heartbeat_at: Instant::now(),
                capabilities: None,
                session_id: registration.process_identity.uid,
                desktop_name: None,
                file_sender_tx: file_sender_slot,
                inprocess_task: None,
                inprocess_restart: None,
                lane_tasks: Vec::new(),
            },
        );
        info!(
            "resident Linux worker {incarnation} started for {:?}",
            identity.key
        );
        Ok(())
    }

    #[cfg(target_os = "windows")]
    async fn start_windows_resident_worker(
        &self,
        key: WorkerKey,
        session_id: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (desktop_name, profile) = match key.desktop {
            DesktopTarget::WindowsDefault => ("Default", WorkerProfile::SessionUser),
            DesktopTarget::WindowsWinlogon => ("Winlogon", WorkerProfile::RestrictedDesktop),
            _ => return Err("invalid Windows resident desktop target".into()),
        };
        if key.session.platform_session_id != session_id.to_string() {
            return Err("Windows worker key/session id mismatch".into());
        }

        let mut inner = self.inner.lock().await;
        if inner.resident_workers.contains_key(&key) {
            return Ok(());
        }
        let pipe_name = format!("lcxl-desk-ipc-{session_id}-{}", uuid::Uuid::new_v4());
        let (ipc_cmd_tx, ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
        let (config_json, ipc_token, log_dir, data_dir) = {
            let settings = self.settings.read().await;
            (
                serde_json::to_string(&*settings)
                    .map_err(|error| format!("failed to serialize worker settings: {error}"))?,
                settings.system.tauri_ipc_token.clone(),
                settings.paths().log_dir().to_string_lossy().into_owned(),
                settings.paths().data_root().to_string_lossy().into_owned(),
            )
        };
        let host_upstream_url = format!(
            "ws://127.0.0.1:{}/ws/host_upstream",
            crate::daemon::local_api::SERVICE_API_PORT
        );
        let sink = self.mint_resident_worker(key.clone());
        let incarnation = sink.incarnation();
        let identity = WorkerIdentity {
            key: key.clone(),
            profile,
            incarnation: incarnation.0,
        };
        let file_sender_slot: Arc<RwLock<Option<Arc<dyn EventSender<FileTransferPayload>>>>> =
            Arc::new(RwLock::new(None));
        let (transport_ready_tx, transport_ready_rx) = oneshot::channel();

        let mgr = self.clone();
        let pipe_name_for_server = pipe_name.clone();
        let file_sender_for_server = Arc::clone(&file_sender_slot);
        let identity_for_server = identity.clone();
        let key_for_server = key.clone();
        let desktop_for_server = desktop_name.to_string();
        let pipe_task = tokio::spawn(async move {
            if let Err(error) = run_pipe_server(
                &pipe_name_for_server,
                session_id,
                Some(desktop_for_server),
                config_json,
                log_dir,
                data_dir,
                ipc_cmd_rx,
                sink,
                mgr.clone(),
                host_upstream_url,
                ipc_token,
                mgr.pc_registry.clone(),
                file_sender_for_server,
                Some(identity_for_server),
                transport_ready_tx,
            )
            .await
            {
                error!(
                    "resident Windows worker pipe server failed for {:?}: {error}",
                    key_for_server
                );
                mgr.handle_resident_crash_recovery(key_for_server, incarnation);
            }
        });

        match tokio::time::timeout(Duration::from_secs(5), transport_ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                pipe_task.abort();
                return Err("resident Windows worker IPC listener exited before readiness".into());
            }
            Err(_) => {
                pipe_task.abort();
                return Err("timed out preparing resident Windows worker IPC listeners".into());
            }
        }

        let process = match self
            .launch_worker_process(&pipe_name, session_id, Some(desktop_name))
            .await
        {
            Ok(process) => process,
            Err(error) => {
                pipe_task.abort();
                return Err(error);
            }
        };
        inner.resident_workers.insert(
            key.clone(),
            WorkerHandle {
                incarnation,
                pipe_name,
                ipc_tx: ipc_cmd_tx,
                process_handle: Some(process),
                last_heartbeat_at: Instant::now(),
                capabilities: None,
                session_id,
                desktop_name: Some(desktop_name.to_string()),
                file_sender_tx: file_sender_slot,
                inprocess_task: None,
                inprocess_restart: None,
                lane_tasks: Vec::new(),
            },
        );
        info!(
            "resident Windows {:?} worker {incarnation} started for session {session_id}",
            key.desktop
        );
        Ok(())
    }

    async fn stop_resident_worker(&self, key: &WorkerKey) {
        let worker = self.inner.lock().await.resident_workers.remove(key);
        let Some(mut worker) = worker else {
            return;
        };
        self.fence_resident_worker(key, worker.incarnation);
        self.interactive_route_acks
            .lock()
            .unwrap()
            .retain(|ack, _| ack.worker_key != *key || ack.incarnation != worker.incarnation);
        self.revoke_interactive_route(key, worker.incarnation);
        if key.desktop != DesktopTarget::WindowsWinlogon {
            self.session_targets
                .set_readiness(&key.session, false, false, false, false);
        }
        let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
        self.retire_resident_worker(&mut worker);
        if let Some(mut process) = worker.process_handle.take()
            && tokio::time::timeout(Duration::from_secs(3), process.wait())
                .await
                .is_err()
        {
            let _ = process.kill().await;
            process.wait().await;
        }
        let session_still_has_worker = self
            .inner
            .lock()
            .await
            .resident_workers
            .keys()
            .any(|resident_key| resident_key.session == key.session);
        if !session_still_has_worker {
            self.active_interactive_routes
                .lock()
                .unwrap()
                .remove(&key.session);
            self.desired_interactive_desktops
                .lock()
                .unwrap()
                .remove(&key.session);
            self.interactive_switch_locks
                .lock()
                .unwrap()
                .remove(&key.session);
            self.interactive_route_epochs
                .lock()
                .unwrap()
                .remove(&key.session);
        }
    }

    #[cfg(target_os = "linux")]
    async fn reconcile_linux_resident_workers(
        &self,
        registry: &crate::host_control::session_shell::SessionShellRegistry,
    ) {
        let registrations = registry.snapshot();
        let desired: HashMap<_, _> = registrations
            .into_iter()
            .map(|registration| (linux_worker_key(&registration), registration))
            .collect();
        let current: Vec<_> = self
            .inner
            .lock()
            .await
            .resident_workers
            .keys()
            .cloned()
            .collect();

        for key in current {
            if !desired.contains_key(&key) {
                for connection_id in self.connection_ids_for_session(&key.session) {
                    crate::daemon::pc_manager::force_disconnect_connection(
                        &self.pc_registry,
                        self,
                        None,
                        &connection_id,
                        "desktop-session-reconcile-removed",
                    )
                    .await;
                }
                self.stop_resident_worker(&key).await;
            }
        }
        for (key, registration) in desired {
            let already_running = self.inner.lock().await.resident_workers.contains_key(&key);
            if !already_running
                && let Err(error) = self.start_linux_resident_worker(registration).await
            {
                error!(
                    "failed to reconcile Linux resident worker {:?}: {error}",
                    key
                );
            }
        }

        // `replace_all` deliberately resets readiness because a Tauri
        // registration alone does not prove that a worker completed Init. A
        // broadcast-lag reconciliation can nevertheless retain already-live
        // workers; restore their capability-derived readiness so they do not
        // remain falsely unavailable until another capability refresh happens.
        let live_capabilities: Vec<_> = self
            .inner
            .lock()
            .await
            .resident_workers
            .iter()
            .filter_map(|(key, worker)| {
                worker
                    .capabilities
                    .clone()
                    .map(|capabilities| (key.clone(), capabilities))
            })
            .collect();
        for (key, capabilities) in live_capabilities {
            self.set_resident_worker_capabilities(&key, capabilities)
                .await;
        }
    }

    #[cfg(target_os = "windows")]
    pub async fn reconcile_windows_resident_workers(
        &self,
        registrations: Vec<crate::daemon::session_monitor::WindowsSessionRegistration>,
    ) {
        use std::collections::HashSet;

        self.session_targeting_enabled
            .store(true, Ordering::Release);
        let desired_sessions: HashSet<_> = registrations
            .iter()
            .map(|registration| registration.session.clone())
            .collect();
        let current_keys: Vec<_> = self
            .inner
            .lock()
            .await
            .resident_workers
            .keys()
            .filter(|key| {
                matches!(
                    key.desktop,
                    DesktopTarget::WindowsDefault | DesktopTarget::WindowsWinlogon
                )
            })
            .cloned()
            .collect();

        for key in current_keys {
            if !desired_sessions.contains(&key.session) {
                for connection_id in self.connection_ids_for_session(&key.session) {
                    crate::daemon::pc_manager::force_disconnect_connection(
                        &self.pc_registry,
                        self,
                        None,
                        &connection_id,
                        "windows-session-no-longer-schedulable",
                    )
                    .await;
                }
                self.stop_resident_worker(&key).await;
            }
        }

        let mut candidates = Vec::with_capacity(registrations.len());
        for registration in &registrations {
            let default_key = WorkerKey {
                session: registration.session.clone(),
                desktop: DesktopTarget::WindowsDefault,
            };
            let default_capabilities_ready = self
                .resident_worker_capabilities(&default_key)
                .await
                .is_some_and(|caps| {
                    !caps.video_codecs.is_empty()
                        && caps
                            .video_device_list
                            .values()
                            .any(|devices| !devices.is_empty())
                });
            let interactive_route_ready = self
                .active_interactive_routes
                .lock()
                .unwrap()
                .get(&registration.session)
                .is_some_and(|route| route.accepting_interactive);
            candidates.push(crate::daemon::session_target::SessionCandidate {
                session: registration.session.clone(),
                display_name: registration.display_name.clone(),
                session_type: Some("windows".to_string()),
                seat: Some(registration.station_name.clone()),
                foreground: registration.foreground,
                remote_desktop_ready: default_capabilities_ready && interactive_route_ready,
                terminal_ready: default_capabilities_ready,
                file_ready: default_capabilities_ready,
                assistant_ready: default_capabilities_ready,
            });
        }
        self.session_targets.replace_all(candidates);

        for registration in registrations {
            for desktop in [
                DesktopTarget::WindowsDefault,
                DesktopTarget::WindowsWinlogon,
            ] {
                let key = WorkerKey {
                    session: registration.session.clone(),
                    desktop,
                };
                if let Err(error) = self
                    .start_windows_resident_worker(key.clone(), registration.session_id)
                    .await
                {
                    error!(
                        "failed to reconcile Windows resident worker {:?}: {error}",
                        key
                    );
                }
            }
        }
    }

    fn remote_access_state(&self) -> crate::daemon::remote_access::RemoteAccessState {
        self.remote_access_gate.read().unwrap().snapshot()
    }

    pub async fn start_worker(
        &self,
        session_id: u32,
        desktop_name: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Clear stale capabilities. The new worker re-sends them on its
        // own Init handshake; until then the daemon ships an empty
        // device list rather than an old (potentially wrong-desktop)
        // snapshot.
        self.clear_worker_capabilities();
        *self.wayland_portal_snapshot.lock().unwrap() = None;
        #[cfg(target_os = "linux")]
        {
            *self.linux_display_server.lock().unwrap() =
                detect_linux_display_environment().active_server();
        }

        let mut inner = self.inner.lock().await;

        if let Some(mut worker) = inner.active_worker.take() {
            info!("Shutting down existing worker before starting new one");
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            self.retire_worker(&mut worker);
            if let Some(mut proc) = worker.process_handle.take() {
                match tokio::time::timeout(Duration::from_secs(3), proc.wait()).await {
                    Ok(()) => info!("Old worker exited gracefully"),
                    Err(_) => {
                        warn!("Old worker did not exit in time, killing");
                        let _ = proc.kill().await;
                    }
                }
            }
        }

        #[cfg(target_os = "windows")]
        let pipe_name = format!("lcxl-desk-ipc-{}-{}", session_id, uuid::Uuid::new_v4());
        #[cfg(not(target_os = "windows"))]
        let pipe_name = allocate_worker_socket_path(session_id)?;

        let (ipc_cmd_tx, ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();

        let (config_json, ipc_token, log_dir, data_dir) = {
            let settings = self.settings.read().await;
            let json = serde_json::to_string(&*settings)
                .map_err(|e| format!("Failed to serialize settings: {e}"))?;
            let token = settings.system.tauri_ipc_token.clone();
            let log_dir = settings.paths().log_dir().to_string_lossy().into_owned();
            let data_dir = settings.paths().data_root().to_string_lossy().into_owned();
            (json, token, log_dir, data_dir)
        };

        // Daemon-side host-upstream endpoint that the worker's Forwarder hub
        // will dial back into. Loopback is fine — workers run on the same host.
        let host_upstream_url = format!(
            "ws://127.0.0.1:{}/ws/host_upstream",
            crate::daemon::local_api::SERVICE_API_PORT
        );

        let worker_msg_tx = self.mint_worker();
        let incarnation = worker_msg_tx.incarnation();
        let pipe_name_c = pipe_name.clone();
        let desktop_c = desktop_name.clone();
        let config_c = config_json.clone();
        let log_dir_c = log_dir.clone();
        let data_dir_c = data_dir.clone();
        let host_upstream_url_c = host_upstream_url.clone();
        let ipc_token_c = ipc_token.clone();
        let mgr_c = self.clone();
        let pc_registry_c = self.pc_registry.clone();
        // Late-publish slot for the file-lane sender. The pipe-server
        // task writes into this once the worker accepts the dedicated
        // file pipe; the WorkerHandle below holds a clone so DC
        // forwarder lookups via `send_file_to_worker` see the sender as
        // soon as it is ready.
        let file_sender_slot: Arc<RwLock<Option<Arc<dyn EventSender<FileTransferPayload>>>>> =
            Arc::new(RwLock::new(None));
        let file_sender_slot_c = Arc::clone(&file_sender_slot);
        let (transport_ready_tx, transport_ready_rx) = oneshot::channel();
        let pipe_task = tokio::spawn(async move {
            if let Err(e) = run_pipe_server(
                &pipe_name_c,
                session_id,
                desktop_c,
                config_c,
                log_dir_c,
                data_dir_c,
                ipc_cmd_rx,
                worker_msg_tx,
                mgr_c,
                host_upstream_url_c,
                ipc_token_c,
                pc_registry_c,
                file_sender_slot_c,
                None,
                #[cfg(not(target_os = "windows"))]
                None,
                transport_ready_tx,
            )
            .await
            {
                error!("Pipe server error: {e}");
            }
        });

        match tokio::time::timeout(Duration::from_secs(5), transport_ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                pipe_task.abort();
                #[cfg(not(target_os = "windows"))]
                cleanup_worker_socket_paths(&pipe_name);
                return Err("worker IPC listener task exited before publishing readiness".into());
            }
            Err(_) => {
                pipe_task.abort();
                #[cfg(not(target_os = "windows"))]
                cleanup_worker_socket_paths(&pipe_name);
                return Err("timed out preparing worker IPC listeners".into());
            }
        }

        let process = match self
            .launch_worker_process(&pipe_name, session_id, desktop_name.as_deref())
            .await
        {
            Ok(process) => process,
            Err(error) => {
                pipe_task.abort();
                #[cfg(not(target_os = "windows"))]
                cleanup_worker_socket_paths(&pipe_name);
                return Err(error);
            }
        };

        inner.active_worker = Some(WorkerHandle {
            incarnation,
            pipe_name,
            ipc_tx: ipc_cmd_tx,
            process_handle: Some(process),
            last_heartbeat_at: Instant::now(),
            capabilities: None,
            session_id,
            desktop_name: desktop_name.clone(),
            file_sender_tx: file_sender_slot,
            inprocess_task: None,
            inprocess_restart: None,
            lane_tasks: Vec::new(),
        });

        info!("Worker {incarnation} started for session {session_id}");
        Ok(())
    }

    /// In-process variant of [`Self::start_worker`] used by portable
    /// mode. Skips `CreateProcessAsUserW` and the named-pipe handshake;
    /// instead constructs in-process tokio mpsc transports
    /// ([`inprocess::make_event`] + [`inprocess::make_media`]) and spawns
    /// the worker as an `actix_web::rt::spawn` task in the same process.
    /// The worker shares the daemon's `Arc<HostControlHub>` directly — no
    /// upstream ws bridge.
    ///
    /// Per-connection accept-state preservation across worker restarts
    /// (relevant in named-pipe daemon mode for UAC / lock-screen swaps)
    /// is intentionally absent here: portable mode does not switch
    /// workers on desktop drift (it can't — single process owns the
    /// capture session), so there is nothing to forward.
    pub async fn start_inprocess_worker(
        &self,
        args: Args,
        session_id: u32,
        desktop_name: Option<String>,
        host_control_hub: Arc<HostControlHub>,
        computer_use_broker: Arc<crate::worker::agent::computer_use_broker::ComputerUseBroker>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.media_worker_restart_required() {
            return Err(
                "media worker could not be stopped safely; host process restart required".into(),
            );
        }
        // Latch the in-process flag so `signaling_proxy::DesktopChanged`
        // and `handle_crash_recovery` skip their swap-to-fresh-worker
        // branches. Once set this manager remains in in-process mode
        // for the rest of its lifetime — switching topologies mid-run
        // is not a supported configuration.
        self.is_inprocess.store(true, Ordering::Relaxed);

        // Mirror start_worker: a fresh worker re-reports capabilities on its
        // own; clearing the cached snapshot avoids handing stale device data
        // to a `RequestRemoteAccess` that lands between Init and the worker's
        // first `Capabilities` emission.
        self.clear_worker_capabilities();
        *self.wayland_portal_snapshot.lock().unwrap() = None;
        #[cfg(target_os = "linux")]
        {
            *self.linux_display_server.lock().unwrap() =
                detect_linux_display_environment().active_server();
        }

        let mut inner = self.inner.lock().await;

        if let Some(mut worker) = inner.active_worker.take() {
            info!("Shutting down existing in-process worker before starting a new one");
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            self.retire_worker(&mut worker);
            computer_use_broker.reset_worker_incarnation();
        }

        let pipe_name = format!("inprocess-{session_id}-{}", uuid::Uuid::new_v4());
        let (ipc_cmd_tx, mut ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();

        let (config_json, log_dir, data_dir) = {
            let s = self.settings.read().await;
            let json = serde_json::to_string(&*s)
                .map_err(|e| format!("Failed to serialize settings: {e}"))?;
            let log_dir = s.paths().log_dir().to_string_lossy().into_owned();
            let data_dir = s.paths().data_root().to_string_lossy().into_owned();
            (json, log_dir, data_dir)
        };

        let remote_access_state = self.remote_access_state();
        let init_payload = WorkerInitPayload {
            worker_identity: None,
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name: desktop_name.clone(),
            config_json,
            log_dir: Some(log_dir),
            data_dir: Some(data_dir),
            signaling_url: None,
            // No upstream WS — the worker shares the daemon's hub via
            // the `shared_hub` parameter to `run_with_transports`.
            auth_token: None,
            host_upstream_url: None,
            // Media transport is in-process below; no named pipe needed.
            media_pipe_name: None,
            // File transport is also in-process: the file pair is
            // handed directly to `run_with_transports`, no pipe name.
            // The worker's named-pipe-mode `file_pipe_name == None`
            // fail-fast does not run on this path because
            // `run_with_transports` is invoked directly (no `ipc_loop`
            // handshake which is where that check lives).
            file_pipe_name: None,
            remote_access_locked: remote_access_state.is_locked(),
            remote_access_state_version: remote_access_state.state_version,
        };

        // Build the four in-process transports:
        // - bidirectional event pair (daemon ↔ worker)
        // - uni-directional media (worker → daemon)
        // - bidirectional file pair (daemon ↔ worker), bounded at
        //   `FILE_QUEUE_CAP = 32` per direction so SCTP backpressure
        //   propagates end-to-end through the file lane without
        //   spilling into the event lane.
        let (s2w_tx, s2w_rx) = inprocess::make_event::<ServiceToWorker>();
        let (w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
        let (media_tx, media_rx) = inprocess::make_media();
        // daemon → worker: daemon emits, worker drains in its file
        // dispatcher loop.
        let (file_d2w_tx, file_d2w_rx) = inprocess::make_file_inprocess::<FileTransferPayload>();
        // worker → daemon: worker dispatcher emits, daemon drains
        // straight into `pc_manager::write_file_transfer_data`.
        let (file_w2d_tx, mut file_w2d_rx) =
            inprocess::make_file_inprocess::<FileTransferPayload>();

        // Spawn the daemon-side bridge: drains `ipc_cmd_rx` → daemon
        // EventSender (worker observes via its EventReceiver), and
        // worker EventReceiver → `worker_msg_tx` (signaling_proxy
        // observes via its drain loop). Reuses `bridge_event_transport`
        // so the in-process and named-pipe paths share the
        // shutdown / closed bookkeeping.
        let pipe_name_for_bridge = pipe_name.clone();
        let worker_msg_tx = self.mint_worker();
        let incarnation = worker_msg_tx.incarnation();
        let lane_gate = worker_msg_tx.gate();
        actix_web::rt::spawn(async move {
            let _ = bridge_event_transport(
                w2s_rx,
                s2w_tx,
                &mut ipc_cmd_rx,
                &worker_msg_tx,
                &pipe_name_for_bridge,
            )
            .await;
        });

        // Daemon-side media receiver: identical to the named-pipe path
        // except the receiver is in-process (no decode work).
        let media_handle =
            spawn_media_receiver_task(media_rx, self.pc_registry.clone(), lane_gate.clone());

        // Daemon-side file-lane drain task: each worker → daemon
        // payload feeds into `pc_manager::write_file_transfer_data`,
        // which routes by `connection_id` to the matching browser DC.
        // Serial single-task drain accepts cross-connection HOL as a
        // known trade-off (see `dual_transport.rs` module docs).
        let file_drain_handle = {
            let pc_registry = self.pc_registry.clone();
            let gate = lane_gate.clone();
            tokio::spawn(async move {
                info!("[worker_manager] in-process file-lane drain starting");
                let mut superseded = false;
                while let Some(payload) = file_w2d_rx.recv().await {
                    if !gate.is_current() {
                        gate.superseded_once("worker_manager", &mut superseded);
                        continue;
                    }
                    crate::daemon::pc_manager::write_file_transfer_data(&pc_registry, payload)
                        .await;
                }
                info!("[worker_manager] in-process file-lane drain exiting (closed)");
            })
        };

        // Spawn the worker on `actix_web::rt::spawn` because
        // `WorkerSession::run_with_transports` awaits actix-web internals
        // (`DeskSession`, `awc::Client`, `actix_web::rt::spawn` from
        // signaling handlers) which all require a `LocalSet` context.
        // `tokio::spawn` would fail with "spawn_local called from
        // outside of a `task::LocalSet`".
        let restart = InprocessRestart {
            args: args.clone(),
            host_control_hub: host_control_hub.clone(),
            computer_use_broker: computer_use_broker.clone(),
        };
        let init_for_worker = init_payload;
        let hub = host_control_hub;
        let inprocess_task = actix_web::rt::spawn(async move {
            let session = crate::worker::session::WorkerSession::new();
            if let Err(e) = session
                .run_with_transports(
                    init_for_worker,
                    s2w_rx,
                    w2s_tx,
                    Some(media_tx),
                    file_w2d_tx,
                    file_d2w_rx,
                    Some(hub),
                    Some(computer_use_broker),
                )
                .await
            {
                error!("In-process worker exited with error: {e}");
            }
            info!("In-process worker task exited");
        });

        // Pre-populate the file_sender slot for in-process mode: there
        // is no async accept step, so the daemon→worker file sender is
        // ready the instant we hand it to the worker above.
        let file_sender_slot: Arc<RwLock<Option<Arc<dyn EventSender<FileTransferPayload>>>>> =
            Arc::new(RwLock::new(Some(file_d2w_tx)));

        inner.active_worker = Some(WorkerHandle {
            incarnation,
            pipe_name,
            ipc_tx: ipc_cmd_tx,
            // No OS process to track in in-process mode. The worker task
            // is owned by the actix-rt System and will be cancelled when
            // the System shuts down; we don't track its JoinHandle on the
            // handle struct because the watchdog / restart paths key off
            // `ipc_tx` alive-ness, not process state.
            process_handle: None,
            last_heartbeat_at: Instant::now(),
            capabilities: None,
            session_id,
            desktop_name,
            file_sender_tx: file_sender_slot,
            inprocess_task: Some(inprocess_task),
            inprocess_restart: Some(restart),
            lane_tasks: vec![media_handle, file_drain_handle],
        });

        info!("In-process worker {incarnation} started for session {session_id}");
        Ok(())
    }

    /// Stash the worker's last reported [`MediaCapabilities`]. Called
    /// from `signaling_proxy` whenever the worker emits
    /// `WorkerToService::Capabilities`. Subsequent `RequestRemoteAccess`
    /// handling uses the snapshot to populate the RemoteAccessInitialized response.
    ///
    /// Bumps [`Self::capabilities_version`] and notifies the watch
    /// channel so awaiters (e.g. `VirtualDisplaySupervisor::ensure_attached`)
    /// see the freshly installed cache. The cache write happens-before
    /// the version bump, so any reader observing the new version is
    /// guaranteed to read the new snapshot.
    pub fn set_worker_capabilities(&self, caps: MediaCapabilities) {
        *self.worker_capabilities.lock().unwrap() = Some(caps);
        let new_version = self.capabilities_version.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.capabilities_version_tx.send_replace(new_version);
    }

    /// Install the readiness snapshot for one resident worker. It is kept out
    /// of the legacy global cache so an unrelated session can never change the
    /// codecs or devices used by an already-bound PC.
    pub async fn set_resident_worker_capabilities(
        &self,
        key: &WorkerKey,
        caps: MediaCapabilities,
    ) -> bool {
        let remote_desktop_ready = !caps.video_codecs.is_empty()
            && caps
                .video_device_list
                .values()
                .any(|devices| !devices.is_empty());
        let incarnation = {
            let mut inner = self.inner.lock().await;
            let Some(worker) = inner.resident_workers.get_mut(key) else {
                return false;
            };
            worker.capabilities = Some(caps);
            worker.incarnation
        };
        let route_is_current = self
            .active_interactive_routes
            .lock()
            .unwrap()
            .get(&key.session)
            .is_some_and(|route| {
                route.worker_key == *key
                    && route.incarnation == incarnation
                    && route.accepting_interactive
            });
        let updated = if key.desktop == DesktopTarget::WindowsWinlogon {
            // The secure-desktop worker contributes only warm interactive
            // readiness. It must never make session-user capabilities appear
            // available and it never replaces the Default worker's catalog
            // snapshot.
            true
        } else {
            self.session_targets.set_readiness(
                &key.session,
                remote_desktop_ready && route_is_current,
                true,
                true,
                true,
            )
        };
        let desired = self
            .desired_interactive_desktops
            .lock()
            .unwrap()
            .get(&key.session)
            .copied()
            .or_else(|| {
                matches!(
                    key.desktop,
                    DesktopTarget::LinuxSession | DesktopTarget::WindowsDefault
                )
                .then_some(key.desktop)
            });
        if remote_desktop_ready && desired == Some(key.desktop) && !route_is_current {
            // Do not await the worker acknowledgement from signaling_proxy's
            // message loop: that same loop must remain free to receive the ack.
            let manager = self.clone();
            let key = key.clone();
            tokio::spawn(async move {
                match manager.activate_interactive_worker(&key).await {
                    Ok(epoch) => {
                        let connection_ids = manager.connection_ids_for_session(&key.session);
                        if !connection_ids.is_empty() {
                            manager
                                .pc_registry
                                .pause_media_for_connections(&connection_ids, epoch)
                                .await;
                            manager
                                .pc_registry
                                .resume_media_for_connections(&manager, None, &connection_ids)
                                .await;
                        }
                    }
                    Err(error) => warn!(
                        "resident worker {:?} could not become interactive: {}",
                        key, error
                    ),
                }
            });
        }
        updated
    }

    async fn request_interactive_route_state(
        &self,
        key: &WorkerKey,
        incarnation: WorkerIncarnation,
        route_epoch: u64,
        active: bool,
    ) -> Result<InteractiveRouteAppliedPayload, String> {
        let sender = {
            let inner = self.inner.lock().await;
            inner
                .resident_workers
                .get(key)
                .filter(|worker| worker.incarnation == incarnation)
                .map(|worker| worker.ipc_tx.clone())
                .ok_or_else(|| format!("resident worker {key:?} was replaced before route apply"))?
        };
        let ack_key = InteractiveRouteAckKey {
            worker_key: key.clone(),
            incarnation,
            route_epoch,
            active,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.interactive_route_acks.lock().unwrap();
            if pending.contains_key(&ack_key) {
                return Err(format!("duplicate interactive route operation {ack_key:?}"));
            }
            pending.insert(ack_key.clone(), tx);
        }
        if let Err(error) = sender.send(ServiceToWorker::SetInteractiveRoute(
            InteractiveRouteCommandPayload {
                route_epoch,
                active,
            },
        )) {
            self.interactive_route_acks.lock().unwrap().remove(&ack_key);
            return Err(format!("failed to send interactive route command: {error}"));
        }
        match tokio::time::timeout(INTERACTIVE_ROUTE_ACK_TIMEOUT, rx).await {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(_)) => Err(format!(
                "interactive route acknowledgement sender dropped for {ack_key:?}"
            )),
            Err(_) => {
                self.interactive_route_acks.lock().unwrap().remove(&ack_key);
                Err(format!(
                    "interactive route acknowledgement timed out for {ack_key:?}"
                ))
            }
        }
    }

    pub fn complete_interactive_route_ack(
        &self,
        key: &WorkerKey,
        incarnation: WorkerIncarnation,
        payload: InteractiveRouteAppliedPayload,
    ) -> bool {
        let ack_key = InteractiveRouteAckKey {
            worker_key: key.clone(),
            incarnation,
            route_epoch: payload.route_epoch,
            active: payload.active,
        };
        self.interactive_route_acks
            .lock()
            .unwrap()
            .remove(&ack_key)
            .is_some_and(|sender| sender.send(payload).is_ok())
    }

    /// Publish one resident slot as the current capture/human-input route for
    /// its session only after that exact worker incarnation acknowledges local
    /// activation. The route and incarnation are then changed in one mutex
    /// critical section, so media drains can never observe a new key with an
    /// old epoch.
    pub async fn activate_interactive_worker(&self, key: &WorkerKey) -> Result<u64, String> {
        let switch_lock = self
            .interactive_switch_locks
            .lock()
            .unwrap()
            .entry(key.session.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _switch_guard = switch_lock.lock().await;
        let existing_route = self
            .active_interactive_routes
            .lock()
            .unwrap()
            .get(&key.session)
            .filter(|route| route.worker_key == *key && route.accepting_interactive)
            .cloned();
        if let Some(route) = existing_route {
            let incarnation_is_current = self
                .inner
                .lock()
                .await
                .resident_workers
                .get(key)
                .is_some_and(|worker| worker.incarnation == route.incarnation);
            if incarnation_is_current {
                return Ok(route.route_epoch);
            }
        }
        if let Some(desired) = self
            .desired_interactive_desktops
            .lock()
            .unwrap()
            .get(&key.session)
            .copied()
            && desired != key.desktop
        {
            return Err(format!(
                "interactive activation for {:?} was superseded by desired desktop {:?}",
                key.desktop, desired
            ));
        }
        let route_epoch = self.allocate_interactive_route_epoch(&key.session);
        self.activate_interactive_worker_at_epoch_locked(key, route_epoch)
            .await
    }

    fn allocate_interactive_route_epoch(
        &self,
        session: &desk_ipc_protocol::message::SessionKey,
    ) -> u64 {
        let mut epochs = self.interactive_route_epochs.lock().unwrap();
        let next = epochs.get(session).copied().unwrap_or(0).saturating_add(1);
        epochs.insert(session.clone(), next);
        next
    }

    async fn activate_interactive_worker_at_epoch_locked(
        &self,
        key: &WorkerKey,
        route_epoch: u64,
    ) -> Result<u64, String> {
        let incarnation = {
            let inner = self.inner.lock().await;
            let worker = inner
                .resident_workers
                .get(key)
                .ok_or_else(|| format!("resident worker {key:?} is unavailable"))?;
            let capabilities = worker
                .capabilities
                .as_ref()
                .ok_or_else(|| format!("resident worker {key:?} is not ready"))?;
            if capabilities.video_codecs.is_empty()
                || !capabilities
                    .video_device_list
                    .values()
                    .any(|devices| !devices.is_empty())
            {
                return Err(format!("resident worker {key:?} has no video capability"));
            }
            worker.incarnation
        };
        self.request_interactive_route_state(key, incarnation, route_epoch, true)
            .await?;
        {
            let inner = self.inner.lock().await;
            if !inner
                .resident_workers
                .get(key)
                .is_some_and(|worker| worker.incarnation == incarnation)
            {
                return Err(format!(
                    "resident worker {key:?} was replaced after activation acknowledgement"
                ));
            }
        }
        let mut routes = self.active_interactive_routes.lock().unwrap();
        routes.insert(
            key.session.clone(),
            ActiveInteractiveRoute {
                worker_key: key.clone(),
                incarnation,
                route_epoch,
                activated_at: Instant::now(),
                activated_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                accepting_interactive: true,
            },
        );
        drop(routes);
        self.session_targets
            .set_remote_desktop_readiness(&key.session, true);
        self.desired_interactive_desktops
            .lock()
            .unwrap()
            .insert(key.session.clone(), key.desktop);
        info!(
            "activated resident interactive route {:?} incarnation {} epoch {}",
            key, incarnation, route_epoch
        );
        Ok(route_epoch)
    }

    /// Windows desktop transition without replacing either resident process.
    /// Old media is stopped best-effort while its route is still authoritative;
    /// publication of the new key then fences every queued old frame before
    /// cached media is restarted on the warm target.
    pub async fn switch_interactive_desktop(
        &self,
        session: &desk_ipc_protocol::message::SessionKey,
        desktop_name: &str,
    ) -> Result<u64, String> {
        let switch_started = Instant::now();
        let switch_lock = self
            .interactive_switch_locks
            .lock()
            .unwrap()
            .entry(session.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _switch_guard = switch_lock.lock().await;
        let desktop = match desktop_name {
            "Default" => DesktopTarget::WindowsDefault,
            "Winlogon" => DesktopTarget::WindowsWinlogon,
            other => {
                return Err(format!(
                    "unknown Windows input desktop {other:?}; retaining the current route"
                ));
            }
        };
        let key = WorkerKey {
            session: session.clone(),
            desktop,
        };
        {
            let inner = self.inner.lock().await;
            let worker = inner
                .resident_workers
                .get(&key)
                .ok_or_else(|| format!("warm desktop worker {key:?} is unavailable"))?;
            if worker.capabilities.is_none() {
                return Err(format!("warm desktop worker {key:?} is not ready"));
            }
        }
        let current_epoch = self
            .active_interactive_routes
            .lock()
            .unwrap()
            .get(session)
            .filter(|route| route.worker_key == key && route.accepting_interactive)
            .map(|route| route.route_epoch);
        if let Some(epoch) = current_epoch {
            return Ok(epoch);
        }

        // Record the observed input desktop before fencing the old route. If
        // activation fails, a later capability refresh may retry only this
        // target; it must never reopen the desktop we just observed leaving.
        self.desired_interactive_desktops
            .lock()
            .unwrap()
            .insert(session.clone(), desktop);
        let old_route = {
            let mut routes = self.active_interactive_routes.lock().unwrap();
            let route = routes
                .get_mut(session)
                .ok_or_else(|| format!("session {session:?} has no current interactive route"))?;
            route.accepting_interactive = false;
            route.clone()
        };
        self.session_targets
            .set_remote_desktop_readiness(session, false);
        let next_epoch = self.allocate_interactive_route_epoch(session);
        let connection_ids = self.connection_ids_for_session(session);
        self.pc_registry
            .pause_media_for_connections(&connection_ids, next_epoch)
            .await;
        if let Err(error) = self
            .request_interactive_route_state(
                &old_route.worker_key,
                old_route.incarnation,
                next_epoch,
                false,
            )
            .await
        {
            warn!(
                "old interactive worker {:?} did not acknowledge deactivation: {}; route remains fenced",
                old_route.worker_key, error
            );
        } else {
            info!(
                "resident_switch stage=deactivate_applied session={:?} route_epoch={} elapsed_ms={}",
                session,
                next_epoch,
                switch_started.elapsed().as_millis()
            );
        }
        let epoch = self
            .activate_interactive_worker_at_epoch_locked(&key, next_epoch)
            .await?;
        info!(
            "resident_switch stage=activate_applied session={:?} desktop={:?} route_epoch={} elapsed_ms={}",
            session,
            key.desktop,
            epoch,
            switch_started.elapsed().as_millis()
        );
        self.pc_registry
            .resume_media_for_connections(self, None, &connection_ids)
            .await;
        info!(
            "resident_switch stage=media_replayed session={:?} route_epoch={} elapsed_ms={}",
            session,
            epoch,
            switch_started.elapsed().as_millis()
        );
        Ok(epoch)
    }

    fn revoke_interactive_route(&self, key: &WorkerKey, incarnation: WorkerIncarnation) {
        let mut routes = self.active_interactive_routes.lock().unwrap();
        if routes
            .get(&key.session)
            .is_some_and(|route| route.worker_key == *key && route.incarnation == incarnation)
        {
            routes.remove(&key.session);
        }
    }

    pub fn resident_worker_is_active_interactive(
        &self,
        key: &WorkerKey,
        incarnation: WorkerIncarnation,
    ) -> bool {
        self.active_interactive_routes
            .lock()
            .unwrap()
            .get(&key.session)
            .is_some_and(|route| {
                route.worker_key == *key
                    && route.incarnation == incarnation
                    && route.accepting_interactive
                    && route.activated_at <= Instant::now()
            })
    }

    pub fn resident_desktop_observation_is_current(
        &self,
        key: &WorkerKey,
        incarnation: WorkerIncarnation,
        observed_at_unix_ms: u64,
    ) -> bool {
        self.active_interactive_routes
            .lock()
            .unwrap()
            .get(&key.session)
            .is_some_and(|route| {
                route.worker_key == *key
                    && route.incarnation == incarnation
                    && route.accepting_interactive
                    && observed_at_unix_ms >= route.activated_at_unix_ms
            })
    }

    pub async fn resident_worker_capabilities(&self, key: &WorkerKey) -> Option<MediaCapabilities> {
        self.inner
            .lock()
            .await
            .resident_workers
            .get(key)
            .and_then(|worker| worker.capabilities.clone())
    }

    pub async fn session_worker_capabilities(
        &self,
        session: &desk_ipc_protocol::message::SessionKey,
    ) -> Option<MediaCapabilities> {
        let inner = self.inner.lock().await;
        [DesktopTarget::LinuxSession, DesktopTarget::WindowsDefault]
            .into_iter()
            .map(|desktop| WorkerKey {
                session: session.clone(),
                desktop,
            })
            .find_map(|key| {
                inner
                    .resident_workers
                    .get(&key)
                    .and_then(|worker| worker.capabilities.clone())
            })
    }

    fn clear_worker_capabilities(&self) {
        *self.worker_capabilities.lock().unwrap() = None;
        let new_version = self.capabilities_version.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.capabilities_version_tx.send_replace(new_version);
    }

    /// Subscribe before reading so a capability publication between those two
    /// operations cannot be missed. A clear notification wakes the loop but is
    /// never returned as a usable snapshot.
    pub async fn wait_current_worker_capabilities(
        &self,
        timeout: Duration,
    ) -> Option<MediaCapabilities> {
        let mut versions = self.subscribe_capabilities_version();
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(capabilities) = self.worker_capabilities() {
                    return Some(capabilities);
                }
                if versions.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Take a snapshot of the latest reported worker capabilities.
    /// Returns `None` until the current worker incarnation has sent
    /// `Capabilities`; remote-desktop admission must use
    /// [`Self::wait_current_worker_capabilities`] instead of treating that
    /// state as an empty capability set.
    pub fn worker_capabilities(&self) -> Option<MediaCapabilities> {
        self.worker_capabilities.lock().unwrap().clone()
    }

    pub fn set_wayland_portal_snapshot(&self, snapshot: PortalSnapshot) {
        *self.wayland_portal_snapshot.lock().unwrap() = Some(snapshot);
        #[cfg(target_os = "linux")]
        {
            *self.linux_display_server.lock().unwrap() = LinuxDisplayServer::Wayland;
        }
    }

    pub fn wayland_portal_snapshot(&self) -> Option<PortalSnapshot> {
        self.wayland_portal_snapshot.lock().unwrap().clone()
    }

    #[cfg(target_os = "linux")]
    pub fn linux_display_server(&self) -> LinuxDisplayServer {
        *self.linux_display_server.lock().unwrap()
    }

    /// Snapshot of the monotonic counter bumped by every
    /// [`Self::set_worker_capabilities`] call. Starts at 0 before any
    /// capabilities have been installed.
    pub fn capabilities_version(&self) -> u64 {
        self.capabilities_version.load(Ordering::SeqCst)
    }

    /// Receiver for the capabilities-version watch channel. Each
    /// `set_worker_capabilities` call triggers a `recv.changed()`
    /// wake-up. Use `borrow_and_update()` to read the latest version
    /// without missing further updates.
    pub fn subscribe_capabilities_version(&self) -> watch::Receiver<u64> {
        self.capabilities_version_tx.subscribe()
    }

    /// Returns `true` when the latest cached `MediaCapabilities` has a
    /// `DisplayInfo.device_name` equal to `display_name` in any of its
    /// per-backend buckets. Used by
    /// `VirtualDisplaySupervisor::ensure_attached` to confirm that the
    /// post-attach capabilities round-trip has actually surfaced the
    /// newly attached IDD before signalling completion. Note that
    /// `video_device_list` is a `BTreeMap<backend_name, Vec<DisplayInfo>>`
    /// — the map key is the backend ("dxgi" / "wgc" / ...), not the
    /// display name itself.
    pub fn capabilities_contains_display(&self, display_name: &str) -> bool {
        self.worker_capabilities
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| {
                c.video_device_list
                    .values()
                    .flatten()
                    .any(|d| d.device_name == display_name)
            })
            .unwrap_or(false)
    }

    /// Test-only: install an `ipc_tx` so `send_to_worker` has a
    /// destination without going through `start_worker` /
    /// `start_inprocess_worker`. Used by routing tests that need to
    /// observe the IPC the daemon sends without standing up a real
    /// worker process.
    #[cfg(test)]
    pub async fn install_active_for_test(
        &self,
        ipc_tx: mpsc::UnboundedSender<ServiceToWorker>,
    ) -> WorkerIncarnation {
        let incarnation = self.mint_worker().incarnation();
        self.clear_worker_capabilities();
        *self.wayland_portal_snapshot.lock().unwrap() = None;
        #[cfg(target_os = "linux")]
        {
            *self.linux_display_server.lock().unwrap() =
                detect_linux_display_environment().active_server();
        }
        let mut inner = self.inner.lock().await;
        inner.active_worker = Some(WorkerHandle {
            incarnation,
            pipe_name: "test".to_string(),
            ipc_tx,
            process_handle: None,
            last_heartbeat_at: Instant::now(),
            capabilities: None,
            session_id: 0,
            desktop_name: None,
            file_sender_tx: Arc::new(RwLock::new(None)),
            inprocess_task: None,
            inprocess_restart: None,
            lane_tasks: Vec::new(),
        });
        incarnation
    }

    #[cfg(test)]
    pub async fn install_resident_for_test(
        &self,
        key: WorkerKey,
        ipc_tx: mpsc::UnboundedSender<ServiceToWorker>,
    ) -> WorkerIncarnation {
        let incarnation = self.mint_resident_worker(key.clone()).incarnation();
        self.inner.lock().await.resident_workers.insert(
            key,
            WorkerHandle {
                incarnation,
                pipe_name: "resident-test".to_string(),
                ipc_tx,
                process_handle: None,
                last_heartbeat_at: Instant::now(),
                capabilities: None,
                session_id: 0,
                desktop_name: None,
                file_sender_tx: Arc::new(RwLock::new(None)),
                inprocess_task: None,
                inprocess_restart: None,
                lane_tasks: Vec::new(),
            },
        );
        incarnation
    }

    pub fn complete_remote_access_ack(
        &self,
        payload: desk_ipc_protocol::message::RemoteAccessStateAppliedPayload,
    ) {
        if let Some(waiter) = self
            .remote_access_acks
            .lock()
            .unwrap()
            .remove(&payload.operation_id)
        {
            let _ = waiter.send(payload);
        }
    }

    pub async fn apply_remote_access_state(
        &self,
        payload: desk_ipc_protocol::message::RemoteAccessStatePayload,
        timeout: Duration,
    ) -> Result<bool, String> {
        let destinations = self.worker_destinations().await;
        if destinations.is_empty() {
            return Ok(false);
        }
        let state_version = payload.state_version;
        let mut waiters = Vec::with_capacity(destinations.len());
        let mut pending_operation_ids = Vec::with_capacity(destinations.len());
        let mut resident_keys = Vec::new();
        for (worker_key, incarnation, ipc_tx) in destinations {
            let operation_id = format!(
                "{}:{}:{incarnation}",
                payload.operation_id,
                uuid::Uuid::new_v4()
            );
            let mut per_worker = payload.clone();
            per_worker.operation_id = operation_id.clone();
            let (tx, rx) = oneshot::channel();
            self.remote_access_acks
                .lock()
                .unwrap()
                .insert(operation_id.clone(), tx);
            if ipc_tx
                .send(ServiceToWorker::SetRemoteAccessState(per_worker))
                .is_err()
            {
                self.remote_access_acks
                    .lock()
                    .unwrap()
                    .remove(&operation_id);
                let mut acknowledgements = self.remote_access_acks.lock().unwrap();
                for pending in &pending_operation_ids {
                    acknowledgements.remove(pending);
                }
                drop(acknowledgements);
                if let Some(key) = worker_key {
                    self.session_targets
                        .set_readiness(&key.session, false, false, false, false);
                }
                return Err(format!("worker {incarnation} event channel is closed"));
            }
            pending_operation_ids.push(operation_id);
            if let Some(key) = worker_key {
                resident_keys.push(key);
            }
            waiters.push((incarnation, rx));
        }

        let outcome = tokio::time::timeout(timeout, async {
            for (incarnation, rx) in waiters {
                match rx.await {
                    Ok(ack) if ack.state_version == state_version => {}
                    Ok(_) => {
                        return Err(format!(
                            "worker {incarnation} acknowledged a different remote-access version"
                        ));
                    }
                    Err(_) => {
                        return Err(format!(
                            "worker {incarnation} remote-access acknowledgement channel closed"
                        ));
                    }
                }
            }
            Ok(())
        })
        .await;
        match outcome {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => {
                let mut acknowledgements = self.remote_access_acks.lock().unwrap();
                for operation_id in &pending_operation_ids {
                    acknowledgements.remove(operation_id);
                }
                drop(acknowledgements);
                for key in &resident_keys {
                    self.session_targets
                        .set_readiness(&key.session, false, false, false, false);
                }
                Err(error)
            }
            Err(_) => {
                let mut acknowledgements = self.remote_access_acks.lock().unwrap();
                for operation_id in &pending_operation_ids {
                    acknowledgements.remove(operation_id);
                }
                drop(acknowledgements);
                for key in &resident_keys {
                    self.session_targets
                        .set_readiness(&key.session, false, false, false, false);
                }
                Err(format!(
                    "not every worker acknowledged remote-access version {state_version} within {timeout:?}"
                ))
            }
        }
    }

    async fn worker_destinations(
        &self,
    ) -> Vec<(
        Option<WorkerKey>,
        WorkerIncarnation,
        mpsc::UnboundedSender<ServiceToWorker>,
    )> {
        let inner = self.inner.lock().await;
        let mut destinations = Vec::with_capacity(
            inner.resident_workers.len() + usize::from(inner.active_worker.is_some()),
        );
        if let Some(worker) = inner.active_worker.as_ref() {
            destinations.push((None, worker.incarnation, worker.ipc_tx.clone()));
        }
        destinations.extend(
            inner.resident_workers.iter().map(|(key, worker)| {
                (Some(key.clone()), worker.incarnation, worker.ipc_tx.clone())
            }),
        );
        destinations
    }

    /// Broadcast process-wide state to every resident worker. Connection-scoped
    /// traffic must use the immutable connection binding helpers instead.
    pub async fn broadcast_to_workers(&self, message: ServiceToWorker) -> Result<usize, String> {
        let destinations = self.worker_destinations().await;
        let mut delivered = 0;
        let mut failed = Vec::new();
        for (worker_key, incarnation, ipc_tx) in destinations {
            if ipc_tx.send(message.clone()).is_ok() {
                delivered += 1;
            } else {
                if let Some(key) = worker_key {
                    self.session_targets
                        .set_readiness(&key.session, false, false, false, false);
                }
                failed.push(incarnation.to_string());
            }
        }
        if failed.is_empty() {
            Ok(delivered)
        } else {
            Err(format!(
                "worker event channels are closed: {}",
                failed.join(", ")
            ))
        }
    }

    pub async fn recycle_for_remote_access_timeout(&self) -> Result<(), String> {
        let (session_id, desktop_name, inprocess_restart, mut inprocess_task, mut process) = {
            let mut inner = self.inner.lock().await;
            let Some(mut worker) = inner.active_worker.take() else {
                return Ok(());
            };
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            self.current_incarnation.store(0, Ordering::Release);
            for task in worker.lane_tasks.drain(..) {
                task.abort();
            }
            (
                worker.session_id,
                worker.desktop_name.clone(),
                worker.inprocess_restart.take(),
                worker.inprocess_task.take(),
                worker.process_handle.take(),
            )
        };
        if let Some(task) = inprocess_task.as_mut()
            && tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
        {
            // Dropping the JoinHandle only detaches the task; it is deliberately
            // not treated as a hard kill because capture backends may own native
            // threads outside the cancelled future. Permanently latch the host
            // and fail every live media session closed.
            self.media_worker_restart_required
                .store(true, Ordering::Release);
            self.clear_worker_capabilities();
            self.pc_registry.fail_media_worker_restart_required().await;
            return Err(
                "in-process media worker did not exit within 5 seconds; host restart required"
                    .to_string(),
            );
        }
        if let Some(process) = process.as_mut() {
            let _ = process.kill().await;
            process.wait().await;
        }
        self.pc_registry.clear_worker_activity();
        if let Some(restart) = inprocess_restart {
            self.start_inprocess_worker(
                restart.args,
                session_id,
                desktop_name,
                restart.host_control_hub,
                restart.computer_use_broker,
            )
            .await
            .map_err(|error| error.to_string())
        } else {
            self.start_worker(session_id, desktop_name)
                .await
                .map_err(|error| error.to_string())
        }
    }

    pub async fn send_to_worker(&self, msg: ServiceToWorker) -> Result<(), String> {
        if self.session_targeting_enabled.load(Ordering::Acquire) {
            return Err(
                "session targeting is enabled; refusing unscoped resident-worker dispatch"
                    .to_string(),
            );
        }
        let inner = self.inner.lock().await;
        if let Some(worker) = &inner.active_worker {
            worker
                .ipc_tx
                .send(msg)
                .map_err(|e| format!("Failed to send to worker: {e}"))
        } else {
            Err("No active worker".to_string())
        }
    }

    pub async fn exec_worker_target_for_connection(
        &self,
        connection_id: Option<&str>,
    ) -> Result<ExecWorkerTarget, String> {
        let inner = self.inner.lock().await;
        if self.session_targeting_enabled.load(Ordering::Acquire) {
            let connection_id = connection_id
                .ok_or_else(|| "PTY execution has no immutable session connection".to_string())?;
            let session = self
                .connection_targets
                .lock()
                .unwrap()
                .get(connection_id)
                .cloned()
                .ok_or_else(|| format!("connection {connection_id} has no session target"))?;
            let key = [DesktopTarget::LinuxSession, DesktopTarget::WindowsDefault]
                .into_iter()
                .map(|desktop| WorkerKey {
                    session: session.clone(),
                    desktop,
                })
                .find(|key| inner.resident_workers.contains_key(key))
                .ok_or_else(|| format!("no session-user worker for target {session:?}"))?;
            let worker = inner
                .resident_workers
                .get(&key)
                .expect("selected resident worker disappeared under manager lock");
            return Ok(ExecWorkerTarget {
                worker_key: Some(key.clone()),
                source_incarnation: worker.incarnation,
                session_target_id: key.session.platform_session_id.clone(),
                registration_generation: key.session.session_generation,
                wire_worker_incarnation: worker.incarnation.get(),
            });
        }
        let worker = inner
            .active_worker
            .as_ref()
            .ok_or_else(|| "No active worker".to_string())?;
        Ok(ExecWorkerTarget {
            worker_key: None,
            source_incarnation: worker.incarnation,
            session_target_id: worker.session_id.to_string(),
            registration_generation: 0,
            wire_worker_incarnation: 0,
        })
    }

    pub async fn send_to_session_worker(
        &self,
        session: &desk_ipc_protocol::message::SessionKey,
        msg: ServiceToWorker,
    ) -> Result<(), String> {
        let inner = self.inner.lock().await;
        let key = [DesktopTarget::LinuxSession, DesktopTarget::WindowsDefault]
            .into_iter()
            .map(|desktop| WorkerKey {
                session: session.clone(),
                desktop,
            })
            .find(|key| inner.resident_workers.contains_key(key))
            .ok_or_else(|| format!("no session-user worker for target {session:?}"))?;
        inner
            .resident_workers
            .get(&key)
            .expect("selected resident key disappeared while manager lock is held")
            .ipc_tx
            .send(msg)
            .map_err(|error| format!("failed to send to resident worker {key:?}: {error}"))
    }

    pub async fn send_to_connection_worker(
        &self,
        connection_id: &str,
        msg: ServiceToWorker,
    ) -> Result<(), String> {
        if let Some(session) = self.connection_target(connection_id) {
            return self.send_to_session_worker(&session, msg).await;
        }
        if self.session_targeting_enabled.load(Ordering::Acquire) {
            return Err(format!(
                "connection {connection_id} has no immutable session target"
            ));
        }
        self.send_to_worker(msg).await
    }

    /// Central actions have no browser peer address. Only the established
    /// single-worker path permits that; resident workers still require the
    /// connection's immutable session selection.
    pub async fn send_central_or_connection_worker(
        &self,
        connection_id: Option<&str>,
        msg: ServiceToWorker,
    ) -> Result<(), String> {
        match connection_id {
            Some(connection_id) => self.send_to_connection_worker(connection_id, msg).await,
            None => self.send_to_worker(msg).await,
        }
    }

    /// Route capture and human-input traffic through the currently active
    /// desktop of the connection's immutable session. Unlike
    /// `send_to_connection_worker`, this may select Winlogon, but it can never
    /// move the connection to another WTS/Linux session.
    pub async fn send_to_interactive_connection_worker(
        &self,
        connection_id: &str,
        msg: ServiceToWorker,
    ) -> Result<(), String> {
        let session = match self.connection_target(connection_id) {
            Some(session) => session,
            None if !self.session_targeting_enabled.load(Ordering::Acquire) => {
                return self.send_to_worker(msg).await;
            }
            None => {
                return Err(format!(
                    "connection {connection_id} has no immutable session target"
                ));
            }
        };
        let route = self
            .active_interactive_routes
            .lock()
            .unwrap()
            .get(&session)
            .cloned()
            .ok_or_else(|| format!("session {session:?} has no active interactive worker"))?;
        if !route.accepting_interactive {
            return Err(format!(
                "session {session:?} is transitioning between interactive desktops"
            ));
        }
        let inner = self.inner.lock().await;
        let worker = inner
            .resident_workers
            .get(&route.worker_key)
            .filter(|worker| worker.incarnation == route.incarnation)
            .ok_or_else(|| format!("interactive route for {session:?} is stale"))?;
        let profile = if route.worker_key.desktop == DesktopTarget::WindowsWinlogon {
            WorkerProfile::RestrictedDesktop
        } else {
            WorkerProfile::SessionUser
        };
        let msg = match (profile, msg) {
            (WorkerProfile::RestrictedDesktop, ServiceToWorker::StartMedia(mut payload)) => {
                // Audio remains a session-user resource. The secure-desktop
                // route may pause it, but must never open an audio device under
                // the persistent SYSTEM worker.
                payload.audio = None;
                ServiceToWorker::StartMedia(payload)
            }
            (_, msg) => msg,
        };
        if !msg.allowed_for_profile(profile) {
            return Err(format!(
                "message is outside the {:?} worker capability profile",
                profile
            ));
        }
        worker.ipc_tx.send(msg).map_err(|error| {
            format!(
                "failed to send through interactive route {:?} epoch {}: {error}",
                route.worker_key, route.route_epoch
            )
        })
    }

    /// Send a `FileTransferPayload` over the dedicated file lane to the
    /// active worker. Used by `pc_manager`'s DC forwarder when a
    /// browser pushes a `file_transfer_event` chunk / control frame.
    ///
    /// **Locking discipline**: clones the file-sender `Arc` under each
    /// guard then drops the guard *before* awaiting the bounded
    /// `send()`. A full file lane parks `send().await` until the worker
    /// drains; holding either `WorkerManagerInner` or the slot
    /// `RwLock` across that wait would head-of-line block worker
    /// recovery / heartbeat / `send_to_worker` for the same window
    /// the SCTP backpressure runs.
    pub async fn send_file_to_worker(&self, payload: FileTransferPayload) -> Result<(), String> {
        if self.session_targeting_enabled.load(Ordering::Acquire) {
            return Err(
                "session targeting is enabled; refusing unscoped resident file dispatch"
                    .to_string(),
            );
        }
        // Step 1: clone the slot Arc under the manager mutex, drop guard.
        let slot = {
            let inner = self.inner.lock().await;
            match inner.active_worker.as_ref() {
                Some(w) => Arc::clone(&w.file_sender_tx),
                None => return Err("No active worker".to_string()),
            }
        };
        // Step 2: clone the inner sender Arc under the slot RwLock, drop guard.
        let sender = {
            let guard = slot.read().await;
            match guard.as_ref() {
                Some(s) => Arc::clone(s),
                None => {
                    return Err("File lane not yet ready (pipe not yet accepted)".to_string());
                }
            }
        };
        // Step 3: bounded send().await runs with no daemon-side locks held.
        sender.send(payload).await.map_err(|e| format!("{e}"))
    }

    pub async fn send_file_to_connection_worker(
        &self,
        connection_id: &str,
        payload: FileTransferPayload,
    ) -> Result<(), String> {
        let session = match self.connection_target(connection_id) {
            Some(session) => session,
            None if !self.session_targeting_enabled.load(Ordering::Acquire) => {
                return self.send_file_to_worker(payload).await;
            }
            None => {
                return Err(format!(
                    "connection {connection_id} has no immutable session target"
                ));
            }
        };
        let slot = {
            let inner = self.inner.lock().await;
            let key = [DesktopTarget::LinuxSession, DesktopTarget::WindowsDefault]
                .into_iter()
                .map(|desktop| WorkerKey {
                    session: session.clone(),
                    desktop,
                })
                .find(|key| inner.resident_workers.contains_key(key))
                .ok_or_else(|| format!("no session-user worker for target {session:?}"))?;
            Arc::clone(
                &inner
                    .resident_workers
                    .get(&key)
                    .expect("selected resident key disappeared while manager lock is held")
                    .file_sender_tx,
            )
        };
        let sender = slot
            .read()
            .await
            .as_ref()
            .cloned()
            .ok_or_else(|| "resident worker file lane is not ready".to_string())?;
        sender
            .send(payload)
            .await
            .map_err(|error| format!("{error}"))
    }

    /// Record that a message arrived from `incarnation`, and report whether
    /// that worker is the one the daemon is running.
    ///
    /// A `false` answer means the message was overtaken: it was sent by a
    /// worker the daemon has already replaced, so it describes a process that
    /// is gone and must not be allowed to speak for the one that took its
    /// place. The reader drops it.
    ///
    /// The two halves are answered together, under one lock, because they are
    /// the same question. Every IPC message — heartbeat or otherwise — counts
    /// as a sign of life, but only for the worker that sent it: crediting a
    /// replaced worker's backlog to its replacement would keep the watchdog
    /// quiet about a replacement that has never said anything.
    pub async fn note_message_from(
        &self,
        worker_key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some(key) = worker_key {
            return match inner.resident_workers.get_mut(key) {
                Some(worker) if worker.incarnation == incarnation => {
                    worker.last_heartbeat_at = Instant::now();
                    true
                }
                _ => false,
            };
        }
        match inner.active_worker.as_mut() {
            Some(worker) if worker.incarnation == incarnation => {
                worker.last_heartbeat_at = Instant::now();
                true
            }
            _ => false,
        }
    }

    /// Send the security policy to every worker and follow up on whether it
    /// arrived at each one.
    ///
    /// Returns as soon as the message is queued. The acknowledgement is awaited
    /// on a background task instead of here because the caller is a settings
    /// commit that has already made the policy durable: the operator's change is
    /// applied whether or not a worker is listening, and a worker that starts
    /// later reads the same values out of its Init payload. What the follow-up
    /// buys is that a worker which never confirms — or confirms while asking to
    /// be resynchronized — says so in the log rather than silently enforcing an
    /// older policy.
    pub async fn publish_security_policy(&self, snapshot: PolicySnapshot, timeout: Duration) {
        let seq = snapshot.seq();
        let destinations = self.worker_destinations().await;
        if destinations.is_empty() {
            debug!(
                "[worker_manager] security policy {seq} has no worker to reach; a worker starting later picks it up from Init"
            );
            return;
        }
        for (worker_key, incarnation, ipc_tx) in destinations {
            let operation_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();
            self.policy_acks
                .lock()
                .unwrap()
                .insert(operation_id.clone(), tx);
            if ipc_tx
                .send(ServiceToWorker::UpdateSecurityPolicy(
                    UpdateSecurityPolicyPayload {
                        operation_id: operation_id.clone(),
                        snapshot: snapshot.clone(),
                    },
                ))
                .is_err()
            {
                self.policy_acks.lock().unwrap().remove(&operation_id);
                if let Some(key) = worker_key {
                    self.session_targets
                        .set_readiness(&key.session, false, false, false, false);
                }
                error!(
                    "[worker_manager] worker {incarnation} event channel closed before security policy {seq} was delivered"
                );
                continue;
            }
            let acks = Arc::clone(&self.policy_acks);
            let targets = self.session_targets.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) | Err(_) => {
                        acks.lock().unwrap().remove(&operation_id);
                        if let Some(key) = worker_key {
                            targets.set_readiness(&key.session, false, false, false, false);
                        }
                        error!(
                            "[worker_manager] worker {incarnation} did not confirm security policy {seq} within {timeout:?}; it may still be enforcing an older one"
                        );
                    }
                }
            });
        }
    }

    /// A successful return means every live worker acknowledged this exact
    /// policy. On failure retire workers so they cannot accept new work under
    /// an obsolete restriction; replacement workers read the durable Init.
    pub async fn publish_application_policy(
        &self,
        policy: crate::model::settings::ComputerUseApplicationPolicy,
        timeout: Duration,
    ) -> Result<(), String> {
        use desk_ipc_protocol::message::ComputerUseApplicationPolicyPayload;
        let destinations = self.worker_destinations().await;
        let mut failed = false;
        for (_, _, ipc_tx) in destinations {
            let operation_id = uuid::Uuid::new_v4().to_string();
            let payload = ComputerUseApplicationPolicyPayload {
                operation_id: operation_id.clone(),
                revision: policy.revision,
                allowed_application_paths: policy.allowed_application_paths.clone(),
            };
            let (tx, rx) = oneshot::channel();
            self.application_policy_acks
                .lock()
                .unwrap()
                .insert(operation_id.clone(), tx);
            let sent = ipc_tx
                .send(ServiceToWorker::UpdateComputerUseApplicationPolicy(
                    payload.clone(),
                ))
                .is_ok();
            let acknowledged = if sent {
                matches!(tokio::time::timeout(timeout, rx).await, Ok(Ok(applied)) if applied == payload)
            } else {
                false
            };
            self.application_policy_acks
                .lock()
                .unwrap()
                .remove(&operation_id);
            failed |= !acknowledged;
        }
        if failed {
            self.shutdown_all().await;
            return Err("Application policy was saved, but a worker did not acknowledge it. Workers were retired; restart the host before retrying.".into());
        }
        Ok(())
    }

    pub fn note_application_policy_applied(
        &self,
        payload: desk_ipc_protocol::message::ComputerUseApplicationPolicyPayload,
    ) {
        if let Some(waiter) = self
            .application_policy_acks
            .lock()
            .unwrap()
            .remove(&payload.operation_id)
        {
            let _ = waiter.send(payload);
        }
    }

    /// Record what the worker reported after a security policy was published to
    /// it, and report whether it is asking to be resynchronized.
    ///
    /// A worker that asks is holding a policy the daemon never published —
    /// deliberately stricter than either side intended, so nothing is permitted
    /// that should not be. It cannot climb out on its own: the local tightening
    /// moved its sequence past what was published, so only a fresh publication
    /// resets it, and until one arrives the symptom on the host is prompts for
    /// capabilities the operator has already allowed with nothing to explain
    /// them. The caller republishes — reaching the policy is the settings
    /// coordinator's job, and it is the coordinator that owns this manager.
    #[must_use = "a worker asking for a resync stays degraded until the policy is republished"]
    pub async fn note_policy_applied(&self, payload: &SecurityPolicyAppliedPayload) -> bool {
        if let Some(waiter) = self
            .policy_acks
            .lock()
            .unwrap()
            .remove(&payload.operation_id)
        {
            let _ = waiter.send(payload.clone());
        }
        match &payload.outcome {
            PolicyApplyOutcome::Applied { seq, .. } => {
                debug!(
                    "[worker_manager] worker applied security policy {} (operation {})",
                    seq, payload.operation_id
                );
                self.policy_applied_seq.store(*seq, Ordering::Release);
                false
            }
            PolicyApplyOutcome::NeedsResync { seq } => {
                error!(
                    "[worker_manager] worker could not reconcile security policy for operation \
                     {}; it is holding a locally tightened policy at {} and needs the current \
                     one republished",
                    payload.operation_id, seq
                );
                true
            }
        }
    }

    /// The policy sequence the worker last confirmed holding, or zero if it has
    /// confirmed none.
    pub fn policy_applied_seq(&self) -> u64 {
        self.policy_applied_seq.load(Ordering::Acquire)
    }

    /// Take a snapshot of the active worker's identity + last
    /// heartbeat — separated out so the watchdog can decide whether
    /// to fire without holding the manager lock during the kill /
    /// restart path.
    async fn active_worker_snapshot(
        &self,
    ) -> Option<(WorkerIncarnation, u32, Option<String>, Instant)> {
        let inner = self.inner.lock().await;
        inner.active_worker.as_ref().map(|w| {
            (
                w.incarnation,
                w.session_id,
                w.desktop_name.clone(),
                w.last_heartbeat_at,
            )
        })
    }

    async fn resident_worker_snapshots(&self) -> Vec<(WorkerKey, WorkerIncarnation, Instant)> {
        self.inner
            .lock()
            .await
            .resident_workers
            .iter()
            .map(|(key, worker)| (key.clone(), worker.incarnation, worker.last_heartbeat_at))
            .collect()
    }

    /// Whether restarting on `incarnation`'s behalf is still the right thing to
    /// do: either it is the worker the daemon is running, or the daemon has no
    /// worker at all, so a restart has nothing newer to trample.
    ///
    /// The second case is what keeps a failed start recoverable — a pipe server
    /// whose worker never dialled in reports the failure long after
    /// `start_worker` gave up, and by then there is no handle to compare
    /// against. The first case is what stops a pipe server from tearing down
    /// the worker that was started while it was giving up.
    async fn restart_is_still_wanted(&self, incarnation: WorkerIncarnation) -> bool {
        let inner = self.inner.lock().await;
        match inner.active_worker.as_ref() {
            Some(worker) => worker.incarnation == incarnation,
            None => true,
        }
    }

    /// Spawn the heartbeat watchdog. Returns the join handle so the
    /// caller can abort it on shutdown. Re-reads settings each tick
    /// so toggling the flag at runtime takes effect immediately.
    pub fn spawn_heartbeat_watchdog(&self) -> tokio::task::JoinHandle<()> {
        let mgr = self.clone();
        tokio::spawn(async move {
            info!(
                "[WorkerWatchdog] starting (check every {:?})",
                WORKER_HEARTBEAT_CHECK_INTERVAL
            );
            loop {
                tokio::time::sleep(WORKER_HEARTBEAT_CHECK_INTERVAL).await;

                let (enabled, timeout) = {
                    let s = mgr.settings.read().await;
                    (
                        s.system.worker_heartbeat_watchdog_enabled.unwrap_or(true),
                        Duration::from_secs(
                            s.system
                                .worker_heartbeat_timeout_secs
                                .unwrap_or(DEFAULT_WORKER_HEARTBEAT_TIMEOUT_SECS),
                        ),
                    )
                };

                if let Some((incarnation, session_id, desktop_name, last)) =
                    mgr.active_worker_snapshot().await
                {
                    let elapsed = Instant::now().saturating_duration_since(last);
                    if worker_is_stale(enabled, timeout, elapsed) {
                        warn!(
                            "[WorkerWatchdog] no IPC traffic for {:?} (timeout={:?}, worker={incarnation}, \
                             session={session_id}, desktop={desktop_name:?}) — declaring worker stuck and \
                             restarting",
                            elapsed, timeout
                        );
                        mgr.handle_crash_recovery(incarnation, session_id, desktop_name);
                    }
                }

                for (key, incarnation, last) in mgr.resident_worker_snapshots().await {
                    let elapsed = Instant::now().saturating_duration_since(last);
                    if worker_is_stale(enabled, timeout, elapsed) {
                        warn!(
                            "[WorkerWatchdog] no IPC traffic for {:?} (timeout={:?}, resident={:?}, \
                             worker={incarnation}) — restarting only that slot",
                            elapsed, timeout, key
                        );
                        mgr.handle_resident_crash_recovery(key, incarnation);
                    }
                }
            }
        })
    }

    /// Pause every PC's media ingestion so frames from the about-to-die
    /// worker are dropped instead of pushed onto the browser PC. The first
    /// IDR from the replacement worker clears each per-PC flag in place.
    ///
    /// **Keep-PC semantics**: the daemon holds the WebRTC PC,
    /// so worker swaps are invisible to the browser apart from a brief
    /// frame-freeze that resolves on the new worker's first IDR. There
    /// is no browser-facing `SignalingType::DesktopSwitching` emission and
    /// no per-connection accept-state shipped to the next worker —
    /// `SignalingState` lives next to the PC in the daemon and is never
    /// torn down on a worker swap.
    pub async fn notify_desktop_switch(&self) {
        self.pc_registry.clear_worker_activity();
        self.pc_registry.pause_all_media().await;
    }

    /// Restart the worker named by `incarnation` because it stopped serving.
    ///
    /// The caller is whoever noticed — a pipe server whose worker never dialled
    /// in or whose transport dropped, or the heartbeat watchdog. Any of them can
    /// be reporting on a worker the daemon has already moved on from: a pipe
    /// server spends up to fifteen seconds waiting for a connection, and a
    /// desktop switch during that wait installs a replacement in the meantime.
    /// Restarting on the stale one's behalf would kill the worker that is
    /// actually running, so the decision is re-checked against the current
    /// worker before anything is torn down.
    pub fn handle_crash_recovery(
        &self,
        incarnation: WorkerIncarnation,
        session_id: u32,
        desktop_name: Option<String>,
    ) {
        // Portable / Default mode: there is no external process to
        // crash-recover. The "worker" is an in-process task — if it
        // unwound the whole runtime is going down anyway, and even if
        // we tried to re-launch we'd hit `CreateProcessAsUserW` from a
        // non-SYSTEM context. Log and bail.
        if self.is_inprocess() {
            self.pc_registry.clear_worker_activity();
            warn!(
                "[WorkerManager] In-process worker {incarnation} exited unexpectedly \
                 (session={session_id}); crash recovery is a no-op in portable mode"
            );
            return;
        }

        let mgr = self.clone();
        // Must use tokio::spawn (not actix_web::rt::spawn / spawn_local) because this
        // is called from within a tokio::spawn task (run_pipe_server) which has no
        // LocalSet; calling spawn_local there panics and silently kills the task.
        tokio::spawn(async move {
            if !mgr.restart_is_still_wanted(incarnation).await {
                info!(
                    "[WorkerManager] Worker {incarnation} reported it stopped, but it has already \
                     been replaced; leaving the current worker alone"
                );
                return;
            }
            warn!(
                "[WorkerManager] Worker {incarnation} exited unexpectedly — restarting \
                 (session={session_id})"
            );
            mgr.notify_desktop_switch().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = mgr.start_worker(session_id, desktop_name).await {
                error!("[WorkerManager] Failed to restart Worker after crash: {e}");
            }
        });
    }

    pub(super) fn handle_worker_transport_failure(
        &self,
        worker_key: Option<WorkerKey>,
        incarnation: WorkerIncarnation,
        session_id: u32,
        desktop_name: Option<String>,
    ) {
        if let Some(key) = worker_key {
            self.handle_resident_crash_recovery(key, incarnation);
        } else {
            self.handle_crash_recovery(incarnation, session_id, desktop_name);
        }
    }

    fn handle_resident_crash_recovery(&self, key: WorkerKey, incarnation: WorkerIncarnation) {
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut removed = {
                let mut inner = mgr.inner.lock().await;
                match inner.resident_workers.get(&key) {
                    Some(worker) if worker.incarnation == incarnation => {
                        inner.resident_workers.remove(&key)
                    }
                    _ => None,
                }
            };
            let Some(mut worker) = removed.take() else {
                debug!(
                    "[WorkerManager] ignoring stale resident-worker failure for {:?} {incarnation}",
                    key
                );
                return;
            };
            #[cfg(target_os = "windows")]
            let session_id = worker.session_id;
            mgr.fence_resident_worker(&key, incarnation);
            mgr.revoke_interactive_route(&key, incarnation);
            mgr.retire_resident_worker(&mut worker);
            if let Some(mut process) = worker.process_handle.take() {
                let _ = process.kill().await;
                process.wait().await;
            }
            if key.desktop != DesktopTarget::WindowsWinlogon {
                mgr.session_targets
                    .set_readiness(&key.session, false, false, false, false);
            }

            #[cfg(target_os = "linux")]
            {
                let registration = mgr
                    .session_shell_registry
                    .read()
                    .unwrap()
                    .as_ref()
                    .and_then(|registry| {
                        registry.snapshot().into_iter().find(|registration| {
                            linux_session_key(
                                &registration.logical_session,
                                registration.registration_generation,
                            ) == key.session
                        })
                    });
                if let Some(registration) = registration {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let Err(error) = mgr.start_linux_resident_worker(registration).await {
                        error!(
                            "[WorkerManager] failed to restart resident worker {:?}: {error}",
                            key
                        );
                    }
                }
            }
            #[cfg(target_os = "windows")]
            {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Err(error) = mgr
                    .start_windows_resident_worker(key.clone(), session_id)
                    .await
                {
                    error!(
                        "[WorkerManager] failed to restart Windows resident worker {:?}: {error}",
                        key
                    );
                }
            }
        });
    }

    pub async fn shutdown_all(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(mut worker) = inner.active_worker.take() {
            info!("Shutting down worker: {}", worker.pipe_name);
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            self.retire_worker(&mut worker);
            if let Some(mut proc) = worker.process_handle.take() {
                match tokio::time::timeout(Duration::from_secs(3), proc.wait()).await {
                    Ok(()) => info!("Worker exited gracefully"),
                    Err(_) => {
                        warn!("Worker did not exit in time, killing");
                        let _ = proc.kill().await;
                    }
                }
            }
        }
        let residents: Vec<_> = inner.resident_workers.drain().collect();
        for (key, mut worker) in residents {
            info!(
                "Shutting down resident worker {:?}: {}",
                key, worker.pipe_name
            );
            self.fence_resident_worker(&key, worker.incarnation);
            self.revoke_interactive_route(&key, worker.incarnation);
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            self.retire_resident_worker(&mut worker);
            if let Some(mut process) = worker.process_handle.take() {
                match tokio::time::timeout(Duration::from_secs(3), process.wait()).await {
                    Ok(()) => info!("Resident worker {:?} exited gracefully", key),
                    Err(_) => {
                        warn!("Resident worker {:?} did not exit in time, killing", key);
                        let _ = process.kill().await;
                    }
                }
            }
        }
    }

    async fn launch_worker_process(
        &self,
        pipe_name: &str,
        session_id: u32,
        desktop_name: Option<&str>,
    ) -> Result<ProcessHandle, Box<dyn std::error::Error + Send + Sync>> {
        let exe_path = std::env::current_exe()?;

        #[cfg(target_os = "windows")]
        {
            let cmd_line = format!(
                "\"{}\" --startup-mode session-worker --pipe {}",
                exe_path.display(),
                pipe_name
            );
            // Winlogon's DACL only grants access to SYSTEM by default, so a
            // user-token worker can't open the secure desktop at all. Force
            // the SYSTEM-token launch path for restricted desktops; for
            // everything else keep the user token (richer profile, narrower
            // privileges).
            let force_system_token = desktop_requires_system_token(desktop_name);
            return match launch_worker_as_user(
                session_id,
                desktop_name,
                &cmd_line,
                force_system_token,
            ) {
                Ok(child) => {
                    info!(
                        "Worker launched via CreateProcessAsUserW (PID {})",
                        child.pid
                    );
                    Ok(ProcessHandle::WindowsNative(child))
                }
                Err(error) => Err(format!(
                    "CreateProcessAsUserW failed for session {session_id} desktop {desktop_name:?}; refusing daemon-identity fallback: {error}"
                )
                .into()),
            };
        }

        #[cfg(not(target_os = "windows"))]
        let _ = (session_id, desktop_name);

        #[cfg(target_os = "linux")]
        if !inherited_linux_worker_identity_is_safe(unsafe { libc::geteuid() }) {
            return Err(
                "refusing to spawn a Linux SessionWorker with inherited root identity; a validated Tauri session launch context is required"
                    .into(),
            );
        }

        #[cfg(not(target_os = "windows"))]
        let child = tokio::process::Command::new(&exe_path)
            .arg("--startup-mode")
            .arg("session-worker")
            .arg("--pipe")
            .arg(pipe_name)
            .spawn()?;

        #[cfg(not(target_os = "windows"))]
        return Ok(ProcessHandle::Tokio(child));

        #[allow(unreachable_code)]
        Err("worker launch is unavailable on this platform".into())
    }
}

mod process_launch;
use process_launch::*;

#[cfg(target_os = "windows")]
mod windows_transport;
#[cfg(target_os = "windows")]
use windows_transport::*;

mod event_drains;
use event_drains::*;

#[cfg(not(target_os = "windows"))]
mod unix_transport;
#[cfg(not(target_os = "windows"))]
use unix_transport::*;

mod bridge;
use bridge::*;

#[cfg(all(test, target_os = "windows"))]
mod tests;

#[cfg(test)]
mod bridge_tests;

#[cfg(test)]
mod incarnation_tests;

#[cfg(test)]
mod policy_tests;

#[cfg(test)]
mod central_routing_tests;
