use crate::model::settings::SharedSettings;
use actix_web::web;
use desk_ipc_protocol::{
    message::{ServiceToWorker, SignalingPayload, WorkerInitPayload, WorkerToService},
    transport::{read_message, write_message},
};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use log::{error, info, warn};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

#[derive(Clone)]
pub struct WorkerManager {
    settings: web::Data<SharedSettings>,
    inner: Arc<Mutex<WorkerManagerInner>>,
    worker_msg_tx: Arc<mpsc::UnboundedSender<WorkerToService>>,
    active_browser_ids: Arc<Mutex<HashSet<String>>>,
}

struct WorkerManagerInner {
    active_worker: Option<WorkerHandle>,
}

struct WorkerHandle {
    pipe_name: String,
    ipc_tx: mpsc::UnboundedSender<ServiceToWorker>,
    process_handle: Option<ProcessHandle>,
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
            TerminateProcess(self.raw_handle(), 1).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("TerminateProcess: {e}"))
            })
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
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
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
            active_browser_ids: Arc::new(Mutex::new(HashSet::new())),
        };
        (mgr, rx)
    }

    pub async fn start_worker(
        &self,
        session_id: u32,
        desktop_name: Option<String>,
        reconnect_ids: Vec<String>,
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

        let config_json = {
            let settings = self.settings.read().await;
            serde_json::to_string(&*settings)
                .map_err(|e| format!("Failed to serialize settings: {e}"))?
        };

        let worker_msg_tx = Arc::clone(&self.worker_msg_tx);
        let pipe_name_c = pipe_name.clone();
        let desktop_c = desktop_name.clone();
        let config_c = config_json.clone();
        let mgr_c = self.clone();
        tokio::spawn(async move {
            if let Err(e) = run_pipe_server(
                &pipe_name_c,
                session_id,
                desktop_c,
                config_c,
                ipc_cmd_rx,
                (*worker_msg_tx).clone(),
                reconnect_ids,
                mgr_c,
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

    pub async fn track_browser_connection(&self, connection_id: String) {
        self.active_browser_ids.lock().await.insert(connection_id);
    }

    pub async fn notify_desktop_switch(&self) -> Vec<String> {
        let browser_ids: Vec<String> = {
            let mut ids = self.active_browser_ids.lock().await;
            ids.drain().collect()
        };

        for id in &browser_ids {
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

        browser_ids
    }

    pub fn handle_crash_recovery(&self, session_id: u32, desktop_name: Option<String>) {
        warn!("[WorkerManager] Worker exited unexpectedly — restarting (session={session_id})");
        let mgr = self.clone();
        // Must use tokio::spawn (not actix_web::rt::spawn / spawn_local) because this
        // is called from within a tokio::spawn task (run_pipe_server) which has no
        // LocalSet; calling spawn_local there panics and silently kills the task.
        tokio::spawn(async move {
            let browser_ids = mgr.notify_desktop_switch().await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = mgr
                .start_worker(session_id, desktop_name, browser_ids)
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
            match launch_worker_as_user(session_id, desktop_name, &cmd_line) {
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

#[cfg(target_os = "windows")]
fn launch_worker_as_user(
    session_id: u32,
    desktop_name: Option<&str>,
    cmd_line: &str,
) -> Result<NativeWindowsChild, Box<dyn std::error::Error + Send + Sync>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            DuplicateTokenEx, SecurityIdentification, SecurityImpersonation,
            SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
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

    info!("CreateProcessAsUserW: session={session_id}, desktop={desktop_name:?}");

    unsafe {
        let mut user_token = HANDLE::default();
        let use_system_token = match WTSQueryUserToken(session_id, &mut user_token) {
            Ok(()) => {
                info!("WTSQueryUserToken succeeded for session {session_id}");
                
                use windows::Win32::Security::{GetTokenInformation, TokenLinkedToken, TOKEN_LINKED_TOKEN};
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
                    info!("Successfully retrieved LinkedToken (elevated token) for session {session_id}");
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
                return Err(format!("SetTokenInformation(TokenSessionId={session_id}): {e}").into());
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
async fn run_pipe_server(
    pipe_name: &str,
    session_id: u32,
    desktop_name: Option<String>,
    config_json: String,
    mut cmd_rx: mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: mpsc::UnboundedSender<WorkerToService>,
    reconnect_ids: Vec<String>,
    worker_mgr: WorkerManager,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
    info!("Creating Named Pipe server: {pipe_path}");

    let server = unsafe {
        use std::ffi::c_void;
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows::Win32::Security::{SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR};
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows_core::w;

        let mut sd: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();
        // D:(A;;GA;;;WD) = Allow Generic All to Everyone
        let sddl = w!("D:(A;;GA;;;WD)");
        
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            1, // SDDL_REVISION_1
            &mut sd,
            None,
        ).is_err() {
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


    write_message(
        &mut writer,
        &ServiceToWorker::Init(WorkerInitPayload {
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name,
            config_json,
            signaling_url: None,
            auth_token: None,
        }),
    )
    .await?;
    info!("Sent Init to Worker");

    for id in &reconnect_ids {
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
async fn run_pipe_server(
    socket_path: &str,
    session_id: u32,
    desktop_name: Option<String>,
    config_json: String,
    mut cmd_rx: mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: mpsc::UnboundedSender<WorkerToService>,
    reconnect_ids: Vec<String>,
    worker_mgr: WorkerManager,
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


    write_message(
        &mut writer,
        &ServiceToWorker::Init(WorkerInitPayload {
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name,
            config_json,
            signaling_url: None,
            auth_token: None,
        }),
    )
    .await?;

    for id in &reconnect_ids {
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
