//! Root-owned Linux one-shot PTY launcher and systemd scope containment.
//!
//! The ServiceDaemon remains the lifecycle owner. The tiny launcher process is
//! deliberately entered before the normal server runtime: it receives one
//! bounded sealed spec over stdin, stops before the approved executable runs,
//! and is continued only after the daemon proves systemd moved it into the
//! deterministic transient scope.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, Metadata};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use wincode::{SchemaRead, SchemaWrite};
use zbus::zvariant::{OwnedObjectPath, Value};

use crate::daemon::exec_pty_carrier::ExecPtyCarrierRegistry;
use crate::host_control::session_shell::RegisteredSessionShell;
use desk_agent_protocol::exec_pty::{
    MAX_PTY_DATA_FRAME_BYTES, PtyCloseReason, PtyOutputFrame, PtyStreamClosed, PtyStreamOpened,
};
use desk_agent_protocol::exec_pty_wire::PtyWireFrame;
use desk_agent_protocol::{AgentErrorKind, AgentOutcome};
use desk_ipc_protocol::message::ExecPtyStartPayload;
use tokio::io::AsyncWriteExt as _;

const LAUNCH_PROTOCOL_VERSION: u16 = 1;
const MAX_LAUNCH_SPEC_BYTES: usize = 4 * 1024 * 1024;
const MAX_LAUNCH_ARG_BYTES: usize = 128 * 1024;
const MAX_LAUNCH_ARGS: usize = 4_096;
const MAX_LAUNCH_ENV_ENTRIES: usize = 4_096;
const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);
const READY_MARKER: &[u8] = b"LRD-PTY-READY\x01";
const RESULT_REDACTION_MARGIN: usize = 8 * 1024;
const STOP_NONE: u8 = 0;
const STOP_CARRIER: u8 = 2;
const STOP_SLOW: u8 = 3;
const STOP_STALE: u8 = 4;
const STOP_INTERNAL: u8 = 5;

static RUNTIME: OnceLock<Arc<LinuxExecPtySupervisor>> = OnceLock::new();

#[derive(Debug, Clone, SchemaWrite, SchemaRead)]
struct LaunchSpec {
    protocol_version: u16,
    parent_pid: u32,
    executable: Vec<u8>,
    argv0: Vec<u8>,
    argv: Vec<Vec<u8>>,
    cwd: Vec<u8>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    uid: u32,
    gid: u32,
    supplementary_groups: Vec<u32>,
    umask: u32,
    slave_path: Vec<u8>,
}

impl LaunchSpec {
    fn validate(&self) -> Result<(), String> {
        if self.protocol_version != LAUNCH_PROTOCOL_VERSION {
            return Err("unsupported launcher protocol version".into());
        }
        if self.parent_pid == 0 || self.parent_pid != unsafe { libc::getppid() } as u32 {
            return Err("launcher parent identity changed".into());
        }
        if self.umask > 0o777 {
            return Err("launcher umask is invalid".into());
        }
        validate_absolute("executable", &self.executable)?;
        validate_absolute("cwd", &self.cwd)?;
        validate_absolute("PTY slave", &self.slave_path)?;
        validate_c_string("argv0", &self.argv0, MAX_LAUNCH_ARG_BYTES)?;
        if self.argv.len() > MAX_LAUNCH_ARGS {
            return Err("launcher argv count exceeds limit".into());
        }
        for value in &self.argv {
            validate_c_string("argv", value, MAX_LAUNCH_ARG_BYTES)?;
        }
        if self.environment.len() > MAX_LAUNCH_ENV_ENTRIES {
            return Err("launcher environment count exceeds limit".into());
        }
        for (key, value) in &self.environment {
            validate_c_string("environment key", key, MAX_LAUNCH_ARG_BYTES)?;
            validate_c_string("environment value", value, MAX_LAUNCH_ARG_BYTES)?;
            if key.is_empty() || key.contains(&b'=') {
                return Err("launcher environment key is invalid".into());
            }
        }
        Ok(())
    }
}

fn validate_c_string(name: &str, value: &[u8], max: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max || value.contains(&0) {
        return Err(format!("launcher {name} is invalid"));
    }
    Ok(())
}

fn validate_absolute(name: &str, value: &[u8]) -> Result<(), String> {
    validate_c_string(name, value, MAX_LAUNCH_ARG_BYTES)?;
    if !Path::new(OsStr::from_bytes(value)).is_absolute() {
        return Err(format!("launcher {name} is not absolute"));
    }
    Ok(())
}

/// Read and execute the private launcher protocol. No logger is initialized and
/// failures are intentionally content-free: the parent owns diagnostic logging.
pub fn run_launcher_mode() -> i32 {
    match launcher_main() {
        Ok(never) => match never {},
        Err(_) => 125,
    }
}

fn launcher_main() -> Result<std::convert::Infallible, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("launcher is not root".into());
    }
    let mut len = [0_u8; 4];
    std::io::stdin()
        .read_exact(&mut len)
        .map_err(|error| format!("launcher spec length read failed: {error}"))?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_LAUNCH_SPEC_BYTES {
        return Err("launcher spec length exceeds limit".into());
    }
    let mut bytes = vec![0_u8; len];
    std::io::stdin()
        .read_exact(&mut bytes)
        .map_err(|error| format!("launcher spec read failed: {error}"))?;
    let spec: LaunchSpec = wincode::deserialize_exact(&bytes)
        .map_err(|error| format!("launcher spec decode failed: {error}"))?;
    spec.validate()?;

    // Arm the parent-death signal before checking the parent a second time, so
    // the daemon cannot disappear in an unprotected gap.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if spec.parent_pid != unsafe { libc::getppid() } as u32 {
        return Err("launcher parent exited during setup".into());
    }
    let current_executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("launcher executable resolution failed: {error}"))?;
    validate_trusted_executable(&current_executable)?;
    validate_trusted_executable(Path::new(OsStr::from_bytes(&spec.executable)))?;

    std::io::stdout()
        .write_all(READY_MARKER)
        .and_then(|_| std::io::stdout().flush())
        .map_err(|error| format!("launcher ready write failed: {error}"))?;
    if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    configure_tty_and_exec(spec)
}

fn configure_tty_and_exec(spec: LaunchSpec) -> Result<std::convert::Infallible, String> {
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let slave_path = CString::new(spec.slave_path).map_err(|_| "invalid PTY slave path")?;
    let slave = unsafe { libc::open(slave_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if unsafe { libc::ioctl(slave, libc::TIOCSCTTY as _, 0) } < 0 {
        close_fd(slave);
        return Err(std::io::Error::last_os_error().to_string());
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(slave, target) } < 0 {
            close_fd(slave);
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    if slave > libc::STDERR_FILENO {
        close_fd(slave);
    }
    close_extra_fds()?;

    let cwd = CString::new(spec.cwd).map_err(|_| "invalid cwd")?;
    let groups = spec
        .supplementary_groups
        .iter()
        .copied()
        .map(|group| group as libc::gid_t)
        .collect::<Vec<_>>();
    if unsafe { libc::setgroups(groups.len(), groups.as_ptr()) } != 0
        || unsafe { libc::setgid(spec.gid as libc::gid_t) } != 0
        || unsafe { libc::setuid(spec.uid as libc::uid_t) } != 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // Resolve the requested working directory with the target session user's
    // credentials, matching ordinary session execution and avoiding root-only
    // path traversal on behalf of an approved command.
    if unsafe { libc::chdir(cwd.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    unsafe { libc::umask(spec.umask as libc::mode_t) };

    let executable = CString::new(spec.executable).map_err(|_| "invalid executable")?;
    let mut argv = Vec::with_capacity(spec.argv.len() + 1);
    argv.push(CString::new(spec.argv0).map_err(|_| "invalid argv0")?);
    for value in spec.argv {
        argv.push(CString::new(value).map_err(|_| "invalid argv")?);
    }
    let env = spec
        .environment
        .into_iter()
        .map(|(key, value)| {
            let mut entry = key;
            entry.push(b'=');
            entry.extend(value);
            CString::new(entry).map_err(|_| "invalid environment")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let argv_ptrs = argv
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect::<Vec<_>>();
    let env_ptrs = env
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect::<Vec<_>>();
    unsafe { libc::execve(executable.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };
    Err(std::io::Error::last_os_error().to_string())
}

fn close_extra_fds() -> Result<(), String> {
    // The launcher may have inherited daemon descriptors that a dependency did
    // not mark CLOEXEC. After stdio has been rebound to the PTY, close the entire
    // remaining descriptor table before dropping privileges and execing user
    // code. This prevents signaling sockets/tokens from becoming ambient handles.
    let result = unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "launcher cannot close inherited descriptors: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn validate_trusted_executable(path: &Path) -> Result<Metadata, String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("trusted executable metadata failed: {error}"))?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err("launcher executable is not a root-owned immutable regular file".into());
    }
    // A root-owned file can still be replaced by an unprivileged user when any
    // parent directory is writable. The daemon and sudo/doas executable paths
    // must therefore be anchored entirely in root-owned, non-writable dirs.
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        let directory_metadata = directory
            .metadata()
            .map_err(|error| format!("trusted executable directory metadata failed: {error}"))?;
        if !directory_metadata.is_dir()
            || directory_metadata.uid() != 0
            || directory_metadata.mode() & 0o022 != 0
        {
            return Err("launcher executable has an untrusted parent directory".into());
        }
        ancestor = directory.parent();
    }
    Ok(metadata)
}

/// Runtime handle exists only when all non-policy elevation prerequisites are
/// currently true. Policy can narrow this later but can never create it.
#[derive(Clone)]
pub struct LinuxExecPtySupervisor {
    systemd: zbus::Connection,
    executable: PathBuf,
    active: Arc<std::sync::Mutex<HashMap<String, SystemdScope>>>,
}

pub struct PreparedPty {
    pub child: Child,
    pub master: File,
    pub scope: SystemdScope,
}

#[derive(Clone)]
pub struct SystemdScope {
    connection: zbus::Connection,
    unit_name: String,
    launcher_pid: u32,
    control_group: String,
}

pub struct RootPtyExecution {
    pub outcome: AgentOutcome,
    pub containment_identity: String,
}

impl LinuxExecPtySupervisor {
    pub async fn probe() -> Result<Self, String> {
        if unsafe { libc::geteuid() } != 0 {
            return Err("ServiceDaemon is not running as root".into());
        }
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file()
            || !Path::new("/run/systemd/system").is_dir()
        {
            return Err("systemd cgroup v2 is unavailable".into());
        }
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| format!("cannot resolve current executable: {error}"))?;
        validate_trusted_executable(&executable)?;
        let systemd = zbus::Connection::system()
            .await
            .map_err(|error| format!("cannot connect to system bus: {error}"))?;
        let manager = systemd_manager(&systemd).await?;
        let service_path: OwnedObjectPath = manager
            .call(
                "GetUnit",
                &(crate::daemon::linux_service::SERVICE_UNIT_NAME),
            )
            .await
            .map_err(|error| format!("ServiceDaemon systemd unit is unavailable: {error}"))?;
        let service = zbus::Proxy::new(
            &systemd,
            "org.freedesktop.systemd1",
            service_path,
            "org.freedesktop.systemd1.Service",
        )
        .await
        .map_err(|error| format!("ServiceDaemon unit proxy failed: {error}"))?;
        let main_pid: u32 = service
            .get_property("MainPID")
            .await
            .map_err(|error| format!("ServiceDaemon MainPID is unavailable: {error}"))?;
        let unit = zbus::Proxy::new(
            &systemd,
            "org.freedesktop.systemd1",
            service.path(),
            "org.freedesktop.systemd1.Unit",
        )
        .await
        .map_err(|error| format!("ServiceDaemon base unit proxy failed: {error}"))?;
        let active_state: String = unit
            .get_property("ActiveState")
            .await
            .map_err(|error| format!("ServiceDaemon ActiveState is unavailable: {error}"))?;
        if main_pid != std::process::id() || active_state != "active" {
            return Err("current process is not the active systemd ServiceDaemon MainPID".into());
        }
        Ok(Self {
            systemd,
            executable,
            active: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub async fn prepare(
        &self,
        plan: &desk_agent_protocol::exec::ExecPlan,
        registration: &RegisteredSessionShell,
    ) -> Result<PreparedPty, String> {
        if !plan.requires_root_pty_containment() {
            return Err("root PTY supervisor received a non-elevation plan".into());
        }
        let executable = resolve_plan_executable(&plan.program, &registration.environment)?;
        let (master, slave_path) = open_pty(plan.io_mode)?;
        let spec = LaunchSpec {
            protocol_version: LAUNCH_PROTOCOL_VERSION,
            parent_pid: std::process::id(),
            executable: executable.as_os_str().as_bytes().to_vec(),
            argv0: plan.program.as_bytes().to_vec(),
            argv: plan
                .argv
                .iter()
                .map(|value| value.as_bytes().to_vec())
                .collect(),
            cwd: plan
                .cwd
                .as_ref()
                .map(|value| value.as_bytes().to_vec())
                .unwrap_or_else(|| registration.cwd.as_os_str().as_bytes().to_vec()),
            environment: registration
                .environment
                .iter()
                .map(|(key, value)| {
                    (
                        key.as_os_str().as_bytes().to_vec(),
                        value.as_os_str().as_bytes().to_vec(),
                    )
                })
                .collect(),
            uid: registration.process_identity.uid,
            gid: registration.process_identity.gid,
            supplementary_groups: registration.process_identity.supplementary_groups.clone(),
            umask: registration.umask,
            slave_path: slave_path.as_os_str().as_bytes().to_vec(),
        };
        spec.validate_for_parent()?;
        let encoded = wincode::serialize(&spec)
            .map_err(|error| format!("launcher spec encode failed: {error}"))?;
        if encoded.len() > MAX_LAUNCH_SPEC_BYTES {
            return Err("launcher spec exceeds limit".into());
        }

        let mut child = std::process::Command::new(&self.executable)
            .arg("exec-pty-launcher")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("launcher spawn failed: {error}"))?;
        let prepare_result = (|| {
            let stdin = child.stdin.as_mut().ok_or("launcher stdin missing")?;
            stdin
                .write_all(&(encoded.len() as u32).to_le_bytes())
                .and_then(|_| stdin.write_all(&encoded))
                .and_then(|_| stdin.flush())
                .map_err(|error| format!("launcher spec write failed: {error}"))?;
            drop(child.stdin.take());
            let stdout = child.stdout.as_mut().ok_or("launcher stdout missing")?;
            wait_readable(stdout.as_raw_fd(), PREPARE_TIMEOUT)?;
            let mut ready = vec![0_u8; READY_MARKER.len()];
            stdout
                .read_exact(&mut ready)
                .map_err(|error| format!("launcher ready read failed: {error}"))?;
            if ready != READY_MARKER {
                return Err("launcher returned an invalid ready marker".into());
            }
            wait_stopped(child.id(), PREPARE_TIMEOUT)?;
            Ok(())
        })();
        if let Err(error) = prepare_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        let unit_name = scope_name(&plan.execution_generation);
        let scope = match start_scope(&self.systemd, &unit_name, child.id()).await {
            Ok(scope) => scope,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(plan.execution_generation.clone(), scope.clone());
        Ok(PreparedPty {
            child,
            master,
            scope,
        })
    }

    fn continue_launcher(&self, prepared: &PreparedPty) -> Result<Instant, String> {
        if unsafe { libc::kill(prepared.child.id() as libc::pid_t, libc::SIGCONT) } != 0 {
            let error = std::io::Error::last_os_error();
            return Err(format!("launcher continue failed: {error}"));
        }
        // The one authoritative command deadline starts at SIGCONT, after the
        // durable ledger contains the deterministic scope identity.
        Ok(Instant::now())
    }

    fn unregister(&self, generation: &str) {
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(generation);
    }

    pub async fn stop_all(&self) {
        let scopes = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for scope in scopes {
            if let Err(error) = scope.stop().await {
                log::error!(
                    "[exec-pty] failed to stop active scope {} during daemon shutdown: {error}",
                    scope.unit_name
                );
            }
        }
    }
}

/// Probe once during ServiceDaemon startup. A failed probe is deliberately not
/// cached, so a later signaling-proxy restart in the same process may recover
/// after systemd/D-Bus becomes available. Once installed, readiness is immutable
/// for that process and every dispatch still rechecks the session generation.
pub async fn initialize_runtime() -> Result<Arc<LinuxExecPtySupervisor>, String> {
    if let Some(runtime) = RUNTIME.get() {
        return Ok(Arc::clone(runtime));
    }
    let runtime = Arc::new(LinuxExecPtySupervisor::probe().await?);
    let _ = RUNTIME.set(Arc::clone(&runtime));
    Ok(RUNTIME.get().cloned().unwrap_or(runtime))
}

pub fn runtime() -> Option<Arc<LinuxExecPtySupervisor>> {
    RUNTIME.get().cloned()
}

pub fn runtime_ready() -> bool {
    RUNTIME.get().is_some()
}

/// Reclaim a deterministic root PTY scope recorded by the durable ledger before
/// the abandoned execution is marked indeterminate. Missing/already-collected
/// scopes are success; an existing non-empty scope must be proven empty.
pub async fn reconcile_containment_identity(identity: &str) -> Result<(), String> {
    let Some(unit_name) = identity.strip_prefix("systemd-scope:") else {
        return Ok(());
    };
    if !unit_name.starts_with("lrd-exec-pty-")
        || !unit_name.ends_with(".scope")
        || !unit_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-.".contains(&byte))
    {
        return Err("ledger contains an invalid PTY scope identity".into());
    }
    let connection = zbus::Connection::system()
        .await
        .map_err(|error| format!("cannot connect to system bus for reconcile: {error}"))?;
    let manager = systemd_manager(&connection).await?;
    let unit_path: OwnedObjectPath = match manager.call("GetUnit", &(unit_name)).await {
        Ok(path) => path,
        Err(error) if is_no_such_unit(&error) => return Ok(()),
        Err(error) => return Err(format!("systemd reconcile GetUnit failed: {error}")),
    };
    let unit = zbus::Proxy::new(
        &connection,
        "org.freedesktop.systemd1",
        unit_path,
        "org.freedesktop.systemd1.Unit",
    )
    .await
    .map_err(|error| format!("systemd reconcile unit proxy failed: {error}"))?;
    let control_group: String = unit
        .get_property("ControlGroup")
        .await
        .map_err(|error| format!("systemd reconcile ControlGroup failed: {error}"))?;
    if control_group.is_empty() {
        return Err("existing PTY scope has no control group".into());
    }
    SystemdScope {
        connection,
        unit_name: unit_name.to_string(),
        launcher_pid: 0,
        control_group,
    }
    .stop()
    .await
}

fn is_no_such_unit(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit"
    )
}

async fn discard_prepared(
    runtime: &LinuxExecPtySupervisor,
    generation: &str,
    prepared: &mut PreparedPty,
) {
    if prepared.scope.stop().await.is_ok() {
        runtime.unregister(generation);
    }
    let _ = prepared.child.kill();
    let _ = prepared.child.wait();
}

/// Run one approved sudo/doas PTY inside the already-probed root containment.
/// Input remains opaque and is never retained; only remote program output is
/// captured, projected, and redacted for the final model-visible result.
pub async fn run_root_pty(
    payload: ExecPtyStartPayload,
    registration: Arc<RegisteredSessionShell>,
    session_registry: crate::host_control::session_shell::SessionShellRegistry,
    carrier: ExecPtyCarrierRegistry,
    mut control_rx: tokio::sync::mpsc::Receiver<PtyWireFrame>,
    ledger: Arc<crate::daemon::exec_ledger::ExecLedger>,
    on_started: impl FnOnce(String),
) -> Result<RootPtyExecution, String> {
    let runtime = runtime().ok_or("root PTY containment is not ready")?;
    if !session_registry.is_current(&registration)
        || registration.registration_generation != payload.registration_generation
    {
        return Err("registered desktop session is stale".into());
    }
    let mut prepared = runtime.prepare(&payload.plan, &registration).await?;
    // Prepare every fallible daemon-side PTY handle while the launcher is still
    // stopped. Once SIGCONT is delivered the command is live, so there must be
    // no setup-only error path that can bypass scope teardown.
    let mut reader = match prepared.master.try_clone() {
        Ok(reader) => reader,
        Err(error) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err(format!("PTY reader clone failed: {error}"));
        }
    };
    let writer_file = match prepared.master.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err(format!("PTY writer clone failed: {error}"));
        }
    };
    let resize_file = match prepared.master.try_clone() {
        Ok(resize) => resize,
        Err(error) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err(format!("PTY resize clone failed: {error}"));
        }
    };
    if !session_registry.is_current(&registration) || !carrier.contains(&payload.stream_id) {
        discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
        return Err("root PTY session or carrier became stale before start".into());
    }
    let containment_identity = prepared.scope.containment_identity();
    match ledger
        .mark_running(
            &payload.plan.execution_generation,
            Some(&containment_identity),
        )
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err("exec ledger refused the root PTY running transition".into());
        }
        Err(error) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err(format!(
                "exec ledger could not persist root PTY scope: {error}"
            ));
        }
    }
    let opened = PtyStreamOpened {
        task_id: payload.plan.exec_request_id.0.clone(),
        execution_generation: payload.plan.execution_generation.clone(),
        stream_id: payload.stream_id.clone(),
        session_target_id: payload.session_target_id.clone(),
        registration_generation: payload.registration_generation,
        worker_incarnation: payload.worker_incarnation,
    };
    let started_at = match carrier.start_daemon(opened, || {
        if !session_registry.is_current(&registration) {
            return Err("registered desktop session became stale before start".to_string());
        }
        runtime.continue_launcher(&prepared)
    }) {
        Ok(Ok(started_at)) => started_at,
        Ok(Err(error)) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err(error);
        }
        Err(error) => {
            discard_prepared(&runtime, &payload.plan.execution_generation, &mut prepared).await;
            return Err(format!("PTY carrier start barrier failed: {error}"));
        }
    };
    on_started(containment_identity.clone());

    let result_cap = (payload.plan.max_stdout_bytes as usize)
        .saturating_add(payload.plan.max_stderr_bytes as usize);
    let retain_cap = result_cap.saturating_add(RESULT_REDACTION_MARGIN);
    let stop = Arc::new(AtomicU8::new(STOP_NONE));
    let output_bytes = Arc::new(AtomicU64::new(0));
    let reader_stop = Arc::clone(&stop);
    let reader_output_bytes = Arc::clone(&output_bytes);
    let reader_carrier = carrier.clone();
    let reader_stream_id = payload.stream_id.clone();
    let reader_generation = payload.plan.execution_generation.clone();
    let reader_target = payload.session_target_id.clone();
    let reader_registration = payload.registration_generation;
    let reader_incarnation = payload.worker_incarnation;
    let reader_task = tokio::task::spawn_blocking(move || {
        let mut retained = Vec::with_capacity(retain_cap.min(64 * 1024));
        let mut overflowed = false;
        let mut sequence = 0_u64;
        let mut chunk = vec![0_u8; MAX_PTY_DATA_FRAME_BYTES.min(8192)];
        loop {
            let count = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => {
                    reader_stop.store(STOP_INTERNAL, Ordering::Release);
                    break;
                }
            };
            let total = reader_output_bytes
                .fetch_add(count as u64, Ordering::AcqRel)
                .saturating_add(count as u64);
            if retained.len() < retain_cap {
                let keep = (retain_cap - retained.len()).min(count);
                retained.extend_from_slice(&chunk[..keep]);
            }
            overflowed |= total > result_cap as u64;
            let frame = PtyOutputFrame {
                stream_id: reader_stream_id.clone(),
                execution_generation: reader_generation.clone(),
                session_target_id: reader_target.clone(),
                registration_generation: reader_registration,
                worker_incarnation: reader_incarnation,
                sequence,
                data: chunk[..count].to_vec(),
            };
            sequence = match sequence.checked_add(1) {
                Some(next) => next,
                None => {
                    reader_stop.store(STOP_INTERNAL, Ordering::Release);
                    break;
                }
            };
            if let Err(error) = reader_carrier.route_daemon_output(frame) {
                reader_stop.store(
                    if matches!(
                        error,
                        crate::daemon::exec_pty_carrier::CarrierError::SlowConsumer
                    ) {
                        STOP_SLOW
                    } else {
                        STOP_CARRIER
                    },
                    Ordering::Release,
                );
                break;
            }
        }
        (retained, overflowed)
    });

    let mut writer = tokio::fs::File::from_std(writer_file);
    let mut input_frames = 0_u64;
    let mut input_bytes = 0_u64;
    let deadline = tokio::time::Instant::from_std(started_at)
        + Duration::from_millis(payload.plan.timeout_ms as u64);
    let mut poll = tokio::time::interval(Duration::from_millis(20));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut status = None;
    let mut close_reason = PtyCloseReason::Exited;

    loop {
        tokio::select! {
            _ = poll.tick() => {
                if !session_registry.is_current(&registration) {
                    stop.store(STOP_STALE, Ordering::Release);
                }
                let requested = stop.load(Ordering::Acquire);
                if requested != STOP_NONE {
                    close_reason = stop_reason(requested);
                    break;
                }
                match prepared.child.try_wait() {
                    Ok(Some(exit)) => {
                        status = Some(exit);
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        close_reason = PtyCloseReason::OutcomeUnknown;
                        break;
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                close_reason = PtyCloseReason::TimedOut;
                break;
            }
            control = control_rx.recv() => {
                let Some(control) = control else {
                    close_reason = PtyCloseReason::CarrierDisconnected;
                    break;
                };
                match control {
                    PtyWireFrame::Input(input) => {
                        if writer.write_all(&input.data).await.is_err()
                            || writer.flush().await.is_err()
                        {
                            close_reason = PtyCloseReason::InternalError;
                            break;
                        }
                        input_frames = input_frames.saturating_add(1);
                        input_bytes = input_bytes.saturating_add(input.data.len() as u64);
                    }
                    PtyWireFrame::Resize(resize) => {
                        let size = libc::winsize {
                            ws_row: resize.rows,
                            ws_col: resize.cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        if unsafe {
                            libc::ioctl(resize_file.as_raw_fd(), libc::TIOCSWINSZ as _, &size)
                        } != 0
                        {
                            close_reason = PtyCloseReason::InternalError;
                            break;
                        }
                    }
                    PtyWireFrame::Cancel(cancel) => {
                        close_reason = cancel.reason;
                        break;
                    }
                    _ => {
                        close_reason = PtyCloseReason::SequenceViolation;
                        break;
                    }
                }
            }
        }
    }

    drop(writer);
    drop(resize_file);
    // StopUnit uses KillMode=control-group, so descendants are reclaimed even
    // after the direct sudo/doas process exits or forks a root child.
    match prepared.scope.stop().await {
        Ok(()) => runtime.unregister(&payload.plan.execution_generation),
        Err(error) => {
            log::warn!(
                "[exec-pty] scope cleanup failed generation={} error={error}",
                payload.plan.execution_generation
            );
            close_reason = PtyCloseReason::OutcomeUnknown;
        }
    }
    if status.is_none() {
        status = prepared.child.try_wait().ok().flatten();
    }
    if status.is_none() {
        let _ = prepared.child.kill();
        status = prepared.child.wait().ok();
    }
    drop(prepared.master);
    let (retained, overflowed) = reader_task
        .await
        .map_err(|_| "PTY output reader failed".to_string())?;
    use std::os::unix::process::ExitStatusExt as _;
    let exit_status = status.as_ref().map(|value| {
        value
            .code()
            .unwrap_or_else(|| -value.signal().unwrap_or(libc::SIGKILL))
    });
    let closed = PtyStreamClosed {
        stream_id: payload.stream_id,
        execution_generation: payload.plan.execution_generation.clone(),
        session_target_id: payload.session_target_id,
        registration_generation: payload.registration_generation,
        worker_incarnation: payload.worker_incarnation,
        exit_status,
        reason: close_reason,
        input_frames,
        input_bytes,
        output_bytes: output_bytes.load(Ordering::Acquire),
    };
    let _ = carrier.route_daemon_closed(closed);

    let outcome = match close_reason {
        PtyCloseReason::Exited => crate::worker::exec_pty::finish_combined_result(
            exit_status.unwrap_or(-1),
            started_at.elapsed(),
            &retained,
            overflowed,
            result_cap,
        ),
        PtyCloseReason::TimedOut => AgentOutcome::Err(crate::worker::exec_pty::agent_error(
            AgentErrorKind::Timeout,
            format!("PTY command timed out after {} ms", payload.plan.timeout_ms),
        )),
        PtyCloseReason::Cancelled
        | PtyCloseReason::CarrierDisconnected
        | PtyCloseReason::SlowConsumer
        | PtyCloseReason::SessionStale
        | PtyCloseReason::SequenceViolation => {
            AgentOutcome::Err(crate::worker::exec_pty::agent_error(
                AgentErrorKind::Cancelled,
                format!("PTY command stopped: {close_reason:?}"),
            ))
        }
        PtyCloseReason::OutcomeUnknown | PtyCloseReason::InternalError => {
            AgentOutcome::Err(crate::worker::exec_pty::agent_error(
                AgentErrorKind::Internal,
                "PTY command outcome is unknown".to_string(),
            ))
        }
    };
    Ok(RootPtyExecution {
        outcome,
        containment_identity,
    })
}

fn stop_reason(value: u8) -> PtyCloseReason {
    match value {
        STOP_CARRIER => PtyCloseReason::CarrierDisconnected,
        STOP_SLOW => PtyCloseReason::SlowConsumer,
        STOP_STALE => PtyCloseReason::SessionStale,
        _ => PtyCloseReason::InternalError,
    }
}

impl LaunchSpec {
    fn validate_for_parent(&self) -> Result<(), String> {
        if self.protocol_version != LAUNCH_PROTOCOL_VERSION || self.parent_pid != std::process::id()
        {
            return Err("invalid parent launch spec".into());
        }
        if self.umask > 0o777 {
            return Err("launcher umask is invalid".into());
        }
        validate_absolute("executable", &self.executable)?;
        validate_absolute("cwd", &self.cwd)?;
        validate_absolute("PTY slave", &self.slave_path)?;
        validate_c_string("argv0", &self.argv0, MAX_LAUNCH_ARG_BYTES)?;
        Ok(())
    }
}

impl SystemdScope {
    pub fn containment_identity(&self) -> String {
        format!("systemd-scope:{}", self.unit_name)
    }

    pub async fn stop(&self) -> Result<(), String> {
        if cgroup_is_empty(&self.control_group)? {
            return Ok(());
        }
        let manager = systemd_manager(&self.connection).await?;
        let stop_result: Result<OwnedObjectPath, _> = manager
            .call("StopUnit", &(&self.unit_name, "replace"))
            .await;
        if let Err(error) = stop_result
            && !cgroup_is_empty(&self.control_group)?
        {
            return Err(format!("systemd StopUnit failed: {error}"));
        }
        let deadline = Instant::now() + PREPARE_TIMEOUT;
        while Instant::now() < deadline {
            if cgroup_is_empty(&self.control_group)? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _: () = manager
            .call("KillUnit", &(&self.unit_name, "all", libc::SIGKILL))
            .await
            .map_err(|error| format!("systemd KillUnit failed: {error}"))?;
        let deadline = Instant::now() + PREPARE_TIMEOUT;
        while Instant::now() < deadline {
            if cgroup_is_empty(&self.control_group)? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err(format!(
            "systemd scope {} still has live processes after SIGKILL (leader pid {})",
            self.unit_name, self.launcher_pid
        ))
    }
}

async fn systemd_manager(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>, String> {
    zbus::Proxy::new(
        connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .map_err(|error| format!("systemd manager proxy failed: {error}"))
}

async fn start_scope(
    connection: &zbus::Connection,
    unit_name: &str,
    pid: u32,
) -> Result<SystemdScope, String> {
    let manager = systemd_manager(connection).await?;
    let properties = vec![
        ("Description", Value::from("LCXL one-shot elevated PTY")),
        ("PIDs", Value::from(vec![pid])),
        ("KillMode", Value::from("control-group")),
        ("CollectMode", Value::from("inactive-or-failed")),
        ("TimeoutStopUSec", Value::from(2_000_000_u64)),
        (
            "BindsTo",
            Value::from(vec![
                crate::daemon::linux_service::SERVICE_UNIT_NAME.to_string(),
            ]),
        ),
        (
            "PartOf",
            Value::from(vec![
                crate::daemon::linux_service::SERVICE_UNIT_NAME.to_string(),
            ]),
        ),
    ];
    let auxiliary: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
    let job: OwnedObjectPath = manager
        .call(
            "StartTransientUnit",
            &(unit_name, "fail", properties, auxiliary),
        )
        .await
        .map_err(|error| format!("systemd StartTransientUnit failed: {error}"))?;

    wait_systemd_job(connection, job).await?;

    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        let unit_path: OwnedObjectPath = manager
            .call("GetUnit", &(unit_name))
            .await
            .map_err(|error| format!("systemd GetUnit failed: {error}"))?;
        let unit = zbus::Proxy::new(
            connection,
            "org.freedesktop.systemd1",
            unit_path,
            "org.freedesktop.systemd1.Unit",
        )
        .await
        .map_err(|error| format!("systemd unit proxy failed: {error}"))?;
        let control_group: String = unit
            .get_property("ControlGroup")
            .await
            .map_err(|error| format!("systemd ControlGroup read failed: {error}"))?;
        let active_state: String = unit
            .get_property("ActiveState")
            .await
            .map_err(|error| format!("systemd ActiveState read failed: {error}"))?;
        if active_state == "active"
            && !control_group.is_empty()
            && cgroup_contains(&control_group, pid)?
        {
            return Ok(SystemdScope {
                connection: connection.clone(),
                unit_name: unit_name.to_string(),
                launcher_pid: pid,
                control_group,
            });
        }
        if Instant::now() >= deadline {
            return Err("systemd scope membership barrier timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_systemd_job(
    connection: &zbus::Connection,
    job_path: OwnedObjectPath,
) -> Result<(), String> {
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        let job = zbus::Proxy::new(
            connection,
            "org.freedesktop.systemd1",
            job_path.clone(),
            "org.freedesktop.systemd1.Job",
        )
        .await
        .map_err(|error| format!("systemd job proxy failed: {error}"))?;
        match job.get_property::<String>("State").await {
            Ok(state) if matches!(state.as_str(), "waiting" | "running") => {}
            Ok(_) | Err(_) => return Ok(()),
        }
        if Instant::now() >= deadline {
            return Err("systemd transient-scope job timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn cgroup_contains(control_group: &str, pid: u32) -> Result<bool, String> {
    let relative = control_group.trim_start_matches('/');
    if relative.split('/').any(|part| part == "..") {
        return Err("systemd returned an invalid control group".into());
    }
    let path = Path::new("/sys/fs/cgroup")
        .join(relative)
        .join("cgroup.procs");
    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read scope membership: {error}"))?;
    Ok(contents
        .lines()
        .any(|line| line.trim().parse::<u32>() == Ok(pid)))
}

fn cgroup_is_empty(control_group: &str) -> Result<bool, String> {
    let relative = control_group.trim_start_matches('/');
    if relative.split('/').any(|part| part == "..") {
        return Err("systemd returned an invalid control group".into());
    }
    let path = Path::new("/sys/fs/cgroup")
        .join(relative)
        .join("cgroup.procs");
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents.lines().all(|line| line.trim().is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("cannot read scope membership: {error}")),
    }
}

fn scope_name(generation: &str) -> String {
    let digest = Sha256::digest(generation.as_bytes());
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("lrd-exec-pty-{suffix}.scope")
}

fn wait_stopped(pid: u32, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|error| format!("launcher status read failed: {error}"))?;
        if status
            .lines()
            .find(|line| line.starts_with("State:"))
            .is_some_and(|line| line.contains('T'))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("launcher did not enter stopped state".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_readable(fd: RawFd, timeout: Duration) -> Result<(), String> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result > 0 && descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
        Ok(())
    } else if result == 0 {
        Err("launcher ready barrier timed out".into())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn resolve_plan_executable(
    program: &str,
    environment: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<PathBuf, String> {
    if program.as_bytes().contains(&0) || program.is_empty() {
        return Err("approved executable is invalid".into());
    }
    let path = Path::new(OsStr::from_bytes(program.as_bytes()));
    let candidates = if path.is_absolute() {
        vec![path.to_path_buf()]
    } else if program.contains('/') {
        return Err("approved executable path must be absolute".into());
    } else {
        let search = environment
            .iter()
            .find(|(key, _)| key.as_os_str().as_bytes() == b"PATH")
            .map(|(_, value)| value.as_os_str().as_bytes())
            .unwrap_or(b"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        search
            .split(|byte| *byte == b':')
            .filter(|entry| !entry.is_empty())
            .map(|entry| Path::new(OsStr::from_bytes(entry)).join(program))
            .collect()
    };
    for candidate in candidates {
        let Ok(metadata) = candidate.metadata() else {
            continue;
        };
        if metadata.is_file() && metadata.mode() & 0o111 != 0 {
            return std::fs::canonicalize(&candidate)
                .map_err(|error| format!("cannot canonicalize approved executable: {error}"));
        }
    }
    Err("approved executable was not found in the registered session PATH".into())
}

fn open_pty(io_mode: desk_agent_protocol::exec::ExecIoMode) -> Result<(File, PathBuf), String> {
    let desk_agent_protocol::exec::ExecIoMode::Pty {
        initial_rows,
        initial_cols,
    } = io_mode
    else {
        return Err("root PTY supervisor received a non-PTY plan".into());
    };
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let result = (|| {
        if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let mut path = vec![0_i8; 256];
        let result = unsafe { libc::ptsname_r(fd, path.as_mut_ptr(), path.len()) };
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result).to_string());
        }
        let slave = unsafe { CStr::from_ptr(path.as_ptr()) };
        let size = libc::winsize {
            ws_row: initial_rows,
            ws_col: initial_cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let master = unsafe { File::from_raw_fd(fd) };
        Ok((master, PathBuf::from(OsStr::from_bytes(slave.to_bytes()))))
    })();
    if result.is_err() {
        close_fd(fd);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_identity_is_deterministic_and_does_not_expose_generation() {
        let first = scope_name("secret-generation-value");
        assert_eq!(first, scope_name("secret-generation-value"));
        assert_ne!(first, scope_name("another-generation"));
        assert!(first.starts_with("lrd-exec-pty-"));
        assert!(first.ends_with(".scope"));
        assert!(!first.contains("secret"));
    }

    #[test]
    fn parent_validation_rejects_relative_paths_and_nul() {
        assert!(validate_absolute("executable", b"sudo").is_err());
        assert!(validate_c_string("argv", b"bad\0arg", 100).is_err());
        assert!(validate_absolute("cwd", b"/tmp").is_ok());
    }

    #[test]
    fn resolver_uses_byte_safe_registered_path() {
        let environment = vec![(
            std::ffi::OsString::from("PATH"),
            std::ffi::OsString::from("/usr/bin:/bin"),
        )];
        let resolved = resolve_plan_executable("sh", &environment).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.metadata().unwrap().is_file());
    }
}
