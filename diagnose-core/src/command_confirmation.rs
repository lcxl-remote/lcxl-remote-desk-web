//! Server-authored command review and immutable execution binding.

use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::command_blocklist::BlocklistRule;
use desk_agent_protocol::command_template::SyncedCommandTemplate;
use desk_agent_protocol::exec::{CommandDraft, ExecDecision, ExecIoMode, ExecPlanDraft};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, ExecInput, ExecTarget, ExecutionMode, RiskLevel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const COMMAND_TOOL: &str = "execute_confirmed_command";

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(command: &str) -> String {
        serde_json::to_string(&CommandDraft {
            schema_version: 1,
            shell: "bash".into(),
            command: command.into(),
            cwd: None,
            timeout_ms: 30_000,
        })
        .unwrap()
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
        let proposal: CommandDraft =
            serde_json::from_str(canonical).map_err(|_| denied("invalid exact command input"))?;
        proposal
            .validate()
            .map_err(|_| denied("invalid exact command input"))?;
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
            io_mode: ExecIoMode::NonInteractive,
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
        if plan.risk > self.max_risk || plan.io_mode.is_pty() {
            return Err(denied(
                "command exceeds the current risk or non-interactive execution policy",
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

impl CommandConfirmation {
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
            || self.plan.io_mode != ExecIoMode::NonInteractive
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
