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

use desk_agent_protocol::computer_use::{
    DesktopSessionInspectParams, FileContentReadParams, FileMetadataInspectParams,
    LiveDocumentInspectParams, ObjectRef, OfficeInspectParams, SpreadsheetFileInspectParams,
    SpreadsheetMergeColumnRule, SpreadsheetMergePreviewParams, SpreadsheetStatisticRequest,
    TerminalOutputInspectParams, UiInspectParams,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, Capability, ContainerListParams, ContextKind, LogRecentParams,
    NetworkPortsParams, OperationInput, ProcessListParams, ReadContextInput, ScreenCaptureParams,
    ServiceStatusParams, SystemInfoParams,
};
use serde::Deserialize;
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

const MAX_PROCESS_LIST_ENTRIES: u32 = 256;
const MAX_LOG_EVENTS: u32 = 200;
const MAX_LOG_WINDOW_MINUTES: u32 = 24 * 60;
const MAX_DIAGNOSTIC_FILTER_CHARS: usize = 128;

fn validate_optional_filter(value: &Option<String>, field: &str) -> Result<(), AgentError> {
    if let Some(value) = value
        && (value.is_empty()
            || value.chars().count() > MAX_DIAGNOSTIC_FILTER_CHARS
            || value.chars().any(char::is_control))
    {
        return Err(bad_arguments(format!(
            "{field} must contain 1..={MAX_DIAGNOSTIC_FILTER_CHARS} non-control characters"
        )));
    }
    Ok(())
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
                },
                "additionalProperties": false
            }),
        ),
        read(
            "read_process_list",
            Capability::ProcessList,
            "List running processes, optionally sorted and limited.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 0, "maximum": MAX_PROCESS_LIST_ENTRIES},
                    "sort": {"type": "string", "enum": ["cpu_desc", "memory_desc", "pid"]},
                    "include_command_line": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "read_network_ports",
            Capability::NetworkPorts,
            "List listening network ports; optionally filter by protocol.",
            json!({
                "type": "object",
                "properties": {"protocol": {"type": "string", "enum": ["tcp", "udp"]}},
                "additionalProperties": false
            }),
        ),
        read(
            "read_service_status",
            Capability::ServiceStatus,
            "Read the status of system services; name one or enumerate.",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string", "minLength": 1, "maxLength": MAX_DIAGNOSTIC_FILTER_CHARS}},
                "additionalProperties": false
            }),
        ),
        read(
            "read_recent_logs",
            Capability::LogRecent,
            "Read recent system log events (redacted).",
            json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "minLength": 1, "maxLength": MAX_DIAGNOSTIC_FILTER_CHARS},
                    "since_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_LOG_WINDOW_MINUTES},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LOG_EVENTS},
                    "severity": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["error", "warning", "info", "debug"]},
                        "maxItems": 4,
                        "uniqueItems": true
                    }
                },
                "additionalProperties": false
            }),
        ),
        read(
            "read_container_list",
            Capability::ContainerList,
            "List containers on the device.",
            json!({"type": "object", "additionalProperties": false}),
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

/// The read-only tools reserved for the Device Assistant surface. They are kept
/// out of [`read_tool_registry`] so Diagnose can never acquire Computer Use by a
/// broad scope or a future default-set change.
pub fn device_assistant_read_tool_registry() -> Vec<RegisteredTool> {
    vec![
        read(
            "inspect_desktop_session",
            Capability::DesktopSessionInspect,
            "Inspect the current interactive desktop session and optionally return an opaque reference to the active application.",
            json!({
                "type": "object",
                "properties": {
                    "include_active_application": {"type": "boolean", "default": true}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_desktop_ui",
            Capability::DesktopUiInspect,
            "Read a bounded, redacted semantic UI tree from the active Windows application. Values from protected fields are never returned.",
            json!({
                "type": "object",
                "properties": {
                    "root": {
                        "anyOf": [
                            {"type": "null"},
                            {
                                "type": "object",
                                "properties": {
                                    "token": {"type": "string"},
                                    "snapshot_id": {"type": "string"},
                                    "object_kind": {"type": "string"},
                                    "expires_at": {"type": "string"}
                                },
                                "required": ["token", "snapshot_id", "object_kind", "expires_at"],
                                "additionalProperties": false
                            }
                        ]
                    },
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": 12, "default": 6},
                    "max_nodes": {"type": "integer", "minimum": 1, "maximum": 4096, "default": 300},
                    "max_bytes": {"type": "integer", "minimum": 1024, "maximum": 1048576, "default": 262144}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_office_selection",
            Capability::OfficeDocumentInspect,
            "Read the active paired Excel selection through the bounded Office.js semantic bridge, including formulas, scalar values, and number formats. No workbook mutation is possible.",
            json!({
                "type": "object",
                "properties": {
                    "document": {
                        "anyOf": [
                            {"type": "null"},
                            {
                                "type": "object",
                                "properties": {
                                    "token": {"type": "string"},
                                    "snapshot_id": {"type": "string"},
                                    "object_kind": {"type": "string", "const": "office_document"},
                                    "expires_at": {"type": "string"}
                                },
                                "required": ["token", "snapshot_id", "object_kind", "expires_at"],
                                "additionalProperties": false
                            }
                        ]
                    },
                    "selection_only": {"type": "boolean", "const": true, "default": true},
                    "max_objects": {"type": "integer", "minimum": 1, "maximum": 16, "default": 16},
                    "max_bytes": {"type": "integer", "minimum": 1024, "maximum": 262144, "default": 262144}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_selected_file_metadata",
            Capability::FileMetadataRead,
            "Read bounded metadata for only the files or directories explicitly selected by the owner. For a selected directory this lists immediate children without recursion, following reparse points, or reading contents. Optional extension, byte-size, and RFC3339 modification-time filters are applied by the edge only to immediate file children.",
            json!({
                "type": "object",
                "properties": {
                    "file_extensions": {
                        "type": "array",
                        "items": {"type": "string", "pattern": "^\\.[A-Za-z0-9][A-Za-z0-9._-]{0,15}$"},
                        "maxItems": 16,
                        "uniqueItems": true,
                        "default": []
                    },
                    "min_file_bytes": {"type": ["integer", "null"], "minimum": 0},
                    "max_file_bytes": {"type": ["integer", "null"], "minimum": 0},
                    "modified_after": {"type": ["string", "null"], "format": "date-time"},
                    "modified_before": {"type": ["string", "null"], "format": "date-time"}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "read_selected_text_file",
            Capability::FileContentRead,
            "Read one explicitly owner-selected regular file as bounded UTF-8 text. The model cannot provide a path or object reference.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_selected_spreadsheets",
            Capability::SpreadsheetFileInspect,
            "Read bounded cell, formula, and value projections from explicitly owner-selected inert .xlsx, .csv, or .tsv files, or from supported direct children of an explicitly selected directory. Directory expansion is non-recursive and bounded; macros, external links, data connections, and model-provided paths are rejected.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        read(
            "preview_spreadsheet_merge",
            Capability::SpreadsheetMergePreview,
            "Preview a bounded multi-workbook merge, dedupe, and statistics operation over explicitly selected inert spreadsheets. Rules are typed data only; no script or formula is executed and no file is written.",
            json!({
                "type": "object",
                "properties": {
                    "source_sheet": {"type": ["string", "null"], "maxLength": 128},
                    "header_row": {"type": "integer", "minimum": 1, "maximum": 32, "default": 1},
                    "columns": {
                        "type": "array", "minItems": 1, "maxItems": 64,
                        "items": {
                            "type": "object",
                            "properties": {
                                "output_header": {"type": "string", "minLength": 1, "maxLength": 128},
                                "source_headers": {"type": "array", "minItems": 1, "maxItems": 8, "items": {"type": "string", "minLength": 1, "maxLength": 128}}
                            },
                            "required": ["output_header", "source_headers"],
                            "additionalProperties": false
                        }
                    },
                    "dedupe_keys": {"type": "array", "maxItems": 8, "items": {"type": "string", "minLength": 1, "maxLength": 128}, "default": []},
                    "statistics": {
                        "type": "array", "maxItems": 16, "default": [],
                        "items": {
                            "type": "object",
                            "properties": {
                                "operation": {"type": "string", "enum": ["count", "sum", "average", "min", "max"]},
                                "column": {"type": ["string", "null"], "maxLength": 128},
                                "group_by": {"type": "array", "maxItems": 4, "items": {"type": "string", "minLength": 1, "maxLength": 128}, "default": []}
                            },
                            "required": ["operation"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["columns"],
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_selected_terminal_output",
            Capability::TerminalOutputRead,
            "Read only the bounded recent terminal output snapshot explicitly attached by the owner. Secrets are redacted at the device before the result is returned.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        read(
            "read_current_screen",
            Capability::ScreenCaptureCurrent,
            "Capture the device's current screen once for this turn. The image is sensitive, is sent only to the selected visual model, and is never stored in the conversation.",
            json!({
                "type": "object",
                "properties": {
                    "display": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
    ]
}

#[derive(Debug, Default, Deserialize)]
struct DesktopSessionToolArgs {
    #[serde(default = "default_true")]
    include_active_application: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
struct DesktopUiToolArgs {
    #[serde(default)]
    root: Option<ObjectRef>,
    #[serde(default = "default_ui_depth")]
    max_depth: u16,
    #[serde(default = "default_ui_nodes")]
    max_nodes: u32,
    #[serde(default = "default_ui_bytes")]
    max_bytes: u32,
}

#[derive(Debug, Deserialize)]
struct OfficeSelectionToolArgs {
    #[serde(default)]
    document: Option<ObjectRef>,
    #[serde(default = "default_true")]
    selection_only: bool,
    #[serde(default = "default_office_objects")]
    max_objects: u32,
    #[serde(default = "default_office_bytes")]
    max_bytes: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveDocumentToolArgs {
    #[serde(default)]
    target: Option<ObjectRef>,
    #[serde(default = "default_office_bytes")]
    max_bytes: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchDocumentToolArgs {
    #[serde(default = "default_office_bytes")]
    max_bytes: u32,
}

impl Default for BatchDocumentToolArgs {
    fn default() -> Self {
        Self {
            max_bytes: default_office_bytes(),
        }
    }
}

impl Default for LiveDocumentToolArgs {
    fn default() -> Self {
        Self {
            target: None,
            max_bytes: default_office_bytes(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoToolArgs {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedFileMetadataToolArgs {
    #[serde(default)]
    file_extensions: Vec<String>,
    #[serde(default)]
    min_file_bytes: Option<u64>,
    #[serde(default)]
    max_file_bytes: Option<u64>,
    #[serde(default)]
    modified_after: Option<String>,
    #[serde(default)]
    modified_before: Option<String>,
}

impl Default for OfficeSelectionToolArgs {
    fn default() -> Self {
        Self {
            document: None,
            selection_only: true,
            max_objects: default_office_objects(),
            max_bytes: default_office_bytes(),
        }
    }
}

const fn default_ui_depth() -> u16 {
    6
}

const fn default_ui_nodes() -> u32 {
    300
}

const fn default_ui_bytes() -> u32 {
    262_144
}

const fn default_office_objects() -> u32 {
    16
}

const fn default_office_bytes() -> u32 {
    262_144
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
            let params = parse_params::<ProcessListParams>(&call.arguments_json)?;
            if params.limit > MAX_PROCESS_LIST_ENTRIES {
                return Err(bad_arguments(format!(
                    "limit must be at most {MAX_PROCESS_LIST_ENTRIES}"
                )));
            }
            ContextKind::ProcessList(params)
        }
        "read_network_ports" => {
            let params = parse_params::<NetworkPortsParams>(&call.arguments_json)?;
            validate_optional_filter(&params.protocol, "protocol")?;
            if params
                .protocol
                .as_deref()
                .is_some_and(|protocol| !matches!(protocol, "tcp" | "udp"))
            {
                return Err(bad_arguments("protocol must be tcp or udp"));
            }
            ContextKind::NetworkPorts(params)
        }
        "read_service_status" => {
            let params = parse_params::<ServiceStatusParams>(&call.arguments_json)?;
            validate_optional_filter(&params.name, "name")?;
            ContextKind::ServiceStatus(params)
        }
        "read_recent_logs" => {
            let params = parse_params::<LogRecentParams>(&call.arguments_json)?;
            validate_optional_filter(&params.source, "source")?;
            if params
                .since_minutes
                .is_some_and(|minutes| minutes == 0 || minutes > MAX_LOG_WINDOW_MINUTES)
            {
                return Err(bad_arguments(format!(
                    "since_minutes must be within 1..={MAX_LOG_WINDOW_MINUTES}"
                )));
            }
            if params
                .limit
                .is_some_and(|limit| limit == 0 || limit > MAX_LOG_EVENTS)
            {
                return Err(bad_arguments(format!(
                    "limit must be within 1..={MAX_LOG_EVENTS}"
                )));
            }
            ContextKind::LogRecent(params)
        }
        "read_container_list" => {
            ContextKind::ContainerList(parse_params::<ContainerListParams>(&call.arguments_json)?)
        }
        "read_current_screen" => ContextKind::ScreenCaptureCurrent(parse_params::<
            ScreenCaptureParams,
        >(&call.arguments_json)?),
        "inspect_desktop_session" => {
            let args = parse_params::<DesktopSessionToolArgs>(&call.arguments_json)?;
            ContextKind::DesktopSessionInspect(DesktopSessionInspectParams {
                include_active_application: args.include_active_application,
            })
        }
        "inspect_desktop_ui" => {
            let args = parse_params::<DesktopUiToolArgs>(&call.arguments_json)?;
            ContextKind::DesktopUiInspect(UiInspectParams {
                root: args.root,
                max_depth: args.max_depth,
                max_nodes: args.max_nodes,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_office_selection" => {
            let args = parse_params::<OfficeSelectionToolArgs>(&call.arguments_json)?;
            ContextKind::OfficeDocumentInspect(OfficeInspectParams {
                document: args.document,
                selection_only: args.selection_only,
                max_objects: args.max_objects,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_live_spreadsheet" => {
            let args = parse_params::<LiveDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::SpreadsheetLiveInspect(LiveDocumentInspectParams {
                target: args.target,
                batch_file: None,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_selected_numbers_with_iwork" => {
            let args = parse_params::<BatchDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::SpreadsheetLiveInspect(LiveDocumentInspectParams {
                target: None,
                batch_file: None,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_live_document" => {
            let args = parse_params::<LiveDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::DocumentLiveInspect(LiveDocumentInspectParams {
                target: args.target,
                batch_file: None,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_selected_pages_with_iwork" => {
            let args = parse_params::<BatchDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::DocumentLiveInspect(LiveDocumentInspectParams {
                target: None,
                batch_file: None,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_live_presentation" => {
            let args = parse_params::<LiveDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::PresentationLiveInspect(LiveDocumentInspectParams {
                target: args.target,
                batch_file: None,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_selected_keynote_with_iwork" => {
            let args = parse_params::<BatchDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::PresentationLiveInspect(LiveDocumentInspectParams {
                target: None,
                batch_file: None,
                max_bytes: args.max_bytes,
            })
        }
        "inspect_selected_file_metadata" => {
            let args = parse_params::<SelectedFileMetadataToolArgs>(&call.arguments_json)?;
            ContextKind::FileMetadataInspect(FileMetadataInspectParams {
                // The central orchestrator replaces this empty placeholder with
                // the exact edge-issued refs selected by the owner. The model
                // schema has no field that can nominate a path or token.
                roots: Vec::new(),
                max_entries: 256,
                max_bytes: 64 * 1024,
                enumerate_directories: false,
                file_extensions: args.file_extensions,
                min_file_bytes: args.min_file_bytes,
                max_file_bytes: args.max_file_bytes,
                modified_after: args.modified_after,
                modified_before: args.modified_before,
            })
        }
        "read_selected_text_file" => {
            let _ = parse_params::<NoToolArgs>(&call.arguments_json)?;
            ContextKind::FileContentRead(FileContentReadParams {
                // Replaced centrally with the exact owner-attached file ref.
                file: ObjectRef {
                    token: "selected:server_resolved".into(),
                    snapshot_id: "selected:server_resolved".into(),
                    object_kind: desk_agent_protocol::computer_use::ObjectKind::File,
                    expires_at: "1970-01-01T00:00:00Z".into(),
                },
                max_bytes: 64 * 1024,
            })
        }
        "inspect_selected_spreadsheets" => {
            let _ = parse_params::<NoToolArgs>(&call.arguments_json)?;
            ContextKind::SpreadsheetFileInspect(SpreadsheetFileInspectParams {
                files: Vec::new(),
                max_workbooks: 8,
                max_sheets: 16,
                max_rows: 200,
                max_columns: 64,
                max_bytes: 256 * 1024,
            })
        }
        "preview_spreadsheet_merge" => {
            #[derive(Default, serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                #[serde(default)]
                source_sheet: Option<String>,
                #[serde(default = "default_header_row")]
                header_row: u32,
                columns: Vec<SpreadsheetMergeColumnRule>,
                #[serde(default)]
                dedupe_keys: Vec<String>,
                #[serde(default)]
                statistics: Vec<SpreadsheetStatisticRequest>,
            }
            fn default_header_row() -> u32 {
                1
            }
            let args = parse_params::<Args>(&call.arguments_json)?;
            ContextKind::SpreadsheetMergePreview(SpreadsheetMergePreviewParams {
                files: Vec::new(),
                source_sheet: args.source_sheet,
                header_row: args.header_row,
                columns: args.columns,
                dedupe_keys: args.dedupe_keys,
                statistics: args.statistics,
                max_rows: 1000,
                max_bytes: 512 * 1024,
            })
        }
        "inspect_selected_terminal_output" => {
            let _ = parse_params::<NoToolArgs>(&call.arguments_json)?;
            ContextKind::TerminalOutputInspect(TerminalOutputInspectParams {
                // Replaced by the central orchestrator with exact user-attached
                // edge refs; model arguments cannot nominate a terminal.
                roots: Vec::new(),
                max_bytes: 32 * 1024,
            })
        }
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

    #[test]
    fn device_assistant_registry_is_isolated_and_maps_to_computer_use_reads() {
        let diagnostic_names: Vec<_> = read_tool_registry()
            .into_iter()
            .map(|tool| tool.spec.name)
            .collect();
        assert!(!diagnostic_names.contains(&"inspect_desktop_session".to_string()));
        assert!(!diagnostic_names.contains(&"inspect_desktop_ui".to_string()));
        assert!(!diagnostic_names.contains(&"inspect_office_selection".to_string()));

        let tools = device_assistant_read_tool_registry();
        assert_eq!(tools.len(), 9);
        for tool in tools {
            assert_eq!(tool.effect, ToolEffect::ReadOnly);
            let arguments_json = if tool.name() == "preview_spreadsheet_merge" {
                r#"{"columns":[{"output_header":"Region","source_headers":["Region"]}]}"#
            } else {
                "{}"
            };
            let (cap, _) = build_read_operation(&ToolCall {
                id: "assistant-call".into(),
                name: tool.name().into(),
                arguments_json: arguments_json.into(),
            })
            .unwrap();
            assert_eq!(cap, tool.required_capability);
            assert!(matches!(
                cap,
                Capability::DesktopSessionInspect
                    | Capability::DesktopUiInspect
                    | Capability::OfficeDocumentInspect
                    | Capability::FileMetadataRead
                    | Capability::FileContentRead
                    | Capability::SpreadsheetFileInspect
                    | Capability::SpreadsheetMergePreview
                    | Capability::TerminalOutputRead
                    | Capability::ScreenCaptureCurrent
            ));
        }

        assert!(
            build_read_operation(&ToolCall {
                id: "assistant-call".into(),
                name: "inspect_selected_file_metadata".into(),
                arguments_json: r#"{"path":"C:\\\\secret.txt"}"#.into(),
            })
            .is_err(),
            "the model-facing file tool must not accept paths or object references",
        );
        let (_, filtered_input) = build_read_operation(&ToolCall {
            id: "assistant-filter-call".into(),
            name: "inspect_selected_file_metadata".into(),
            arguments_json: r#"{"file_extensions":[".CSV"],"min_file_bytes":4,"max_file_bytes":16,"modified_after":"2026-08-25T00:00:00Z","modified_before":"2026-08-27T00:00:00Z"}"#.into(),
        })
        .unwrap();
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::FileMetadataInspect(filtered),
        }) = filtered_input
        else {
            panic!("file metadata tool must map to its typed read context")
        };
        assert!(filtered.roots.is_empty());
        assert_eq!(filtered.file_extensions, vec![".CSV"]);
        assert_eq!(filtered.min_file_bytes, Some(4));
        assert_eq!(filtered.max_file_bytes, Some(16));
        assert_eq!(
            filtered.modified_after.as_deref(),
            Some("2026-08-25T00:00:00Z")
        );
        assert_eq!(
            filtered.modified_before.as_deref(),
            Some("2026-08-27T00:00:00Z")
        );
        assert!(
            build_read_operation(&ToolCall {
                id: "assistant-terminal-call".into(),
                name: "inspect_selected_terminal_output".into(),
                arguments_json: r#"{"terminal_id":"other-terminal"}"#.into(),
            })
            .is_err(),
            "the model-facing terminal tool must not accept terminal identifiers or references",
        );
    }

    #[test]
    fn batch_iwork_inspection_has_only_a_server_injected_source() {
        for (name, expected) in [
            (
                "inspect_selected_numbers_with_iwork",
                Capability::SpreadsheetLiveInspect,
            ),
            (
                "inspect_selected_pages_with_iwork",
                Capability::DocumentLiveInspect,
            ),
            (
                "inspect_selected_keynote_with_iwork",
                Capability::PresentationLiveInspect,
            ),
        ] {
            let (capability, input) = build_read_operation(&ToolCall {
                id: format!("call-{name}"),
                name: name.into(),
                arguments_json: "{}".into(),
            })
            .unwrap();
            assert_eq!(capability, expected);
            let OperationInput::ReadContext(ReadContextInput { kind }) = input else {
                panic!("BatchDocument inspection must map to a read context")
            };
            let params = match kind {
                ContextKind::SpreadsheetLiveInspect(params)
                | ContextKind::DocumentLiveInspect(params)
                | ContextKind::PresentationLiveInspect(params) => params,
                _ => panic!("unexpected BatchDocument read context"),
            };
            assert!(params.target.is_none());
            assert!(params.batch_file.is_none());

            assert!(
                build_read_operation(&ToolCall {
                    id: format!("bad-{name}"),
                    name: name.into(),
                    arguments_json: r#"{"path":"/tmp/secret","batch_file":{}}"#.into(),
                })
                .is_err()
            );
        }
    }
}
