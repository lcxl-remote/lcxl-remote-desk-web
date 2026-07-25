//! The mutating exec-tool registry and the model-call → exec-operation mapping,
//! shared by both runtimes so they can never drift.
//!
//! The agent loop offers one mutating tool — [`EXEC_TOOL_NAME`] — gated (for
//! exposure) behind [`Capability::ShellExecConfirmed`] and a confirm-or-higher
//! mode. [`build_exec_input`] turns a tool call into the neutral
//! [`OperationInput::Exec`] the runtime then classifies + approves + executes. The
//! tool's *real* required capability (`shell.exec.readonly` vs
//! `shell.exec.confirmed`) is **not** derivable from the input — it is the output of
//! server-side risk classification (see [`OperationInput::required_capability`]) —
//! so the registry's `required_capability` is only the model-exposure gate, not the
//! authz decision.

use desk_agent_protocol::{
    AgentError, AgentErrorKind, Capability, ExecInput, ExecTarget, OperationInput,
};
use desk_utils::error::DeskErrorCode;
use serde::Deserialize;
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::registry::{RegisteredTool, ToolEffect};

/// The single mutating exec tool the agent loop exposes.
pub const EXEC_TOOL_NAME: &str = "exec_command";
/// Canonical free-form shell names understood by the shared classifier.
///
/// `ExecShellKind::Native` is deliberately absent: it is an internal direct-spawn
/// mode used by sealed templates, not a model-selectable interpreter.
pub const SUPPORTED_EXEC_SHELLS: &[&str] = &["powershell", "pwsh", "bash", "sh"];

/// Default execution limits applied when the model omits them. Concrete values are
/// tuned against M1a measurement; the contract is that the model never sets
/// unbounded limits.
// The agent loop stops waiting after its much shorter foreground threshold and
// tracks the command in the background. The wall-time limit remains finite and
// defaults to ten minutes unless the target advertises a narrower local ceiling.
const DEFAULT_EXEC_TIMEOUT_MS: u32 = desk_agent_protocol::exec_policy::DEFAULT_TIMEOUT_MS;
const DEFAULT_EXEC_MAX_STDOUT_BYTES: u32 = 65_536;
const DEFAULT_EXEC_MAX_STDERR_BYTES: u32 = 65_536;
/// Requested shell family when the model does not specify one; the server's
/// classifier resolves the concrete interpreter.
const DEFAULT_EXEC_SHELL: &str = "auto";

/// The exec tool's model-facing arguments.
#[derive(Debug, Clone, Deserialize, Default)]
struct ExecCommandParams {
    /// The command to run (free-form; classified + template-matched server-side).
    #[serde(default)]
    command: String,
    /// Requested shell family (e.g. `powershell`, `bash`); defaults to `auto`.
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u32>,
    /// Free-text "why" the model wants to run this; flows into the audit event.
    #[serde(default)]
    reason: Option<String>,
}

fn bad_arguments(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid exec tool arguments: {detail}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// The mutating tools the agent loop exposes (subject to scope/mode filtering and
/// the no-mutation-while-unknown rule). One tool today: [`EXEC_TOOL_NAME`].
pub fn exec_tool_registry() -> Vec<RegisteredTool> {
    exec_tool_registry_for_shells(
        &SUPPORTED_EXEC_SHELLS
            .iter()
            .map(|shell| (*shell).to_string())
            .collect::<Vec<_>>(),
    )
}

/// Build the exec tool with a target-specific shell enum.
///
/// Constraining the JSON schema keeps the model from spending tool calls trying
/// interpreters that cannot run on the target. The execution seam still
/// revalidates the choice authoritatively.
pub fn exec_tool_registry_for_shells(available_shells: &[String]) -> Vec<RegisteredTool> {
    exec_tool_registry_for_shells_with_timeout(available_shells, DEFAULT_EXEC_TIMEOUT_MS)
}

/// Build the exec tool with the target's local command-runtime ceiling.
pub fn exec_tool_registry_for_shells_with_timeout(
    available_shells: &[String],
    max_runtime_ms: u32,
) -> Vec<RegisteredTool> {
    let available_shells = sanitize_available_exec_shells(available_shells);
    let max_runtime_ms = max_runtime_ms.clamp(
        desk_agent_protocol::exec_policy::MIN_TIMEOUT_MS,
        desk_agent_protocol::exec_policy::MAX_TIMEOUT_MS,
    );
    let shell_description = if available_shells.is_empty() {
        "No AI execution shell is currently available on the target device.".to_string()
    } else {
        format!(
            "Shell verified as available on the target device. Choose one of: {}. \
             Prefer {} unless the command specifically requires another listed shell.",
            available_shells.join(", "),
            available_shells[0]
        )
    };
    let preferred_shell = available_shells.first().cloned();
    vec![RegisteredTool {
        spec: ToolSpec {
            name: EXEC_TOOL_NAME.to_string(),
            description: "Run a shell command on the device. Requires explicit \
                operator approval before it executes. Commands outside a known \
                template are admitted only for the authenticated device owner, are \
                always classified Critical, and still pass the server blocklist."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run."},
                    "shell": {
                        "type": "string",
                        "enum": available_shells,
                        "default": preferred_shell,
                        "description": shell_description
                    },
                    "cwd": {"type": "string"},
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": max_runtime_ms,
                        "default": max_runtime_ms,
                        "description": format!(
                            "Command wall-time limit in milliseconds. Defaults to the target device ceiling of {max_runtime_ms}; set it explicitly only when a shorter limit is required."
                        )
                    },
                    "reason": {"type": "string", "description": "Why this command is needed."}
                },
                "required": ["command", "shell"]
            }),
        },
        // Exposure gate only; the real authz capability is decided by classification.
        required_capability: Capability::ShellExecConfirmed,
        effect: ToolEffect::Mutating,
    }]
}

/// Apply the target device's local wall-time ceiling before classification.
///
/// A missing model value adopts the ceiling. An explicit shorter value is kept;
/// a larger one is narrowed. Central and edge both call this helper so the sealed
/// draft remains byte-for-byte reproducible.
pub fn apply_exec_runtime_ceiling(input: &mut ExecInput, max_runtime_ms: u32) {
    let ceiling = max_runtime_ms.clamp(
        desk_agent_protocol::exec_policy::MIN_TIMEOUT_MS,
        desk_agent_protocol::exec_policy::MAX_TIMEOUT_MS,
    );
    input.timeout_ms = if input.timeout_ms == 0 {
        ceiling
    } else {
        input.timeout_ms.min(ceiling)
    };
}

/// Normalize a model-provided interpreter name to the canonical executor name.
pub fn canonical_exec_shell(shell: &str) -> Option<&'static str> {
    match shell.trim().to_ascii_lowercase().as_str() {
        "powershell" | "powershell.exe" => Some("powershell"),
        "pwsh" | "pwsh.exe" => Some("pwsh"),
        "bash" | "bash.exe" => Some("bash"),
        "sh" | "sh.exe" => Some("sh"),
        _ => None,
    }
}

/// Keep only canonical classifier-supported names, preserving host preference
/// order and removing duplicates. A host report never widens the executor.
pub fn sanitize_available_exec_shells(shells: &[String]) -> Vec<String> {
    let mut sanitized = Vec::new();
    for shell in shells {
        let Some(canonical) = canonical_exec_shell(shell) else {
            continue;
        };
        if !sanitized.iter().any(|item| item == canonical) {
            sanitized.push(canonical.to_string());
        }
    }
    sanitized
}

/// Whether the requested interpreter is in the target's verified capability set.
pub fn exec_shell_is_available(shell: &str, available_shells: &[String]) -> bool {
    let Some(requested) = canonical_exec_shell(shell) else {
        return false;
    };
    sanitize_available_exec_shells(available_shells)
        .iter()
        .any(|available| available == requested)
}

/// Model-safe structured error for an interpreter that is unsupported or not
/// usable on this target.
pub fn unsupported_exec_shell_error(
    requested_shell: &str,
    available_shells: &[String],
) -> AgentError {
    let details = json!({
        "error_code": "unsupported_exec_shell",
        "requested_shell": requested_shell,
        "available_shells": sanitize_available_exec_shells(available_shells),
        "retryable": true
    });
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: details.to_string(),
        retryable: true,
        safe_for_model: true,
        error_code: Some(DeskErrorCode::AI_EXEC_SHELL_UNSUPPORTED.code()),
    }
}

/// Map an exec tool call to the neutral [`OperationInput::Exec`] plus the model's
/// free-text reason. The runtime classifies + approves + executes from here.
pub fn build_exec_input(call: &ToolCall) -> Result<(OperationInput, Option<String>), AgentError> {
    if call.name != EXEC_TOOL_NAME {
        return Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: format!("unknown exec tool `{}`", call.name),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
    }
    let trimmed = call.arguments_json.trim();
    let params: ExecCommandParams = if trimmed.is_empty() {
        ExecCommandParams::default()
    } else {
        serde_json::from_str(trimmed).map_err(bad_arguments)?
    };
    if params.command.trim().is_empty() {
        return Err(bad_arguments("`command` is required and must be non-empty"));
    }
    let input = ExecInput {
        target: ExecTarget::Shell {
            shell: params
                .shell
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_EXEC_SHELL.to_string()),
        },
        command: params.command,
        cwd: params.cwd,
        // Keep an omitted timeout distinguishable until the target-specific
        // execution ceiling is applied by the central execution seam.
        timeout_ms: params.timeout_ms.unwrap_or(0),
        max_stdout_bytes: DEFAULT_EXEC_MAX_STDOUT_BYTES,
        max_stderr_bytes: DEFAULT_EXEC_MAX_STDERR_BYTES,
    };
    Ok((OperationInput::Exec(input), params.reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: EXEC_TOOL_NAME.into(),
            arguments_json: args.into(),
        }
    }

    /// The registry exposes exactly one mutating tool gated on the confirmed-exec
    /// capability.
    #[test]
    fn registry_is_one_mutating_tool() {
        let reg = exec_tool_registry();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg[0].name(), EXEC_TOOL_NAME);
        assert_eq!(reg[0].effect, ToolEffect::Mutating);
        assert_eq!(reg[0].required_capability, Capability::ShellExecConfirmed);
        assert_eq!(
            reg[0].spec.parameters_schema["properties"]["shell"]["enum"],
            json!(SUPPORTED_EXEC_SHELLS)
        );
        assert_eq!(
            reg[0].spec.parameters_schema["required"],
            json!(["command", "shell"])
        );
        assert_eq!(
            reg[0].spec.parameters_schema["properties"]["shell"]["default"],
            json!("powershell")
        );
    }

    #[test]
    fn target_shells_are_filtered_and_deduplicated() {
        let shells = sanitize_available_exec_shells(&[
            "powershell.exe".into(),
            "cmd".into(),
            "bash".into(),
            "powershell".into(),
        ]);
        assert_eq!(shells, vec!["powershell".to_string(), "bash".to_string()]);
    }

    #[test]
    fn target_runtime_ceiling_is_advertised_and_applied() {
        let registry = exec_tool_registry_for_shells_with_timeout(&["powershell".into()], 120_000);
        assert_eq!(
            registry[0].spec.parameters_schema["properties"]["timeout_ms"]["maximum"],
            json!(120_000)
        );

        let mut input = match build_exec_input(&call(
            r#"{"command":"Get-Process","shell":"powershell","timeout_ms":900000}"#,
        ))
        .unwrap()
        .0
        {
            OperationInput::Exec(input) => input,
            other => panic!("expected Exec, got {other:?}"),
        };
        apply_exec_runtime_ceiling(&mut input, 120_000);
        assert_eq!(input.timeout_ms, 120_000);
        input.timeout_ms = 0;
        apply_exec_runtime_ceiling(&mut input, 300_000);
        assert_eq!(input.timeout_ms, 300_000);
    }

    #[test]
    fn unsupported_shell_error_carries_retry_details() {
        let error = unsupported_exec_shell_error("cmd", &["powershell".into()]);
        assert_eq!(
            error.error_code,
            Some(DeskErrorCode::AI_EXEC_SHELL_UNSUPPORTED.code())
        );
        assert!(error.retryable);
        assert!(error.message.contains("\"requested_shell\":\"cmd\""));
        assert!(
            error
                .message
                .contains("\"available_shells\":[\"powershell\"]")
        );
    }

    /// A populated call maps to an `Exec` input with the command, shell, and bounded
    /// limits, and surfaces the reason.
    #[test]
    fn builds_exec_input_with_defaults_and_overrides() {
        let (input, reason) = build_exec_input(&call(
            r#"{"command":"Restart-Service Spooler","shell":"powershell","reason":"fix printing"}"#,
        ))
        .unwrap();
        assert_eq!(reason.as_deref(), Some("fix printing"));
        match input {
            OperationInput::Exec(e) => {
                assert_eq!(e.command, "Restart-Service Spooler");
                assert_eq!(e.timeout_ms, 0);
                assert_eq!(e.max_stdout_bytes, DEFAULT_EXEC_MAX_STDOUT_BYTES);
                assert!(matches!(e.target, ExecTarget::Shell { shell } if shell == "powershell"));
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// A blank shell falls back to the default family.
    #[test]
    fn blank_shell_defaults() {
        let (input, _) = build_exec_input(&call(r#"{"command":"ls","shell":"  "}"#)).unwrap();
        match input {
            OperationInput::Exec(e) => {
                assert!(
                    matches!(e.target, ExecTarget::Shell { shell } if shell == DEFAULT_EXEC_SHELL)
                );
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// A missing / empty command is a model-safe error, not a silent default.
    #[test]
    fn empty_command_is_rejected() {
        assert!(build_exec_input(&call("{}")).is_err());
        assert!(build_exec_input(&call(r#"{"command":"   "}"#)).is_err());
        // Malformed JSON is an error too.
        assert!(build_exec_input(&call("{not json")).is_err());
    }

    /// A call for a different tool name is rejected.
    #[test]
    fn wrong_tool_name_rejected() {
        let c = ToolCall {
            id: "c".into(),
            name: "read_system_info".into(),
            arguments_json: "{}".into(),
        };
        assert!(build_exec_input(&c).is_err());
    }
}
