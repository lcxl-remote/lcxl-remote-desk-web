//! The `wait_for_task` tool: lets the model actively wait on the background task it
//! just dispatched, so a completed result becomes that call's real tool result
//! rather than a passively injected notification.
//!
//! Exposure is gated by [`ToolEffect::WaitTask`](crate::registry::ToolEffect::WaitTask)
//! (offered only while the session has an in-flight task); the loop validates the
//! requested `task_id` against the session's own execution identity before calling
//! [`ToolSeam::wait_for_task`](crate::seam::ToolSeam::wait_for_task), so a control
//! end can never steer it at another session's work.

use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde::Deserialize;
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::registry::{RegisteredTool, ToolEffect};

/// The tool the agent loop exposes to wait on a running background task.
pub const WAIT_TOOL_NAME: &str = "wait_for_task";

/// The wait tool's model-facing arguments.
#[derive(Debug, Clone, Deserialize, Default)]
struct WaitTaskParams {
    /// The id of the background task to wait on — the `exec_request_id` the dispatch
    /// tool result named ("dispatched as background task <id>").
    #[serde(default)]
    task_id: String,
}

fn bad_arguments(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid wait_for_task arguments: {detail}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// The wait tool registry (one tool). Its `required_capability` is unused — the
/// registry exposes [`ToolEffect::WaitTask`] purely on the presence of an in-flight
/// task (see [`is_exposed`](crate::registry::is_exposed)) — but the field is
/// required, so a harmless read capability stands in.
pub fn wait_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: WAIT_TOOL_NAME.to_string(),
            description: "Wait for a previously dispatched background command to \
                finish and return its result. Pass the task id from the dispatch \
                message. Returns promptly if it is still running so you can keep \
                working or wait again."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The background task id to wait on."
                    }
                },
                "required": ["task_id"]
            }),
        },
        // Unused for WaitTask exposure; a benign placeholder.
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::WaitTask,
    }]
}

/// Parse the requested task id from a wait tool call. Rejects a wrong tool name or a
/// missing / blank `task_id` as a model-safe error.
pub fn parse_wait_task_id(call: &ToolCall) -> Result<String, AgentError> {
    if call.name != WAIT_TOOL_NAME {
        return Err(AgentError {
            kind: AgentErrorKind::UnsupportedCapability,
            message: format!("unknown wait tool `{}`", call.name),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
    }
    let trimmed = call.arguments_json.trim();
    let params: WaitTaskParams = if trimmed.is_empty() {
        WaitTaskParams::default()
    } else {
        serde_json::from_str(trimmed).map_err(bad_arguments)?
    };
    let task_id = params.task_id.trim();
    if task_id.is_empty() {
        return Err(bad_arguments("`task_id` is required and must be non-empty"));
    }
    Ok(task_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments_json: args.into(),
        }
    }

    /// The registry exposes exactly one wait tool with the `WaitTask` effect.
    #[test]
    fn registry_is_one_wait_tool() {
        let reg = wait_tool_registry();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg[0].name(), WAIT_TOOL_NAME);
        assert_eq!(reg[0].effect, ToolEffect::WaitTask);
    }

    /// A populated call yields the requested task id.
    #[test]
    fn parses_the_task_id() {
        let id = parse_wait_task_id(&call(WAIT_TOOL_NAME, r#"{"task_id":"exec_ab12"}"#)).unwrap();
        assert_eq!(id, "exec_ab12");
    }

    /// A missing / blank task id, malformed JSON, or a wrong tool name is a
    /// model-safe error rather than a silent default.
    #[test]
    fn rejects_bad_input() {
        assert!(parse_wait_task_id(&call(WAIT_TOOL_NAME, "{}")).is_err());
        assert!(parse_wait_task_id(&call(WAIT_TOOL_NAME, r#"{"task_id":"  "}"#)).is_err());
        assert!(parse_wait_task_id(&call(WAIT_TOOL_NAME, "{not json")).is_err());
        assert!(parse_wait_task_id(&call("read_system_info", r#"{"task_id":"x"}"#)).is_err());
    }
}
