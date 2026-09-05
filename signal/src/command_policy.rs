//! Fresh command policy for authenticated single-account assistant requests.

use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::{AgentError, AgentErrorKind, RiskLevel};
use desk_diagnose_core::command_confirmation::CommandPolicyContext;
use desk_signal_facade::model::connection::SharedConnectionMap;
use sea_orm::DatabaseConnection;

pub(crate) async fn current(
    db: &DatabaseConnection,
    connections: &SharedConnectionMap,
    target_connection_id: &str,
    actor_id: &str,
) -> Result<CommandPolicyContext, AgentError> {
    let denied = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "current owner command policy is unavailable".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    if actor_id != crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string()
        || !crate::device_assistant_gate::global_device_assistant_gate().is_enabled()
    {
        return Err(denied());
    }
    let map = connections.read().await;
    let target = map.get(target_connection_id).ok_or_else(denied)?;
    if target.auth_context.auth_kind != desk_signal_facade::model::auth_context::AuthKind::TokenAuth
        || target.auth_context.remote_desk_type
            != desk_signal_facade::model::signal::RemoteDeskTypeEnum::Server
    {
        return Err(denied());
    }
    let device = target
        .model
        .version_info
        .client_id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(denied)?;
    let shells = target.model.version_info.available_exec_shell_list();
    let max_runtime_ms = target
        .model
        .version_info
        .max_ai_command_runtime_ms
        .unwrap_or(desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS);
    drop(map);
    let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
        .get_fresh(target_connection_id, chrono::Utc::now())
        .ok_or_else(denied)?;
    let mode = crate::model_provider::load(db)
        .await
        .map_err(|_| denied())?
        .execution_mode;
    Ok(CommandPolicyContext {
        actor_id: actor_id.into(),
        target_device_id: device,
        target_session_id: format!(
            "{}:{}",
            target_connection_id, readiness.readiness.interactive_session_incarnation
        ),
        policy_revision: desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        admission_policy: ExecAdmissionPolicy::OwnerInteractive,
        execution_mode: mode,
        max_risk: RiskLevel::Critical,
        available_shells: shells,
        max_runtime_ms,
        operator_templates: vec![],
        effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
        policy_version: format!("oss-owner:{}", readiness.readiness.local_ceiling_revision),
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn frozen_macos_disk_scan_plan_executes_the_original_multiline_script_in_an_isolated_directory()
    {
        let fixture = tempfile::Builder::new()
            .prefix("assistant command ")
            .tempdir()
            .unwrap();
        for (name, bytes) in [("small directory", 4096), ("large directory", 131072)] {
            let path = fixture.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("fixture.bin"), vec![42u8; bytes]).unwrap();
        }
        let policy = CommandPolicyContext {
            actor_id: "1".into(),
            target_device_id: "fixture-device".into(),
            target_session_id: "fixture-host:session".into(),
            policy_revision: 1,
            admission_policy: ExecAdmissionPolicy::OwnerInteractive,
            execution_mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
            max_risk: RiskLevel::Critical,
            available_shells: vec!["bash".into()],
            max_runtime_ms: 10000,
            operator_templates: vec![],
            effective_blocklist: desk_agent_protocol::exec_policy::builtin_blocklist().to_vec(),
            policy_version: "fixture-owner-policy".into(),
        };
        let script = "du -k -d 1 . | sort -n\ndf -k .";
        let input = serde_json::json!({"schema_version":1, "shell":"bash", "command":script,
            "cwd":fixture.path().to_str().unwrap(), "timeout_ms":10000})
        .to_string();
        let confirmation = policy.prepare(&input, 1).unwrap();
        policy.revalidate(&confirmation, &input, 1).unwrap();
        assert_eq!(confirmation.plan.argv, vec!["-lc", script]);
        assert_eq!(
            confirmation.plan.io_mode,
            desk_agent_protocol::exec::ExecIoMode::NonInteractive
        );
        // This is a real shell/argv smoke test, not a substitute for the remote
        // permission, transport, worker containment, or model end-to-end tests.
        let output = std::process::Command::new(&confirmation.plan.program)
            .args(&confirmation.plan.argv)
            .current_dir(confirmation.plan.cwd.as_ref().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let size = |directory: &str| {
            stdout
                .lines()
                .find_map(|line| {
                    let (size, path) = line.split_once('\t')?;
                    (path == format!("./{directory}")).then(|| size.parse::<u64>().unwrap())
                })
                .unwrap()
        };
        assert!(size("large directory") > size("small directory"));
        assert!(stdout.contains("Filesystem"));
    }
}
