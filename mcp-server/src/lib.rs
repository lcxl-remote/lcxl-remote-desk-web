//! Single-machine, **read-only** MCP server exposing the diagnose agent's read
//! capabilities to a local MCP client (e.g. an external AI assistant).
//!
//! This crate carries only the MCP protocol layer (tool whitelist, schemas,
//! dispatch) on top of the official Rust SDK [`rmcp`] over a stdio transport. It
//! depends solely on [`desk_agent_protocol`]; the concrete read agent and the
//! diagnose orchestrator are injected by `lcxl-remote-desk-server` through the
//! [`ReadContextProvider`] / [`DiagnoseProvider`] traits, so there is no
//! dependency cycle (server → mcp-server → agent-protocol) and the trust-field
//! injection / auditing stay server-side.
//!
//! Security stance (codex protocol §14):
//! - The tool set is a **static whitelist** of read-only tools. No exec / write
//!   / control tool exists — it cannot be reached because it is not defined.
//! - `lcxl_diagnose` runs through [`DiagnoseProvider`], whose signature carries
//!   **no screenshot option**, so an MCP client structurally cannot pull a
//!   screen capture through the diagnose path.
//! - `lcxl_recent_logs` is refused unless the server policy permits log reads;
//!   `lcxl_diagnose` is refused unless the model gateway is configured.

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

use desk_agent_protocol::diagnose::Diagnosis;
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

/// Runs a one-shot (non-streaming) diagnosis and returns the final
/// [`Diagnosis`]. The signature deliberately carries **no screenshot option** so
/// an MCP client cannot request a screen capture; the implementation forces
/// `include_screen = false` and audits the run.
#[async_trait]
pub trait DiagnoseProvider: Send + Sync {
    async fn diagnose(
        &self,
        question: String,
        locale: Option<String>,
    ) -> Result<Diagnosis, AgentError>;
}

/// Tool name for the system-info read.
pub const TOOL_SYSTEM_INFO: &str = "lcxl_system_info";
/// Tool name for the process-list read.
pub const TOOL_PROCESS_LIST: &str = "lcxl_process_list";
/// Tool name for the network-ports read.
pub const TOOL_NETWORK_PORTS: &str = "lcxl_network_ports";
/// Tool name for the recent-logs read (gated by the `allow_logs` policy).
pub const TOOL_RECENT_LOGS: &str = "lcxl_recent_logs";
/// Tool name for a one-shot diagnosis (gated by gateway configuration).
pub const TOOL_DIAGNOSE: &str = "lcxl_diagnose";

/// The complete, static set of exposed tool names. The list is fixed — there is
/// deliberately no exec / write / control tool.
pub const TOOL_WHITELIST: &[&str] = &[
    TOOL_SYSTEM_INFO,
    TOOL_PROCESS_LIST,
    TOOL_NETWORK_PORTS,
    TOOL_RECENT_LOGS,
    TOOL_DIAGNOSE,
];

/// Whether `lcxl_diagnose` can run, mirroring the diagnose gate's precedence so
/// the MCP path reports the same reason as the signaling / model layers. The
/// server computes this once (manager-proxy is checked before configuration, so
/// it wins even without direct credentials).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnoseAvailability {
    /// Diagnosis can run.
    Available,
    /// The model gateway is not configured (no model / base URL / API key).
    NotConfigured,
    /// A manager-proxied gateway is selected but not implemented yet.
    ManagerProxyUnavailable,
}

/// The read-only MCP server handler.
#[derive(Clone)]
pub struct McpServer {
    reader: Arc<dyn ReadContextProvider>,
    diagnose: Arc<dyn DiagnoseProvider>,
    /// Whether log reads (`lcxl_recent_logs`) are permitted by server policy.
    allow_logs: bool,
    /// Whether / why `lcxl_diagnose` may run.
    diagnose_availability: DiagnoseAvailability,
}

impl McpServer {
    pub fn new(
        reader: Arc<dyn ReadContextProvider>,
        diagnose: Arc<dyn DiagnoseProvider>,
        allow_logs: bool,
        diagnose_availability: DiagnoseAvailability,
    ) -> Self {
        Self {
            reader,
            diagnose,
            allow_logs,
            diagnose_availability,
        }
    }

    /// The static tool definitions (whitelist + JSON input schemas). Always the
    /// same five read-only tools regardless of policy; policy is enforced at call
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
            tool(
                TOOL_DIAGNOSE,
                "Run a one-shot AI diagnosis from read-only evidence and return a \
                 structured result. Never captures the screen. Refused unless the \
                 model gateway is configured.",
                json!({
                    "type": "object",
                    "properties": {
                        "question": {"type": "string",
                            "description": "The problem to diagnose."},
                        "locale": {"type": "string",
                            "description": "BCP-47 language tag for the answer."}
                    },
                    "required": ["question"],
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
                if !self.allow_logs {
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
            TOOL_DIAGNOSE => {
                match self.diagnose_availability {
                    // Manager-proxy precedence matches the model / router layers:
                    // report "proxy not available" even when direct credentials
                    // are absent, not the misleading "not configured".
                    DiagnoseAvailability::ManagerProxyUnavailable => {
                        return Ok(error_result(
                            "manager-proxied model gateway is not available yet",
                        ));
                    }
                    DiagnoseAvailability::NotConfigured => {
                        return Ok(error_result(
                            "the AI model gateway is not configured; diagnosis is unavailable",
                        ));
                    }
                    DiagnoseAvailability::Available => {}
                }
                let a: DiagnoseArgs = parse_args(args)?;
                Ok(match self.diagnose.diagnose(a.question, a.locale).await {
                    Ok(d) => success_json(&d),
                    Err(e) => error_result(e.message),
                })
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

#[derive(Deserialize)]
struct DiagnoseArgs {
    question: String,
    #[serde(default)]
    locale: Option<String>,
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

    #[derive(Default)]
    struct RecordingDiagnose {
        calls: Mutex<Vec<(String, Option<String>)>>,
    }
    #[async_trait]
    impl DiagnoseProvider for RecordingDiagnose {
        async fn diagnose(
            &self,
            question: String,
            locale: Option<String>,
        ) -> Result<Diagnosis, AgentError> {
            self.calls.lock().unwrap().push((question, locale));
            Ok(Diagnosis::default())
        }
    }

    fn server(
        allow_logs: bool,
        availability: DiagnoseAvailability,
    ) -> (McpServer, Arc<RecordingReader>, Arc<RecordingDiagnose>) {
        let reader = Arc::new(RecordingReader::default());
        let diagnose = Arc::new(RecordingDiagnose::default());
        let server = McpServer::new(reader.clone(), diagnose.clone(), allow_logs, availability);
        (server, reader, diagnose)
    }

    fn is_error(result: &CallToolResult) -> bool {
        result.is_error.unwrap_or(false)
    }

    /// The advertised tool set is exactly the five read-only whitelist tools.
    #[test]
    fn tools_are_exactly_the_readonly_whitelist() {
        let names: Vec<String> = McpServer::tools()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, TOOL_WHITELIST);
        assert_eq!(names.len(), 5);
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

    /// An unknown / non-whitelist tool is rejected without touching the providers.
    #[tokio::test]
    async fn unknown_tool_is_rejected_without_touching_providers() {
        let (server, reader, diagnose) = server(true, DiagnoseAvailability::Available);
        let err = server
            .dispatch_tool("lcxl_exec", Map::new())
            .await
            .expect_err("unknown tool must be a protocol error");
        let _ = err;
        assert!(reader.calls.lock().unwrap().is_empty());
        assert!(diagnose.calls.lock().unwrap().is_empty());
    }

    /// A read tool dispatches to the reader with the right capability.
    #[tokio::test]
    async fn system_info_dispatches_to_reader() {
        let (server, reader, _) = server(true, DiagnoseAvailability::Available);
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
        let (server, reader, _) = server(false, DiagnoseAvailability::Available);
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

    /// `lcxl_recent_logs` reaches the reader when log access is permitted.
    #[tokio::test]
    async fn recent_logs_allowed_when_enabled() {
        let (server, reader, _) = server(true, DiagnoseAvailability::Available);
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

    /// `lcxl_diagnose` is refused (and the diagnose provider untouched) when the
    /// model gateway is not configured.
    #[tokio::test]
    async fn diagnose_denied_when_not_configured() {
        let (server, _, diagnose) = server(true, DiagnoseAvailability::NotConfigured);
        let mut args = Map::new();
        args.insert("question".into(), json!("why is the cpu high?"));
        let result = server.dispatch_tool(TOOL_DIAGNOSE, args).await.unwrap();
        assert!(is_error(&result));
        assert!(
            diagnose.calls.lock().unwrap().is_empty(),
            "diagnose must not run"
        );
    }

    /// With `manager_proxy` selected, `lcxl_diagnose` is refused with the proxy
    /// message (not "not configured") and never reaches the provider — matching
    /// the model / router precedence even without direct credentials.
    #[tokio::test]
    async fn diagnose_manager_proxy_takes_precedence_over_not_configured() {
        let (server, _, diagnose) = server(true, DiagnoseAvailability::ManagerProxyUnavailable);
        let mut args = Map::new();
        args.insert("question".into(), json!("why is the cpu high?"));
        let result = server.dispatch_tool(TOOL_DIAGNOSE, args).await.unwrap();
        assert!(is_error(&result));
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            json.contains("manager-proxied"),
            "expected the proxy message, got: {json}"
        );
        assert!(
            diagnose.calls.lock().unwrap().is_empty(),
            "diagnose must not run"
        );
    }

    /// `lcxl_diagnose` runs through the diagnose provider when configured. The
    /// provider signature carries no screenshot option, so an MCP client cannot
    /// request a screen capture through this path.
    #[tokio::test]
    async fn diagnose_runs_when_configured() {
        let (server, _, diagnose) = server(true, DiagnoseAvailability::Available);
        let mut args = Map::new();
        args.insert("question".into(), json!("why is the cpu high?"));
        args.insert("locale".into(), json!("zh-CN"));
        let result = server.dispatch_tool(TOOL_DIAGNOSE, args).await.unwrap();
        assert!(!is_error(&result));
        let calls = diagnose.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "why is the cpu high?");
        assert_eq!(calls[0].1.as_deref(), Some("zh-CN"));
    }

    /// `lcxl_diagnose` without the required `question` argument is an invalid-params
    /// error and never reaches the provider.
    #[tokio::test]
    async fn diagnose_requires_question() {
        let (server, _, diagnose) = server(true, DiagnoseAvailability::Available);
        let err = server.dispatch_tool(TOOL_DIAGNOSE, Map::new()).await;
        assert!(err.is_err(), "missing question must be invalid params");
        assert!(diagnose.calls.lock().unwrap().is_empty());
    }
}
