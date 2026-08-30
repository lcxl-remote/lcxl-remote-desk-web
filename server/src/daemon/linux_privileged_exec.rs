//! Linux administrator authorization and exact one-shot permit foundation.
//!
//! Passwords are handled exclusively by the desktop session's registered
//! polkit authentication agent. The daemon identifies that session by the
//! already `/proc`-verified Tauri process tuple and invokes `pkcheck` with the
//! race-safe `pid,start_time,uid` subject form. It never enables pkcheck's
//! textual/internal agent and never receives authentication input itself.

use async_trait::async_trait;
use desk_agent_protocol::exec::ExecPlan;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command;
use uuid::Uuid;

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

fn plan_digest(plan: &ExecPlan) -> [u8; 32] {
    // ExecPlan has a deterministic field order and no maps. This digest is a
    // local authority key, not a cross-version wire protocol.
    let bytes = serde_json::to_vec(plan).expect("ExecPlan serialization cannot fail");
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::{
        ApprovalId, ExecContainmentSnapshot, ExecExecutionBasis, ExecPlanDraft, ExecRequestId,
        ExecShellKind, ExecutionPrincipal, RequiredEnforcement,
    };
    use std::sync::atomic::{AtomicU8, Ordering};

    struct FakeRunner(AtomicU8);

    #[async_trait]
    impl PolkitCommandRunner for FakeRunner {
        async fn check(&self, _subject: PolkitSubject) -> PolkitCommandOutcome {
            PolkitCommandOutcome::Exit(self.0.load(Ordering::Acquire) as i32)
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
            ExecPlanDraft {
                program: "/usr/bin/systemctl".into(),
                argv: vec!["restart".into(), "lcxl-remote-desk.service".into()],
                cwd: None,
                shell: ExecShellKind::Native,
                risk: RiskLevel::Critical,
                execution_basis: ExecExecutionBasis::Template,
                principal: ExecutionPrincipal::Administrator,
                template_id: "linux.systemd.restart.lcxl-remote-desk".into(),
                fingerprint: "principal-bound".into(),
                timeout_ms: 30_000,
                max_stdout_bytes: 65_536,
                max_stderr_bytes: 65_536,
                containment: ExecContainmentSnapshot {
                    required_enforcement: RequiredEnforcement::NativeHard,
                    max_processes: Some(16),
                    max_memory_bytes: Some(128 << 20),
                    ..Default::default()
                },
            },
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
                runner: Arc::new(FakeRunner(AtomicU8::new(code))),
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
            runner: Arc::new(FakeRunner(AtomicU8::new(0))),
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
    fn installed_policy_never_keeps_or_grants_inactive_authority() {
        assert!(POLKIT_POLICY_XML.contains("<allow_any>no</allow_any>"));
        assert!(POLKIT_POLICY_XML.contains("<allow_inactive>no</allow_inactive>"));
        assert!(POLKIT_POLICY_XML.contains("<allow_active>auth_admin</allow_active>"));
        assert!(!POLKIT_POLICY_XML.contains("auth_admin_keep"));
    }
}
