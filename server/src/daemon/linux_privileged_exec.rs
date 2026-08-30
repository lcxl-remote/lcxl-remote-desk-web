//! Linux administrator authorization and exact one-shot permit foundation.
//!
//! Passwords are handled exclusively by the desktop session's registered
//! polkit authentication agent. The daemon identifies that session by the
//! already `/proc`-verified Tauri process tuple and invokes `pkcheck` with the
//! race-safe `pid,start_time,uid` subject form. It never enables pkcheck's
//! textual/internal agent and never receives authentication input itself.

use async_trait::async_trait;
use desk_agent_protocol::RiskLevel;
use desk_agent_protocol::exec::{
    ExecContainmentSnapshot, ExecExecutionBasis, ExecPlan, ExecPlanDraft, ExecShellKind,
    ExecutionPrincipal, RequiredEnforcement,
};
use desk_agent_protocol::exec_policy::{ExecLimits, fingerprint_for_principal};
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome, ExecOutput, OperationOutput};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command;
use uuid::Uuid;

use crate::agent_adapter::redaction::{Redactor, RegexRedactor};
use crate::daemon::exec_ledger::{ExecLedger, Reservation, Terminal};
use crate::host_control::session_shell::{
    RegisteredSessionShell, SessionShellRegistry, read_process_identity,
};

pub const POLKIT_ACTION_ID: &str = "com.lcxl.remote-desk.ai.execute-administrator-command";
pub const POLKIT_POLICY_PATH: &str = "/usr/share/polkit-1/actions/com.lcxl.remote-desk.ai.policy";
pub const EXPERIMENTAL_PRIVILEGED_EXEC_ENV: &str = "LRD_EXPERIMENTAL_LINUX_PRIVILEGED_EXEC";
pub const POLKIT_POLICY_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC
 "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <vendor>LCXL</vendor>
  <vendor_url>https://github.com/lcxl/lcxl-remote-desk</vendor_url>
  <action id="com.lcxl.remote-desk.ai.execute-administrator-command">
    <description>Run an approved administrator command</description>
    <message>Authentication is required to run the approved administrator command</message>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin</allow_active>
    </defaults>
  </action>
</policyconfig>
"#;

const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(120);
const PERMIT_TTL: Duration = Duration::from_secs(30);
const PKCHECK_PATH: &str = "/usr/bin/pkcheck";
pub const SYSTEMD_RUN_PATH: &str = "/usr/bin/systemd-run";
pub const SYSTEMCTL_PATH: &str = "/usr/bin/systemctl";
pub const MANAGED_SERVICE_UNIT: &str = "lcxl-remote-desk.service";
pub const PRIVILEGED_OUTPUT_DIR: &str = "/run/lcxl-remote-desk/privileged-exec";
const PRIVILEGED_TEMPLATE_PROFILE: &str = "linux.privileged.systemd.v1";
const PRIVILEGED_TEMPLATE_REVISION: i64 = 1;
const PRIVILEGED_TIMEOUT_MS: u32 = 30_000;
const PRIVILEGED_OUTPUT_BYTES: u32 = 64 * 1024;
// Match the worker redaction look-ahead margin. RLIMIT_FSIZE bounds each raw
// root-owned output file before the daemon reads and redacts it.
const PRIVILEGED_RAW_OUTPUT_BYTES: u32 = PRIVILEGED_OUTPUT_BYTES + 8 * 1024;
const PRIVILEGED_MAX_PROCESSES: u32 = 16;
const PRIVILEGED_MAX_MEMORY_BYTES: u64 = 128 * 1024 * 1024;
const PRIVILEGED_CPU_MAX_PERCENT: u16 = 50;
const SYSTEMD_CLIENT_GRACE: Duration = Duration::from_secs(15);
const UNIT_OBSERVE_INTERVAL: Duration = Duration::from_millis(25);
const SYSTEMCTL_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
const SYSTEMD_CLIENT_DIAGNOSTIC_BYTES: usize = 16 * 1024;

pub fn experimental_privileged_exec_enabled() -> bool {
    std::env::var_os(EXPERIMENTAL_PRIVILEGED_EXEC_ENV)
        .is_some_and(|value| value == std::ffi::OsStr::new("1"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolkitSubject {
    pub pid: u32,
    pub start_ticks: u64,
    pub uid: u32,
}

impl PolkitSubject {
    fn from_registration(
        registration: &RegisteredSessionShell,
    ) -> Result<Self, AuthorizationError> {
        let actual = read_process_identity(registration.pid)
            .map_err(|_| AuthorizationError::StaleRegistration)?;
        if actual != registration.process_identity {
            return Err(AuthorizationError::StaleRegistration);
        }
        Ok(Self {
            pid: registration.pid,
            start_ticks: actual.start_ticks,
            uid: actual.uid,
        })
    }

    fn process_argument(self) -> String {
        format!("{},{},{}", self.pid, self.start_ticks, self.uid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationError {
    StaleRegistration,
    Denied,
    AgentUnavailable,
    Cancelled,
    TimedOut,
    BackendUnavailable,
}

impl std::fmt::Display for AuthorizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::StaleRegistration => "session registration is stale",
            Self::Denied => "administrator authorization was denied",
            Self::AgentUnavailable => "no suitable polkit authentication agent is available",
            Self::Cancelled => "administrator authorization was cancelled",
            Self::TimedOut => "administrator authorization timed out",
            Self::BackendUnavailable => "polkit authorization backend is unavailable",
        })
    }
}

impl std::error::Error for AuthorizationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolkitCommandOutcome {
    Exit(i32),
    TimedOut,
    Failed,
}

#[async_trait]
trait PolkitCommandRunner: Send + Sync {
    async fn check(&self, subject: PolkitSubject) -> PolkitCommandOutcome;
}

struct RealPolkitCommandRunner;

#[async_trait]
impl PolkitCommandRunner for RealPolkitCommandRunner {
    async fn check(&self, subject: PolkitSubject) -> PolkitCommandOutcome {
        let mut command = Command::new(PKCHECK_PATH);
        command
            .arg("--action-id")
            .arg(POLKIT_ACTION_ID)
            .arg("--process")
            .arg(subject.process_argument())
            .arg("--allow-user-interaction")
            // Deliberately do not pass --enable-internal-agent: authentication
            // must appear in the registered desktop session, never in daemon
            // stdin or a hidden terminal prompt.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let Ok(mut child) = command.spawn() else {
            return PolkitCommandOutcome::Failed;
        };
        match tokio::time::timeout(AUTHORIZATION_TIMEOUT, child.wait()).await {
            Ok(Ok(status)) => PolkitCommandOutcome::Exit(status.code().unwrap_or(127)),
            Ok(Err(_)) => PolkitCommandOutcome::Failed,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                PolkitCommandOutcome::TimedOut
            }
        }
    }
}

#[derive(Clone)]
pub struct LinuxPolkitAuthorizer {
    runner: Arc<dyn PolkitCommandRunner>,
}

impl Default for LinuxPolkitAuthorizer {
    fn default() -> Self {
        Self {
            runner: Arc::new(RealPolkitCommandRunner),
        }
    }
}

impl LinuxPolkitAuthorizer {
    pub async fn authorize(
        &self,
        registry: &SessionShellRegistry,
        registration: &Arc<RegisteredSessionShell>,
    ) -> Result<(), AuthorizationError> {
        if !registry.is_current(registration) {
            return Err(AuthorizationError::StaleRegistration);
        }
        let subject = PolkitSubject::from_registration(registration)?;
        let outcome = self.runner.check(subject).await;
        let result = match outcome {
            PolkitCommandOutcome::Exit(0) => Ok(()),
            PolkitCommandOutcome::Exit(1) => Err(AuthorizationError::Denied),
            PolkitCommandOutcome::Exit(2) => Err(AuthorizationError::AgentUnavailable),
            PolkitCommandOutcome::Exit(3) => Err(AuthorizationError::Cancelled),
            PolkitCommandOutcome::TimedOut => Err(AuthorizationError::TimedOut),
            PolkitCommandOutcome::Exit(_) | PolkitCommandOutcome::Failed => {
                Err(AuthorizationError::BackendUnavailable)
            }
        };
        result?;

        // Authorization may have blocked on user interaction. Revalidate both
        // the registry generation and the kernel process identity afterwards;
        // an approval for a logged-out/replaced Tauri process mints no permit.
        if !registry.is_current(registration) {
            return Err(AuthorizationError::StaleRegistration);
        }
        PolkitSubject::from_registration(registration)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedPermit {
    pub permit_id: Uuid,
    plan_digest_sha256: [u8; 32],
    registration_id: Uuid,
    registration_generation: u64,
    execution_generation: String,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermitError {
    Missing,
    Expired,
    BindingMismatch,
    StaleRegistration,
}

#[derive(Clone, Default)]
pub struct PrivilegedPermitStore {
    permits: Arc<Mutex<HashMap<Uuid, PrivilegedPermit>>>,
}

impl PrivilegedPermitStore {
    pub fn mint(&self, plan: &ExecPlan, registration: &RegisteredSessionShell) -> PrivilegedPermit {
        let permit = PrivilegedPermit {
            permit_id: Uuid::new_v4(),
            plan_digest_sha256: plan_digest(plan),
            registration_id: registration.registration_id,
            registration_generation: registration.registration_generation,
            execution_generation: plan.execution_generation.clone(),
            expires_at: Instant::now() + PERMIT_TTL,
        };
        self.permits
            .lock()
            .unwrap()
            .insert(permit.permit_id, permit.clone());
        permit
    }

    /// Atomically removes and validates a permit. Every consume attempt burns
    /// the token, including a mismatch, so a captured id cannot be probed and
    /// retried with alternate plans or generations.
    pub fn consume(
        &self,
        permit_id: Uuid,
        plan: &ExecPlan,
        registry: &SessionShellRegistry,
        registration: &Arc<RegisteredSessionShell>,
    ) -> Result<(), PermitError> {
        let permit = self
            .permits
            .lock()
            .unwrap()
            .remove(&permit_id)
            .ok_or(PermitError::Missing)?;
        if Instant::now() > permit.expires_at {
            return Err(PermitError::Expired);
        }
        if permit.plan_digest_sha256 != plan_digest(plan)
            || permit.execution_generation != plan.execution_generation
            || permit.registration_id != registration.registration_id
            || permit.registration_generation != registration.registration_generation
        {
            return Err(PermitError::BindingMismatch);
        }
        if !registry.is_current(registration) {
            return Err(PermitError::StaleRegistration);
        }
        Ok(())
    }

    pub fn revoke_registration(&self, registration_id: Uuid, registration_generation: u64) {
        self.permits.lock().unwrap().retain(|_, permit| {
            permit.registration_id != registration_id
                || permit.registration_generation != registration_generation
        });
    }

    fn revoke(&self, permit_id: Uuid) {
        self.permits.lock().unwrap().remove(&permit_id);
    }

    fn retain_current_registrations(&self, registry: &SessionShellRegistry) {
        let current: HashSet<_> = registry
            .snapshot()
            .into_iter()
            .map(|registration| {
                (
                    registration.registration_id,
                    registration.registration_generation,
                )
            })
            .collect();
        self.permits.lock().unwrap().retain(|_, permit| {
            current.contains(&(permit.registration_id, permit.registration_generation))
        });
    }

    fn revoke_all(&self) {
        self.permits.lock().unwrap().clear();
    }

    fn revoke_expired(&self) {
        let now = Instant::now();
        self.permits
            .lock()
            .unwrap()
            .retain(|_, permit| permit.expires_at >= now);
    }
}

/// Keep unconsumed one-shot permits fenced to the exact live Tauri registration.
/// Subscribe before taking the initial snapshot so a disconnect racing startup is
/// either present in the snapshot or queued as an event. A lagged consumer performs
/// an authoritative registry reconciliation instead of trusting the incomplete
/// event stream.
pub(crate) fn spawn_permit_revocation_watcher(
    permits: PrivilegedPermitStore,
    registry: SessionShellRegistry,
) -> tokio::task::JoinHandle<()> {
    let mut events = registry.subscribe();
    let mut expiry_tick = tokio::time::interval(Duration::from_secs(1));
    expiry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    permits.retain_current_registrations(&registry);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = expiry_tick.tick() => permits.revoke_expired(),
                event = events.recv() => match event {
                    Ok(crate::host_control::session_shell::SessionShellRegistryEvent::Registered(
                        _,
                    )) => {}
                    Ok(
                        crate::host_control::session_shell::SessionShellRegistryEvent::Disconnected {
                            registration_id,
                            registration_generation,
                            ..
                        },
                    ) => permits.revoke_registration(registration_id, registration_generation),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        permits.retain_current_registrations(&registry);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        permits.revoke_all();
                        break;
                    }
                }
            }
        }
    })
}

/// The initial root allowlist is intentionally tiny. Each variant maps to one
/// compiled-in, revisioned template over the product's own system service. It
/// cannot name an arbitrary unit, program, or argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedServiceAction {
    Start,
    Stop,
    Restart,
}

impl PrivilegedServiceAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }

    fn template_id(self) -> &'static str {
        match self {
            Self::Start => "linux.systemd.start.lcxl-remote-desk.v1",
            Self::Stop => "linux.systemd.stop.lcxl-remote-desk.v1",
            Self::Restart => "linux.systemd.restart.lcxl-remote-desk.v1",
        }
    }

    fn from_template_id(template_id: &str) -> Option<Self> {
        [Self::Start, Self::Stop, Self::Restart]
            .into_iter()
            .find(|action| action.template_id() == template_id)
    }
}

/// Rebuild one privileged built-in from constants owned by the root daemon.
/// Callers must never construct an Administrator draft from manager-supplied or
/// worker-supplied argv. The revision is part of the template id and resource
/// profile, while every executable field is covered by the fingerprint.
pub fn privileged_service_draft(action: PrivilegedServiceAction) -> ExecPlanDraft {
    let program = SYSTEMCTL_PATH.to_string();
    let argv = vec![action.verb().to_string(), MANAGED_SERVICE_UNIT.to_string()];
    let containment = ExecContainmentSnapshot {
        allow_background: false,
        required_enforcement: RequiredEnforcement::NativeHard,
        max_processes: Some(PRIVILEGED_MAX_PROCESSES),
        max_memory_bytes: Some(PRIVILEGED_MAX_MEMORY_BYTES),
        cpu_max_percent: Some(PRIVILEGED_CPU_MAX_PERCENT),
        // systemd needs a concrete block device to apply IO bandwidth limits.
        // The first template therefore declares no fake/un-enforced IO cap.
        io_max_bytes_per_sec: None,
        resource_profile_id: Some(PRIVILEGED_TEMPLATE_PROFILE.to_string()),
        resource_profile_revision: Some(PRIVILEGED_TEMPLATE_REVISION),
    };
    let limits = ExecLimits {
        timeout_ms: PRIVILEGED_TIMEOUT_MS,
        max_stdout_bytes: PRIVILEGED_OUTPUT_BYTES,
        max_stderr_bytes: PRIVILEGED_OUTPUT_BYTES,
    };
    let principal = ExecutionPrincipal::Administrator;
    let fingerprint =
        fingerprint_for_principal(&program, &argv, None, &limits, &containment, principal);
    ExecPlanDraft {
        program,
        argv,
        cwd: None,
        shell: ExecShellKind::Native,
        risk: RiskLevel::Critical,
        execution_basis: ExecExecutionBasis::Template,
        principal,
        template_id: action.template_id().to_string(),
        fingerprint,
        timeout_ms: limits.timeout_ms,
        max_stdout_bytes: limits.max_stdout_bytes,
        max_stderr_bytes: limits.max_stderr_bytes,
        containment,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedPlanError {
    WrongPrincipal,
    UnknownTemplate,
    InputDrift,
    PlanDrift,
}

impl std::fmt::Display for PrivilegedPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::WrongPrincipal => "the plan is not an Administrator plan",
            Self::UnknownTemplate => "the privileged template is not allowlisted",
            Self::InputDrift => "the privileged command input does not match the sealed template",
            Self::PlanDrift => "the privileged plan differs from its compiled-in template",
        })
    }
}

impl std::error::Error for PrivilegedPlanError {}

fn plan_draft(plan: &ExecPlan) -> ExecPlanDraft {
    ExecPlanDraft {
        program: plan.program.clone(),
        argv: plan.argv.clone(),
        cwd: plan.cwd.clone(),
        shell: plan.shell,
        risk: plan.risk,
        execution_basis: plan.execution_basis,
        principal: plan.principal,
        template_id: plan.template_id.clone(),
        fingerprint: plan.fingerprint.clone(),
        timeout_ms: plan.timeout_ms,
        max_stdout_bytes: plan.max_stdout_bytes,
        max_stderr_bytes: plan.max_stderr_bytes,
        containment: plan.containment.clone(),
    }
}

fn validate_privileged_plan(plan: &ExecPlan) -> Result<(), PrivilegedPlanError> {
    if plan.principal != ExecutionPrincipal::Administrator {
        return Err(PrivilegedPlanError::WrongPrincipal);
    }
    let action = PrivilegedServiceAction::from_template_id(&plan.template_id)
        .ok_or(PrivilegedPlanError::UnknownTemplate)?;
    if plan_draft(plan) != privileged_service_draft(action) {
        return Err(PrivilegedPlanError::PlanDrift);
    }
    Ok(())
}

pub(crate) fn validate_privileged_agentic_request(
    plan: &ExecPlan,
    input: &desk_agent_protocol::ExecInput,
) -> Result<(), PrivilegedPlanError> {
    validate_privileged_plan(plan)?;
    let action = PrivilegedServiceAction::from_template_id(&plan.template_id)
        .ok_or(PrivilegedPlanError::UnknownTemplate)?;
    if input.cwd.is_some()
        || !matches!(input.target, desk_agent_protocol::ExecTarget::Shell { .. })
        || !["systemctl", SYSTEMCTL_PATH].into_iter().any(|program| {
            input.command.trim() == format!("{program} {} {MANAGED_SERVICE_UNIT}", action.verb())
        })
    {
        return Err(PrivilegedPlanError::InputDrift);
    }
    Ok(())
}

/// A fully generated transient-service invocation. `program` and every option
/// are daemon constants; the only varying option is a SHA-256-derived unit name.
/// The sealed executable and argv appear after a literal `--` and are never
/// parsed as systemd-run options.
#[derive(Debug, PartialEq, Eq)]
pub struct SystemdTransientSpec {
    unit_name: String,
    stdout_path: String,
    stderr_path: String,
    program: &'static str,
    argv: Vec<String>,
}

impl SystemdTransientSpec {
    fn from_plan(plan: &ExecPlan) -> Result<Self, PrivilegedPlanError> {
        validate_privileged_plan(plan)?;
        let unit_name = transient_unit_name(&plan.execution_generation);
        let digest = execution_digest_hex(&plan.execution_generation);
        let stdout_path = format!("{PRIVILEGED_OUTPUT_DIR}/{digest}.stdout");
        let stderr_path = format!("{PRIVILEGED_OUTPUT_DIR}/{digest}.stderr");
        let mut argv = vec![
            format!("--unit={unit_name}"),
            "--collect".to_string(),
            "--wait".to_string(),
            "--quiet".to_string(),
            "--property=Type=exec".to_string(),
            "--property=User=root".to_string(),
            "--property=WorkingDirectory=/".to_string(),
            "--property=UMask=0077".to_string(),
            "--property=StandardInput=null".to_string(),
            format!("--property=StandardOutput=file:{stdout_path}"),
            format!("--property=StandardError=file:{stderr_path}"),
            format!("--property=LimitFSIZE={PRIVILEGED_RAW_OUTPUT_BYTES}"),
            "--property=KillMode=control-group".to_string(),
            "--property=SendSIGKILL=yes".to_string(),
            "--property=TimeoutStopSec=5s".to_string(),
            format!("--property=RuntimeMaxSec={}ms", plan.timeout_ms),
            format!(
                "--property=TasksMax={}",
                plan.containment.max_processes.expect("validated template")
            ),
            format!(
                "--property=MemoryMax={}",
                plan.containment
                    .max_memory_bytes
                    .expect("validated template")
            ),
            format!(
                "--property=CPUQuota={}%",
                plan.containment
                    .cpu_max_percent
                    .expect("validated template")
            ),
            "--property=NoNewPrivileges=yes".to_string(),
            "--property=PrivateTmp=yes".to_string(),
            "--property=PrivateDevices=yes".to_string(),
            "--property=ProtectHome=yes".to_string(),
            "--property=ProtectSystem=strict".to_string(),
            format!("--property=ReadWritePaths={PRIVILEGED_OUTPUT_DIR}"),
            "--property=ProtectKernelTunables=yes".to_string(),
            "--property=ProtectKernelModules=yes".to_string(),
            "--property=ProtectControlGroups=yes".to_string(),
            "--property=RestrictSUIDSGID=yes".to_string(),
            "--property=LockPersonality=yes".to_string(),
            "--property=MemoryDenyWriteExecute=yes".to_string(),
            "--property=RestrictAddressFamilies=AF_UNIX".to_string(),
            "--setenv=PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            "--setenv=LANG=C.UTF-8".to_string(),
            "--setenv=LC_ALL=C.UTF-8".to_string(),
            "--".to_string(),
            plan.program.clone(),
        ];
        argv.extend(plan.argv.iter().cloned());
        Ok(Self {
            unit_name,
            stdout_path,
            stderr_path,
            program: SYSTEMD_RUN_PATH,
            argv,
        })
    }

    pub fn unit_name(&self) -> &str {
        &self.unit_name
    }
}

/// Unit identity contains no caller-controlled text. The full digest keeps the
/// collision domain at SHA-256 while remaining well under systemd's name limit.
pub fn transient_unit_name(execution_generation: &str) -> String {
    format!(
        "lcxl-ai-exec-{}.service",
        execution_digest_hex(execution_generation)
    )
}

fn execution_digest_hex(execution_generation: &str) -> String {
    let digest = Sha256::digest(execution_generation.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

/// True only for a ledger row that can be handed to the Linux privileged
/// recovery supervisor. Prefix matching alone is insufficient: the sealed plan,
/// task/generation/fingerprint, and deterministic unit identity must all agree.
pub fn is_recoverable_privileged_ledger_row(
    row: &crate::daemon::exec_ledger::exec_ledger_entry::Model,
) -> bool {
    let Some(plan_json) = row.plan_json.as_deref() else {
        return false;
    };
    let Ok(plan) = serde_json::from_str::<ExecPlan>(plan_json) else {
        return false;
    };
    validate_privileged_plan(&plan).is_ok()
        && plan.exec_request_id.0 == row.task_id
        && plan.execution_generation == row.execution_generation
        && plan.fingerprint == row.plan_fingerprint
        && row.containment_identity.as_deref()
            == Some(transient_unit_name(&plan.execution_generation).as_str())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TransientRunOutcome {
    Exited {
        exit_code: i32,
        unit_observed: bool,
        duration_ms: u32,
    },
    SpawnFailed(String),
    WaitFailed {
        unit_observed: bool,
        reason: String,
    },
    TimedOut {
        unit_observed: bool,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TransientUnitInspection {
    Missing,
    Running,
    Exited { exit_code: i32 },
    Unknown(String),
}

#[async_trait]
trait TransientCommandRunner: Send + Sync {
    async fn run(
        &self,
        spec: &SystemdTransientSpec,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    ) -> TransientRunOutcome;
    async fn inspect(&self, unit_name: &str) -> Result<TransientUnitInspection, String>;
    async fn terminate(&self, unit_name: &str);
}

struct RealTransientCommandRunner;

#[async_trait]
impl TransientCommandRunner for RealTransientCommandRunner {
    async fn run(
        &self,
        spec: &SystemdTransientSpec,
        timeout: Duration,
        cancelled: Arc<AtomicBool>,
    ) -> TransientRunOutcome {
        if cancelled.load(Ordering::Acquire) {
            return TransientRunOutcome::Cancelled;
        }
        if let Err(error) = prepare_privileged_output_paths(spec, 0) {
            return TransientRunOutcome::SpawnFailed(error);
        }
        if cancelled.load(Ordering::Acquire) {
            cleanup_privileged_output_paths(spec);
            return TransientRunOutcome::Cancelled;
        }

        let mut command = Command::new(spec.program);
        command
            .args(&spec.argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // This is only systemd-run's own fixed-binary diagnostic stream;
            // service output goes to the bounded root-only files in `spec`.
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let started = Instant::now();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                cleanup_privileged_output_paths(spec);
                return TransientRunOutcome::SpawnFailed(format!(
                    "systemd-run could not be started: {error}"
                ));
            }
        };
        let mut diagnostic = child.stderr.take();
        let diagnostic_reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut retained = Vec::new();
            let mut chunk = [0u8; 1024];
            if let Some(reader) = diagnostic.as_mut() {
                loop {
                    match reader.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(count) if retained.len() < SYSTEMD_CLIENT_DIAGNOSTIC_BYTES => {
                            let take =
                                (SYSTEMD_CLIENT_DIAGNOSTIC_BYTES - retained.len()).min(count);
                            retained.extend_from_slice(&chunk[..take]);
                        }
                        Ok(_) => {}
                    }
                }
            }
            retained
        });

        let deadline = timeout.saturating_add(SYSTEMD_CLIENT_GRACE);
        let mut unit_observed = false;
        let mut was_cancelled = false;
        let status = loop {
            if cancelled.load(Ordering::Acquire) {
                was_cancelled = true;
                break None;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(Ok(status)),
                Err(error) => break Some(Err(error)),
                Ok(None) => {}
            }
            if !unit_observed {
                unit_observed = systemd_unit_exists(&spec.unit_name).await;
            }
            if started.elapsed() >= deadline {
                break None;
            }
            tokio::time::sleep(UNIT_OBSERVE_INTERVAL).await;
        };

        if status.is_none() {
            let _ = child.start_kill();
            let _ = child.wait().await;
            self.terminate(&spec.unit_name).await;
            let _ = diagnostic_reader.await;
            unit_observed |= privileged_output_exists(spec);
            if was_cancelled {
                return TransientRunOutcome::Cancelled;
            }
            return TransientRunOutcome::TimedOut { unit_observed };
        }

        let diagnostic = diagnostic_reader.await.unwrap_or_default();
        let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
        unit_observed |= privileged_output_exists(spec);
        match status.expect("checked above") {
            Ok(status) => TransientRunOutcome::Exited {
                exit_code: status.code().unwrap_or(-1),
                unit_observed,
                duration_ms,
            },
            Err(error) => TransientRunOutcome::WaitFailed {
                unit_observed,
                reason: format!(
                    "systemd-run wait failed: {error}; diagnostic_bytes={}",
                    diagnostic.len()
                ),
            },
        }
    }

    async fn inspect(&self, unit_name: &str) -> Result<TransientUnitInspection, String> {
        inspect_systemd_unit(unit_name).await
    }

    async fn terminate(&self, unit_name: &str) {
        // Unit names are SHA-256-derived, never caller text. Kill the whole
        // control group first, then stop/reset the transient unit. Every call is
        // best effort because the unit may already have been collected.
        for args in [
            vec!["kill", "--kill-whom=all", "--signal=SIGKILL", unit_name],
            vec!["stop", unit_name],
            vec!["reset-failed", unit_name],
        ] {
            let mut command = Command::new(SYSTEMCTL_PATH);
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let _ = tokio::time::timeout(SYSTEMCTL_OPERATION_TIMEOUT, command.status()).await;
        }
    }
}

#[cfg(test)]
struct TestAllowPolkitRunner;

#[cfg(test)]
#[async_trait]
impl PolkitCommandRunner for TestAllowPolkitRunner {
    async fn check(&self, _subject: PolkitSubject) -> PolkitCommandOutcome {
        PolkitCommandOutcome::Exit(0)
    }
}

#[cfg(test)]
struct TestSpawnFailedTransientRunner;

#[cfg(test)]
#[async_trait]
impl TransientCommandRunner for TestSpawnFailedTransientRunner {
    async fn run(
        &self,
        _spec: &SystemdTransientSpec,
        _timeout: Duration,
        _cancelled: Arc<AtomicBool>,
    ) -> TransientRunOutcome {
        TransientRunOutcome::SpawnFailed("test runner refused spawn".to_string())
    }

    async fn inspect(&self, _unit_name: &str) -> Result<TransientUnitInspection, String> {
        Ok(TransientUnitInspection::Missing)
    }

    async fn terminate(&self, _unit_name: &str) {}
}

async fn inspect_systemd_unit(unit_name: &str) -> Result<TransientUnitInspection, String> {
    let mut command = Command::new(SYSTEMCTL_PATH);
    command
        .args([
            "show",
            unit_name,
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=Result",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
            "--no-pager",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(SYSTEMCTL_OPERATION_TIMEOUT, command.output())
        .await
        .map_err(|_| "systemctl show timed out".to_string())?
        .map_err(|error| format!("systemctl show failed: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_systemd_unit_inspection(output.status.success(), &text)
}

fn parse_systemd_unit_inspection(
    command_succeeded: bool,
    text: &str,
) -> Result<TransientUnitInspection, String> {
    // `systemctl show` may return non-zero for a collected/missing unit while
    // still printing LoadState=not-found. Parse that explicit state first.
    let mut fields = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key, value);
        }
    }
    if fields.get("LoadState") == Some(&"not-found") {
        return Ok(TransientUnitInspection::Missing);
    }
    if !command_succeeded {
        return Err("systemctl could not query transient unit state".to_string());
    }
    match fields.get("ActiveState").copied() {
        Some("active" | "activating" | "reloading" | "deactivating") => {
            Ok(TransientUnitInspection::Running)
        }
        Some("inactive" | "failed") => {
            let status = fields
                .get("ExecMainStatus")
                .and_then(|value| value.parse::<i32>().ok());
            let code_is_exit = matches!(fields.get("ExecMainCode").copied(), Some("1" | "exited"));
            if code_is_exit {
                return status
                    .map(|exit_code| TransientUnitInspection::Exited { exit_code })
                    .ok_or_else(|| "transient unit has no parseable exit status".to_string());
            }
            if fields.get("Result") == Some(&"success") {
                return Ok(TransientUnitInspection::Exited { exit_code: 0 });
            }
            Ok(TransientUnitInspection::Unknown(format!(
                "active_state={:?} sub_state={:?} result={:?} exec_main_code={:?}",
                fields.get("ActiveState"),
                fields.get("SubState"),
                fields.get("Result"),
                fields.get("ExecMainCode")
            )))
        }
        other => Ok(TransientUnitInspection::Unknown(format!(
            "unrecognized active state {other:?}"
        ))),
    }
}

async fn systemd_unit_exists(unit_name: &str) -> bool {
    let mut command = Command::new(SYSTEMCTL_PATH);
    command
        .args([
            "show",
            unit_name,
            "--property=LoadState",
            "--value",
            "--no-pager",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(SYSTEMCTL_OPERATION_TIMEOUT, command.output()).await;
    output.is_ok_and(|output| {
        output.is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "not-found"
        })
    })
}

fn prepare_privileged_output_paths(
    spec: &SystemdTransientSpec,
    required_uid: u32,
) -> Result<(), String> {
    fs::create_dir_all(PRIVILEGED_OUTPUT_DIR)
        .map_err(|error| format!("could not create privileged output directory: {error}"))?;
    fs::set_permissions(PRIVILEGED_OUTPUT_DIR, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not protect privileged output directory: {error}"))?;
    let metadata = fs::symlink_metadata(PRIVILEGED_OUTPUT_DIR)
        .map_err(|error| format!("could not inspect privileged output directory: {error}"))?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != required_uid
        || metadata.mode() & 0o077 != 0
    {
        return Err("privileged output directory has unsafe owner, mode, or type".to_string());
    }
    for path in [&spec.stdout_path, &spec.stderr_path] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not inspect privileged output path: {error}")),
            Ok(_) => return Err("privileged output path already exists".to_string()),
        }
    }
    Ok(())
}

fn privileged_output_exists(spec: &SystemdTransientSpec) -> bool {
    [&spec.stdout_path, &spec.stderr_path]
        .into_iter()
        .any(|path| fs::symlink_metadata(path).is_ok())
}

fn cleanup_privileged_output_paths(spec: &SystemdTransientSpec) {
    for path in [&spec.stdout_path, &spec.stderr_path] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("could not remove privileged output artifact: {error}"),
        }
    }
}

fn read_privileged_output(path: &str, required_uid: u32) -> Result<(Vec<u8>, bool), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false));
        }
        Err(error) => return Err(format!("could not inspect privileged output: {error}")),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != required_uid
        || metadata.mode() & 0o077 != 0
        || metadata.len() > PRIVILEGED_RAW_OUTPUT_BYTES as u64
    {
        return Err("privileged output has unsafe owner, mode, type, or size".to_string());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("could not open privileged output: {error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("could not read privileged output: {error}"))?;
    if bytes.len() > PRIVILEGED_RAW_OUTPUT_BYTES as usize {
        return Err("privileged output exceeded its hard size limit".to_string());
    }
    Ok((bytes, metadata.len() >= PRIVILEGED_RAW_OUTPUT_BYTES as u64))
}

fn sanitize_privileged_output(
    plan: &ExecPlan,
    exit_code: i32,
    duration_ms: u32,
    stdout: (Vec<u8>, bool),
    stderr: (Vec<u8>, bool),
) -> AgentOutcome {
    let redactor = RegexRedactor::new();
    let stdout_redacted = match redactor.redact(&String::from_utf8_lossy(&stdout.0)) {
        Ok(output) => output,
        Err(_) => return AgentOutcome::Err(redaction_failed()),
    };
    let stderr_redacted = match redactor.redact(&String::from_utf8_lossy(&stderr.0)) {
        Ok(output) => output,
        Err(_) => return AgentOutcome::Err(redaction_failed()),
    };
    let mut redactions = stdout_redacted.kinds;
    redactions.extend(stderr_redacted.kinds);
    let (stdout, stdout_truncated) = finalize_redacted_output(
        stdout_redacted.text,
        plan.max_stdout_bytes as usize,
        stdout.1,
    );
    let (stderr, stderr_truncated) = finalize_redacted_output(
        stderr_redacted.text,
        plan.max_stderr_bytes as usize,
        stderr.1,
    );
    AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
        exit_code,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration_ms,
        redactions,
    }))
}

fn finalize_redacted_output(text: String, cap: usize, overflowed: bool) -> (String, bool) {
    if text.len() <= cap && !overflowed {
        return (text, false);
    }
    let mut end = cap.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = text[..end].to_string();
    if end < text.len()
        && let Some(position) = output.rfind(char::is_whitespace)
    {
        output.truncate(position);
    }
    (output, true)
}

fn redaction_failed() -> AgentError {
    agent_error(
        AgentErrorKind::RedactionFailed,
        "administrator command output was withheld because redaction failed",
    )
}

fn agent_error(kind: AgentErrorKind, message: impl Into<String>) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[derive(Debug)]
pub enum PrivilegedPrepareError {
    Permit(PermitError),
    Plan(PrivilegedPlanError),
    Ledger(String),
    Duplicate,
    GenerationFingerprintMismatch,
}

#[derive(Debug)]
pub enum PrivilegedAuthorizationError {
    Plan(PrivilegedPlanError),
    Authorization(AuthorizationError),
}

impl std::fmt::Display for PrivilegedAuthorizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(error) => write!(formatter, "privileged plan rejected: {error}"),
            Self::Authorization(error) => {
                write!(formatter, "administrator authorization failed: {error}")
            }
        }
    }
}

impl std::error::Error for PrivilegedAuthorizationError {}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PrivilegedReconcileSummary {
    pub recovered_running: usize,
    pub recovered_terminal: usize,
    pub marked_indeterminate: usize,
}

impl std::fmt::Display for PrivilegedPrepareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permit(error) => write!(formatter, "privilege permit rejected: {error:?}"),
            Self::Plan(error) => write!(formatter, "privileged plan rejected: {error}"),
            Self::Ledger(error) => write!(formatter, "privileged ledger unavailable: {error}"),
            Self::Duplicate => formatter.write_str("execution generation was already reserved"),
            Self::GenerationFingerprintMismatch => {
                formatter.write_str("execution generation was replayed with a different plan")
            }
        }
    }
}

impl std::error::Error for PrivilegedPrepareError {}

/// Narrow root-side admission seam. A successful return means the one-shot
/// permit is gone and the deterministic unit identity is durably in the ledger.
/// Only then may the future launcher invoke systemd-run with the returned spec.
#[derive(Clone)]
pub struct LinuxPrivilegedExecSupervisor {
    permits: PrivilegedPermitStore,
    authorizer: LinuxPolkitAuthorizer,
    ledger: Arc<ExecLedger>,
    runner: Arc<dyn TransientCommandRunner>,
    active: Arc<Mutex<HashMap<String, ActivePrivilegedDispatch>>>,
}

struct ActivePrivilegedDispatch {
    cancelled: Arc<AtomicBool>,
    launch_started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedCancelOutcome {
    NotPrivileged,
    CancelledBeforeStart,
    CancelRequested,
    RecoveredTerminal,
    Indeterminate,
    AlreadyTerminal,
}

impl LinuxPrivilegedExecSupervisor {
    pub fn new(permits: PrivilegedPermitStore, ledger: Arc<ExecLedger>) -> Self {
        Self {
            permits,
            authorizer: LinuxPolkitAuthorizer::default(),
            ledger,
            runner: Arc::new(RealTransientCommandRunner),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    fn with_runner(
        permits: PrivilegedPermitStore,
        ledger: Arc<ExecLedger>,
        runner: Arc<dyn TransientCommandRunner>,
    ) -> Self {
        Self {
            permits,
            authorizer: LinuxPolkitAuthorizer::default(),
            ledger,
            runner,
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_authorized_spawn_failure(ledger: Arc<ExecLedger>) -> Self {
        Self {
            permits: PrivilegedPermitStore::default(),
            authorizer: LinuxPolkitAuthorizer {
                runner: Arc::new(TestAllowPolkitRunner),
            },
            ledger,
            runner: Arc::new(TestSpawnFailedTransientRunner),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn authorize_and_mint(
        &self,
        plan: &ExecPlan,
        registry: &SessionShellRegistry,
        registration: &Arc<RegisteredSessionShell>,
    ) -> Result<Uuid, PrivilegedAuthorizationError> {
        validate_privileged_plan(plan).map_err(PrivilegedAuthorizationError::Plan)?;
        self.authorizer
            .authorize(registry, registration)
            .await
            .map_err(PrivilegedAuthorizationError::Authorization)?;
        Ok(self.permits.mint(plan, registration).permit_id)
    }

    pub async fn prepare_dispatch(
        &self,
        permit_id: Uuid,
        plan: &ExecPlan,
        registry: &SessionShellRegistry,
        registration: &Arc<RegisteredSessionShell>,
    ) -> Result<SystemdTransientSpec, PrivilegedPrepareError> {
        self.permits
            .consume(permit_id, plan, registry, registration)
            .map_err(PrivilegedPrepareError::Permit)?;
        let spec = SystemdTransientSpec::from_plan(plan).map_err(PrivilegedPrepareError::Plan)?;
        let plan_json = serde_json::to_string(plan)
            .map_err(|error| PrivilegedPrepareError::Ledger(error.to_string()))?;
        match self
            .ledger
            .reserve_with_sealed_plan(
                &plan.exec_request_id.0,
                &plan.execution_generation,
                &plan.fingerprint,
                &spec.unit_name,
                &plan_json,
            )
            .await
            .map_err(|error| PrivilegedPrepareError::Ledger(error.to_string()))?
        {
            Reservation::Granted => {
                self.active.lock().unwrap().insert(
                    plan.execution_generation.clone(),
                    ActivePrivilegedDispatch {
                        cancelled: Arc::new(AtomicBool::new(false)),
                        launch_started: false,
                    },
                );
                Ok(spec)
            }
            Reservation::Duplicate(_) => Err(PrivilegedPrepareError::Duplicate),
            Reservation::FingerprintMismatch => {
                Err(PrivilegedPrepareError::GenerationFingerprintMismatch)
            }
        }
    }

    /// Burn an authorization that became stale before the dispatch handshake
    /// consumed it. A stale host/manager decision must never remain usable for
    /// the rest of the permit TTL.
    pub(crate) fn discard_permit(&self, permit_id: Uuid) {
        self.permits.revoke(permit_id);
    }

    /// Execute a spec returned by [`Self::prepare_dispatch`]. This is kept
    /// separate from authorization so Stage 3 can perform its final freshness
    /// checks between permit mint and dispatch intent. The spec is rebuilt from
    /// the plan again before spawn; even an internal stale/mismatched value is
    /// therefore refused.
    pub async fn execute_prepared(
        &self,
        plan: &ExecPlan,
        spec: SystemdTransientSpec,
    ) -> AgentOutcome {
        let expected = match SystemdTransientSpec::from_plan(plan) {
            Ok(expected) => expected,
            Err(error) => {
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::PermissionDenied,
                    format!("privileged plan rejected before launch: {error}"),
                ));
            }
        };
        if spec != expected {
            return AgentOutcome::Err(agent_error(
                AgentErrorKind::PermissionDenied,
                "privileged transient-service specification drifted before launch",
            ));
        }

        // A pre-launch cancel (or another terminal recovery decision) can win
        // after prepare reserved the row but before this future is polled. The
        // durable terminal state is authoritative: never recreate an active
        // token and spawn after it.
        match self.ledger.get(&plan.execution_generation).await {
            Ok(Some(row)) if row.state == crate::daemon::exec_ledger::State::Terminal.as_str() => {
                return row
                    .result_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<AgentOutcome>(json).ok())
                    .unwrap_or_else(|| {
                        AgentOutcome::Err(agent_error(
                            AgentErrorKind::Internal,
                            "administrator command already ended but its result is unavailable",
                        ))
                    });
            }
            Ok(Some(row))
                if matches!(
                    crate::daemon::exec_ledger::State::parse(&row.state),
                    Some(
                        crate::daemon::exec_ledger::State::Indeterminate
                            | crate::daemon::exec_ledger::State::SpawnFailed
                    )
                ) =>
            {
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "administrator command is already terminal and cannot be launched",
                ));
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "administrator command has no durable launch reservation",
                ));
            }
        }

        let cancelled = {
            let mut active = self.active.lock().unwrap();
            let entry = active
                .entry(plan.execution_generation.clone())
                .or_insert_with(|| ActivePrivilegedDispatch {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    launch_started: false,
                });
            entry.launch_started = true;
            entry.cancelled.clone()
        };

        let run = self
            .runner
            .run(
                &spec,
                Duration::from_millis(plan.timeout_ms as u64),
                cancelled,
            )
            .await;
        let outcome = match run {
            TransientRunOutcome::SpawnFailed(reason) => {
                cleanup_privileged_output_paths(&spec);
                if let Err(error) = self
                    .ledger
                    .mark_terminal(
                        &plan.execution_generation,
                        Terminal::SpawnFailed(reason.clone()),
                    )
                    .await
                {
                    log::error!(
                        "could not record privileged spawn failure for {}: {error}",
                        plan.execution_generation
                    );
                }
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "administrator command was not started because systemd launch failed",
                ))
            }
            TransientRunOutcome::Exited {
                exit_code,
                unit_observed,
                duration_ms,
            } if unit_observed || exit_code == 0 => {
                self.record_privileged_running(plan, &spec).await;
                let output =
                    read_and_cleanup_privileged_outputs(&spec, plan, exit_code, duration_ms);
                self.record_privileged_terminal(plan, &output).await;
                output
            }
            TransientRunOutcome::Exited { .. } => {
                cleanup_privileged_output_paths(&spec);
                self.record_privileged_indeterminate(plan).await;
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "systemd returned an error before the daemon could prove whether the administrator command started",
                ))
            }
            TransientRunOutcome::TimedOut { unit_observed } if unit_observed => {
                self.record_privileged_running(plan, &spec).await;
                cleanup_privileged_output_paths(&spec);
                let outcome = AgentOutcome::Err(agent_error(
                    AgentErrorKind::Timeout,
                    "administrator command timed out and its transient service was reclaimed",
                ));
                self.record_privileged_terminal(plan, &outcome).await;
                outcome
            }
            TransientRunOutcome::TimedOut { .. } => {
                cleanup_privileged_output_paths(&spec);
                self.record_privileged_indeterminate(plan).await;
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "administrator command launch timed out before start could be proven",
                ))
            }
            TransientRunOutcome::WaitFailed {
                unit_observed: true,
                reason,
            } => {
                self.record_privileged_running(plan, &spec).await;
                cleanup_privileged_output_paths(&spec);
                self.record_privileged_indeterminate(plan).await;
                log::warn!(
                    "lost privileged systemd-run result for {}: {reason}",
                    plan.execution_generation
                );
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "administrator command started but its final state is unknown",
                ))
            }
            TransientRunOutcome::WaitFailed {
                unit_observed: false,
                reason,
            } => {
                cleanup_privileged_output_paths(&spec);
                self.record_privileged_indeterminate(plan).await;
                log::warn!(
                    "could not determine privileged systemd-run start for {}: {reason}",
                    plan.execution_generation
                );
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "administrator command state is unknown",
                ))
            }
            TransientRunOutcome::Cancelled => {
                cleanup_privileged_output_paths(&spec);
                let outcome = privileged_cancelled_outcome(
                    "administrator command was cancelled and its transient service was reclaimed",
                );
                self.record_privileged_terminal(plan, &outcome).await;
                outcome
            }
        };
        self.active
            .lock()
            .unwrap()
            .remove(&plan.execution_generation);
        outcome
    }

    /// Cancel one generation without ever falling through to a session worker
    /// when the durable row proves that it is a privileged transient. The
    /// in-memory pre-launch token closes the reserve→spawn race; after daemon
    /// restart the sealed ledger row and deterministic unit name provide the
    /// recovery authority instead.
    pub async fn cancel_generation(
        &self,
        execution_generation: &str,
    ) -> Result<PrivilegedCancelOutcome, String> {
        let Some(row) = self
            .ledger
            .get(execution_generation)
            .await
            .map_err(|error| format!("could not read privileged execution: {error}"))?
        else {
            return Ok(PrivilegedCancelOutcome::NotPrivileged);
        };
        if !is_recoverable_privileged_ledger_row(&row) {
            return Ok(PrivilegedCancelOutcome::NotPrivileged);
        }
        if !matches!(
            crate::daemon::exec_ledger::State::parse(&row.state),
            Some(
                crate::daemon::exec_ledger::State::Reserved
                    | crate::daemon::exec_ledger::State::Running
            )
        ) {
            return Ok(PrivilegedCancelOutcome::AlreadyTerminal);
        }
        let plan = serde_json::from_str::<ExecPlan>(
            row.plan_json
                .as_deref()
                .expect("recoverable privileged row has a sealed plan"),
        )
        .map_err(|error| format!("could not decode privileged execution: {error}"))?;
        let spec = SystemdTransientSpec::from_plan(&plan)
            .map_err(|error| format!("stored privileged plan became invalid: {error}"))?;

        let active_state = {
            let mut active = self.active.lock().unwrap();
            active.get_mut(execution_generation).map(|entry| {
                entry.cancelled.store(true, Ordering::Release);
                entry.launch_started
            })
        };
        if active_state == Some(false) {
            cleanup_privileged_output_paths(&spec);
            let outcome = privileged_cancelled_outcome(
                "administrator command was cancelled before it started",
            );
            self.record_privileged_terminal(&plan, &outcome).await;
            self.active.lock().unwrap().remove(execution_generation);
            return Ok(PrivilegedCancelOutcome::CancelledBeforeStart);
        }
        if active_state == Some(true) {
            // The runner observes the same token before spawn and throughout
            // its wait loop. Terminating here as well shortens cancellation for
            // a unit that systemd has already admitted.
            self.runner.terminate(&spec.unit_name).await;
            return Ok(PrivilegedCancelOutcome::CancelRequested);
        }

        // Recovery path: there is no live launch token after daemon restart, so
        // systemd state is the only authority. Never infer "not started" from a
        // missing unit because --collect may already have removed it.
        match self.runner.inspect(&spec.unit_name).await {
            Ok(TransientUnitInspection::Running) => {
                self.runner.terminate(&spec.unit_name).await;
                cleanup_privileged_output_paths(&spec);
                let outcome = privileged_cancelled_outcome(
                    "administrator command was cancelled and its recovered transient service was reclaimed",
                );
                self.record_privileged_terminal(&plan, &outcome).await;
                Ok(PrivilegedCancelOutcome::CancelRequested)
            }
            Ok(TransientUnitInspection::Exited { exit_code }) => {
                let outcome = read_and_cleanup_privileged_outputs(&spec, &plan, exit_code, 0);
                self.record_privileged_terminal(&plan, &outcome).await;
                self.runner.terminate(&spec.unit_name).await;
                Ok(PrivilegedCancelOutcome::RecoveredTerminal)
            }
            Ok(TransientUnitInspection::Missing | TransientUnitInspection::Unknown(_)) | Err(_) => {
                cleanup_privileged_output_paths(&spec);
                self.record_privileged_indeterminate(&plan).await;
                Ok(PrivilegedCancelOutcome::Indeterminate)
            }
        }
    }

    /// Reconcile only the privileged rows explicitly deferred by daemon startup.
    /// A malformed row is never treated as a systemd unit. Running units get a
    /// bounded background watcher; terminal units recover their root-only output;
    /// missing/ambiguous units become Indeterminate and can never be re-spawned.
    pub async fn reconcile_in_flight(&self) -> Result<PrivilegedReconcileSummary, String> {
        let rows = self
            .ledger
            .in_flight()
            .await
            .map_err(|error| format!("could not read privileged recovery ledger: {error}"))?;
        let mut summary = PrivilegedReconcileSummary::default();
        for row in rows {
            if !is_recoverable_privileged_ledger_row(&row) {
                continue;
            }
            let plan = match row
                .plan_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<ExecPlan>(json).ok())
            {
                Some(plan) => plan,
                None => continue,
            };
            let spec = SystemdTransientSpec::from_plan(&plan)
                .map_err(|error| format!("stored privileged plan became invalid: {error}"))?;
            match self.runner.inspect(&spec.unit_name).await {
                Ok(TransientUnitInspection::Running) => {
                    self.record_privileged_running(&plan, &spec).await;
                    summary.recovered_running += 1;
                    let supervisor = self.clone();
                    tokio::spawn(async move {
                        supervisor.watch_recovered_unit(plan, spec).await;
                    });
                }
                Ok(TransientUnitInspection::Exited { exit_code }) => {
                    self.record_privileged_running(&plan, &spec).await;
                    let outcome = read_and_cleanup_privileged_outputs(&spec, &plan, exit_code, 0);
                    self.record_privileged_terminal(&plan, &outcome).await;
                    self.runner.terminate(&spec.unit_name).await;
                    summary.recovered_terminal += 1;
                }
                Ok(TransientUnitInspection::Missing | TransientUnitInspection::Unknown(_))
                | Err(_) => {
                    cleanup_privileged_output_paths(&spec);
                    self.record_privileged_indeterminate(&plan).await;
                    summary.marked_indeterminate += 1;
                }
            }
        }
        Ok(summary)
    }

    async fn watch_recovered_unit(&self, plan: ExecPlan, spec: SystemdTransientSpec) {
        let started = Instant::now();
        let deadline =
            Duration::from_millis(plan.timeout_ms as u64).saturating_add(SYSTEMD_CLIENT_GRACE);
        loop {
            match self.runner.inspect(&spec.unit_name).await {
                Ok(TransientUnitInspection::Running) if started.elapsed() < deadline => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Ok(TransientUnitInspection::Exited { exit_code }) => {
                    let outcome = read_and_cleanup_privileged_outputs(
                        &spec,
                        &plan,
                        exit_code,
                        started.elapsed().as_millis().min(u32::MAX as u128) as u32,
                    );
                    self.record_privileged_terminal(&plan, &outcome).await;
                    self.runner.terminate(&spec.unit_name).await;
                    return;
                }
                Ok(TransientUnitInspection::Running) => {
                    self.runner.terminate(&spec.unit_name).await;
                    cleanup_privileged_output_paths(&spec);
                    let outcome = AgentOutcome::Err(agent_error(
                        AgentErrorKind::Timeout,
                        "recovered administrator command exceeded its deadline and was reclaimed",
                    ));
                    self.record_privileged_terminal(&plan, &outcome).await;
                    return;
                }
                Ok(TransientUnitInspection::Missing | TransientUnitInspection::Unknown(_))
                | Err(_) => {
                    cleanup_privileged_output_paths(&spec);
                    self.record_privileged_indeterminate(&plan).await;
                    return;
                }
            }
        }
    }

    async fn record_privileged_running(&self, plan: &ExecPlan, spec: &SystemdTransientSpec) {
        if let Err(error) = self
            .ledger
            .mark_running(&plan.execution_generation, Some(&spec.unit_name))
            .await
        {
            log::error!(
                "could not mark privileged execution {} running: {error}",
                plan.execution_generation
            );
        }
    }

    async fn record_privileged_terminal(&self, plan: &ExecPlan, outcome: &AgentOutcome) {
        let result = serde_json::to_string(outcome).unwrap_or_else(|_| "null".to_string());
        if let Err(error) = self
            .ledger
            .mark_terminal(&plan.execution_generation, Terminal::Completed(result))
            .await
        {
            log::error!(
                "could not settle privileged execution {}: {error}",
                plan.execution_generation
            );
        }
    }

    async fn record_privileged_indeterminate(&self, plan: &ExecPlan) {
        if let Err(error) = self
            .ledger
            .mark_terminal(&plan.execution_generation, Terminal::Indeterminate)
            .await
        {
            log::error!(
                "could not mark privileged execution {} indeterminate: {error}",
                plan.execution_generation
            );
        }
    }
}

fn read_and_cleanup_privileged_outputs(
    spec: &SystemdTransientSpec,
    plan: &ExecPlan,
    exit_code: i32,
    duration_ms: u32,
) -> AgentOutcome {
    let stdout = read_privileged_output(&spec.stdout_path, 0);
    let stderr = read_privileged_output(&spec.stderr_path, 0);
    cleanup_privileged_output_paths(spec);
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => {
            sanitize_privileged_output(plan, exit_code, duration_ms, stdout, stderr)
        }
        _ => AgentOutcome::Err(agent_error(
            AgentErrorKind::RedactionFailed,
            "administrator command output was withheld because its root-owned artifact failed validation",
        )),
    }
}

fn privileged_cancelled_outcome(message: &str) -> AgentOutcome {
    AgentOutcome::Err(agent_error(AgentErrorKind::Cancelled, message))
}

fn plan_digest(plan: &ExecPlan) -> [u8; 32] {
    // ExecPlan has a deterministic field order and no maps. This digest is a
    // local authority key, not a cross-version wire protocol.
    let bytes = serde_json::to_vec(plan).expect("ExecPlan serialization cannot fail");
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::exec::{ApprovalId, ExecRequestId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeRunner(PolkitCommandOutcome);

    #[async_trait]
    impl PolkitCommandRunner for FakeRunner {
        async fn check(&self, _subject: PolkitSubject) -> PolkitCommandOutcome {
            self.0
        }
    }

    struct FakeTransientRunner {
        outcome: TransientRunOutcome,
        inspection: TransientUnitInspection,
        runs: AtomicUsize,
        terminations: AtomicUsize,
    }

    #[async_trait]
    impl TransientCommandRunner for FakeTransientRunner {
        async fn run(
            &self,
            _spec: &SystemdTransientSpec,
            _timeout: Duration,
            cancelled: Arc<AtomicBool>,
        ) -> TransientRunOutcome {
            self.runs.fetch_add(1, Ordering::AcqRel);
            if cancelled.load(Ordering::Acquire) {
                return TransientRunOutcome::Cancelled;
            }
            self.outcome.clone()
        }

        async fn inspect(&self, _unit_name: &str) -> Result<TransientUnitInspection, String> {
            Ok(self.inspection.clone())
        }

        async fn terminate(&self, _unit_name: &str) {
            self.terminations.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn current_registration() -> (SessionShellRegistry, Arc<RegisteredSessionShell>) {
        use crate::host_control::protocol::{SESSION_SHELL_PROTOCOL_VERSION, SessionShellInfo};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let identity = read_process_identity(std::process::id()).unwrap();
        let registry = SessionShellRegistry::default();
        let registration = registry
            .register(
                7,
                SessionShellInfo {
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: SESSION_SHELL_PROTOCOL_VERSION,
                    pid: std::process::id(),
                    process_start_ticks: identity.start_ticks,
                    reported_uid: identity.uid,
                    session_id: Some("test-session".to_string()),
                    seat: Some("seat-test".to_string()),
                    session_type: Some("wayland".to_string()),
                    cwd_base64: STANDARD.encode(b"/tmp"),
                    umask: 0o022,
                    environment: Vec::new(),
                },
            )
            .unwrap();
        (registry, registration)
    }

    fn administrator_plan(generation: &str) -> ExecPlan {
        ExecPlan::from_draft(
            ExecRequestId("request-1".into()),
            generation,
            ApprovalId("approval-1".into()),
            privileged_service_draft(PrivilegedServiceAction::Restart),
        )
    }

    #[tokio::test]
    async fn polkit_exit_codes_have_stable_fail_closed_meanings() {
        let (registry, registration) = current_registration();
        for (code, expected) in [
            (1, AuthorizationError::Denied),
            (2, AuthorizationError::AgentUnavailable),
            (3, AuthorizationError::Cancelled),
            (127, AuthorizationError::BackendUnavailable),
        ] {
            let authorizer = LinuxPolkitAuthorizer {
                runner: Arc::new(FakeRunner(PolkitCommandOutcome::Exit(code))),
            };
            assert_eq!(
                authorizer.authorize(&registry, &registration).await,
                Err(expected)
            );
        }
    }

    #[tokio::test]
    async fn timeout_and_backend_failure_are_distinct_fail_closed_results() {
        let (registry, registration) = current_registration();
        for (outcome, expected) in [
            (PolkitCommandOutcome::TimedOut, AuthorizationError::TimedOut),
            (
                PolkitCommandOutcome::Failed,
                AuthorizationError::BackendUnavailable,
            ),
        ] {
            let authorizer = LinuxPolkitAuthorizer {
                runner: Arc::new(FakeRunner(outcome)),
            };
            assert_eq!(
                authorizer.authorize(&registry, &registration).await,
                Err(expected)
            );
        }
    }

    #[tokio::test]
    async fn successful_authorization_requires_a_current_registration() {
        let (registry, registration) = current_registration();
        let authorizer = LinuxPolkitAuthorizer {
            runner: Arc::new(FakeRunner(PolkitCommandOutcome::Exit(0))),
        };
        authorizer
            .authorize(&registry, &registration)
            .await
            .unwrap();
        registry.unregister_websocket(7).unwrap();
        assert_eq!(
            authorizer.authorize(&registry, &registration).await,
            Err(AuthorizationError::StaleRegistration)
        );
    }

    #[tokio::test]
    async fn supervisor_authorization_mints_an_exact_registration_bound_permit() {
        let (registry, registration) = current_registration();
        let ledger = Arc::new(ExecLedger::open_in_memory().await.unwrap());
        let supervisor = LinuxPrivilegedExecSupervisor {
            permits: PrivilegedPermitStore::default(),
            authorizer: LinuxPolkitAuthorizer {
                runner: Arc::new(FakeRunner(PolkitCommandOutcome::Exit(0))),
            },
            ledger,
            runner: Arc::new(RealTransientCommandRunner),
            active: Arc::new(Mutex::new(HashMap::new())),
        };
        let plan = administrator_plan("generation-authorized");

        let permit_id = supervisor
            .authorize_and_mint(&plan, &registry, &registration)
            .await
            .unwrap();

        supervisor
            .permits
            .consume(permit_id, &plan, &registry, &registration)
            .unwrap();
    }

    #[test]
    fn permit_is_single_use_and_bound_to_plan_and_registration() {
        let (registry, registration) = current_registration();
        let store = PrivilegedPermitStore::default();
        let plan = administrator_plan("generation-1");
        let permit = store.mint(&plan, &registration);
        store
            .consume(permit.permit_id, &plan, &registry, &registration)
            .unwrap();
        assert_eq!(
            store.consume(permit.permit_id, &plan, &registry, &registration),
            Err(PermitError::Missing)
        );

        let permit = store.mint(&plan, &registration);
        let different = administrator_plan("generation-2");
        assert_eq!(
            store.consume(permit.permit_id, &different, &registry, &registration),
            Err(PermitError::BindingMismatch)
        );
        assert_eq!(
            store.consume(permit.permit_id, &plan, &registry, &registration),
            Err(PermitError::Missing)
        );
    }

    #[test]
    fn disconnect_revokes_unconsumed_permits() {
        let (registry, registration) = current_registration();
        let store = PrivilegedPermitStore::default();
        let plan = administrator_plan("generation-1");
        let permit = store.mint(&plan, &registration);
        store.revoke_registration(
            registration.registration_id,
            registration.registration_generation,
        );
        assert_eq!(
            store.consume(permit.permit_id, &plan, &registry, &registration),
            Err(PermitError::Missing)
        );
    }

    #[tokio::test]
    async fn registry_disconnect_event_revokes_unconsumed_permits() {
        let (registry, registration) = current_registration();
        let store = PrivilegedPermitStore::default();
        let plan = administrator_plan("generation-watched-disconnect");
        let permit = store.mint(&plan, &registration);
        let watcher = spawn_permit_revocation_watcher(store.clone(), registry.clone());

        registry.unregister_websocket(7).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !store
                    .permits
                    .lock()
                    .unwrap()
                    .contains_key(&permit.permit_id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect event should revoke the permit promptly");
        watcher.abort();

        assert_eq!(
            store.consume(permit.permit_id, &plan, &registry, &registration),
            Err(PermitError::Missing)
        );
    }

    #[test]
    fn expired_permit_is_burned_and_never_reusable() {
        let (registry, registration) = current_registration();
        let store = PrivilegedPermitStore::default();
        let plan = administrator_plan("generation-expired");
        let permit = store.mint(&plan, &registration);
        store
            .permits
            .lock()
            .unwrap()
            .get_mut(&permit.permit_id)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            store.consume(permit.permit_id, &plan, &registry, &registration),
            Err(PermitError::Expired)
        );
        assert_eq!(
            store.consume(permit.permit_id, &plan, &registry, &registration),
            Err(PermitError::Missing)
        );
    }

    #[test]
    fn expired_permit_is_removed_without_a_consume_attempt() {
        let (_registry, registration) = current_registration();
        let store = PrivilegedPermitStore::default();
        let plan = administrator_plan("generation-expired-cleanup");
        let permit = store.mint(&plan, &registration);
        store
            .permits
            .lock()
            .unwrap()
            .get_mut(&permit.permit_id)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_millis(1);

        store.revoke_expired();

        assert!(
            !store
                .permits
                .lock()
                .unwrap()
                .contains_key(&permit.permit_id)
        );
    }

    #[test]
    fn privileged_plan_is_rebuilt_exactly_and_rejects_every_drift() {
        let plan = administrator_plan("generation-validate");
        validate_privileged_plan(&plan).unwrap();

        let mut wrong_principal = plan.clone();
        wrong_principal.principal = ExecutionPrincipal::SessionUser;
        assert_eq!(
            validate_privileged_plan(&wrong_principal),
            Err(PrivilegedPlanError::WrongPrincipal)
        );

        let mut unknown = plan.clone();
        unknown.template_id = "operator.root.anything".into();
        assert_eq!(
            validate_privileged_plan(&unknown),
            Err(PrivilegedPlanError::UnknownTemplate)
        );

        for mutate in [
            |plan: &mut ExecPlan| plan.argv[1] = "sshd.service".into(),
            |plan: &mut ExecPlan| plan.timeout_ms += 1,
            |plan: &mut ExecPlan| plan.fingerprint = "forged".into(),
        ] {
            let mut drifted = plan.clone();
            mutate(&mut drifted);
            assert_eq!(
                validate_privileged_plan(&drifted),
                Err(PrivilegedPlanError::PlanDrift)
            );
        }
    }

    #[test]
    fn privileged_agentic_input_must_name_the_same_exact_service_action() {
        let plan = administrator_plan("generation-input-binding");
        let input = desk_agent_protocol::ExecInput {
            target: desk_agent_protocol::ExecTarget::Shell {
                shell: "bash".to_string(),
            },
            command: "systemctl restart lcxl-remote-desk.service".to_string(),
            cwd: None,
            timeout_ms: 0,
            max_stdout_bytes: 0,
            max_stderr_bytes: 0,
        };
        validate_privileged_agentic_request(&plan, &input).unwrap();

        let mut wrong_unit = input.clone();
        wrong_unit.command = "systemctl restart ssh.service".to_string();
        assert_eq!(
            validate_privileged_agentic_request(&plan, &wrong_unit),
            Err(PrivilegedPlanError::InputDrift)
        );
        let mut with_cwd = input;
        with_cwd.cwd = Some("/tmp".to_string());
        assert_eq!(
            validate_privileged_agentic_request(&plan, &with_cwd),
            Err(PrivilegedPlanError::InputDrift)
        );
    }

    #[test]
    fn central_and_root_privileged_template_renders_match_for_every_action() {
        for action in [
            PrivilegedServiceAction::Start,
            PrivilegedServiceAction::Stop,
            PrivilegedServiceAction::Restart,
        ] {
            let input = desk_agent_protocol::ExecInput {
                target: desk_agent_protocol::ExecTarget::Shell {
                    shell: "bash".to_string(),
                },
                command: format!("systemctl {} {MANAGED_SERVICE_UNIT}", action.verb()),
                cwd: None,
                timeout_ms: 0,
                max_stdout_bytes: 0,
                max_stderr_bytes: 0,
            };
            let central = desk_diagnose_core::exec_classify::classify_command(&input)
                .draft
                .expect("central privileged draft");
            assert_eq!(central, privileged_service_draft(action));
        }
    }

    #[test]
    fn transient_spec_has_fixed_hardening_and_exact_exec_after_separator() {
        let plan = administrator_plan("generation/with user controlled text");
        let spec = SystemdTransientSpec::from_plan(&plan).unwrap();
        assert_eq!(spec.program, SYSTEMD_RUN_PATH);
        assert!(spec.unit_name.starts_with("lcxl-ai-exec-"));
        assert!(spec.unit_name.ends_with(".service"));
        assert!(!spec.unit_name.contains("generation"));
        assert_eq!(spec.argv[0], format!("--unit={}", spec.unit_name));
        for required in [
            "--collect",
            "--wait",
            "--property=Type=exec",
            "--property=User=root",
            "--property=KillMode=control-group",
            "--property=TasksMax=16",
            "--property=MemoryMax=134217728",
            "--property=CPUQuota=50%",
            "--property=LimitFSIZE=73728",
            "--property=NoNewPrivileges=yes",
            "--property=ProtectSystem=strict",
            "--property=RestrictAddressFamilies=AF_UNIX",
        ] {
            assert!(spec.argv.iter().any(|argument| argument == required));
        }
        assert!(!spec.argv.iter().any(|argument| argument == "--pipe"));
        assert!(spec.stdout_path.starts_with(PRIVILEGED_OUTPUT_DIR));
        assert!(spec.stderr_path.starts_with(PRIVILEGED_OUTPUT_DIR));
        assert!(!spec.stdout_path.contains("generation"));
        assert!(spec.argv.iter().any(|argument| {
            argument == &format!("--property=StandardOutput=file:{}", spec.stdout_path)
        }));
        assert!(spec.argv.iter().any(|argument| {
            argument == &format!("--property=StandardError=file:{}", spec.stderr_path)
        }));
        let separator = spec
            .argv
            .iter()
            .position(|argument| argument == "--")
            .unwrap();
        assert_eq!(
            &spec.argv[separator + 1..],
            [SYSTEMCTL_PATH, "restart", MANAGED_SERVICE_UNIT]
        );
        assert_eq!(
            spec.argv
                .iter()
                .filter(|argument| *argument == "--")
                .count(),
            1
        );
    }

    #[test]
    fn systemd_show_parser_distinguishes_missing_running_exit_and_signal() {
        assert_eq!(
            parse_systemd_unit_inspection(false, "LoadState=not-found\nActiveState=inactive\n")
                .unwrap(),
            TransientUnitInspection::Missing
        );
        assert_eq!(
            parse_systemd_unit_inspection(
                true,
                "LoadState=loaded\nActiveState=active\nSubState=running\nResult=success\n",
            )
            .unwrap(),
            TransientUnitInspection::Running
        );
        assert_eq!(
            parse_systemd_unit_inspection(
                true,
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=23\n",
            )
            .unwrap(),
            TransientUnitInspection::Exited { exit_code: 23 }
        );
        assert!(matches!(
            parse_systemd_unit_inspection(
                true,
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=signal\nExecMainCode=2\nExecMainStatus=9\n",
            )
            .unwrap(),
            TransientUnitInspection::Unknown(_)
        ));
        assert!(parse_systemd_unit_inspection(false, "").is_err());
    }

    #[tokio::test]
    async fn supervisor_burns_permit_then_durably_reserves_unit_before_return() {
        let (registry, registration) = current_registration();
        let permits = PrivilegedPermitStore::default();
        let ledger = Arc::new(ExecLedger::open_in_memory().await.unwrap());
        let supervisor = LinuxPrivilegedExecSupervisor::new(permits.clone(), ledger.clone());
        let plan = administrator_plan("generation-reserved");
        let permit = permits.mint(&plan, &registration);

        let spec = supervisor
            .prepare_dispatch(permit.permit_id, &plan, &registry, &registration)
            .await
            .unwrap();
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.containment_identity.as_deref(),
            Some(spec.unit_name.as_str())
        );
        assert_eq!(
            supervisor
                .prepare_dispatch(permit.permit_id, &plan, &registry, &registration,)
                .await
                .unwrap_err()
                .to_string(),
            "privilege permit rejected: Missing"
        );

        let replay_permit = permits.mint(&plan, &registration);
        assert!(matches!(
            supervisor
                .prepare_dispatch(replay_permit.permit_id, &plan, &registry, &registration,)
                .await,
            Err(PrivilegedPrepareError::Duplicate)
        ));
    }

    async fn prepared_supervisor(
        generation: &str,
        outcome: TransientRunOutcome,
    ) -> (
        LinuxPrivilegedExecSupervisor,
        Arc<ExecLedger>,
        ExecPlan,
        SystemdTransientSpec,
    ) {
        let (registry, registration) = current_registration();
        let permits = PrivilegedPermitStore::default();
        let ledger = Arc::new(ExecLedger::open_in_memory().await.unwrap());
        let runner = Arc::new(FakeTransientRunner {
            outcome,
            inspection: TransientUnitInspection::Missing,
            runs: AtomicUsize::new(0),
            terminations: AtomicUsize::new(0),
        });
        let supervisor =
            LinuxPrivilegedExecSupervisor::with_runner(permits.clone(), ledger.clone(), runner);
        let plan = administrator_plan(generation);
        let permit = permits.mint(&plan, &registration);
        let spec = supervisor
            .prepare_dispatch(permit.permit_id, &plan, &registry, &registration)
            .await
            .unwrap();
        (supervisor, ledger, plan, spec)
    }

    async fn supervisor_for_reconcile(
        generation: &str,
        inspection: TransientUnitInspection,
    ) -> (LinuxPrivilegedExecSupervisor, Arc<ExecLedger>, ExecPlan) {
        let (registry, registration) = current_registration();
        let permits = PrivilegedPermitStore::default();
        let ledger = Arc::new(ExecLedger::open_in_memory().await.unwrap());
        let runner = Arc::new(FakeTransientRunner {
            outcome: TransientRunOutcome::SpawnFailed("unused".into()),
            inspection,
            runs: AtomicUsize::new(0),
            terminations: AtomicUsize::new(0),
        });
        let supervisor =
            LinuxPrivilegedExecSupervisor::with_runner(permits.clone(), ledger.clone(), runner);
        let plan = administrator_plan(generation);
        let permit = permits.mint(&plan, &registration);
        let _spec = supervisor
            .prepare_dispatch(permit.permit_id, &plan, &registry, &registration)
            .await
            .unwrap();
        (supervisor, ledger, plan)
    }

    async fn recovered_supervisor_for_cancel(
        generation: &str,
        inspection: TransientUnitInspection,
    ) -> (
        LinuxPrivilegedExecSupervisor,
        Arc<ExecLedger>,
        ExecPlan,
        Arc<FakeTransientRunner>,
    ) {
        let (registry, registration) = current_registration();
        let permits = PrivilegedPermitStore::default();
        let ledger = Arc::new(ExecLedger::open_in_memory().await.unwrap());
        let runner = Arc::new(FakeTransientRunner {
            outcome: TransientRunOutcome::SpawnFailed("unused".into()),
            inspection,
            runs: AtomicUsize::new(0),
            terminations: AtomicUsize::new(0),
        });
        let preparer = LinuxPrivilegedExecSupervisor::with_runner(
            permits.clone(),
            ledger.clone(),
            runner.clone(),
        );
        let plan = administrator_plan(generation);
        let permit = permits.mint(&plan, &registration);
        preparer
            .prepare_dispatch(permit.permit_id, &plan, &registry, &registration)
            .await
            .unwrap();
        drop(preparer);
        let recovered = LinuxPrivilegedExecSupervisor::with_runner(
            PrivilegedPermitStore::default(),
            ledger.clone(),
            runner.clone(),
        );
        (recovered, ledger, plan, runner)
    }

    #[tokio::test]
    async fn observed_transient_exit_is_redacted_and_settled_terminal() {
        let (supervisor, ledger, plan, spec) = prepared_supervisor(
            "generation-complete",
            TransientRunOutcome::Exited {
                exit_code: 7,
                unit_observed: true,
                duration_ms: 321,
            },
        )
        .await;
        let outcome = supervisor.execute_prepared(&plan, spec).await;
        match &outcome {
            AgentOutcome::Ok(OperationOutput::Exec(output)) => {
                assert_eq!(output.exit_code, 7);
                assert_eq!(output.duration_ms, 321);
            }
            other => panic!("expected exec output, got {other:?}"),
        }
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::daemon::exec_ledger::State::Terminal.as_str()
        );
        assert_eq!(
            serde_json::from_str::<AgentOutcome>(row.result_json.as_deref().unwrap()).unwrap(),
            outcome
        );
    }

    #[tokio::test]
    async fn unobserved_nonzero_systemd_exit_is_indeterminate_not_spawn_failed() {
        let (supervisor, ledger, plan, spec) = prepared_supervisor(
            "generation-unknown",
            TransientRunOutcome::Exited {
                exit_code: 1,
                unit_observed: false,
                duration_ms: 10,
            },
        )
        .await;
        assert!(matches!(
            supervisor.execute_prepared(&plan, spec).await,
            AgentOutcome::Err(_)
        ));
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::daemon::exec_ledger::State::Indeterminate.as_str()
        );
    }

    #[tokio::test]
    async fn systemd_client_spawn_failure_is_definitely_not_started() {
        let (supervisor, ledger, plan, spec) = prepared_supervisor(
            "generation-not-started",
            TransientRunOutcome::SpawnFailed("missing systemd-run".into()),
        )
        .await;
        assert!(matches!(
            supervisor.execute_prepared(&plan, spec).await,
            AgentOutcome::Err(_)
        ));
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::daemon::exec_ledger::State::SpawnFailed.as_str()
        );
    }

    #[tokio::test]
    async fn observed_timeout_is_terminal_but_reports_timeout() {
        let (supervisor, ledger, plan, spec) = prepared_supervisor(
            "generation-timeout",
            TransientRunOutcome::TimedOut {
                unit_observed: true,
            },
        )
        .await;
        let outcome = supervisor.execute_prepared(&plan, spec).await;
        assert!(matches!(
            outcome,
            AgentOutcome::Err(ref error) if error.kind == AgentErrorKind::Timeout
        ));
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::daemon::exec_ledger::State::Terminal.as_str()
        );
    }

    #[tokio::test]
    async fn prelaunch_cancel_is_terminal_and_execute_cannot_spawn_afterward() {
        let (supervisor, ledger, plan, spec) = prepared_supervisor(
            "generation-cancel-before-start",
            TransientRunOutcome::Exited {
                exit_code: 0,
                unit_observed: true,
                duration_ms: 1,
            },
        )
        .await;
        assert_eq!(
            supervisor
                .cancel_generation(&plan.execution_generation)
                .await
                .unwrap(),
            PrivilegedCancelOutcome::CancelledBeforeStart
        );
        let outcome = supervisor.execute_prepared(&plan, spec).await;
        assert!(matches!(
            outcome,
            AgentOutcome::Err(ref error) if error.kind == AgentErrorKind::Cancelled
        ));
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::daemon::exec_ledger::State::Terminal.as_str()
        );
    }

    #[tokio::test]
    async fn active_cancel_sets_launch_token_and_reclaims_the_unit() {
        let (supervisor, _ledger, plan, _spec) = prepared_supervisor(
            "generation-cancel-running",
            TransientRunOutcome::Exited {
                exit_code: 0,
                unit_observed: true,
                duration_ms: 1,
            },
        )
        .await;
        {
            let mut active = supervisor.active.lock().unwrap();
            active
                .get_mut(&plan.execution_generation)
                .unwrap()
                .launch_started = true;
        }
        assert_eq!(
            supervisor
                .cancel_generation(&plan.execution_generation)
                .await
                .unwrap(),
            PrivilegedCancelOutcome::CancelRequested
        );
        let active = supervisor.active.lock().unwrap();
        assert!(
            active[&plan.execution_generation]
                .cancelled
                .load(Ordering::Acquire)
        );
    }

    #[tokio::test]
    async fn recovered_exited_unit_keeps_its_real_result_when_cancel_is_late() {
        let (supervisor, ledger, plan, runner) = recovered_supervisor_for_cancel(
            "generation-cancel-late",
            TransientUnitInspection::Exited { exit_code: 11 },
        )
        .await;
        assert_eq!(
            supervisor
                .cancel_generation(&plan.execution_generation)
                .await
                .unwrap(),
            PrivilegedCancelOutcome::RecoveredTerminal
        );
        assert_eq!(runner.terminations.load(Ordering::Acquire), 1);
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        let outcome =
            serde_json::from_str::<AgentOutcome>(row.result_json.as_deref().unwrap()).unwrap();
        assert!(matches!(
            outcome,
            AgentOutcome::Ok(OperationOutput::Exec(ExecOutput { exit_code: 11, .. }))
        ));
    }

    #[tokio::test]
    async fn recovered_missing_unit_cancel_is_indeterminate_not_retryable() {
        let (supervisor, ledger, plan, _runner) = recovered_supervisor_for_cancel(
            "generation-cancel-missing",
            TransientUnitInspection::Missing,
        )
        .await;
        assert_eq!(
            supervisor
                .cancel_generation(&plan.execution_generation)
                .await
                .unwrap(),
            PrivilegedCancelOutcome::Indeterminate
        );
        assert_eq!(
            ledger
                .get(&plan.execution_generation)
                .await
                .unwrap()
                .unwrap()
                .state,
            crate::daemon::exec_ledger::State::Indeterminate.as_str()
        );
    }

    #[tokio::test]
    async fn missing_recovered_unit_is_indeterminate_and_never_respawned() {
        let (supervisor, ledger, plan) = supervisor_for_reconcile(
            "generation-reconcile-missing",
            TransientUnitInspection::Missing,
        )
        .await;
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert!(is_recoverable_privileged_ledger_row(&row));
        let summary = supervisor.reconcile_in_flight().await.unwrap();
        assert_eq!(summary.marked_indeterminate, 1);
        assert_eq!(summary.recovered_running, 0);
        assert_eq!(
            ledger
                .get(&plan.execution_generation)
                .await
                .unwrap()
                .unwrap()
                .state,
            crate::daemon::exec_ledger::State::Indeterminate.as_str()
        );
    }

    #[tokio::test]
    async fn terminal_recovered_unit_replays_bounded_result() {
        let (supervisor, ledger, plan) = supervisor_for_reconcile(
            "generation-reconcile-terminal",
            TransientUnitInspection::Exited { exit_code: 9 },
        )
        .await;
        let summary = supervisor.reconcile_in_flight().await.unwrap();
        assert_eq!(summary.recovered_terminal, 1);
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::daemon::exec_ledger::State::Terminal.as_str()
        );
        let outcome =
            serde_json::from_str::<AgentOutcome>(row.result_json.as_deref().unwrap()).unwrap();
        assert!(matches!(
            outcome,
            AgentOutcome::Ok(OperationOutput::Exec(ExecOutput { exit_code: 9, .. }))
        ));
    }

    #[tokio::test]
    async fn malformed_privileged_row_is_not_claimed_by_recovery() {
        let ledger = ExecLedger::open_in_memory().await.unwrap();
        let plan = administrator_plan("generation-reconcile-malformed");
        let plan_json = serde_json::to_string(&plan).unwrap();
        ledger
            .reserve_with_sealed_plan(
                &plan.exec_request_id.0,
                &plan.execution_generation,
                &plan.fingerprint,
                "lcxl-ai-exec-EVIL.service",
                &plan_json,
            )
            .await
            .unwrap();
        let row = ledger
            .get(&plan.execution_generation)
            .await
            .unwrap()
            .unwrap();
        assert!(!is_recoverable_privileged_ledger_row(&row));
    }

    #[test]
    fn privileged_output_is_redacted_before_utf8_safe_truncation() {
        let mut plan = administrator_plan("generation-redaction");
        plan.max_stdout_bytes = 24;
        let outcome = sanitize_privileged_output(
            &plan,
            0,
            5,
            (
                b"prefix AKIA1234567890ABCDEF trailing secret text".to_vec(),
                true,
            ),
            (Vec::new(), false),
        );
        match outcome {
            AgentOutcome::Ok(OperationOutput::Exec(output)) => {
                assert!(!output.stdout.contains("AKIA1234567890ABCDEF"));
                assert!(output.stdout_truncated);
                assert!(
                    output
                        .redactions
                        .iter()
                        .any(|kind| kind == "aws_access_key")
                );
            }
            other => panic!("expected redacted output, got {other:?}"),
        }
    }

    #[test]
    fn privileged_output_reader_rejects_symlinks_and_permissive_files() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"safe").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let uid = fs::metadata(&target).unwrap().uid();
        assert_eq!(
            read_privileged_output(target.to_str().unwrap(), uid)
                .unwrap()
                .0,
            b"safe"
        );

        let link = directory.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(read_privileged_output(link.to_str().unwrap(), uid).is_err());

        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_privileged_output(target.to_str().unwrap(), uid).is_err());
    }

    #[test]
    fn installed_policy_never_keeps_or_grants_inactive_authority() {
        assert!(POLKIT_POLICY_XML.contains("<allow_any>no</allow_any>"));
        assert!(POLKIT_POLICY_XML.contains("<allow_inactive>no</allow_inactive>"));
        assert!(POLKIT_POLICY_XML.contains("<allow_active>auth_admin</allow_active>"));
        assert!(!POLKIT_POLICY_XML.contains("auth_admin_keep"));
    }
}
