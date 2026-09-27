//! Server-authored command review and immutable execution binding.

use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::command_blocklist::BlocklistRule;
use desk_agent_protocol::command_template::SyncedCommandTemplate;
use desk_agent_protocol::exec::{CommandDraft, CommandIoMode, ExecDecision, ExecPlanDraft};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, ExecInput, ExecTarget, ExecutionMode, RiskLevel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const COMMAND_TOOL: &str = "exec_command";

/// Trusted, per-request policy. Never deserialize this from model arguments.
#[derive(Debug, Clone)]
pub struct CommandPolicyContext {
    pub actor_id: String,
    pub target_device_id: String,
    /// The selected host connection/session identity, not the controller connection.
    pub target_session_id: String,
    pub policy_revision: i64,
    pub admission_policy: ExecAdmissionPolicy,
    pub execution_mode: ExecutionMode,
    pub max_risk: RiskLevel,
    pub available_shells: Vec<String>,
    pub max_runtime_ms: u32,
    pub operator_templates: Vec<SyncedCommandTemplate>,
    pub effective_blocklist: Vec<BlocklistRule>,
    /// Stable policy axes; heartbeat timestamps are deliberately excluded.
    pub policy_version: String,
    /// The target host reports that interactive (PTY) execution is enabled.
    pub exec_pty: bool,
    /// The target host reports that interactive elevation (`sudo`/`doas`
    /// inside a PTY) is enabled.
    pub exec_pty_elevation: bool,
}

#[cfg(test)]
pub(crate) fn test_policy() -> CommandPolicyContext {
    CommandPolicyContext {
        actor_id: "1".into(),
        target_device_id: "device-1".into(),
        target_session_id: "host:session-1".into(),
        policy_revision: 1,
        admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        execution_mode: ExecutionMode::ConfirmEachAction,
        max_risk: RiskLevel::Critical,
        available_shells: vec!["bash".into(), "powershell".into()],
        max_runtime_ms: 10_000,
        operator_templates: vec![],
        effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
        policy_version: "test:1".into(),
        exec_pty: true,
        exec_pty_elevation: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::exec::ExecIoMode;
    fn input(command: &str) -> String {
        input_with(command, CommandIoMode::NonInteractive)
    }

    fn input_with(command: &str, io_mode: CommandIoMode) -> String {
        serde_json::to_string(&CommandDraft {
            schema_version: 1,
            shell: "bash".into(),
            command: command.into(),
            cwd: None,
            timeout_ms: 30_000,
            io_mode,
        })
        .unwrap()
    }

    #[test]
    fn interactive_commands_seal_a_pty_plan_only_when_the_host_allows_it() {
        let canonical = input_with("top", CommandIoMode::Pty);
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert!(confirmation.plan.io_mode.is_pty());
        assert!(confirmation.is_interactive());
        assert!(!confirmation.is_elevated());
        test_policy()
            .revalidate(&confirmation, &canonical, 1)
            .unwrap();
        // The PTY mode is part of the plan fingerprint and of the proposal.
        let mut downgraded = confirmation.clone();
        downgraded.plan.io_mode = ExecIoMode::NonInteractive;
        assert!(downgraded.validate(&canonical).is_err());
        let mut policy = test_policy();
        policy.exec_pty = false;
        assert!(policy.prepare(&canonical, 1).is_err());
        assert!(policy.revalidate(&confirmation, &canonical, 1).is_err());
        // Non-interactive commands never need the PTY capability.
        assert!(policy.prepare(&input("df -h"), 1).is_ok());
    }

    #[test]
    fn interactive_elevation_requires_the_elevation_capability() {
        let canonical = input_with("sudo docker ps", CommandIoMode::Pty);
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert!(confirmation.is_elevated());
        let mut policy = test_policy();
        policy.exec_pty_elevation = false;
        assert!(policy.prepare(&canonical, 1).is_err());
        // Elevation outside a PTY stays blocked.
        assert!(test_policy().prepare(&input("sudo df -h"), 1).is_err());
    }

    #[test]
    fn interactive_execution_ids_are_stable_per_permission_item() {
        let first = interactive_exec_request_id("run", "request", "digest");
        assert_eq!(
            first,
            interactive_exec_request_id("run", "request", "digest")
        );
        assert!(first.starts_with("exec_pty_"));
        assert!(first.len() <= desk_agent_protocol::exec_pty::MAX_PTY_STREAM_ID_BYTES);
        for other in [
            interactive_exec_request_id("run2", "request", "digest"),
            interactive_exec_request_id("run", "request2", "digest"),
            interactive_exec_request_id("run", "request", "digest2"),
        ] {
            assert_ne!(first, other);
        }
    }

    #[test]
    fn freeform_preserves_script_and_freezes_effective_limits_before_approval() {
        let script = "du -d 1 '/tmp/a b' | sort -nr\nprintf done";
        let canonical = input(script);
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert_eq!(confirmation.plan.argv, vec!["-lc", script]);
        assert_eq!(confirmation.plan.risk, RiskLevel::Critical);
        assert_eq!(confirmation.plan.timeout_ms, 10_000);
        assert_eq!(confirmation.plan.io_mode, ExecIoMode::NonInteractive);
        test_policy()
            .revalidate(&confirmation, &canonical, 1)
            .unwrap();
    }

    #[test]
    fn missing_authority_blocked_commands_and_insufficient_risk_never_produce_plans() {
        let mut policy = test_policy();
        policy.admission_policy = ExecAdmissionPolicy::TemplateOnly;
        assert!(policy.prepare(&input("df -h"), 1).is_err());
        policy = test_policy();
        policy.max_risk = RiskLevel::High;
        assert!(policy.prepare(&input("df -h"), 1).is_err());
        for mode in [ExecutionMode::ReadOnly, ExecutionMode::SuggestOnly] {
            policy = test_policy();
            policy.execution_mode = mode;
            assert!(policy.prepare(&input("df -h"), 1).is_err());
        }
        assert!(test_policy().prepare(&input("sudo df -h"), 1).is_err());
        assert!(test_policy().prepare(&input("df\0-h"), 1).is_err());
    }

    #[test]
    fn input_target_policy_limit_and_plan_changes_invalidate_original_confirmation() {
        let canonical = input("df -h");
        let confirmation = test_policy().prepare(&canonical, 1).unwrap();
        assert!(
            test_policy()
                .revalidate(&confirmation, &input("df -H"), 1)
                .is_err()
        );
        assert!(
            test_policy()
                .revalidate(&confirmation, &canonical, 2)
                .is_err()
        );
        for mutate in [
            |p: &mut CommandPolicyContext| p.target_session_id.push('2'),
            |p: &mut CommandPolicyContext| p.policy_version.push('2'),
            |p: &mut CommandPolicyContext| p.max_runtime_ms = 5_000,
            |p: &mut CommandPolicyContext| p.available_shells.clear(),
        ] {
            let mut policy = test_policy();
            mutate(&mut policy);
            assert!(policy.revalidate(&confirmation, &canonical, 1).is_err());
        }
        let mut changed = confirmation.clone();
        changed.plan.argv.push("extra".into());
        assert_ne!(
            changed.resource_scope().unwrap(),
            confirmation.resource_scope().unwrap()
        );
        assert!(test_policy().revalidate(&changed, &canonical, 1).is_err());
    }
}

/// Persisted on the permission item before it is shown to the owner. The grant
/// scope contains this whole snapshot's digest in addition to the input digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandConfirmation {
    pub schema_version: u16,
    pub actor_id: String,
    pub target_device_id: String,
    pub target_session_id: String,
    pub policy_revision: i64,
    pub input_revision: u64,
    pub policy_version: String,
    pub admission_policy: ExecAdmissionPolicy,
    pub proposal: CommandDraft,
    pub validation_input: ExecInput,
    pub plan: ExecPlanDraft,
    pub canonical_input_digest_sha256: String,
}

fn denied(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

impl CommandPolicyContext {
    pub fn prepare(
        &self,
        canonical: &str,
        input_revision: u64,
    ) -> Result<CommandConfirmation, AgentError> {
        if self.actor_id.is_empty()
            || self.target_device_id.is_empty()
            || self.target_session_id.is_empty()
            || self.policy_version.is_empty()
            || self.policy_revision < 1
            || input_revision == 0
            || !matches!(
                self.execution_mode,
                ExecutionMode::ConfirmEachAction | ExecutionMode::SessionApproved
            )
        {
            return Err(denied(
                "confirmed command execution is unavailable under current policy",
            ));
        }
        let proposal: CommandDraft = serde_json::from_str(canonical).map_err(|e| {
            denied(crate::model_input::describe_error(
                COMMAND_TOOL,
                &format!("Invalid exact command input: {e}"),
            ))
        })?;
        proposal.validate().map_err(|e| {
            denied(crate::model_input::describe_error(
                COMMAND_TOOL,
                &format!("Invalid exact command input: {e}"),
            ))
        })?;
        if proposal.io_mode == CommandIoMode::Pty && !self.exec_pty {
            return Err(denied(
                "interactive (PTY) execution is not enabled on this device",
            ));
        }
        let shell = crate::exec_tools::canonical_exec_shell(&proposal.shell)
            .ok_or_else(|| denied("exact command shell is not supported"))?;
        if !crate::exec_tools::exec_shell_is_available(shell, &self.available_shells) {
            return Err(denied("exact command shell is unavailable on this device"));
        }
        let mut input = ExecInput {
            target: ExecTarget::Shell {
                shell: shell.into(),
            },
            command: proposal.command.clone(),
            cwd: proposal.cwd.clone(),
            io_mode: proposal.io_mode.exec_io_mode(),
            timeout_ms: proposal.timeout_ms,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        };
        crate::exec_tools::apply_exec_runtime_ceiling(&mut input, self.max_runtime_ms);
        let classified = crate::exec_classify::classify_command_with_policy(
            &input,
            &self.operator_templates,
            &self.effective_blocklist,
            self.admission_policy,
        );
        if classified.classification.decision != ExecDecision::ConfirmRequired {
            return Err(denied(format!(
                "command rejected by current policy: {}",
                classified.classification.impact
            )));
        }
        let plan = classified
            .draft
            .ok_or_else(|| denied("command has no executable plan"))?;
        if plan.risk > self.max_risk {
            return Err(denied("command exceeds the current risk policy"));
        }
        if plan.requires_root_pty_containment() && !self.exec_pty_elevation {
            return Err(denied(
                "interactive elevation is not enabled on this device",
            ));
        }
        let confirmation = CommandConfirmation {
            schema_version: 1,
            actor_id: self.actor_id.clone(),
            target_device_id: self.target_device_id.clone(),
            target_session_id: self.target_session_id.clone(),
            policy_revision: self.policy_revision,
            input_revision,
            policy_version: self.policy_version.clone(),
            admission_policy: self.admission_policy,
            proposal,
            validation_input: input,
            plan,
            canonical_input_digest_sha256: format!("{:x}", Sha256::digest(canonical.as_bytes())),
        };
        confirmation.validate(canonical)?;
        Ok(confirmation)
    }

    pub fn revalidate(
        &self,
        confirmation: &CommandConfirmation,
        canonical: &str,
        input_revision: u64,
    ) -> Result<(), AgentError> {
        if self.prepare(canonical, input_revision)? != *confirmation {
            return Err(denied(
                "the command plan, target or policy changed; request permission again",
            ));
        }
        Ok(())
    }
}

/// Stable execution id of an interactive command, derived from the permission
/// item that approves it. The owner's terminal carrier is prepared for this id
/// while the permission is still pending, and the approved execution reuses it,
/// so the host's stream-opened report matches the carrier it was bound to.
/// Prefix of every permission-derived interactive execution id.
pub const INTERACTIVE_EXEC_REQUEST_PREFIX: &str = "exec_pty_";

/// Whether an execution id was derived from an owner-approved PTY permission.
/// Such an execution binds the carrier consumed by that approval, whichever
/// client connection later runs the approved turn.
pub fn is_permission_bound_exec_request_id(exec_request_id: &str) -> bool {
    exec_request_id.starts_with(INTERACTIVE_EXEC_REQUEST_PREFIX)
}

pub fn interactive_exec_request_id(
    run_id: &str,
    permission_request_id: &str,
    canonical_input_digest_sha256: &str,
) -> String {
    let digest = Sha256::digest(
        [
            "pty-permission",
            run_id,
            permission_request_id,
            canonical_input_digest_sha256,
        ]
        .join("\0")
        .as_bytes(),
    );
    format!(
        "{INTERACTIVE_EXEC_REQUEST_PREFIX}{}",
        &format!("{digest:x}")[..32]
    )
}

/// The interactive command a permission decision approves, and the carrier
/// binding it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveApproval {
    pub exec_request_id: String,
    pub target_connection_id: String,
}

/// How a decision may use the client's `carrierId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierUsageError {
    /// Approving an interactive command needs the owner's ready carrier.
    CarrierRequired,
    /// A carrier was supplied for a decision that approves no interactive
    /// command (a denial, or an ordinary request).
    CarrierNotAllowed,
}

/// Validates the carrier of one pending permission decision. Returns the
/// interactive approval that must consume the carrier, or `None` when the
/// decision involves no carrier. A request that is no longer pending (a
/// replay of a committed decision) never consumes one.
pub fn interactive_approval(
    run_id: &str,
    request: &crate::dynamic_run::PermissionRequest,
    decisions: &[crate::dynamic_run::PermissionDecisionItem],
    carrier_id: Option<&str>,
) -> Result<Option<InteractiveApproval>, CarrierUsageError> {
    use crate::dynamic_run::{PermissionItemDecision, PermissionRequestState};
    let carrier_id = carrier_id.filter(|value| !value.is_empty());
    let approved = request
        .items
        .iter()
        .filter_map(|item| {
            let confirmation = item
                .command_confirmation
                .as_ref()
                .filter(|confirmation| confirmation.is_interactive())?;
            decisions
                .iter()
                .any(|decision| {
                    decision.item_id == item.item_id
                        && matches!(decision.decision, PermissionItemDecision::Approve { .. })
                })
                .then(|| InteractiveApproval {
                    exec_request_id: interactive_exec_request_id(
                        run_id,
                        &request.request_id,
                        &confirmation.canonical_input_digest_sha256,
                    ),
                    target_connection_id: confirmation
                        .target_session_id
                        .rsplit_once(':')
                        .map_or(
                            confirmation.target_session_id.as_str(),
                            |(connection, _)| connection,
                        )
                        .to_owned(),
                })
        })
        .next();
    match (approved, carrier_id) {
        (None, Some(_)) => Err(CarrierUsageError::CarrierNotAllowed),
        (None, None) => Ok(None),
        (Some(_), _) if request.state != PermissionRequestState::Pending => Ok(None),
        (Some(_), None) => Err(CarrierUsageError::CarrierRequired),
        (Some(approval), Some(_)) => Ok(Some(approval)),
    }
}

impl CommandConfirmation {
    pub fn is_interactive(&self) -> bool {
        self.plan.io_mode.is_pty()
    }

    pub fn is_elevated(&self) -> bool {
        self.plan.requires_root_pty_containment()
    }

    /// The approved confirmation for this exact call together with the id of
    /// the permission request that approved it.
    pub fn approved_request_for_call<'a>(
        session: &'a crate::session::PersistedAgentSession,
        canonical: &str,
    ) -> Result<(&'a str, &'a Self), AgentError> {
        use crate::dynamic_run::PermissionRequestState;
        let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        session
            .permission_requests
            .iter()
            .rev()
            .filter(|request| {
                request.input_revision == session.input_revision
                    && matches!(
                        request.state,
                        PermissionRequestState::Approved
                            | PermissionRequestState::PartiallyApproved
                    )
            })
            .flat_map(|request| {
                request
                    .items
                    .iter()
                    .map(move |item| (request.request_id.as_str(), item))
            })
            .filter(|(_, item)| {
                item.tool_name == COMMAND_TOOL
                    && item.canonical_input_digest_sha256.as_deref() == Some(digest.as_str())
            })
            .filter_map(|(request_id, item)| {
                item.command_confirmation
                    .as_ref()
                    .map(|confirmation| (request_id, confirmation))
            })
            .find(|(_, confirmation)| {
                confirmation.actor_id == session.actor_id
                    && confirmation.target_device_id == session.device_id
                    && confirmation.input_revision == session.input_revision
                    && confirmation.policy_revision == session.policy_revision
                    && confirmation.validate(canonical).is_ok()
            })
            .ok_or_else(|| denied("command has no approved exact plan; request permission first"))
    }

    /// Execution id for this approved call: the permission-derived id for an
    /// interactive command, `None` otherwise.
    pub fn interactive_exec_request_id_for_call(
        session: &crate::session::PersistedAgentSession,
        canonical: &str,
    ) -> Result<Option<String>, AgentError> {
        let (request_id, confirmation) = Self::approved_request_for_call(session, canonical)?;
        Ok(confirmation.is_interactive().then(|| {
            interactive_exec_request_id(
                &session.conversation_id,
                request_id,
                &confirmation.canonical_input_digest_sha256,
            )
        }))
    }

    pub fn approved_for_call<'a>(
        session: &'a crate::session::PersistedAgentSession,
        canonical: &str,
    ) -> Result<&'a Self, AgentError> {
        use crate::dynamic_run::PermissionRequestState;
        let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
        session
            .permission_requests
            .iter()
            .rev()
            .filter(|request| {
                request.input_revision == session.input_revision
                    && matches!(
                        request.state,
                        PermissionRequestState::Approved
                            | PermissionRequestState::PartiallyApproved
                    )
            })
            .flat_map(|request| &request.items)
            .filter(|item| {
                item.tool_name == COMMAND_TOOL
                    && item.canonical_input_digest_sha256.as_deref() == Some(digest.as_str())
            })
            .filter_map(|item| item.command_confirmation.as_ref())
            .find(|confirmation| {
                confirmation.actor_id == session.actor_id
                    && confirmation.target_device_id == session.device_id
                    && confirmation.input_revision == session.input_revision
                    && confirmation.policy_revision == session.policy_revision
                    && confirmation.validate(canonical).is_ok()
            })
            .ok_or_else(|| denied("command has no approved exact plan; request permission first"))
    }

    pub fn validate(&self, canonical: &str) -> Result<(), AgentError> {
        if self.schema_version != 1
            || self.actor_id.is_empty()
            || self.target_session_id.is_empty()
            || self.policy_version.is_empty()
            || self.plan.io_mode != self.proposal.io_mode.exec_io_mode()
            || serde_json::from_str::<CommandDraft>(canonical)
                .ok()
                .as_ref()
                != Some(&self.proposal)
            || format!("{:x}", Sha256::digest(canonical.as_bytes()))
                != self.canonical_input_digest_sha256
        {
            return Err(denied("invalid command confirmation"));
        }
        self.proposal
            .validate()
            .map_err(|_| denied("invalid command proposal"))?;
        let limits = desk_agent_protocol::exec_policy::ExecLimits {
            timeout_ms: self.plan.timeout_ms,
            max_stdout_bytes: self.plan.max_stdout_bytes,
            max_stderr_bytes: self.plan.max_stderr_bytes,
        };
        if self.plan.fingerprint
            != desk_agent_protocol::exec_policy::fingerprint_with_io_mode(
                &self.plan.program,
                &self.plan.argv,
                self.plan.cwd.as_deref(),
                &limits,
                &self.plan.containment,
                self.plan.io_mode,
            )
        {
            return Err(denied("command plan fingerprint mismatch"));
        }
        desk_agent_protocol::exec::CanonicalCommandIdentity {
            schema_version: 1,
            target_device_id: self.target_device_id.clone(),
            policy_revision: self.policy_revision,
            input_revision: self.input_revision,
            plan: self.plan.clone(),
            canonical_input_digest_sha256: self.canonical_input_digest_sha256.clone(),
        }
        .validate()
        .map_err(|_| denied("invalid frozen command plan"))?;
        if self.plan.execution_basis
            == desk_agent_protocol::exec::ExecExecutionBasis::OwnerBlocklistOnly
            && self.admission_policy != ExecAdmissionPolicy::OwnerInteractive
        {
            return Err(denied("freeform command requires owner-interactive policy"));
        }
        Ok(())
    }

    pub fn resource_scope(&self) -> Result<Vec<String>, AgentError> {
        let bytes = serde_json::to_vec(self).map_err(|_| denied("invalid command confirmation"))?;
        let mut scope = crate::capability_grant::exact_command_resource_scope(
            &self.canonical_input_digest_sha256,
        );
        scope.push(format!("command_plan:sha256:{:x}", Sha256::digest(bytes)));
        Ok(scope)
    }
}
