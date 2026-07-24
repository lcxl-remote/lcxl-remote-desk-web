//! Single-machine, **read-only** MCP server exposing the diagnose agent's read
//! capabilities to a local MCP client (e.g. an external AI assistant).
//!
//! This crate carries only the MCP protocol layer (tool whitelist, schemas,
//! dispatch) on top of the official Rust SDK [`rmcp`] over a stdio transport. It
//! depends solely on [`desk_agent_protocol`]; the concrete read agent is injected
//! by `lcxl-remote-desk-server` through the [`ReadContextProvider`] trait, so
//! there is no dependency cycle (server → mcp-server → agent-protocol) and the
//! trust-field injection / auditing stay server-side.
//!
//! Security stance (agent protocol §14):
//! - The tool set is a **static whitelist** of read-only tools. No exec / write
//!   / control tool exists — it cannot be reached because it is not defined. AI
//!   diagnosis is **not** an MCP tool: it is orchestrated by the central signaling
//!   brain, so the MCP surface stays a pure read-only context provider.
//! - `lcxl_recent_logs` is refused unless the server policy permits log reads.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use desk_agent_protocol::{
    AgentError, ContextKind, LogRecentParams, LogSeverity, NetworkPortsParams, ProcessListParams,
    ReadContextOutput, SystemInfoParams,
};

/// Runs a single read-context capability. The server-side implementation builds
/// the server-stamped envelope, enforces the trust fields, calls the in-process
/// device agent, and audits the call.
#[async_trait]
pub trait ReadContextProvider: Send + Sync {
    async fn read(&self, kind: ContextKind) -> Result<ReadContextOutput, AgentError>;
}

/// Tool name for the system-info read.
pub const TOOL_SYSTEM_INFO: &str = "lcxl_system_info";
/// Tool name for the process-list read.
pub const TOOL_PROCESS_LIST: &str = "lcxl_process_list";
/// Tool name for the network-ports read.
pub const TOOL_NETWORK_PORTS: &str = "lcxl_network_ports";
/// Tool name for the recent-logs read (gated by the `allow_logs` policy).
pub const TOOL_RECENT_LOGS: &str = "lcxl_recent_logs";

/// The complete, static set of exposed tool names. The list is fixed — there is
/// deliberately no exec / write / control tool, and no AI diagnosis (that is
/// orchestrated centrally, not exposed as an MCP tool).
pub const TOOL_WHITELIST: &[&str] = &[
    TOOL_SYSTEM_INFO,
    TOOL_PROCESS_LIST,
    TOOL_NETWORK_PORTS,
    TOOL_RECENT_LOGS,
];

/// Live policy gate, queried on **every** tool call so a permission change takes
/// effect on the next call rather than only on a server restart. The server-side
/// implementation reads the authoritative (persisted) policy each time.
#[async_trait]
pub trait McpPolicy: Send + Sync {
    /// Whether log reads (`lcxl_recent_logs`) are permitted right now.
    async fn allow_logs(&self) -> bool;
}

/// The read-only MCP server handler.
#[derive(Clone)]
pub struct McpServer {
    reader: Arc<dyn ReadContextProvider>,
    policy: Arc<dyn McpPolicy>,
}

impl McpServer {
    pub fn new(reader: Arc<dyn ReadContextProvider>, policy: Arc<dyn McpPolicy>) -> Self {
        Self { reader, policy }
    }

    /// The static tool definitions (whitelist + JSON input schemas). Always the
    /// same four read-only tools regardless of policy; policy is enforced at call
    /// time, not by hiding tools.
    pub fn tools() -> Vec<Tool> {
        vec![
            tool(
                TOOL_SYSTEM_INFO,
                "Return host system information: OS, architecture, uptime, CPU, \
                 memory, and disks. Read-only.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            tool(
                TOOL_PROCESS_LIST,
                "List running processes (sorted by CPU usage). Read-only; command \
                 lines are never returned.",
                json!({
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 0,
                            "description": "Max processes to return (0 = server default cap)."}
                    },
                    "additionalProperties": false
                }),
            ),
            tool(
                TOOL_NETWORK_PORTS,
                "List listening network ports and their owning processes. Read-only.",
                json!({
                    "type": "object",
                    "properties": {
                        "protocol": {"type": "string", "enum": ["tcp", "udp"],
                            "description": "Filter to one transport; omit for both."}
                    },
                    "additionalProperties": false
                }),
            ),
            tool(
                TOOL_RECENT_LOGS,
                "Return recent system log entries. Read-only. Refused unless the \
                 server policy permits sending logs.",
                json!({
                    "type": "object",
                    "properties": {
                        "source": {"type": "string", "description": "Log source/channel."},
                        "since_minutes": {"type": "integer", "minimum": 0},
                        "limit": {"type": "integer", "minimum": 0},
                        "severity": {"type": "array", "items": {"type": "string",
                            "enum": ["error", "warning", "info", "debug"]}}
                    },
                    "additionalProperties": false
                }),
            ),
        ]
    }

    /// Dispatch a tool call by name. Unknown / non-whitelist names are rejected
    /// without touching the providers; policy gates produce an error result.
    pub async fn dispatch_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        match name {
            TOOL_SYSTEM_INFO => {
                self.read(ContextKind::SystemInfo(SystemInfoParams {
                    include_hardware: true,
                    include_network_summary: true,
                }))
                .await
            }
            TOOL_PROCESS_LIST => {
                let a: ProcessListArgs = parse_args(args)?;
                self.read(ContextKind::ProcessList(ProcessListParams {
                    limit: a.limit,
                    ..Default::default()
                }))
                .await
            }
            TOOL_NETWORK_PORTS => {
                let a: NetworkPortsArgs = parse_args(args)?;
                self.read(ContextKind::NetworkPorts(NetworkPortsParams {
                    protocol: a.protocol,
                }))
                .await
            }
            TOOL_RECENT_LOGS => {
                if !self.policy.allow_logs().await {
                    return Ok(error_result(
                        "log access is disabled by server policy (allow_logs=false)",
                    ));
                }
                let a: RecentLogsArgs = parse_args(args)?;
                let severity = a
                    .severity
                    .iter()
                    .filter_map(|s| parse_severity(s))
                    .collect();
                self.read(ContextKind::LogRecent(LogRecentParams {
                    source: a.source,
                    since_minutes: a.since_minutes,
                    limit: a.limit,
                    severity,
                }))
                .await
            }
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    /// Run one read capability and wrap its output (or error) as a tool result.
    async fn read(&self, kind: ContextKind) -> Result<CallToolResult, McpError> {
        Ok(match self.reader.read(kind).await {
            Ok(output) => success_json(&output),
            Err(e) => error_result(e.message),
        })
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "lcxl-mcp-server",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Read-only single-machine diagnostics for the lcxl remote desk \
                 agent. Exposes only read tools; no command execution.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request.arguments.unwrap_or_default();
        self.dispatch_tool(request.name.as_ref(), args).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tools().into_iter().find(|t| t.name.as_ref() == name)
    }
}

/// Serve the MCP server over stdio until the client disconnects.
pub async fn serve_stdio(
    server: McpServer,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ----------------------------- helpers -----------------------------

/// Build a [`Tool`] from a name, description, and a JSON-schema object value.
fn tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    let object = match schema {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    Tool::new(name, description, Arc::new(object))
}

/// Serialize a value as the (single) text content of a successful tool result.
fn success_json<T: serde::Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => CallToolResult::success(vec![Content::text(text)]),
        Err(e) => error_result(format!("failed to serialize result: {e}")),
    }
}

/// An error tool result carrying a single text message.
fn error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

/// Deserialize tool arguments, mapping any failure to an MCP invalid-params error.
fn parse_args<T: serde::de::DeserializeOwned>(args: Map<String, Value>) -> Result<T, McpError> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| McpError::invalid_params(format!("invalid arguments: {e}"), None))
}

/// Map a severity string to the protocol enum; unknown values are dropped.
fn parse_severity(s: &str) -> Option<LogSeverity> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Some(LogSeverity::Error),
        "warning" => Some(LogSeverity::Warning),
        "info" => Some(LogSeverity::Info),
        "debug" => Some(LogSeverity::Debug),
        _ => None,
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ProcessListArgs {
    limit: u32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NetworkPortsArgs {
    protocol: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RecentLogsArgs {
    source: Option<String>,
    since_minutes: Option<u32>,
    limit: Option<u32>,
    severity: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::NetworkPortsOutput;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingReader {
        calls: Mutex<Vec<ContextKind>>,
    }
    #[async_trait]
    impl ReadContextProvider for RecordingReader {
        async fn read(&self, kind: ContextKind) -> Result<ReadContextOutput, AgentError> {
            self.calls.lock().unwrap().push(kind);
            Ok(ReadContextOutput::NetworkPorts(NetworkPortsOutput {
                ports: vec![],
                truncated: false,
            }))
        }
    }

    /// Fixed policy for tests that do not exercise liveness.
    struct StaticPolicy {
        allow_logs: bool,
    }
    #[async_trait]
    impl McpPolicy for StaticPolicy {
        async fn allow_logs(&self) -> bool {
            self.allow_logs
        }
    }

    fn server(allow_logs: bool) -> (McpServer, Arc<RecordingReader>) {
        let reader = Arc::new(RecordingReader::default());
        let policy = Arc::new(StaticPolicy { allow_logs });
        let server = McpServer::new(reader.clone(), policy);
        (server, reader)
    }

    fn is_error(result: &CallToolResult) -> bool {
        result.is_error.unwrap_or(false)
    }

    /// The advertised tool set is exactly the four read-only whitelist tools.
    #[test]
    fn tools_are_exactly_the_readonly_whitelist() {
        let names: Vec<String> = McpServer::tools()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, TOOL_WHITELIST);
        assert_eq!(names.len(), 4);
    }

    /// No exec / write / control tool is exposed — the whitelist is read-only.
    #[test]
    fn no_exec_write_or_control_tools_are_exposed() {
        for t in McpServer::tools() {
            let n = t.name.to_ascii_lowercase();
            for forbidden in ["exec", "write", "delete", "kill", "control", "shell", "run"] {
                assert!(
                    !n.contains(forbidden),
                    "tool {n:?} must not expose a {forbidden} surface"
                );
            }
        }
    }

    /// An unknown / non-whitelist tool is rejected without touching the provider.
    #[tokio::test]
    async fn unknown_tool_is_rejected_without_touching_providers() {
        let (server, reader) = server(true);
        let err = server
            .dispatch_tool("lcxl_exec", Map::new())
            .await
            .expect_err("unknown tool must be a protocol error");
        let _ = err;
        assert!(reader.calls.lock().unwrap().is_empty());
    }

    /// `lcxl_diagnose` is no longer an MCP tool (diagnosis is orchestrated
    /// centrally): it is rejected as an unknown tool, never reaching the reader.
    #[tokio::test]
    async fn diagnose_is_not_an_mcp_tool() {
        let (server, reader) = server(true);
        let mut args = Map::new();
        args.insert("question".into(), json!("why is the cpu high?"));
        let err = server
            .dispatch_tool("lcxl_diagnose", args)
            .await
            .expect_err("diagnose must be an unknown tool");
        let _ = err;
        assert!(reader.calls.lock().unwrap().is_empty());
    }

    /// A read tool dispatches to the reader with the right capability.
    #[tokio::test]
    async fn system_info_dispatches_to_reader() {
        let (server, reader) = server(true);
        let result = server
            .dispatch_tool(TOOL_SYSTEM_INFO, Map::new())
            .await
            .unwrap();
        assert!(!is_error(&result));
        let calls = reader.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], ContextKind::SystemInfo(_)));
    }

    /// `lcxl_recent_logs` is refused (and the reader untouched) when log access
    /// is disabled by policy.
    #[tokio::test]
    async fn recent_logs_denied_when_logs_disabled() {
        let (server, reader) = server(false);
        let result = server
            .dispatch_tool(TOOL_RECENT_LOGS, Map::new())
            .await
            .unwrap();
        assert!(is_error(&result));
        assert!(
            reader.calls.lock().unwrap().is_empty(),
            "reader must not be called"
        );
    }

    /// The log gate is evaluated **live on every call**: revoking `allow_logs`
    /// between calls denies the next `lcxl_recent_logs` without restarting.
    #[tokio::test]
    async fn recent_logs_gate_is_evaluated_live_per_call() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MutablePolicy {
            allow_logs: Arc<AtomicBool>,
        }
        #[async_trait]
        impl McpPolicy for MutablePolicy {
            async fn allow_logs(&self) -> bool {
                self.allow_logs.load(Ordering::SeqCst)
            }
        }

        let flag = Arc::new(AtomicBool::new(true));
        let reader = Arc::new(RecordingReader::default());
        let server = McpServer::new(
            reader.clone(),
            Arc::new(MutablePolicy {
                allow_logs: flag.clone(),
            }),
        );

        // Allowed at first.
        let r = server
            .dispatch_tool(TOOL_RECENT_LOGS, Map::new())
            .await
            .unwrap();
        assert!(!is_error(&r));

        // Operator revokes log access; the next call is denied (live re-check).
        flag.store(false, Ordering::SeqCst);
        let r = server
            .dispatch_tool(TOOL_RECENT_LOGS, Map::new())
            .await
            .unwrap();
        assert!(is_error(&r));
    }

    /// `lcxl_recent_logs` reaches the reader when log access is permitted.
    #[tokio::test]
    async fn recent_logs_allowed_when_enabled() {
        let (server, reader) = server(true);
        let result = server
            .dispatch_tool(TOOL_RECENT_LOGS, Map::new())
            .await
            .unwrap();
        assert!(!is_error(&result));
        assert!(matches!(
            reader.calls.lock().unwrap()[0],
            ContextKind::LogRecent(_)
        ));
    }
}
