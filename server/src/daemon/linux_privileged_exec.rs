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
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command;
use uuid::Uuid;

use crate::daemon::exec_ledger::{ExecLedger, Reservation};
use crate::host_control::session_shell::{
    RegisteredSessionShell, SessionShellRegistry, read_process_identity,
};

pub const POLKIT_ACTION_ID: &str = "com.lcxl.remote-desk.ai.execute-administrator-command";
pub const POLKIT_POLICY_PATH: &str = "/usr/share/polkit-1/actions/com.lcxl.remote-desk.ai.policy";
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
    PlanDrift,
}

impl std::fmt::Display for PrivilegedPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::WrongPrincipal => "the plan is not an Administrator plan",
            Self::UnknownTemplate => "the privileged template is not allowlisted",
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

/// A fully generated transient-service invocation. `program` and every option
/// are daemon constants; the only varying option is a SHA-256-derived unit name.
/// The sealed executable and argv appear after a literal `--` and are never
/// parsed as systemd-run options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdTransientSpec {
    pub unit_name: String,
    pub stdout_path: String,
    pub stderr_path: String,
    pub program: &'static str,
    pub argv: Vec<String>,
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

#[derive(Debug)]
pub enum PrivilegedPrepareError {
    Permit(PermitError),
    Plan(PrivilegedPlanError),
    Ledger(String),
    Duplicate,
    GenerationFingerprintMismatch,
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
    ledger: Arc<ExecLedger>,
}

impl LinuxPrivilegedExecSupervisor {
    pub fn new(permits: PrivilegedPermitStore, ledger: Arc<ExecLedger>) -> Self {
        Self { permits, ledger }
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
        match self
            .ledger
            .reserve(
                &plan.exec_request_id.0,
                &plan.execution_generation,
                &plan.fingerprint,
                Some(&spec.unit_name),
            )
            .await
            .map_err(|error| PrivilegedPrepareError::Ledger(error.to_string()))?
        {
            Reservation::Granted => Ok(spec),
            Reservation::Duplicate(_) => Err(PrivilegedPrepareError::Duplicate),
            Reservation::FingerprintMismatch => {
                Err(PrivilegedPrepareError::GenerationFingerprintMismatch)
            }
        }
    }
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

    struct FakeRunner(PolkitCommandOutcome);

    #[async_trait]
    impl PolkitCommandRunner for FakeRunner {
        async fn check(&self, _subject: PolkitSubject) -> PolkitCommandOutcome {
            self.0
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

    #[test]
    fn installed_policy_never_keeps_or_grants_inactive_authority() {
        assert!(POLKIT_POLICY_XML.contains("<allow_any>no</allow_any>"));
        assert!(POLKIT_POLICY_XML.contains("<allow_inactive>no</allow_inactive>"));
        assert!(POLKIT_POLICY_XML.contains("<allow_active>auth_admin</allow_active>"));
        assert!(!POLKIT_POLICY_XML.contains("auth_admin_keep"));
    }
}
