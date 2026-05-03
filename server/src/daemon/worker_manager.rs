use crate::model::settings::SharedSettings;
use actix_web::web;
use desk_ipc_protocol::{
    message::{
        ConnectionAcceptState, ServiceToWorker, SignalingPayload, WorkerInitPayload,
        WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use log::{error, info, warn};
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
    pub fn new(settings: web::Data<SharedSettings>) -> (Self, WorkerMessageReceiver) {
        let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
        let mgr = WorkerManager {
            settings,
            inner: Arc::new(Mutex::new(WorkerManagerInner {
                active_worker: None,
            })),
            worker_msg_tx: Arc::new(tx),
            active_connections: Arc::new(StdMutex::new(HashMap::new())),
        };
        (mgr, rx)
    }

    pub async fn start_worker(
        &self,
        session_id: u32,
        desktop_name: Option<String>,
        preapproved: Vec<(String, ConnectionAcceptState)>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    pub fn update_connection_accept(
        &self,
        connection_id: &str,
        state: ConnectionAcceptState,
    ) {
        let mut map = self.active_connections.lock().unwrap();
        if let Some(slot) = map.get_mut(connection_id) {
            *slot = state;
        }
    }

    /// Drop the cached entry for a connection. Called when the worker
    /// reports `WorkerToService::ConnectionClosed`. Bounds memory growth on
    /// long-running daemons across many connect/disconnect cycles.
    pub fn remove_connection(&self, connection_id: &str) {
        self.active_connections.lock().unwrap().remove(connection_id);
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

    /// Drain the per-connection cache, push `DesktopSwitching` to each
    /// browser, and tell the active worker to begin shutdown. Returns the
    /// drained `(connection_id, accept_state)` tuples so the caller can
    /// hand them to the next worker via `start_worker(..., preapproved)`.
    pub async fn notify_desktop_switch(&self) -> Vec<(String, ConnectionAcceptState)> {
        let preapproved: Vec<(String, ConnectionAcceptState)> = {
            let mut map = self.active_connections.lock().unwrap();
            map.drain().collect()
        };

        for (id, _) in &preapproved {
            if let Some(json) = build_signaling_event_json(SignalingType::DesktopSwitching, id) {
                let _ =
                    self.worker_msg_tx
                        .send(WorkerToService::SignalingMessage(SignalingPayload {
                            message: json,
                            connection_id: None,
                        }));
            }
        }

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

fn build_signaling_event_json(
    signaling_type: SignalingType,
    to_connection_id: &str,
) -> Option<String> {
    let model =
        SignalingModel::new_request::<()>(signaling_type, Some(to_connection_id.to_string()), None)
            .ok()?;
    serde_json::to_string(&model).ok()
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
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ServerOptions;

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

    let server = unsafe {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows_core::PCWSTR;

        // SDDL must be a UTF-16 NUL-terminated buffer.
        let sddl_w: Vec<u16> = std::ffi::OsStr::new(&sddl_str)
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
            .create_with_security_attributes_raw(&pipe_path, &mut sa as *mut _ as *mut c_void);

        let _ = LocalFree(Some(HLOCAL(sd.0)));
        srv_res?
    };

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
        }),
    )
    .await?;
    info!("Sent Init to Worker");

    for (id, _) in &preapproved {
        if let Some(json) = build_signaling_event_json(SignalingType::DesktopReady, id) {
            let _ = msg_tx.send(WorkerToService::SignalingMessage(SignalingPayload {
                message: json,
                connection_id: None,
            }));
        }
    }

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, pipe_name).await;
    info!("Pipe server for {pipe_name} exiting");

    if !expected {
        worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
    }

    Ok(())
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
        }),
    )
    .await?;

    for (id, _) in &preapproved {
        if let Some(json) = build_signaling_event_json(SignalingType::DesktopReady, id) {
            let _ = msg_tx.send(WorkerToService::SignalingMessage(SignalingPayload {
                message: json,
                connection_id: None,
            }));
        }
    }

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, socket_path).await;
    let _ = std::fs::remove_file(socket_path);

    if !expected {
        worker_mgr.handle_crash_recovery(session_id, desktop_name_copy);
    }

    Ok(())
}

async fn bridge_loop<R, W>(
    mut reader: R,
    mut writer: W,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &mpsc::UnboundedSender<WorkerToService>,
    name: &str,
) -> bool
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (worker_msg_tx, mut worker_msg_rx) =
        mpsc::unbounded_channel::<std::io::Result<WorkerToService>>();
    tokio::spawn(async move {
        loop {
            let result = read_message::<_, WorkerToService>(&mut reader).await;
            let should_stop = result.is_err();
            if worker_msg_tx.send(result).is_err() || should_stop {
                break;
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
                        if let Err(e) = write_message(&mut writer, &msg).await {
                            error!("Failed to write to Worker pipe [{name}]: {e}");
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
                    Some(Ok(msg)) => {
                        if msg_tx.send(msg).is_err() {
                            error!("SignalingProxy receiver dropped for [{name}]");
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        if e.kind() == std::io::ErrorKind::UnexpectedEof {
                            info!("Worker disconnected from [{name}]");
                        } else {
                            error!("Pipe read error [{name}]: {e}");
                        }
                        break;
                    }
                    None => {
                        info!("Worker pipe reader stopped for [{name}]");
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
        WorkerManager::new(settings)
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
