//! The read-only tool registry and the model-call → read-operation mapping,
//! shared by both runtimes so they can never drift.
//!
//! The model is offered a fixed set of read tools ([`read_tool_registry`]); each
//! tool name maps to exactly one [`ContextKind`] in [`build_read_operation`], and
//! the required [`Capability`] is **derived** from the built input
//! ([`OperationInput::capability`]) — one source of truth for the permission
//! point. The Direct runtime (daemon, in-process [`desk_agent_protocol::DeviceAgent`])
//! and the Manager runtime (central orchestrator, remote edge) both build the
//! exact same operation from a given tool call, so the tool surface, the
//! capability gate, and the audit can never disagree.

use desk_agent_protocol::{
    AgentError, AgentErrorKind, Capability, ContainerListParams, ContextKind, LogRecentParams,
    NetworkPortsParams, OperationInput, ProcessListParams, ReadContextInput, ScreenCaptureParams,
    ServiceStatusParams, SystemInfoParams,
};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::chat::{ToolCall, ToolSpec};
use crate::registry::{RegisteredTool, ToolEffect};

/// A model-safe "invalid tool arguments" error (the loop turns it into an error
/// tool-result so the model can correct itself).
fn bad_arguments(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid tool arguments: {detail}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Build a read tool's spec from its model-facing name, description, and a JSON
/// Schema for its arguments.
fn spec(name: &str, description: &str, parameters_schema: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters_schema,
    }
}

fn read(
    name: &str,
    cap: Capability,
    description: &str,
    schema: serde_json::Value,
) -> RegisteredTool {
    RegisteredTool {
        spec: spec(name, description, schema),
        required_capability: cap,
        effect: ToolEffect::ReadOnly,
    }
}

/// The read-only tools the agent loop exposes (subject to scope/mode filtering).
/// Each tool name maps to one [`ContextKind`] in [`build_read_operation`].
pub fn read_tool_registry() -> Vec<RegisteredTool> {
    vec![
        read(
            "read_system_info",
            Capability::SystemInfo,
            "Read the device's OS, CPU, memory, and uptime summary.",
            json!({
                "type": "object",
                "properties": {
                    "include_hardware": {"type": "boolean"},
                    "include_network_summary": {"type": "boolean"}
                }
            }),
        ),
        read(
            "read_process_list",
            Capability::ProcessList,
            "List running processes, optionally sorted and limited.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 0},
                    "sort": {"type": "string", "enum": ["cpu_desc", "memory_desc", "pid"]},
                    "include_command_line": {"type": "boolean"}
                }
            }),
        ),
        read(
            "read_network_ports",
            Capability::NetworkPorts,
            "List listening network ports; optionally filter by protocol.",
            json!({
                "type": "object",
                "properties": {"protocol": {"type": "string"}}
            }),
        ),
        read(
            "read_service_status",
            Capability::ServiceStatus,
            "Read the status of system services; name one or enumerate.",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string"}}
            }),
        ),
        read(
            "read_recent_logs",
            Capability::LogRecent,
            "Read recent system log events (redacted).",
            json!({"type": "object"}),
        ),
        read(
            "read_container_list",
            Capability::ContainerList,
            "List containers on the device.",
            json!({"type": "object"}),
        ),
        read(
            "read_current_screen",
            Capability::ScreenCaptureCurrent,
            "Capture the device's current screen for visual diagnosis.",
            json!({
                "type": "object",
                "properties": {
                    "display": {"type": "string"}
                }
            }),
        ),
    ]
}

/// Parse a params struct from the model's `arguments_json`, treating empty / `{}`
/// as defaults (every read params type is all-optional).
fn parse_params<T: DeserializeOwned + Default>(arguments_json: &str) -> Result<T, AgentError> {
    let trimmed = arguments_json.trim();
    if trimmed.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(trimmed).map_err(bad_arguments)
}

/// Map a read tool call (name + arguments) to a server-side read operation and
/// the capability it requires (derived from the built input — one source).
pub fn build_read_operation(call: &ToolCall) -> Result<(Capability, OperationInput), AgentError> {
    let kind = match call.name.as_str() {
        "read_system_info" => {
            ContextKind::SystemInfo(parse_params::<SystemInfoParams>(&call.arguments_json)?)
        }
        "read_process_list" => {
            ContextKind::ProcessList(parse_params::<ProcessListParams>(&call.arguments_json)?)
        }
        "read_network_ports" => {
            ContextKind::NetworkPorts(parse_params::<NetworkPortsParams>(&call.arguments_json)?)
        }
        "read_service_status" => {
            ContextKind::ServiceStatus(parse_params::<ServiceStatusParams>(&call.arguments_json)?)
        }
        "read_recent_logs" => {
            ContextKind::LogRecent(parse_params::<LogRecentParams>(&call.arguments_json)?)
        }
        "read_container_list" => {
            ContextKind::ContainerList(parse_params::<ContainerListParams>(&call.arguments_json)?)
        }
        "read_current_screen" => ContextKind::ScreenCaptureCurrent(parse_params::<
            ScreenCaptureParams,
        >(&call.arguments_json)?),
        other => {
            return Err(AgentError {
                kind: AgentErrorKind::UnsupportedCapability,
                message: format!("unknown read tool `{other}`"),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
    };
    let input = OperationInput::ReadContext(ReadContextInput { kind });
    let cap = input.capability().ok_or_else(|| AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: "read tool maps to no capability".to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })?;
    Ok((cap, input))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_read_operation` maps each known tool name to the right capability
    /// and accepts both empty and populated arguments.
    #[test]
    fn read_operation_mapping() {
        let (cap, _) = build_read_operation(&ToolCall {
            id: "c".into(),
            name: "read_system_info".into(),
            arguments_json: String::new(),
        })
        .unwrap();
        assert_eq!(cap, Capability::SystemInfo);

        let (cap, input) = build_read_operation(&ToolCall {
            id: "c".into(),
            name: "read_process_list".into(),
            arguments_json: r#"{"limit": 5, "sort": "memory_desc"}"#.into(),
        })
        .unwrap();
        assert_eq!(cap, Capability::ProcessList);
        assert!(matches!(
            input,
            OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ProcessList(_)
            })
        ));

        let (cap, input) = build_read_operation(&ToolCall {
            id: "c".into(),
            name: "read_current_screen".into(),
            arguments_json: r#"{"display":"primary"}"#.into(),
        })
        .unwrap();
        assert_eq!(cap, Capability::ScreenCaptureCurrent);
        assert!(matches!(
            input,
            OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ScreenCaptureCurrent(_)
            })
        ));

        // Unknown tool is rejected.
        assert!(
            build_read_operation(&ToolCall {
                id: "c".into(),
                name: "nope".into(),
                arguments_json: String::new(),
            })
            .is_err()
        );

        // Malformed arguments are an error (not a silent default).
        assert!(
            build_read_operation(&ToolCall {
                id: "c".into(),
                name: "read_process_list".into(),
                arguments_json: "{not json".into(),
            })
            .is_err()
        );
    }

    /// Every registered tool is read-only and its name maps back to its declared
    /// capability via the built operation.
    #[test]
    fn registry_is_read_only_and_maps() {
        for tool in read_tool_registry() {
            assert_eq!(tool.effect, ToolEffect::ReadOnly);
            let (cap, _) = build_read_operation(&ToolCall {
                id: "c".into(),
                name: tool.name().into(),
                arguments_json: String::new(),
            })
            .unwrap();
            assert_eq!(cap, tool.required_capability);
        }
    }
}
