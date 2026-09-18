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
            "Search conditions are required unless allow_unfiltered=true explicitly opts into bounded enumeration. Find running processes with queries (OR, case-insensitive name substrings). Return matching query indices; filter before limit. Default to at most 20 names/PIDs for discovery. Request include_details=true only for CPU/memory/owner diagnostics.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 0, "maximum": MAX_PROCESS_LIST_ENTRIES},
                    "sort": {"type": "string", "enum": ["cpu_desc", "memory_desc", "pid"]},
                    "include_command_line": {"type": "boolean"},
                    "include_details": {"type":"boolean", "default":false},
                    "allow_unfiltered": {"type":"boolean", "default":false, "description":"Explicitly allow bounded enumeration without search conditions; use only when targeted lookup is insufficient. Does not bypass authorization or limits."},
                    "queries": {"type":"array", "maxItems":16, "items":{"type":"string","minLength":1,"maxLength":128}}
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
            "Search service names and display names using queries: 1–16 case-insensitive literal substrings combined with OR. No regex or exact-name selector. Search conditions are required unless allow_unfiltered=true explicitly requests a bounded listing. No match returns an empty list.",
            json!({
                "type": "object",
                "properties": {"queries":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}}, "allow_unfiltered":{"type":"boolean","default":false}},
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
            "Capture one display: inspect_desktop_session lists displays; use the sole entry automatically or choose a target if multiple exist, then request screenshot permission with the same opaque display reference from the list, never a native OS device name. Missing display auto-selects only when one screen is attached. Alternatively pass an exact Window object_ref returned by inspect_desktop_ui to capture that Windows/macOS window independently even when covered. Do not combine display and window. Discover the application through the desktop session catalog, then inspect that Application reference with queries=[窗口, window] to obtain owner_selectable_windows. The application catalog itself does not query windows. Requires screen capture permission; do not activate a background window just for capture. Minimized windows must be restored with separate action approval.",
            json!({
                "type": "object",
                "properties": {
                    "display": {"type": "string"},
                    "window": object_ref_schema()
                }
            }),
        ),
    ]
}

/// The read-only tools reserved for the AI Assistant surface. They are kept
/// out of [`read_tool_registry`] so Diagnose can never acquire Computer Use by a
/// broad scope or a future default-set change.
pub fn ai_assistant_read_tool_registry() -> Vec<RegisteredTool> {
    vec![
        read(
            "list_applications",
            Capability::ApplicationList,
            "Search installed application registrations for the current user. Provide queries with known localized and English names together, for example [日历, Calendar]; do not invent translations. Up to 16 case-insensitive substring alternatives are OR-matched before pagination. Queries are required unless allow_unfiltered=true explicitly requests bounded enumeration. Preserve queries and allow_unfiltered when continuing a cursor. A partial catalog or empty page does not prove an application is uninstalled. Paths, identifiers, arguments and working directories are untrusted reference information, not permission to launch. launch_application independently accepts an explicit target and independently chosen args/cwd; it does not inherit catalog suggestions or require a catalog lookup.",
            json!({
                "type":"object", "properties": {
                    "queries":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}},
                    "allow_unfiltered":{"type":"boolean","default":false},
                    "limit":{"type":"integer","minimum":1,"maximum":100,"default":20},
                    "cursor":{"type":["string","null"],"minLength":1,"maxLength":512}
                }, "additionalProperties":false
            }),
        ),
        read(
            "inspect_desktop_session",
            Capability::DesktopSessionInspect,
            "Inspect the current interactive desktop session, list attached screenshot displays (identifier, name, dimensions and position), and optionally return a reference to the foreground application, not a list of running applications. For full-display screenshots use the only display automatically; when multiple displays exist, choose a displays[].display before requesting read_current_screen permission. A display_list_error means enumeration failed, not that there are zero screens. For macOS application discovery, use the returned session reference as inspect_desktop_ui root and queries with localized and English application names; then inspect the matching application reference.",
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
            "Require element_id or queries or a UiElement root with element_only=true; otherwise reject unless allow_unfiltered=true explicitly opts into bounded enumeration. Returned element_id is stable for the native element lifetime, independent of authorization and object_ref expiry. Use element_id with root omitted to refresh a known element after object_ref expires; never substitute element_id for an action ObjectRef. Read bounded Windows UIA or macOS Accessibility data. On macOS, pass the DesktopSession reference from inspect_desktop_session as root to list GUI applications (application nodes with selectable references); then pass one Application reference to read that app, including in the background, or a Window reference to read that window. A null root reads the foreground app. scope=content (default) reads ordinary UI without menus; scope=menus returns only menu subtrees, useful after the window was already read; scope=all reads both. Choose an Application or null root for the application menu bar; a Window root searches only that window. Pass an existing UI element reference as root with element_only=true to refresh only its current value; otherwise read its subtree. queries searches name/native_id/role substrings and bilingual control-type aliases (OR, case-insensitive): 日期/date, 时间/time, 输入框/input, 按钮/button, 弹层/popover and 对话框/dialog. First locate the target window and relevant dialog/popover/editor when available, then query within that root using task-specific localized/English labels or observed native_id. Group needed controls in one query. If no separate container exists, use the window. Only after targeted misses add control types; broad text/date/time terms can match unrelated static calendar text. No match does not prove an operation is unsupported; queries fuzzy-match native_id, role and name within the selected root and return matching nodes only (OR across terms). If traversal is incomplete, narrow the root when possible and increase depth/node/byte bounds as needed; do not reduce depth to hide matches. Refresh known controls with element_id and element_only=true. Application catalog nodes expose application_state=foreground/background/hidden when available (hidden takes priority); missing state means unknown. This is application activation/visibility, not window minimization or screen visibility. Application catalog nodes omit matched_queries and do not contain UI contents or authorize actions. The catalog does not query windows. Use its returned Application reference as root with queries=[窗口, window] to obtain owner_selectable_windows for application screenshots; missing window entries in the catalog do not establish that capture is unavailable. On macOS, search the session application catalog before searching an app UI. If the application is found, use its Application reference as root. If a complete application-name search is empty, increasing max_depth or searching button/date labels cannot find a non-running app: use the independently authorized launch_application tool with an explicit installed target, then refresh the running application catalog. If the launch tool is unavailable, report that limitation; do not fall back to a shell command. Empty control searches within an existing app do not mean the app is absent. Protected field values are never returned.",
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
                    "allow_unfiltered": {"type":"boolean", "default":false, "description":"Explicitly allow bounded enumeration without search conditions; use only when targeted lookup is insufficient. Does not bypass authorization or limits."},
                    "overview": {"type":"boolean", "default":true},
                    "element_only": {"type":"boolean", "default":false},
                    "queries":{"type":"array","maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}},
                    "element_id":{"type":"string","minLength":1,"maxLength":512},
                    "scope": {"type": "string", "enum": ["content", "menus", "all"], "default": "content"},
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": 12, "default": 12},
                    "max_nodes": {"type": "integer", "minimum": 1, "maximum": 4096, "default": 300},
                    "max_bytes": {"type": "integer", "minimum": 1024, "maximum": 32768, "default": 32768}
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
                    "max_bytes": {"type": "integer", "minimum": 1024, "maximum": 32768, "default": 32768}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_files",
            Capability::FileMetadataRead,
            "Read bounded metadata by supplying directory_request_id for an approved conversation directory and request separate metadata permission with these exact arguments. Lists immediate children only, without following links or reading contents. A returned regular-file reference can be selected by read_text_file using this result call id and exact entry_name; content reading still requires a separate grant. Reduce max_entries or use the supported file filters to narrow large results; max_bytes is at most 32768 and does not expand your permission.",
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
                    "directory_request_id": {"type":"string", "minLength":1, "maxLength":256},
                    "max_entries": {"type":"integer", "minimum":1, "maximum":256, "default":256},
                    "max_bytes": {"type":"integer", "minimum":1024, "maximum":32768, "default":32768},
                    "min_file_bytes": {"type": ["integer", "null"], "minimum": 0},
                    "max_file_bytes": {"type": ["integer", "null"], "minimum": 0},
                    "modified_after": {"type": ["string", "null"], "format": "date-time"},
                    "modified_before": {"type": ["string", "null"], "format": "date-time"}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "read_text_file",
            Capability::FileContentRead,
            "Read one file from a verified result in this conversation as bounded UTF-8. For a creation/read/update result, provide only file_result_call_id (omit entry_name). For a child from inspect_files, provide file_result_call_id plus entry_name. Request read_text_file permission with these exact arguments; metadata permission is insufficient. Do not read just to prepare an update when verified creation/update content and SHA-256 are already known. A creation grant never authorizes reading or model egress. Never provide a path or object reference.",
            json!({
                "type": "object",
                "properties": {"file_result_call_id": {"type":"string", "minLength":1, "maxLength":256}, "entry_name": {"type":"string", "minLength":1, "maxLength":512, "description":"Exact immediate regular-file name from the identified metadata result; omit for a creation/read/update result."}},
                "additionalProperties": false
            }),
        ),
        read(
            "inspect_spreadsheets",
            Capability::SpreadsheetFileInspect,
            "Read bounded cell, formula, and value projections from inert .xlsx, .csv, or .tsv files discovered in approved conversation directories. Select 1–8 exact files using file_sources; macros, external links, data connections, and model-provided paths are rejected. Reduce max_workbooks, max_sheets, max_rows, max_columns or max_bytes to narrow an oversized result. Returned truncated projections do not contain the omitted cells.",
            json!({
                "type": "object",
                "properties": {
                    "max_workbooks": {"type":"integer", "minimum":1, "maximum":8, "default":8},
                    "max_sheets": {"type":"integer", "minimum":1, "maximum":16, "default":16},
                    "max_rows": {"type":"integer", "minimum":1, "maximum":200, "default":200},
                    "max_columns": {"type":"integer", "minimum":1, "maximum":64, "default":64},
                    "max_bytes": {"type":"integer", "minimum":1024, "maximum":32768, "default":32768}
                },
                "additionalProperties": false
            }),
        ),
        read(
            "preview_spreadsheet_merge",
            Capability::SpreadsheetMergePreview,
            "Preview a bounded multi-workbook merge, dedupe, and statistics operation over inert spreadsheets selected from recorded directory results with file_sources. Rules are typed data only; no script or formula is executed and no file is written. Narrow source_sheet, columns, statistics or max_rows to reduce output; max_bytes is at most 32768. A truncated merge preview cannot be materialized as a complete workbook or report.",
            json!({
                "type": "object",
                "properties": {
                    "source_sheet": {"type": ["string", "null"], "maxLength": 128},
                    "max_rows": {"type":"integer", "minimum":1, "maximum":1000, "default":1000},
                    "max_bytes": {"type":"integer", "minimum":1024, "maximum":32768, "default":32768},
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
            "read_terminal_output",
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
            "Capture one display once: first inspect_desktop_session for displays, automatically use the only entry or choose a target if multiple exist, then request capture permission with its exact opaque display reference, never a native OS device name. Missing display only works with a single attached screen. Alternatively pass an exact Window object_ref from inspect_desktop_ui for independent Windows/macOS window capture. Never combine display and window. Requires capture permission. A minimized window must first be restored with separate action approval. The image is sensitive, sent only to the selected visual model, and not stored in conversation history.",
            json!({
                "type": "object",
                "properties": {
                    "display": {"type": "string"},
                    "window": object_ref_schema()
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentScreenToolArgs {
    #[serde(default)]
    window: Option<ObjectRef>,
    #[serde(default)]
    display: Option<String>,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopUiToolArgs {
    #[serde(default)]
    allow_unfiltered: bool,
    #[serde(default = "default_true")]
    overview: bool,
    #[serde(default)]
    queries: Vec<String>,
    #[serde(default)]
    element_id: Option<String>,
    #[serde(default)]
    element_only: bool,
    #[serde(default)]
    scope: desk_agent_protocol::computer_use::UiInspectScope,
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
    max_entries: Option<u32>,
    #[serde(default)]
    max_bytes: Option<u32>,
    #[serde(default)]
    directory_request_id: Option<String>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedSpreadsheetToolArgs {
    max_workbooks: Option<u32>,
    max_sheets: Option<u32>,
    max_rows: Option<u32>,
    max_columns: Option<u32>,
    max_bytes: Option<u32>,
}

fn result_bound(
    value: Option<u32>,
    default: u32,
    min: u32,
    max: u32,
    name: &str,
) -> Result<u32, AgentError> {
    let value = value.unwrap_or(default);
    if !(min..=max).contains(&value) {
        return Err(bad_arguments(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn json_result_budget(value: Option<u32>) -> Result<u32, AgentError> {
    let maximum = crate::conversation_attachment::MAX_JSON_BYTES as u32;
    result_bound(value, maximum, 1024, maximum, "max_bytes")
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
    12
}

const fn default_ui_nodes() -> u32 {
    300
}

const fn default_ui_bytes() -> u32 {
    32_768
}

const fn default_office_objects() -> u32 {
    16
}

const fn default_office_bytes() -> u32 {
    32_768
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
    let normalized;
    let call = if crate::provider_preflight::text_file::selection::supports(&call.name) {
        normalized = crate::provider_preflight::text_file::selection::without_selectors(call)?;
        &normalized
    } else {
        call
    };
    let kind = match call.name.as_str() {
        "read_system_info" => {
            ContextKind::SystemInfo(parse_params::<SystemInfoParams>(&call.arguments_json)?)
        }
        "read_process_list" => {
            let params = parse_params::<ProcessListParams>(&call.arguments_json)?;
            params.validate_selection().map_err(bad_arguments)?;
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
            let params = parse_params::<ServiceStatusParams>(&call.arguments_json).map_err(|error| bad_arguments(format!("{}. Required format: {{\"queries\":[\"service name\",\"another name\"]}}; the old name selector is not supported. No services were read.", error.message)))?;
            params.validate_selection().map_err(bad_arguments)?;
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
        "read_current_screen" => {
            let args = parse_params::<CurrentScreenToolArgs>(&call.arguments_json)?;
            ContextKind::ScreenCaptureCurrent(ScreenCaptureParams {
                display: args.display,
                window: args.window,
            })
        }
        "inspect_desktop_session" => {
            let args = parse_params::<DesktopSessionToolArgs>(&call.arguments_json)?;
            ContextKind::DesktopSessionInspect(DesktopSessionInspectParams {
                include_active_application: args.include_active_application,
            })
        }
        "list_applications" => {
            let mut params = serde_json::from_str::<
                desk_agent_protocol::application_launch::ListApplicationsRequest,
            >(&call.arguments_json)
            .map_err(bad_arguments)?;
            params.normalize().map_err(bad_arguments)?;
            ContextKind::ApplicationList(params)
        }
        "inspect_desktop_ui" => {
            let args = parse_params::<DesktopUiToolArgs>(&call.arguments_json).map_err(|error| bad_arguments(format!("{}. Required format: {{\"queries\":[\"localized name\",\"English name\"],\"root_id\":\"<observed root>\"}}. Use top-level element_id only to locate a known control. query/name/native_id/role exact-search parameters are not supported; pass their text in queries. No UI was read.", error.message)))?;
            let params = UiInspectParams {
                allow_unfiltered: args.allow_unfiltered,
                overview: args.overview,
                query: if args.queries.is_empty() && args.element_id.is_none() {
                    None
                } else {
                    Some(desk_agent_protocol::computer_use::UiInspectQuery {
                        queries: args.queries,
                        element_id: args.element_id,
                    })
                },
                element_only: args.element_only,
                scope: args.scope,
                root: args.root,
                max_depth: args.max_depth,
                max_nodes: args.max_nodes,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            };
            params.validate_selection().map_err(bad_arguments)?;
            ContextKind::DesktopUiInspect(params)
        }
        "inspect_office_selection" => {
            let args = parse_params::<OfficeSelectionToolArgs>(&call.arguments_json)?;
            ContextKind::OfficeDocumentInspect(OfficeInspectParams {
                document: args.document,
                selection_only: args.selection_only,
                max_objects: args.max_objects,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_live_spreadsheet" => {
            let args = parse_params::<LiveDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::SpreadsheetLiveInspect(LiveDocumentInspectParams {
                target: args.target,
                batch_file: None,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_numbers_file" => {
            let args = parse_params::<BatchDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::SpreadsheetLiveInspect(LiveDocumentInspectParams {
                target: None,
                batch_file: None,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_excel_cell" => {
            let args = serde_json::from_str::<crate::ai_assistant::windows_excel::InspectArgs>(
                &call.arguments_json,
            )
            .map_err(bad_arguments)?;
            let params = desk_agent_protocol::computer_use::SpreadsheetBatchInspectParams {
                file: None,
                sheet_name: args.sheet_name,
                address: args.address,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            };
            params.validate_selection().map_err(bad_arguments)?;
            ContextKind::SpreadsheetBatchInspect(params)
        }
        "inspect_live_document" => {
            let args = parse_params::<LiveDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::DocumentLiveInspect(LiveDocumentInspectParams {
                target: args.target,
                batch_file: None,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_pages_file" | "inspect_word_file" => {
            let args = parse_params::<BatchDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::DocumentLiveInspect(LiveDocumentInspectParams {
                target: None,
                batch_file: None,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_live_presentation" => {
            let args = parse_params::<LiveDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::PresentationLiveInspect(LiveDocumentInspectParams {
                target: args.target,
                batch_file: None,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_keynote_file" | "inspect_powerpoint_file" => {
            let args = parse_params::<BatchDocumentToolArgs>(&call.arguments_json)?;
            ContextKind::PresentationLiveInspect(LiveDocumentInspectParams {
                target: None,
                batch_file: None,
                max_bytes: json_result_budget(Some(args.max_bytes))?,
            })
        }
        "inspect_files" => {
            let args = parse_params::<SelectedFileMetadataToolArgs>(&call.arguments_json)?;
            if args.directory_request_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
            }) {
                return Err(bad_arguments("invalid conversation directory selector"));
            }
            ContextKind::FileMetadataInspect(FileMetadataInspectParams {
                // The central orchestrator replaces this empty placeholder with
                // the exact edge-issued refs selected by the owner. The model
                // schema has no field that can nominate a path or token.
                roots: Vec::new(),
                max_entries: result_bound(args.max_entries, 256, 1, 256, "max_entries")?,
                max_bytes: json_result_budget(args.max_bytes)?,
                enumerate_directories: false,
                file_extensions: args.file_extensions,
                min_file_bytes: args.min_file_bytes,
                max_file_bytes: args.max_file_bytes,
                modified_after: args.modified_after,
                modified_before: args.modified_before,
            })
        }
        "read_text_file" => {
            crate::provider_preflight::text_file::read_result_id(call)?;
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
        "inspect_spreadsheets" => {
            let args = parse_params::<SelectedSpreadsheetToolArgs>(&call.arguments_json)?;
            ContextKind::SpreadsheetFileInspect(SpreadsheetFileInspectParams {
                files: Vec::new(),
                max_workbooks: result_bound(args.max_workbooks, 8, 1, 8, "max_workbooks")?,
                max_sheets: result_bound(args.max_sheets, 16, 1, 16, "max_sheets")?,
                max_rows: result_bound(args.max_rows, 200, 1, 200, "max_rows")?,
                max_columns: result_bound(args.max_columns, 64, 1, 64, "max_columns")?,
                max_bytes: json_result_budget(args.max_bytes)?,
            })
        }
        "preview_spreadsheet_merge" => {
            #[derive(Default, serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                max_rows: Option<u32>,
                max_bytes: Option<u32>,
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
                max_rows: result_bound(args.max_rows, 1000, 1, 1000, "max_rows")?,
                max_bytes: json_result_budget(args.max_bytes)?,
            })
        }
        "read_terminal_output" => {
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

    #[test]
    fn source_result_limits_are_model_adjustable_and_validated() {
        let make = |name: &str, arguments: &str| ToolCall {
            id: "bounded-read".into(),
            name: name.into(),
            arguments_json: arguments.into(),
        };
        let (_, input) = build_read_operation(&make(
            "inspect_spreadsheets",
            r#"{"max_workbooks":1,"max_sheets":2,"max_rows":3,"max_columns":4,"max_bytes":8192}"#,
        ))
        .unwrap();
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SpreadsheetFileInspect(params),
        }) = input
        else {
            panic!("wrong read operation");
        };
        assert_eq!(
            (
                params.max_workbooks,
                params.max_sheets,
                params.max_rows,
                params.max_columns,
                params.max_bytes
            ),
            (1, 2, 3, 4, 8192)
        );
        let (_, input) = build_read_operation(&make(
            "inspect_files",
            r#"{"max_entries":2,"max_bytes":4096}"#,
        ))
        .unwrap();
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::FileMetadataInspect(params),
        }) = input
        else {
            panic!("wrong read operation");
        };
        assert_eq!((params.max_entries, params.max_bytes), (2, 4096));
        let (_, input) = build_read_operation(&make("preview_spreadsheet_merge",
            r#"{"columns":[{"output_header":"name","source_headers":["name"]}],"max_rows":3,"max_bytes":8192}"#)).unwrap();
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SpreadsheetMergePreview(params),
        }) = input
        else {
            panic!("wrong read operation");
        };
        assert_eq!((params.max_rows, params.max_bytes), (3, 8192));
        for tool in ["inspect_files", "inspect_spreadsheets"] {
            for invalid in [
                r#"{"max_bytes":0}"#,
                r#"{"max_bytes":32769}"#,
                r#"{"path":"C:/private"}"#,
            ] {
                assert!(
                    build_read_operation(&make(tool, invalid)).is_err(),
                    "{tool}: {invalid}"
                );
            }
        }
        assert!(build_read_operation(&make("inspect_spreadsheets", r#"{"max_rows":0}"#)).is_err());
        assert!(build_read_operation(&make("inspect_files", r#"{"max_entries":257}"#)).is_err());
    }

    /// `build_read_operation` maps each known tool name to the right capability
    /// and accepts both empty and populated arguments.
    #[test]
    fn unified_search_schema_and_parser_reject_legacy_exact_selectors() {
        for name in [
            "inspect_desktop_ui",
            "read_service_status",
            "read_process_list",
        ] {
            let call = ToolCall {
                id: "query".into(),
                name: name.into(),
                arguments_json: r#"{"queries":["calendar","CALC"]}"#.into(),
            };
            assert!(build_read_operation(&call).is_ok());
            for invalid in [
                r#"{"query":{"any":["calendar"]}}"#,
                r#"{"query":{"name":"calendar"}}"#,
                r#"{"name":"calendar"}"#,
                r#"{"queries":[""]}"#,
            ] {
                assert!(
                    build_read_operation(&ToolCall {
                        arguments_json: invalid.into(),
                        ..call.clone()
                    })
                    .is_err(),
                    "{name}: {invalid}"
                );
            }
        }
    }

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
            arguments_json: r#"{"allow_unfiltered":true,"limit": 5, "sort": "memory_desc"}"#.into(),
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
            &input,
            OperationInput::ReadContext(ReadContextInput {
                kind: ContextKind::ScreenCaptureCurrent(_)
            })
        ));
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ScreenCaptureCurrent(params),
        }) = input
        else {
            unreachable!()
        };
        assert!(params.window.is_none());
        assert!(
            build_read_operation(&ToolCall {
                id: "c".into(),
                name: "read_current_screen".into(),
                arguments_json: r#"{"window":{"token":"model-authored"}}"#.into(),
            })
            .is_err(),
            "the model cannot nominate a window reference",
        );

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
                arguments_json: if matches!(
                    tool.name(),
                    "read_process_list" | "inspect_desktop_ui" | "read_service_status"
                ) {
                    r#"{"allow_unfiltered":true}"#.into()
                } else {
                    String::new()
                },
            })
            .unwrap();
            assert_eq!(cap, tool.required_capability);
        }
    }

    #[test]
    fn ai_assistant_registry_is_isolated_and_maps_to_computer_use_reads() {
        let diagnostic_names: Vec<_> = read_tool_registry()
            .into_iter()
            .map(|tool| tool.spec.name)
            .collect();
        assert!(!diagnostic_names.contains(&"inspect_desktop_session".to_string()));
        assert!(!diagnostic_names.contains(&"inspect_desktop_ui".to_string()));
        assert!(!diagnostic_names.contains(&"inspect_office_selection".to_string()));

        let tools = ai_assistant_read_tool_registry();
        assert_eq!(tools.len(), 10);
        for tool in tools {
            assert_eq!(tool.effect, ToolEffect::ReadOnly);
            let arguments_json = if tool.name() == "preview_spreadsheet_merge" {
                r#"{"columns":[{"output_header":"Region","source_headers":["Region"]}]}"#
            } else if matches!(tool.name(), "inspect_desktop_ui" | "list_applications") {
                r#"{"allow_unfiltered":true}"#
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
                Capability::ApplicationList
                    | Capability::DesktopSessionInspect
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
                name: "inspect_files".into(),
                arguments_json: r#"{"path":"C:\\\\secret.txt"}"#.into(),
            })
            .is_err(),
            "the model-facing file tool must not accept paths or object references",
        );
        let (_, filtered_input) = build_read_operation(&ToolCall {
            id: "assistant-filter-call".into(),
            name: "inspect_files".into(),
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
                name: "read_terminal_output".into(),
                arguments_json: r#"{"terminal_id":"other-terminal"}"#.into(),
            })
            .is_err(),
            "the model-facing terminal tool must not accept terminal identifiers or references",
        );
    }

    #[test]
    fn batch_iwork_inspection_has_only_a_server_injected_source() {
        for (name, expected) in [
            ("inspect_numbers_file", Capability::SpreadsheetLiveInspect),
            ("inspect_pages_file", Capability::DocumentLiveInspect),
            ("inspect_keynote_file", Capability::PresentationLiveInspect),
            (
                "inspect_powerpoint_file",
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

    #[test]
    fn excel_cell_inspection_requires_explicit_selection_and_server_bound_source() {
        let call = |arguments: &str| ToolCall {
            id: "excel-cell".into(),
            name: "inspect_excel_cell".into(),
            arguments_json: arguments.into(),
        };
        for arguments in [
            "",
            "{}",
            r#"{"sheet_name":"Sheet1","address":"A1"}"#,
            r#"{"sheet_name":"Sheet1","address":"A1","max_bytes":1024,"path":"C:\\private.xlsx"}"#,
        ] {
            assert!(build_read_operation(&call(arguments)).is_err());
        }
        let (_, input) = build_read_operation(&call(
            r#"{"sheet_name":"Sheet1","address":"A1","max_bytes":1024}"#,
        ))
        .unwrap();
        let OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::SpreadsheetBatchInspect(params),
        }) = input
        else {
            panic!("expected file batch read")
        };
        assert!(params.file.is_none());
        assert_eq!(params.sheet_name, "Sheet1");
        assert_eq!(params.address, "A1");
    }
}

#[cfg(test)]
mod menu_scope_tests {
    use super::*;
    use desk_agent_protocol::computer_use::UiInspectScope;

    #[test]
    fn precise_ui_and_window_capture_arguments_preserve_exact_references() {
        let reference = json!({"token":"opaque", "snapshot_id":"snapshot", "object_kind":"window", "expires_at":"2030-01-01T00:00:00Z"});
        let call = ToolCall {
            id: "capture".into(),
            name: "read_current_screen".into(),
            arguments_json: json!({"window":reference}).to_string(),
        };
        let (_, OperationInput::ReadContext(input)) = build_read_operation(&call).unwrap() else {
            panic!()
        };
        let ContextKind::ScreenCaptureCurrent(params) = input.kind else {
            panic!()
        };
        assert_eq!(
            serde_json::to_value(params.window.unwrap()).unwrap(),
            reference
        );
        let call = ToolCall {
            id: "find".into(),
            name: "inspect_desktop_ui".into(),
            arguments_json:
                json!({"root":reference,"queries":["result","AXStaticText"],"element_only":true})
                    .to_string(),
        };
        let (_, OperationInput::ReadContext(input)) = build_read_operation(&call).unwrap() else {
            panic!()
        };
        let ContextKind::DesktopUiInspect(params) = input.kind else {
            panic!()
        };
        assert_eq!(params.max_depth, 12);
        assert!(params.element_only);
        assert_eq!(
            params.query.unwrap().queries,
            vec!["result", "AXStaticText"]
        );
        let call = ToolCall {
            arguments_json: r#"{"id":"typo"}"#.into(),
            ..call
        };
        assert!(build_read_operation(&call).is_err());
    }

    #[test]
    fn batch_search_and_overview_arguments_reach_the_native_contract() {
        let call = ToolCall {
            id: "search".into(),
            name: "inspect_desktop_ui".into(),
            arguments_json: r#"{"queries":["Calendar","日历"]}"#.into(),
        };
        let (_, OperationInput::ReadContext(input)) = build_read_operation(&call).unwrap() else {
            panic!("read");
        };
        let ContextKind::DesktopUiInspect(params) = input.kind else {
            panic!("UI");
        };
        assert!(params.overview);
        assert_eq!(params.query.unwrap().queries, vec!["Calendar", "日历"]);
        let call = ToolCall {
            id: "search".into(),
            name: "read_process_list".into(),
            arguments_json: r#"{"queries":["Calendar","Calculator"],"limit":8}"#.into(),
        };
        let (_, OperationInput::ReadContext(input)) = build_read_operation(&call).unwrap() else {
            panic!("read");
        };
        let ContextKind::ProcessList(params) = input.kind else {
            panic!("process");
        };
        assert_eq!(params.queries.len(), 2);
        assert!(!params.include_details);
        let call = ToolCall {
            arguments_json: r#"{"queries":[" "]}"#.into(),
            ..call
        };
        assert!(build_read_operation(&call).is_err());
    }

    #[test]
    fn ui_scope_defaults_to_content_and_supports_menus_without_other_ui() {
        for (args, expected) in [
            (r#"{"allow_unfiltered":true}"#, UiInspectScope::Content),
            (
                r#"{"allow_unfiltered":true,"scope":"menus"}"#,
                UiInspectScope::Menus,
            ),
            (
                r#"{"allow_unfiltered":true,"scope":"all"}"#,
                UiInspectScope::All,
            ),
        ] {
            let call = ToolCall {
                id: "scope-test".into(),
                name: "inspect_desktop_ui".into(),
                arguments_json: args.into(),
            };
            let (_, OperationInput::ReadContext(input)) = build_read_operation(&call).unwrap()
            else {
                panic!("read input")
            };
            let ContextKind::DesktopUiInspect(params) = input.kind else {
                panic!("UI input")
            };
            assert_eq!(params.scope, expected);
            let wire = serde_json::to_string(&params).unwrap();
            assert_eq!(
                serde_json::from_str::<UiInspectParams>(&wire)
                    .unwrap()
                    .scope,
                expected
            );
        }
        let call = ToolCall {
            id: "bad".into(),
            name: "inspect_desktop_ui".into(),
            arguments_json: r#"{"scope":"unknown"}"#.into(),
        };
        assert!(build_read_operation(&call).is_err());
    }
}

fn object_ref_schema() -> serde_json::Value {
    json!({"type":"object","additionalProperties":false,"required":["token","snapshot_id","object_kind","expires_at"],"properties":{
        "token":{"type":"string"},"snapshot_id":{"type":"string"},"object_kind":{"type":"string","enum":["window"]},"expires_at":{"type":"string"}
    }})
}

#[cfg(test)]
mod required_search_tests {
    use super::*;
    #[test]
    fn tool_arguments_require_filters_before_enumeration() {
        for name in ["read_process_list", "inspect_desktop_ui"] {
            for args in ["{}", r#"{"allow_unfiltered":false}"#] {
                let err = build_read_operation(&ToolCall {
                    id: "search".into(),
                    name: name.into(),
                    arguments_json: args.into(),
                })
                .unwrap_err();
                assert!(format!("{err:?}").contains("allow_unfiltered"));
            }
            assert!(
                build_read_operation(&ToolCall {
                    id: "search".into(),
                    name: name.into(),
                    arguments_json: r#"{"allow_unfiltered":true}"#.into()
                })
                .is_ok()
            );
        }
        for (name, args) in [
            (
                "read_process_list",
                r#"{"queries":["Calendar","Calculator"]}"#,
            ),
            ("inspect_desktop_ui", r#"{"queries":["结果","result"]}"#),
            ("inspect_desktop_ui", r#"{"queries":["result"]}"#),
            ("inspect_desktop_ui", r#"{"element_id":"ui-known"}"#),
        ] {
            assert!(
                build_read_operation(&ToolCall {
                    id: "search".into(),
                    name: name.into(),
                    arguments_json: args.into()
                })
                .is_ok()
            );
        }
        for (name, args) in [
            ("read_process_list", r#"{"limit":1}"#),
            (
                "read_process_list",
                r#"{"queries":[" "],"allow_unfiltered":true}"#,
            ),
            (
                "inspect_desktop_ui",
                r#"{"queries":[],"scope":"menus","overview":true}"#,
            ),
            (
                "inspect_desktop_ui",
                r#"{"queries":[" "],"allow_unfiltered":true}"#,
            ),
        ] {
            assert!(
                build_read_operation(&ToolCall {
                    id: "search".into(),
                    name: name.into(),
                    arguments_json: args.into()
                })
                .is_err()
            );
        }
    }
}
