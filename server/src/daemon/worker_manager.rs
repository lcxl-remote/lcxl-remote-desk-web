use crate::daemon::pc_manager::PcRegistry;
use crate::host_control::HostControlHub;
use crate::model::settings::{Args, SharedSettings};
use actix_web::web;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaReceiver, framed, inprocess},
    message::{
        FileTransferPayload, MediaCapabilities, PolicyApplyOutcome, SecurityPolicyAppliedPayload,
        ServiceToWorker, WorkerInitPayload, WorkerToService,
    },
    transport::{read_message, write_message},
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};
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

#[derive(Clone)]
pub struct WorkerManager {
    settings: web::Data<SharedSettings>,
    inner: Arc<Mutex<WorkerManagerInner>>,
    worker_msg_tx: Arc<mpsc::UnboundedSender<WorkerToService>>,
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
    /// Monotonic counter bumped every time [`Self::set_worker_capabilities`]
    /// installs a fresh snapshot. Paired with [`Self::capabilities_version_tx`]
    /// so async callers can wait until the cache reflects a known-newer
    /// snapshot (e.g. `VirtualDisplaySupervisor::ensure_attached` waits
    /// for the post-attach `RefreshCapabilities` round-trip to update
    /// the cache before letting `RequestRemote` assemble the Init reply).
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
    remote_access_gate: Arc<StdRwLock<crate::daemon::remote_access::RemoteAccessGate>>,
    remote_access_acks: Arc<
        StdMutex<
            HashMap<
                String,
                oneshot::Sender<desk_ipc_protocol::message::RemoteAccessStateAppliedPayload>,
            >,
        >,
    >,
}

struct WorkerManagerInner {
    active_worker: Option<WorkerHandle>,
}

struct WorkerHandle {
    pipe_name: String,
    ipc_tx: mpsc::UnboundedSender<ServiceToWorker>,
    process_handle: Option<ProcessHandle>,
    /// Last instant the daemon received any IPC message from this
    /// worker (initialised to spawn time). Used by the heartbeat
    /// watchdog — if no heartbeat (or any other message) shows up
    /// within the configured timeout the worker is presumed stuck.
    last_heartbeat_at: Instant,
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
}

#[derive(Clone)]
struct InprocessRestart {
    args: Args,
    host_control_hub: Arc<HostControlHub>,
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

pub type WorkerMessageReceiver = mpsc::UnboundedReceiver<WorkerToService>;

impl WorkerManager {
    pub fn new(
        settings: web::Data<SharedSettings>,
        pc_registry: PcRegistry,
    ) -> (Self, WorkerMessageReceiver) {
        let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
        let (cap_version_tx, _cap_version_rx) = watch::channel::<u64>(0);
        let mgr = WorkerManager {
            settings,
            inner: Arc::new(Mutex::new(WorkerManagerInner {
                active_worker: None,
            })),
            worker_msg_tx: Arc::new(tx),
            pc_registry,
            worker_capabilities: Arc::new(StdMutex::new(None)),
            capabilities_version: Arc::new(AtomicU64::new(0)),
            policy_applied_seq: Arc::new(AtomicU64::new(0)),
            capabilities_version_tx: Arc::new(cap_version_tx),
            is_inprocess: Arc::new(AtomicBool::new(false)),
            remote_access_gate: Arc::new(StdRwLock::new(
                crate::daemon::remote_access::RemoteAccessGate::startup_locked(),
            )),
            remote_access_acks: Arc::new(StdMutex::new(HashMap::new())),
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

    pub fn bind_remote_access_gate(&self, gate: crate::daemon::remote_access::RemoteAccessGate) {
        *self.remote_access_gate.write().unwrap() = gate;
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
        *self.worker_capabilities.lock().unwrap() = None;

        let mut inner = self.inner.lock().await;

        if let Some(mut worker) = inner.active_worker.take() {
            info!("Shutting down existing worker before starting new one");
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            if let Some(mut proc) = worker.process_handle.take() {
                match tokio::time::timeout(Duration::from_secs(3), proc.wait()).await {
                    Ok(()) => info!("Old worker exited gracefully"),
                    Err(_) => {
                        warn!("Old worker did not exit in time, killing");
                        let _ = proc.kill().await;
                    }
                }
            }
            if let Some(task) = worker.inprocess_task.take() {
                task.abort();
            }
        }

        let pipe_name = format!("lcxl-desk-ipc-{}-{}", session_id, uuid::Uuid::new_v4());

        let (ipc_cmd_tx, ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();

        let (config_json, ipc_token, config_file_path) = {
            let settings = self.settings.read().await;
            let json = serde_json::to_string(&*settings)
                .map_err(|e| format!("Failed to serialize settings: {e}"))?;
            let token = settings.system.tauri_ipc_token.clone();
            let path = settings.args.config_file_path.clone();
            (json, token, path)
        };

        // Daemon-side host-upstream endpoint that the worker's Forwarder hub
        // will dial back into. Loopback is fine — workers run on the same host.
        let host_upstream_url = format!(
            "ws://127.0.0.1:{}/ws/host_upstream",
            crate::daemon::local_api::SERVICE_API_PORT
        );

        let worker_msg_tx = Arc::clone(&self.worker_msg_tx);
        let pipe_name_c = pipe_name.clone();
        let desktop_c = desktop_name.clone();
        let config_c = config_json.clone();
        let config_file_path_c = if config_file_path.is_empty() {
            None
        } else {
            Some(config_file_path)
        };
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
        tokio::spawn(async move {
            if let Err(e) = run_pipe_server(
                &pipe_name_c,
                session_id,
                desktop_c,
                config_c,
                config_file_path_c,
                ipc_cmd_rx,
                (*worker_msg_tx).clone(),
                mgr_c,
                host_upstream_url_c,
                ipc_token_c,
                pc_registry_c,
                file_sender_slot_c,
            )
            .await
            {
                error!("Pipe server error: {e}");
            }
        });

        let process = self
            .launch_worker_process(&pipe_name, session_id, desktop_name.as_deref())
            .await?;

        inner.active_worker = Some(WorkerHandle {
            pipe_name,
            ipc_tx: ipc_cmd_tx,
            process_handle: Some(process),
            last_heartbeat_at: Instant::now(),
            session_id,
            desktop_name: desktop_name.clone(),
            file_sender_tx: file_sender_slot,
            inprocess_task: None,
            inprocess_restart: None,
        });

        info!("Worker started for session {session_id}");
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
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Latch the in-process flag so `signaling_proxy::DesktopChanged`
        // and `handle_crash_recovery` skip their swap-to-fresh-worker
        // branches. Once set this manager remains in in-process mode
        // for the rest of its lifetime — switching topologies mid-run
        // is not a supported configuration.
        self.is_inprocess.store(true, Ordering::Relaxed);

        // Mirror start_worker: a fresh worker re-reports capabilities on its
        // own; clearing the cached snapshot avoids handing stale device data
        // to a `RequestRemote` that lands between Init and the worker's
        // first `Capabilities` emission.
        *self.worker_capabilities.lock().unwrap() = None;

        let mut inner = self.inner.lock().await;

        if let Some(mut worker) = inner.active_worker.take() {
            info!("Shutting down existing in-process worker before starting a new one");
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            if let Some(task) = worker.inprocess_task.take() {
                task.abort();
            }
        }

        let pipe_name = format!("inprocess-{session_id}-{}", uuid::Uuid::new_v4());
        let (ipc_cmd_tx, mut ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();

        let (config_json, config_file_path) = {
            let s = self.settings.read().await;
            let json = serde_json::to_string(&*s)
                .map_err(|e| format!("Failed to serialize settings: {e}"))?;
            let path = s.args.config_file_path.clone();
            (json, path)
        };

        let remote_access_state = self.remote_access_state();
        let init_payload = WorkerInitPayload {
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name: desktop_name.clone(),
            config_json,
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
            // In-process portable / DeskServer modes share the daemon's
            // settings file path so worker-side `Settings::save()` (e.g.
            // for a "remember" auth approval) writes back to the same
            // file. See `WorkerInitPayload::config_file_path` docs.
            config_file_path: if config_file_path.is_empty() {
                None
            } else {
                Some(config_file_path)
            },
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
        let worker_msg_tx = (*self.worker_msg_tx).clone();
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
        let _media_handle = spawn_media_receiver_task(media_rx, self.pc_registry.clone());

        // Daemon-side file-lane drain task: each worker → daemon
        // payload feeds into `pc_manager::write_file_transfer_data`,
        // which routes by `connection_id` to the matching browser DC.
        // Serial single-task drain accepts cross-connection HOL as a
        // known trade-off (see `dual_transport.rs` module docs).
        {
            let pc_registry = self.pc_registry.clone();
            tokio::spawn(async move {
                info!("[worker_manager] in-process file-lane drain starting");
                while let Some(payload) = file_w2d_rx.recv().await {
                    crate::daemon::pc_manager::write_file_transfer_data(&pc_registry, payload)
                        .await;
                }
                info!("[worker_manager] in-process file-lane drain exiting (closed)");
            });
        }

        // Spawn the worker on `actix_web::rt::spawn` because
        // `WorkerSession::run_with_transports` awaits actix-web internals
        // (`DeskSession`, `awc::Client`, `actix_web::rt::spawn` from
        // signaling handlers) which all require a `LocalSet` context.
        // `tokio::spawn` would fail with "spawn_local called from
        // outside of a `task::LocalSet`".
        let restart = InprocessRestart {
            args: args.clone(),
            host_control_hub: host_control_hub.clone(),
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
            pipe_name,
            ipc_tx: ipc_cmd_tx,
            // No OS process to track in in-process mode. The worker task
            // is owned by the actix-rt System and will be cancelled when
            // the System shuts down; we don't track its JoinHandle on the
            // handle struct because the watchdog / restart paths key off
            // `ipc_tx` alive-ness, not process state.
            process_handle: None,
            last_heartbeat_at: Instant::now(),
            session_id,
            desktop_name,
            file_sender_tx: file_sender_slot,
            inprocess_task: Some(inprocess_task),
            inprocess_restart: Some(restart),
        });

        info!("In-process worker started for session {session_id}");
        Ok(())
    }

    /// Stash the worker's last reported [`MediaCapabilities`]. Called
    /// from `signaling_proxy` whenever the worker emits
    /// `WorkerToService::Capabilities`. Subsequent `RequestRemote`
    /// handling uses the snapshot to populate the Init reply.
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

    /// Take a snapshot of the latest reported worker capabilities.
    /// Returns `None` until the worker has sent Capabilities at least
    /// once after Init; in that window the daemon ships an empty
    /// device list, the same behaviour as the legacy single-process
    /// path on first connection.
    pub fn worker_capabilities(&self) -> Option<MediaCapabilities> {
        self.worker_capabilities.lock().unwrap().clone()
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
    pub async fn install_active_for_test(&self, ipc_tx: mpsc::UnboundedSender<ServiceToWorker>) {
        let mut inner = self.inner.lock().await;
        inner.active_worker = Some(WorkerHandle {
            pipe_name: "test".to_string(),
            ipc_tx,
            process_handle: None,
            last_heartbeat_at: Instant::now(),
            session_id: 0,
            desktop_name: None,
            file_sender_tx: Arc::new(RwLock::new(None)),
            inprocess_task: None,
            inprocess_restart: None,
        });
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
        let has_worker = self.inner.lock().await.active_worker.is_some();
        if !has_worker {
            return Ok(false);
        }
        let operation_id = payload.operation_id.clone();
        let state_version = payload.state_version;
        let (tx, rx) = oneshot::channel();
        self.remote_access_acks
            .lock()
            .unwrap()
            .insert(operation_id.clone(), tx);
        if let Err(error) = self
            .send_to_worker(ServiceToWorker::SetRemoteAccessState(payload))
            .await
        {
            self.remote_access_acks
                .lock()
                .unwrap()
                .remove(&operation_id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(ack)) if ack.state_version == state_version => Ok(true),
            Ok(Ok(_)) => Err("worker acknowledged a different remote-access version".into()),
            Ok(Err(_)) => Err("worker remote-access acknowledgement channel closed".into()),
            Err(_) => {
                self.remote_access_acks
                    .lock()
                    .unwrap()
                    .remove(&operation_id);
                Err(format!(
                    "worker did not acknowledge remote-access version {state_version} within {timeout:?}"
                ))
            }
        }
    }

    pub async fn recycle_for_remote_access_timeout(&self) -> Result<(), String> {
        let (session_id, desktop_name, inprocess_restart, mut process, task) = {
            let mut inner = self.inner.lock().await;
            let Some(mut worker) = inner.active_worker.take() else {
                return Ok(());
            };
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            (
                worker.session_id,
                worker.desktop_name.clone(),
                worker.inprocess_restart.take(),
                worker.process_handle.take(),
                worker.inprocess_task.take(),
            )
        };
        if let Some(task) = task {
            task.abort();
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

    /// Record that the daemon just received an IPC message from the
    /// active worker. The watchdog uses this to detect when a worker
    /// has stopped responding (every IPC message — heartbeat or
    /// otherwise — counts as a sign of life).
    pub async fn note_heartbeat(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(worker) = inner.active_worker.as_mut() {
            worker.last_heartbeat_at = Instant::now();
        }
    }

    /// Record what the worker reported after a security policy was published to
    /// it.
    ///
    /// A worker asking to be resynchronized is holding a policy the daemon
    /// never published — deliberately stricter than either side intended, so
    /// nothing is permitted that should not be, but it stays that way until the
    /// daemon publishes again. That is worth saying out loud, because the
    /// symptom on the host is prompts for capabilities the operator has already
    /// allowed, with nothing else to explain it.
    pub async fn note_policy_applied(&self, payload: &SecurityPolicyAppliedPayload) {
        match &payload.outcome {
            PolicyApplyOutcome::Applied { seq, .. } => {
                debug!(
                    "[worker_manager] worker applied security policy {} (operation {})",
                    seq, payload.operation_id
                );
                self.policy_applied_seq.store(*seq, Ordering::Release);
            }
            PolicyApplyOutcome::NeedsResync { seq } => {
                error!(
                    "[worker_manager] worker could not reconcile security policy for operation \
                     {}; it is holding a locally tightened policy at {} and needs the current \
                     one republished",
                    payload.operation_id, seq
                );
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
    async fn active_worker_snapshot(&self) -> Option<(u32, Option<String>, Instant)> {
        let inner = self.inner.lock().await;
        inner
            .active_worker
            .as_ref()
            .map(|w| (w.session_id, w.desktop_name.clone(), w.last_heartbeat_at))
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

                let Some((session_id, desktop_name, last)) = mgr.active_worker_snapshot().await
                else {
                    continue;
                };
                let elapsed = Instant::now().saturating_duration_since(last);
                if !worker_is_stale(enabled, timeout, elapsed) {
                    continue;
                }

                warn!(
                    "[WorkerWatchdog] no IPC traffic for {:?} (timeout={:?}, session={session_id}, \
                     desktop={desktop_name:?}) — declaring worker stuck and restarting",
                    elapsed, timeout
                );
                mgr.handle_crash_recovery(session_id, desktop_name);
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

    pub fn handle_crash_recovery(&self, session_id: u32, desktop_name: Option<String>) {
        self.pc_registry.clear_worker_activity();
        // Portable / Default mode: there is no external process to
        // crash-recover. The "worker" is an in-process task — if it
        // unwound the whole runtime is going down anyway, and even if
        // we tried to re-launch we'd hit `CreateProcessAsUserW` from a
        // non-SYSTEM context. Log and bail.
        if self.is_inprocess() {
            warn!(
                "[WorkerManager] In-process worker exited unexpectedly (session={session_id}); \
                 crash recovery is a no-op in portable mode"
            );
            return;
        }

        warn!("[WorkerManager] Worker exited unexpectedly — restarting (session={session_id})");
        let mgr = self.clone();
        // Must use tokio::spawn (not actix_web::rt::spawn / spawn_local) because this
        // is called from within a tokio::spawn task (run_pipe_server) which has no
        // LocalSet; calling spawn_local there panics and silently kills the task.
        tokio::spawn(async move {
            mgr.notify_desktop_switch().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = mgr.start_worker(session_id, desktop_name).await {
                error!("[WorkerManager] Failed to restart Worker after crash: {e}");
            }
        });
    }

    pub async fn shutdown_all(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(mut worker) = inner.active_worker.take() {
            info!("Shutting down worker: {}", worker.pipe_name);
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            if let Some(mut proc) = worker.process_handle.take() {
                match tokio::time::timeout(Duration::from_secs(3), proc.wait()).await {
                    Ok(()) => info!("Worker exited gracefully"),
                    Err(_) => {
                        warn!("Worker did not exit in time, killing");
                        let _ = proc.kill().await;
                    }
                }
            }
            if let Some(task) = worker.inprocess_task.take() {
                task.abort();
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
            match launch_worker_as_user(session_id, desktop_name, &cmd_line, force_system_token) {
                Ok(child) => {
                    info!(
                        "Worker launched via CreateProcessAsUserW (PID {})",
                        child.pid
                    );
                    return Ok(ProcessHandle::WindowsNative(child));
                }
                Err(e) => {
                    warn!(
                        "CreateProcessAsUserW failed (not SYSTEM?), falling back to simple spawn: {e}"
                    );
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        let _ = (session_id, desktop_name);

        let child = tokio::process::Command::new(&exe_path)
            .arg("--startup-mode")
            .arg("session-worker")
            .arg("--pipe")
            .arg(pipe_name)
            .spawn()?;

        Ok(ProcessHandle::Tokio(child))
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
