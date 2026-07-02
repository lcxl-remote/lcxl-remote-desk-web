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
use serde::Deserialize;
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::registry::{RegisteredTool, ToolEffect};

/// The single mutating exec tool the agent loop exposes.
pub const EXEC_TOOL_NAME: &str = "exec_command";

/// Default execution limits applied when the model omits them. Concrete values are
/// tuned against M1a measurement; the contract is that the model never sets
/// unbounded limits.
const DEFAULT_EXEC_TIMEOUT_MS: u32 = 10_000;
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
    vec![RegisteredTool {
        spec: ToolSpec {
            name: EXEC_TOOL_NAME.to_string(),
            description: "Run a shell command on the device. Requires explicit \
                operator approval before it executes; only whitelisted commands are \
                executable."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run."},
                    "shell": {"type": "string", "description": "Shell family, e.g. powershell or bash."},
                    "cwd": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 0},
                    "reason": {"type": "string", "description": "Why this command is needed."}
                },
                "required": ["command"]
            }),
        },
        // Exposure gate only; the real authz capability is decided by classification.
        required_capability: Capability::ShellExecConfirmed,
        effect: ToolEffect::Mutating,
    }]
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
        timeout_ms: params.timeout_ms.unwrap_or(DEFAULT_EXEC_TIMEOUT_MS),
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
                assert_eq!(e.timeout_ms, DEFAULT_EXEC_TIMEOUT_MS);
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
