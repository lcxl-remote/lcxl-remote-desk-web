use crate::daemon::pc_manager::PcRegistry;
use crate::host_control::HostControlHub;
use crate::model::settings::{Args, SharedSettings};
use actix_web::web;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaReceiver, framed, inprocess},
    message::{
        ConnectionAcceptState, MediaCapabilities, ServiceToWorker, WorkerInitPayload,
        WorkerToService,
    },
    transport::{read_message, write_message},
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

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
    /// Per-connection accept-state cache. The daemon is a durable cache for
    /// the worker's authoritative state — entries are inserted on first
    /// `RequestRemote`, updated when the worker emits
    /// `ConnectionAcceptStateChanged`, removed when the worker emits
    /// `ConnectionClosed`, and drained into the next worker's
    /// `WorkerInitPayload.preapproved_connections` on desktop / session
    /// switch + crash recovery.
    ///
    /// Uses `std::sync::Mutex` because every critical section is a short,
    /// synchronous map op with no `.await` inside the guard.
    active_connections: Arc<StdMutex<HashMap<String, ConnectionAcceptState>>>,
    /// Daemon-side per-`connection_id` PeerConnection registry (Arch IV).
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
    #[allow(dead_code)]
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
        let mgr = WorkerManager {
            settings,
            inner: Arc::new(Mutex::new(WorkerManagerInner {
                active_worker: None,
            })),
            worker_msg_tx: Arc::new(tx),
            active_connections: Arc::new(StdMutex::new(HashMap::new())),
            pc_registry,
            worker_capabilities: Arc::new(StdMutex::new(None)),
        };
        (mgr, rx)
    }

    pub async fn start_worker(
        &self,
        session_id: u32,
        desktop_name: Option<String>,
        preapproved: Vec<(String, ConnectionAcceptState)>,
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
        }

        let pipe_name = format!("lcxl-desk-ipc-{}-{}", session_id, uuid::Uuid::new_v4());

        let (ipc_cmd_tx, ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();

        let (config_json, ipc_token) = {
            let settings = self.settings.read().await;
            let json = serde_json::to_string(&*settings)
                .map_err(|e| format!("Failed to serialize settings: {e}"))?;
            let token = settings.system.tauri_ipc_token.clone();
            (json, token)
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
        let host_upstream_url_c = host_upstream_url.clone();
        let ipc_token_c = ipc_token.clone();
        let mgr_c = self.clone();
        let pc_registry_c = self.pc_registry.clone();
        tokio::spawn(async move {
            if let Err(e) = run_pipe_server(
                &pipe_name_c,
                session_id,
                desktop_c,
                config_c,
                ipc_cmd_rx,
                (*worker_msg_tx).clone(),
                preapproved,
                mgr_c,
                host_upstream_url_c,
                ipc_token_c,
                pc_registry_c,
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
        });

        info!("Worker started for session {session_id}");
        Ok(())
    }

    /// In-process variant of [`Self::start_worker`] used by PR 5 portable
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
        // Mirror start_worker: a fresh worker re-reports capabilities on its
        // own; clearing the cached snapshot avoids handing stale device data
        // to a `RequestRemote` that lands between Init and the worker's
        // first `Capabilities` emission.
        *self.worker_capabilities.lock().unwrap() = None;

        let mut inner = self.inner.lock().await;

        if let Some(worker) = inner.active_worker.take() {
            info!("Shutting down existing in-process worker before starting a new one");
            let _ = worker.ipc_tx.send(ServiceToWorker::Shutdown);
            // No process_handle in in-process mode — the worker task
            // observes Shutdown on its event channel and unwinds on its
            // own; we cannot `wait()` on it without storing the
            // JoinHandle, which the rest of WorkerHandle isn't shaped
            // for. The loose-end is acceptable in portable mode where
            // restarts are rare (only on explicit caller request).
        }

        let pipe_name = format!("inprocess-{session_id}-{}", uuid::Uuid::new_v4());
        let (ipc_cmd_tx, mut ipc_cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();

        let config_json = {
            let s = self.settings.read().await;
            serde_json::to_string(&*s).map_err(|e| format!("Failed to serialize settings: {e}"))?
        };

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
            // Portable mode never swaps workers on UAC; nothing to
            // preapprove (the daemon's accept-state cache stays empty
            // for the entire process lifetime).
            preapproved_connections: Vec::new(),
            // Media transport is in-process below; no named pipe needed.
            media_pipe_name: None,
        };

        // Build the three in-process transports:
        // - bidirectional event pair (daemon ↔ worker)
        // - uni-directional media (worker → daemon)
        let (s2w_tx, s2w_rx) = inprocess::make_event::<ServiceToWorker>();
        let (w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
        let (media_tx, media_rx) = inprocess::make_media();

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

        // Spawn the worker on `actix_web::rt::spawn` because
        // `WorkerSession::run_with_transports` awaits actix-web internals
        // (`DeskSession`, `awc::Client`, `actix_web::rt::spawn` from
        // signaling handlers) which all require a `LocalSet` context.
        // `tokio::spawn` would fail with "spawn_local called from
        // outside of a `task::LocalSet`".
        let worker_args = args;
        let init_for_worker = init_payload;
        let hub = host_control_hub;
        actix_web::rt::spawn(async move {
            let session = crate::worker::session::WorkerSession::new();
            if let Err(e) = session
                .run_with_transports(init_for_worker, s2w_rx, w2s_tx, Some(media_tx), Some(hub))
                .await
            {
                error!("In-process worker exited with error: {e}");
            }
            info!("In-process worker task exited");
            let _ = worker_args; // reserved for future per-mode toggles
        });

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
        });

        info!("In-process worker started for session {session_id}");
        Ok(())
    }

    /// Stash the worker's last reported [`MediaCapabilities`]. Called
    /// from `signaling_proxy` whenever the worker emits
    /// `WorkerToService::Capabilities`. Subsequent `RequestRemote`
    /// handling uses the snapshot to populate the Init reply.
    pub fn set_worker_capabilities(&self, caps: MediaCapabilities) {
        *self.worker_capabilities.lock().unwrap() = Some(caps);
    }

    /// Take a snapshot of the latest reported worker capabilities.
    /// Returns `None` until the worker has sent Capabilities at least
    /// once after Init; in that window the daemon ships an empty
    /// device list, which is the same behaviour as Arch III on first
    /// connection.
    pub fn worker_capabilities(&self) -> Option<MediaCapabilities> {
        self.worker_capabilities.lock().unwrap().clone()
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

    /// Insert a fresh entry on first `RequestRemote` observation. Idempotent
    /// — re-observing a known id keeps the existing accept-state, so a
    /// browser that drops then re-issues `RequestRemote` mid-session does
    /// not silently lose its prior approvals.
    pub fn track_browser_connection(&self, connection_id: String) {
        self.active_connections
            .lock()
            .unwrap()
            .entry(connection_id)
            .or_default();
    }

    /// Replace the cached accept-state for a connection. No-op if the id is
    /// unknown (race with browser drop / unrelated worker chatter).
    pub fn update_connection_accept(&self, connection_id: &str, state: ConnectionAcceptState) {
        let mut map = self.active_connections.lock().unwrap();
        if let Some(slot) = map.get_mut(connection_id) {
            *slot = state;
        }
    }

    /// Drop the cached entry for a connection. Called when the worker
    /// reports `WorkerToService::ConnectionClosed`. Bounds memory growth on
    /// long-running daemons across many connect/disconnect cycles.
    pub fn remove_connection(&self, connection_id: &str) {
        self.active_connections
            .lock()
            .unwrap()
            .remove(connection_id);
    }

    /// Aggregator-only test seam — read the current accept-state without
    /// mutating. Production code should not need this.
    #[cfg(test)]
    pub fn connection_accept_state(&self, connection_id: &str) -> Option<ConnectionAcceptState> {
        self.active_connections
            .lock()
            .unwrap()
            .get(connection_id)
            .copied()
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

    /// Drain the per-connection cache, pause every PC's media ingestion,
    /// and tell the active worker to begin shutting down its encoders.
    /// Returns the drained `(connection_id, accept_state)` tuples so the
    /// caller can hand them to the next worker via
    /// `start_worker(..., preapproved)`.
    ///
    /// **PR 6 — keep-PC semantics**: this no longer emits browser-facing
    /// `SignalingType::DesktopSwitching`. Arch IV holds the WebRTC PC in
    /// the daemon, so worker swaps are invisible to the browser apart
    /// from a brief frame-freeze that resolves on the new worker's
    /// first IDR (the per-PC `media_paused` flag set here gates frames
    /// from the dying worker until the new worker reports
    /// `Capabilities` and `pc_registry.resume_active_media` re-issues
    /// `StartMedia` + `ForceKeyframe`).
    pub async fn notify_desktop_switch(&self) -> Vec<(String, ConnectionAcceptState)> {
        let preapproved: Vec<(String, ConnectionAcceptState)> = {
            let mut map = self.active_connections.lock().unwrap();
            map.drain().collect()
        };

        // PR 6: pause all PCs so write_video_frame drops samples the
        // about-to-die worker is still producing while the browser PC
        // keeps its existing reference frame. The first IDR from the
        // replacement worker clears each per-PC flag in place.
        self.pc_registry.pause_all_media().await;

        let inner = self.inner.lock().await;
        if let Some(worker) = &inner.active_worker {
            let _ = worker.ipc_tx.send(ServiceToWorker::DesktopSwitching);
        }

        preapproved
    }

    pub fn handle_crash_recovery(&self, session_id: u32, desktop_name: Option<String>) {
        warn!("[WorkerManager] Worker exited unexpectedly — restarting (session={session_id})");
        let mgr = self.clone();
        // Must use tokio::spawn (not actix_web::rt::spawn / spawn_local) because this
        // is called from within a tokio::spawn task (run_pipe_server) which has no
        // LocalSet; calling spawn_local there panics and silently kills the task.
        tokio::spawn(async move {
            let preapproved = mgr.notify_desktop_switch().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = mgr
                .start_worker(session_id, desktop_name, preapproved)
                .await
            {
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

/// Restricted desktops whose DACL refuses ordinary user tokens; capturing
/// them needs the daemon's own SYSTEM token re-targeted to the user's
/// session. Right now only Windows' UAC secure desktop qualifies.
#[cfg(target_os = "windows")]
fn desktop_requires_system_token(desktop_name: Option<&str>) -> bool {
    matches!(
        desktop_name,
        Some(name) if name == crate::worker::desktop_monitor::RESTRICTED_DESKTOP_NAME
    )
}

/// Watchdog decision: should we declare the worker stuck and trigger
/// a restart? Pulled into a free function so the timing semantics
/// can be exercised without spawning a real watchdog task.
///
/// Returns `false` when the watchdog is disabled (operator-controlled
/// debug aid: hung worker stays alive long enough to capture a
/// stack trace) or when the elapsed time hasn't yet exceeded the
/// configured timeout. The strict `>` (not `>=`) keeps boundary
/// behaviour predictable when timeout is set to a round number
/// equal to the heartbeat interval.
pub(crate) fn worker_is_stale(
    enabled: bool,
    timeout: Duration,
    elapsed_since_heartbeat: Duration,
) -> bool {
    enabled && elapsed_since_heartbeat > timeout
}

#[cfg(target_os = "windows")]
fn launch_worker_as_user(
    session_id: u32,
    desktop_name: Option<&str>,
    cmd_line: &str,
    force_system_token: bool,
) -> Result<NativeWindowsChild, Box<dyn std::error::Error + Send + Sync>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            DuplicateTokenEx, SecurityIdentification, SecurityImpersonation, SetTokenInformation,
            TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            RemoteDesktop::WTSQueryUserToken,
            Threading::{
                CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
                GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
    };

    info!(
        "CreateProcessAsUserW: session={session_id}, desktop={desktop_name:?}, \
         force_system_token={force_system_token}"
    );

    unsafe {
        let mut user_token = HANDLE::default();
        let use_system_token = if force_system_token {
            // Skip WTSQueryUserToken entirely — even a successful user
            // token cannot open Winlogon, so the only viable path is the
            // SYSTEM token with `SetTokenInformation(TokenSessionId)`.
            info!(
                "Forcing SYSTEM token launch path for desktop={desktop_name:?} \
                 (user-token DACL would deny access)"
            );
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut user_token)
                .map_err(|e| format!("OpenProcessToken: {e}"))?;
            true
        } else {
            match WTSQueryUserToken(session_id, &mut user_token) {
                Ok(()) => {
                    info!("WTSQueryUserToken succeeded for session {session_id}");

                    use windows::Win32::Security::{
                        GetTokenInformation, TOKEN_LINKED_TOKEN, TokenLinkedToken,
                    };
                    let mut linked_token = TOKEN_LINKED_TOKEN::default();
                    let mut return_length = 0;
                    let res = GetTokenInformation(
                        user_token,
                        TokenLinkedToken,
                        Some(&mut linked_token as *mut _ as *mut std::ffi::c_void),
                        std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                        &mut return_length,
                    );
                    if res.is_ok() && !linked_token.LinkedToken.is_invalid() {
                        info!(
                            "Successfully retrieved LinkedToken (elevated token) for session {session_id}"
                        );
                        let _ = CloseHandle(user_token);
                        user_token = linked_token.LinkedToken;
                    } else {
                        info!("Could not retrieve LinkedToken, using default user token");
                    }

                    false
                }
                Err(e) => {
                    warn!(
                        "WTSQueryUserToken failed (session={session_id}): {e}, \
                         falling back to SYSTEM token with SessionId injection"
                    );
                    OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut user_token)
                        .map_err(|e| format!("OpenProcessToken: {e}"))?;
                    true
                }
            }
        };

        let mut dup_token = HANDLE::default();
        let dup_result = DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            if use_system_token {
                SecurityImpersonation
            } else {
                SecurityIdentification
            },
            TokenPrimary,
            &mut dup_token,
        );
        let _ = CloseHandle(user_token);
        dup_result.map_err(|e| format!("DuplicateTokenEx: {e}"))?;

        // When using SYSTEM token, inject the target Session ID so the worker
        // process is associated with the correct user session / desktop.
        if use_system_token {
            let mut target_session_id = session_id;
            let set_result = SetTokenInformation(
                dup_token,
                TokenSessionId,
                &mut target_session_id as *mut _ as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            if let Err(e) = set_result {
                let _ = CloseHandle(dup_token);
                return Err(
                    format!("SetTokenInformation(TokenSessionId={session_id}): {e}").into(),
                );
            }
            info!("Set SYSTEM token SessionId to {session_id}");
        }

        let mut env_block: *mut std::ffi::c_void = std::ptr::null_mut();
        let env_ok = CreateEnvironmentBlock(&mut env_block, Some(dup_token), false);
        let env_ptr: Option<*const std::ffi::c_void> = if env_ok.is_ok() {
            Some(env_block as *const _)
        } else {
            warn!("CreateEnvironmentBlock failed, proceeding without user env");
            None
        };

        let desktop_str = match desktop_name {
            Some(n) => format!("WinSta0\\{n}"),
            None => "WinSta0\\Default".to_string(),
        };
        let mut desktop_wide: Vec<u16> = OsStr::new(&desktop_str)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = windows::core::PWSTR(desktop_wide.as_mut_ptr());

        let mut pi = PROCESS_INFORMATION::default();
        let mut cmd_wide: Vec<u16> = OsStr::new(cmd_line)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let create_result = CreateProcessAsUserW(
            Some(dup_token),
            None,
            Some(windows::core::PWSTR(cmd_wide.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            env_ptr,
            None,
            &si,
            &mut pi,
        );

        if let Some(ptr) = env_ptr {
            let _ = DestroyEnvironmentBlock(ptr);
        }
        let _ = CloseHandle(dup_token);

        create_result.map_err(|e| format!("CreateProcessAsUserW: {e}"))?;

        info!(
            "Worker process created: PID={}, desktop={desktop_str}, system_token_fallback={use_system_token}",
            pi.dwProcessId
        );

        let _ = CloseHandle(pi.hThread);
        Ok(NativeWindowsChild::new(pi.hProcess, pi.dwProcessId))
    }
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
async fn run_pipe_server(
    pipe_name: &str,
    session_id: u32,
    desktop_name: Option<String>,
    config_json: String,
    mut cmd_rx: mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: mpsc::UnboundedSender<WorkerToService>,
    preapproved: Vec<(String, ConnectionAcceptState)>,
    worker_mgr: WorkerManager,
    host_upstream_url: String,
    ipc_token: Option<String>,
    pc_registry: PcRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
    info!("Creating Named Pipe server: {pipe_path}");

    // Look up the SID owning the target session so the pipe ACL grants
    // access only to SYSTEM + Administrators + that user. A failure to
    // resolve falls back to SY+BA only (never to "Everyone") — see
    // `pipe_security::query_session_user_sid` for the contract.
    let allowed_user_sid = match crate::daemon::pipe_security::query_session_user_sid(session_id) {
        Ok(sid) => sid,
        Err(e) => {
            warn!(
                "Failed to query user SID for session {session_id}: {e}; \
                 falling back to SY+BA-only pipe ACL"
            );
            None
        }
    };
    let sddl_str = crate::daemon::pipe_security::build_pipe_sddl(allowed_user_sid.as_deref());
    info!("Pipe ACL SDDL = '{sddl_str}'");

    let server = create_named_pipe_with_sddl(&pipe_path, &sddl_str)?;

    // Arch IV cut 4: pre-create the secondary "media" pipe under the
    // same ACL so it exists by the time the worker (which receives the
    // pipe name in Init) tries to connect. Creating both up-front means
    // the worker never races against pipe creation; it only ever races
    // against connect.
    let media_pipe_name = format!("{pipe_name}-media");
    let media_pipe_path = format!(r"\\.\pipe\{media_pipe_name}");
    let media_server = create_named_pipe_with_sddl(&media_pipe_path, &sddl_str)?;

    let desktop_name_copy = desktop_name.clone();

    info!("Waiting for Worker to connect on {pipe_path}...");
    match tokio::time::timeout(Duration::from_secs(15), server.connect()).await {
        Ok(Ok(())) => info!("Worker connected"),
        Ok(Err(e)) => {
            error!("Pipe connection error for {pipe_path}: {e}");
            worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
            return Ok(());
        }
        Err(_) => {
            warn!("Timed out waiting for worker to connect on {pipe_path}; triggering recovery");
            worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
            return Ok(());
        }
    }

    let (mut reader, mut writer) = tokio::io::split(server);

    match read_message::<_, WorkerToService>(&mut reader).await? {
        WorkerToService::Ready => info!("Worker reported Ready"),
        other => warn!("Expected Ready, got: {other:?}"),
    }

    // Re-seed the daemon's per-connection cache from `preapproved` BEFORE
    // sending Init. The new worker will emit `ConnectionAcceptStateChanged`
    // for each restored connection during PC creation, which arrives over
    // IPC and updates the cache to the worker's new (post-restart)
    // authoritative state — but we still want the cache to be populated in
    // the meantime so a quick desktop re-switch right after restart still
    // ships state forward.
    {
        let mut map = worker_mgr.active_connections.lock().unwrap();
        for (id, state) in &preapproved {
            map.insert(id.clone(), *state);
        }
    }

    write_message(
        &mut writer,
        &ServiceToWorker::Init(WorkerInitPayload {
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name,
            config_json,
            signaling_url: None,
            auth_token: ipc_token,
            host_upstream_url: Some(host_upstream_url),
            preapproved_connections: preapproved.clone(),
            media_pipe_name: Some(media_pipe_name.clone()),
        }),
    )
    .await?;
    info!("Sent Init to Worker (media_pipe_name={})", media_pipe_name);

    // Wait for the worker to dial back on the media pipe. The connect
    // timeout is generous because some workers (Winlogon under SYSTEM
    // token) take longer to spin up their media producer; on timeout we
    // proceed *without* media so the rest of the IPC continues to work,
    // and surface a warning so operators know media frames will not flow
    // for this worker.
    let media_handle =
        match tokio::time::timeout(Duration::from_secs(15), media_server.connect()).await {
            Ok(Ok(())) => {
                info!("Worker connected on media pipe {media_pipe_path}");
                let (media_reader, _media_writer) = tokio::io::split(media_server);
                let receiver = framed::make_media_receiver(media_reader);
                Some(spawn_media_receiver_task(receiver, pc_registry.clone()))
            }
            Ok(Err(e)) => {
                warn!(
                    "Media pipe connect failed for {media_pipe_path}: {e}; \
                 worker will run without media transport (no video frames will flow)"
                );
                None
            }
            Err(_) => {
                warn!(
                    "Timed out waiting for worker on media pipe {media_pipe_path}; \
                 worker will run without media transport"
                );
                None
            }
        };

    // PR 6 keep-PC semantics: browser-facing `SignalingType::DesktopReady`
    // is no longer emitted on worker (re)spawn. The browser's WebRTC PC
    // stays up across worker swaps; the daemon's `signaling_proxy` calls
    // `pc_registry.resume_active_media` on the worker's first
    // `Capabilities` to re-issue cached `StartMedia` + `ForceKeyframe`,
    // and the per-PC `media_paused` flag clears on the first IDR.
    let _ = &preapproved; // retained for the in-flight transition; PR 7 strips the field.

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, pipe_name).await;
    info!("Pipe server for {pipe_name} exiting");

    // Stop the media receiver so its read loop doesn't keep a reference
    // to the now-dead worker pipe alive.
    if let Some(handle) = media_handle {
        handle.abort();
    }

    if !expected {
        worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
    }

    Ok(())
}

/// Build a `tokio::net::windows::named_pipe::NamedPipeServer` whose
/// DACL is derived from the supplied SDDL string. Pulled out so the
/// event pipe and the Arch IV media pipe share exactly the same ACL
/// path — the security analysis in `pipe_security` covers both.
#[cfg(target_os = "windows")]
fn create_named_pipe_with_sddl(
    pipe_path: &str,
    sddl_str: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ServerOptions;
    unsafe {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows_core::PCWSTR;

        let sddl_w: Vec<u16> = std::ffi::OsStr::new(sddl_str)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut sd: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            1, // SDDL_REVISION_1
            &mut sd,
            None,
        )
        .is_err()
        {
            return Err("Failed to convert SDDL to Security Descriptor".into());
        }

        let mut sa = SECURITY_ATTRIBUTES::default();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.lpSecurityDescriptor = sd.0 as *mut c_void;
        sa.bInheritHandle = windows::Win32::Foundation::FALSE;

        let srv_res = ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(pipe_path, &mut sa as *mut _ as *mut c_void);

        let _ = LocalFree(Some(HLOCAL(sd.0)));
        Ok(srv_res?)
    }
}

/// Spawn the daemon-side media receiver. Owns a [`MediaReceiver`] (already
/// constructed by the caller — `framed::make_media_receiver` for named-pipe
/// mode, `inprocess::make_media` for the in-process portable path), decodes
/// each [`MediaFrame`] and forwards to
/// [`crate::daemon::pc_manager::write_video_frame`] for
/// `track.write_sample(...)`. Exits when `recv_frame` returns `None`
/// (transport closed).
fn spawn_media_receiver_task(
    mut receiver: Box<dyn MediaReceiver>,
    pc_registry: PcRegistry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("[MediaReceiver] starting");
        while let Some(frame) = receiver.recv_frame().await {
            debug!(
                "[MediaReceiver] frame seq={} kind={:?} len={} for {}",
                frame.seq,
                frame.kind,
                frame.payload.len(),
                frame.connection_id
            );
            crate::daemon::pc_manager::write_video_frame(&pc_registry, frame).await;
        }
        info!("[MediaReceiver] exiting (transport closed)");
    })
}

#[cfg(not(target_os = "windows"))]
#[allow(clippy::too_many_arguments)]
async fn run_pipe_server(
    socket_path: &str,
    session_id: u32,
    desktop_name: Option<String>,
    config_json: String,
    mut cmd_rx: mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: mpsc::UnboundedSender<WorkerToService>,
    preapproved: Vec<(String, ConnectionAcceptState)>,
    worker_mgr: WorkerManager,
    host_upstream_url: String,
    ipc_token: Option<String>,
    _pc_registry: PcRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    info!("Creating Unix socket server: {socket_path}");
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;

    let desktop_name_copy = desktop_name.clone();

    info!("Waiting for Worker to connect...");
    let stream = match tokio::time::timeout(Duration::from_secs(15), listener.accept()).await {
        Ok(Ok((stream, _))) => {
            info!("Worker connected");
            stream
        }
        Ok(Err(e)) => {
            error!("Unix socket accept error for {socket_path}: {e}");
            worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
            let _ = std::fs::remove_file(socket_path);
            return Ok(());
        }
        Err(_) => {
            warn!("Timed out waiting for worker to connect on {socket_path}; triggering recovery");
            worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
            let _ = std::fs::remove_file(socket_path);
            return Ok(());
        }
    };

    let (mut reader, mut writer) = tokio::io::split(stream);

    match read_message::<_, WorkerToService>(&mut reader).await? {
        WorkerToService::Ready => info!("Worker reported Ready"),
        other => warn!("Expected Ready, got: {other:?}"),
    }

    {
        let mut map = worker_mgr.active_connections.lock().unwrap();
        for (id, state) in &preapproved {
            map.insert(id.clone(), *state);
        }
    }

    write_message(
        &mut writer,
        &ServiceToWorker::Init(WorkerInitPayload {
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name,
            config_json,
            signaling_url: None,
            auth_token: ipc_token,
            host_upstream_url: Some(host_upstream_url),
            preapproved_connections: preapproved.clone(),
            // Arch IV media pipe wiring lands in PR 2 cut 4. Until then
            // the worker stays single-pipe (Arch III).
            media_pipe_name: None,
        }),
    )
    .await?;

    // PR 6 keep-PC: see the Windows path above; browser-facing
    // DesktopReady is no longer emitted on worker spawn.
    let _ = &preapproved;

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, socket_path).await;
    let _ = std::fs::remove_file(socket_path);

    if !expected {
        worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
    }

    Ok(())
}

/// Named-pipe / Unix-socket bridge: wrap the byte-stream halves in framed
/// event transports and delegate to [`bridge_event_transport`]. The
/// transport-agnostic main loop is shared with the in-process portable
/// path so behavioural differences (cmd → wire, wire → msg, daemon-
/// initiated vs unexpected exit) live in exactly one place.
async fn bridge_loop<R, W>(
    reader: R,
    writer: W,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &mpsc::UnboundedSender<WorkerToService>,
    name: &str,
) -> bool
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let event_tx: Arc<dyn EventSender<ServiceToWorker>> = framed::spawn_event_sender(writer);
    let event_rx: Box<dyn EventReceiver<WorkerToService>> = framed::make_event_receiver(reader);
    bridge_event_transport(event_rx, event_tx, cmd_rx, msg_tx, name).await
}

/// Transport-agnostic bridge between the daemon's internal mpsc channels
/// (`cmd_rx` for daemon → worker; `msg_tx` for worker → daemon) and the
/// supplied event transport pair. Returns `true` when the daemon initiated
/// the shutdown (Shutdown / DesktopSwitching command sent or cmd channel
/// closed) and `false` when the worker side dropped first — the caller
/// uses this to decide whether to trigger crash-recovery.
async fn bridge_event_transport(
    mut event_rx: Box<dyn EventReceiver<WorkerToService>>,
    event_tx: Arc<dyn EventSender<ServiceToWorker>>,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &mpsc::UnboundedSender<WorkerToService>,
    name: &str,
) -> bool {
    let (worker_msg_tx, mut worker_msg_rx) = mpsc::unbounded_channel::<Option<WorkerToService>>();
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Some(m) => {
                    if worker_msg_tx.send(Some(m)).is_err() {
                        break;
                    }
                }
                None => {
                    let _ = worker_msg_tx.send(None);
                    break;
                }
            }
        }
    });

    let mut daemon_initiated = false;
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(msg) => {
                        if matches!(
                            msg,
                            ServiceToWorker::Shutdown | ServiceToWorker::DesktopSwitching
                        ) {
                            daemon_initiated = true;
                        }
                        if let Err(e) = event_tx.send(msg).await {
                            error!("Failed to send to Worker [{name}]: {e}");
                            break;
                        }
                    }
                    None => {
                        info!("Command channel closed for [{name}], shutting down");
                        daemon_initiated = true;
                        break;
                    }
                }
            }
            msg_result = worker_msg_rx.recv() => {
                match msg_result {
                    Some(Some(msg)) => {
                        if msg_tx.send(msg).is_err() {
                            error!("SignalingProxy receiver dropped for [{name}]");
                            break;
                        }
                    }
                    Some(None) => {
                        info!("Worker event transport closed for [{name}]");
                        break;
                    }
                    None => {
                        info!("Worker reader task stopped for [{name}]");
                        break;
                    }
                }
            }
        }
    }
    daemon_initiated
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    /// `desktop_requires_system_token` is the gate the launch path uses to
    /// pick between user-token (`WTSQueryUserToken`) and SYSTEM-token
    /// (`OpenProcessToken` + `SetTokenInformation(TokenSessionId)`) paths.
    /// The classification has to stay tight: any false positive would
    /// downgrade an ordinary worker to SYSTEM (loses user profile / network
    /// drives); any false negative would route a Winlogon launch through
    /// the user token and `CreateProcessAsUserW` would fail with
    /// ERROR_ACCESS_DENIED.
    #[test]
    fn winlogon_requires_system_token() {
        assert!(desktop_requires_system_token(Some("Winlogon")));
    }

    #[test]
    fn ordinary_desktops_do_not_require_system_token() {
        assert!(!desktop_requires_system_token(Some("Default")));
        assert!(!desktop_requires_system_token(Some("Screen-saver")));
        assert!(!desktop_requires_system_token(None));
    }

    /// Case-sensitive: Windows desktop names are conventionally fixed-case
    /// and our `desktop_monitor::names_equal` is strict. Aligning with that
    /// keeps the routing decision consistent with the detection side.
    #[test]
    fn winlogon_check_is_case_sensitive() {
        assert!(!desktop_requires_system_token(Some("winlogon")));
        assert!(!desktop_requires_system_token(Some("WINLOGON")));
    }

    /// When the operator disabled the watchdog (debug aid), even an
    /// indefinitely-stale heartbeat must not trigger a restart — that's
    /// the entire point of the toggle.
    #[test]
    fn disabled_watchdog_never_fires() {
        assert!(!worker_is_stale(
            false,
            Duration::from_secs(30),
            Duration::from_secs(0),
        ));
        assert!(!worker_is_stale(
            false,
            Duration::from_secs(30),
            Duration::from_secs(3600),
        ));
    }

    /// Heartbeats are 5s apart and timeout defaults to 30s; healthy
    /// elapsed values should not trip the watchdog.
    #[test]
    fn fresh_heartbeat_does_not_fire() {
        assert!(!worker_is_stale(
            true,
            Duration::from_secs(30),
            Duration::from_secs(0),
        ));
        assert!(!worker_is_stale(
            true,
            Duration::from_secs(30),
            Duration::from_secs(5),
        ));
        assert!(!worker_is_stale(
            true,
            Duration::from_secs(30),
            Duration::from_secs(29),
        ));
    }

    /// Construct a bare WorkerManager for unit testing the connection-state
    /// API. Settings are defaulted (none of these tests touch the watchdog
    /// or settings hot-reread).
    fn test_manager() -> (WorkerManager, WorkerMessageReceiver) {
        let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
            crate::model::settings::Settings::default(),
        )));
        WorkerManager::new(settings, PcRegistry::new())
    }

    /// Track-then-update-then-drain round trip — the path used by
    /// `signaling_proxy` (track on RequestRemote, update on
    /// ConnectionAcceptStateChanged, drain on desktop switch).
    #[tokio::test]
    async fn track_update_drain_round_trip() {
        let (mgr, _rx) = test_manager();

        mgr.track_browser_connection("conn-1".to_string());
        mgr.update_connection_accept(
            "conn-1",
            ConnectionAcceptState {
                accept_control: true,
                accept_clipboard_sync: true,
            },
        );

        let drained = mgr.notify_desktop_switch().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, "conn-1");
        assert!(drained[0].1.accept_control);
        assert!(drained[0].1.accept_clipboard_sync);

        // Second drain after first must be empty — `notify_desktop_switch`
        // owns the drain side-effect.
        let drained_again = mgr.notify_desktop_switch().await;
        assert!(drained_again.is_empty());
    }

    /// PR 6 keep-PC: `notify_desktop_switch` must NOT emit a
    /// browser-facing `WorkerToService::SignalingMessage`. The Arch
    /// III code shipped a `SignalingType::DesktopSwitching` over the
    /// outbound channel for each tracked connection; with PR 6 the
    /// browser PC stays up across the swap and the daemon resumes
    /// media itself once the new worker reports `Capabilities`.
    #[tokio::test]
    async fn notify_desktop_switch_keep_pc_does_not_emit_browser_signaling() {
        let (mgr, mut rx) = test_manager();
        mgr.track_browser_connection("conn-1".to_string());
        mgr.track_browser_connection("conn-2".to_string());

        let drained = mgr.notify_desktop_switch().await;
        assert_eq!(drained.len(), 2, "drain still returns the cached entries");

        // No SignalingMessage should land on the worker-message channel.
        // try_recv returns Empty when the channel has nothing buffered.
        match rx.try_recv() {
            Err(mpsc::error::TryRecvError::Empty) => {}
            other => panic!("PR 6 keep-PC must not emit browser signaling; got {other:?}"),
        }
    }

    /// PR 6 keep-PC: `notify_desktop_switch` pauses every PC in the
    /// registry it was constructed with. This is the contract the
    /// daemon relies on so frames from the about-to-die worker are
    /// dropped instead of pushed to the browser with stale references.
    #[tokio::test]
    async fn notify_desktop_switch_pauses_all_pcs() {
        use crate::daemon::pc_manager::PcRegistry;
        use desk_signal_facade::model::signal::RequestRemoteModel;

        let pc_registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
        };
        let mut s = crate::model::settings::Settings::default();
        s.args.startup_mode = crate::model::settings::StartupMode::ServiceDaemon;
        for id in ["pc-a", "pc-b"] {
            pc_registry
                .create_for_request_remote(id, &request_remote, &s)
                .await
                .expect("create");
        }

        let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(s)));
        let (mgr, _rx) = WorkerManager::new(settings, pc_registry.clone());

        // Pre-condition: nothing paused.
        for id in ["pc-a", "pc-b"] {
            let ctx = pc_registry.get(id).await.unwrap();
            assert!(
                !ctx.read()
                    .await
                    .media_paused
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        }

        let _drained = mgr.notify_desktop_switch().await;

        // Post-condition: every PC is paused.
        for id in ["pc-a", "pc-b"] {
            let ctx = pc_registry.get(id).await.unwrap();
            assert!(
                ctx.read()
                    .await
                    .media_paused
                    .load(std::sync::atomic::Ordering::Relaxed),
                "notify_desktop_switch must pause {id}"
            );
        }
    }

    /// Capabilities round-trip: `set_worker_capabilities` stores the
    /// snapshot and `worker_capabilities()` returns it. The daemon's
    /// signaling_proxy relies on this to bridge `WorkerToService::
    /// Capabilities` into the `RequestRemote` Init reply path.
    #[tokio::test]
    async fn worker_capabilities_round_trip() {
        let (mgr, _rx) = test_manager();
        assert!(
            mgr.worker_capabilities().is_none(),
            "capabilities are None until the worker reports"
        );
        let caps = MediaCapabilities {
            video_codecs: vec![
                desk_ipc_protocol::message::MediaCodec::H264,
                desk_ipc_protocol::message::MediaCodec::Vp9,
            ],
            audio_codecs: vec![desk_ipc_protocol::message::MediaCodec::Opus],
            video_devices: vec!["display-1".to_string()],
            audio_devices: vec!["mic-1".to_string()],
            has_tauri: true,
            is_admin: false,
            desktop_name: "Default".to_string(),
        };
        mgr.set_worker_capabilities(caps.clone());
        let got = mgr.worker_capabilities().expect("capabilities present");
        assert_eq!(got.video_codecs, caps.video_codecs);
        assert_eq!(got.audio_codecs, caps.audio_codecs);
        assert_eq!(got.desktop_name, "Default");
        assert!(got.has_tauri);
    }

    /// `update_connection_accept` on an unknown id is a silent no-op (race
    /// against browser disconnect / unrelated worker chatter).
    #[test]
    fn update_unknown_id_is_noop() {
        let (mgr, _rx) = test_manager();
        mgr.update_connection_accept(
            "ghost",
            ConnectionAcceptState {
                accept_control: true,
                accept_clipboard_sync: true,
            },
        );
        assert!(mgr.connection_accept_state("ghost").is_none());
    }

    /// `track_browser_connection` is idempotent: re-tracking a known id
    /// must NOT clobber the existing accept-state. This guards against a
    /// browser quickly disconnecting and re-issuing RequestRemote within
    /// the same worker lifetime — its prior approvals would otherwise be
    /// silently downgraded.
    #[test]
    fn track_is_idempotent_keeps_existing_state() {
        let (mgr, _rx) = test_manager();
        mgr.track_browser_connection("conn-1".to_string());
        mgr.update_connection_accept(
            "conn-1",
            ConnectionAcceptState {
                accept_control: true,
                accept_clipboard_sync: false,
            },
        );

        // Re-track with the same id.
        mgr.track_browser_connection("conn-1".to_string());

        let after = mgr
            .connection_accept_state("conn-1")
            .expect("entry must still exist");
        assert!(after.accept_control, "accept_control must not be reset");
        assert!(!after.accept_clipboard_sync);
    }

    /// `remove_connection` drops the entry. Subsequent `notify_desktop_switch`
    /// does not include it. This is the path used by
    /// `WorkerToService::ConnectionClosed` to bound memory growth.
    #[tokio::test]
    async fn remove_drops_entry_from_drain() {
        let (mgr, _rx) = test_manager();
        mgr.track_browser_connection("alive".to_string());
        mgr.track_browser_connection("doomed".to_string());

        mgr.remove_connection("doomed");

        let drained = mgr.notify_desktop_switch().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, "alive");
    }

    /// Boundary: strictly greater than. Setting timeout exactly equal
    /// to a round multiple of the heartbeat interval shouldn't cause
    /// jitter-driven false fires.
    #[test]
    fn heartbeat_at_exactly_timeout_does_not_fire() {
        assert!(!worker_is_stale(
            true,
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
    }

    /// Once elapsed exceeds the timeout the watchdog must report
    /// stuck — this is the entire reason the watchdog exists.
    #[test]
    fn stale_heartbeat_fires_when_enabled() {
        assert!(worker_is_stale(
            true,
            Duration::from_secs(30),
            Duration::from_secs(31),
        ));
        assert!(worker_is_stale(
            true,
            Duration::from_secs(30),
            Duration::from_secs(120),
        ));
    }
}

/// Cross-platform tests for the transport-agnostic bridge — exercises the
/// in-process `EventSender` / `EventReceiver` path the PR 5 portable mode
/// uses without needing Windows named pipes or a daemon process. The
/// Windows-only `tests` module above stays gated because it pulls in
/// `WTSQueryUserToken` / Windows token APIs.
#[cfg(test)]
mod bridge_tests {
    use super::*;
    use desk_ipc_protocol::dual_transport::inprocess;
    use desk_ipc_protocol::message::{
        ForceKeyframePayload, HeartbeatPayload, ServiceToWorker, WorkerToService,
    };

    /// `bridge_event_transport` shuttles a daemon command (cmd_rx →
    /// EventSender) onto the worker's event transport. Verifies the
    /// happy path before going on to lifecycle tests below.
    #[tokio::test]
    async fn bridge_forwards_cmd_to_worker() {
        let (s2w_tx, mut s2w_rx) = inprocess::make_event::<ServiceToWorker>();
        let (_w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<WorkerToService>();

        let handle = tokio::spawn(async move {
            bridge_event_transport(w2s_rx, s2w_tx, &mut cmd_rx, &msg_tx, "bridge-test").await
        });

        cmd_tx
            .send(ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                connection_id: "c1".to_string(),
            }))
            .expect("cmd send");

        let received = tokio::time::timeout(tokio::time::Duration::from_secs(1), s2w_rx.recv())
            .await
            .expect("worker should receive cmd quickly")
            .expect("transport open");
        assert!(matches!(received, ServiceToWorker::ForceKeyframe(_)));

        // Drop cmd_tx → bridge observes None on cmd channel and exits
        // (daemon-initiated shutdown).
        drop(cmd_tx);
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("bridge must exit on cmd channel close")
            .expect("task did not panic");
        assert!(result, "cmd channel close counts as daemon-initiated");
    }

    /// `bridge_event_transport` forwards worker → daemon messages (worker
    /// EventSender → daemon msg_tx). Daemon-side msg_rx must observe the
    /// payload in order without re-encoding.
    #[tokio::test]
    async fn bridge_forwards_worker_msg_to_daemon() {
        let (s2w_tx, _s2w_rx) = inprocess::make_event::<ServiceToWorker>();
        let (w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<WorkerToService>();

        let handle = tokio::spawn(async move {
            bridge_event_transport(w2s_rx, s2w_tx, &mut cmd_rx, &msg_tx, "bridge-test").await
        });

        w2s_tx
            .send(WorkerToService::Heartbeat(HeartbeatPayload {
                timestamp_ms: 42,
                active_connections: 0,
                cpu_usage: None,
                memory_usage: None,
            }))
            .await
            .expect("worker send");

        let observed = tokio::time::timeout(tokio::time::Duration::from_secs(1), msg_rx.recv())
            .await
            .expect("daemon should observe worker msg")
            .expect("daemon msg channel open");
        match observed {
            WorkerToService::Heartbeat(p) => assert_eq!(p.timestamp_ms, 42),
            other => panic!("expected Heartbeat, got {other:?}"),
        }

        // Drop the worker EventSender (mpsc closes) → bridge observes
        // None on the worker side and exits with `daemon_initiated=false`
        // (worker disconnected first; outer caller would trigger
        // crash-recovery in the named-pipe path).
        drop(w2s_tx);
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("bridge must exit on worker close")
            .expect("task did not panic");
        assert!(
            !result,
            "worker close means worker initiated; daemon should treat as crash"
        );
    }

    /// `Shutdown` command sent by the daemon must mark `daemon_initiated`
    /// even when the worker side is still alive — that's the signal the
    /// named-pipe `run_pipe_server` uses to skip crash-recovery.
    #[tokio::test]
    async fn bridge_shutdown_cmd_marks_daemon_initiated() {
        let (s2w_tx, mut s2w_rx) = inprocess::make_event::<ServiceToWorker>();
        let (_w2s_tx, w2s_rx) = inprocess::make_event::<WorkerToService>();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
        let (msg_tx, _msg_rx) = mpsc::unbounded_channel::<WorkerToService>();

        let handle = tokio::spawn(async move {
            bridge_event_transport(w2s_rx, s2w_tx, &mut cmd_rx, &msg_tx, "bridge-test").await
        });

        cmd_tx
            .send(ServiceToWorker::Shutdown)
            .expect("send Shutdown");

        // Worker side must observe the Shutdown.
        let observed = tokio::time::timeout(tokio::time::Duration::from_secs(1), s2w_rx.recv())
            .await
            .expect("worker should receive Shutdown")
            .expect("transport open");
        assert!(matches!(observed, ServiceToWorker::Shutdown));

        // Drop cmd_tx so the bridge exits (Shutdown doesn't itself break
        // the loop — it just flips the flag; the loop ends on cmd close
        // or worker close).
        drop(cmd_tx);
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("bridge must exit")
            .expect("task did not panic");
        assert!(result, "Shutdown cmd must mark daemon-initiated");
    }
}
