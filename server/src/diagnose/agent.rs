//! Direct-runtime wiring for the agentic tool-calling loop.
//!
//! Implements the three seams the shared [`desk_diagnose_core::agent_loop`] runs
//! over, for the Default / DeskServer (daemon-local) runtime:
//!
//! - [`AdapterModelSeam`]: maps a neutral [`ModelRequest`] to the configured
//!   provider's [`ChatRequest`] and calls the streaming adapter, returning the
//!   normalized [`ModelTurn`].
//! - [`DirectToolSeam`]: runs a read tool by mapping the model's call to a
//!   server-stamped read envelope, invoking the in-process [`LocalDeviceAgent`],
//!   redacting the result (fail-closed), and serializing it for the model.
//! - [`InMemorySessionSeam`]: keeps sessions in process memory with a
//!   per-conversation atomic claim (one daemon process owns its sessions).
//!
//! [`read_tool_registry`] is the read-only tool set exposed to the model; each
//! tool maps onto one [`ContextKind`].
//!
//! Only read tools are wired here; the mutating path lands in a later PR. There
//! is no token streaming yet (the loop's `TurnSink` is unused on this path);
//! streaming is added with the UI PR.

use std::collections::HashMap;
use std::sync::Arc;

use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentOutcome,
    AgentScope, AuditMeta, CallerRef, CallerType, Capability, ContainerListParams, ContextKind,
    DeviceAgent, ExecutionMode, LogRecentParams, NetworkPortsParams, OperationInput,
    ProcessListParams, ProtocolVersion, ReadContextInput, RequestId, ServiceStatusParams,
    SystemInfoParams, TargetRef,
};
use desk_diagnose_core::chat::{ModelTurn, StopReason, ToolCall, ToolSpec};
use desk_diagnose_core::registry::{RegisteredTool, ToolEffect};
use desk_diagnose_core::seam::{
    ClaimError, ClaimTurnParams, ModelRequest, ModelSeam, SessionSeam, ToolRunOutput, ToolSeam,
    TurnSink,
};
use desk_diagnose_core::session::PersistedAgentSession;
use serde::de::DeserializeOwned;

use super::model::{AdapterSelector, ChatRequest};
use super::redaction::{Redactor, redact_snapshot};
use crate::model::settings::SharedSettings;
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::eval::EvidenceSnapshot;

/// Current time as an RFC3339 string.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn bad_arguments(detail: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("invalid tool arguments: {detail}"),
        retryable: false,
        safe_for_model: true,
    }
}

// ============================ Read tool registry ============================

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
    use serde_json::json;
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
fn build_read_operation(call: &ToolCall) -> Result<(Capability, OperationInput), AgentError> {
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
        other => {
            return Err(AgentError {
                kind: AgentErrorKind::UnsupportedCapability,
                message: format!("unknown read tool `{other}`"),
                retryable: false,
                safe_for_model: true,
            });
        }
    };
    let input = OperationInput::ReadContext(ReadContextInput { kind });
    let cap = input.capability().ok_or_else(|| AgentError {
        kind: AgentErrorKind::UnsupportedCapability,
        message: "read tool maps to no capability".to_string(),
        retryable: false,
        safe_for_model: true,
    })?;
    Ok((cap, input))
}

// ============================ Tool seam (read) ============================

/// Runs read tools against the in-process device agent, redacting each result
/// before it returns to the loop (fail-closed).
pub struct DirectToolSeam {
    agent: Arc<LocalDeviceAgent>,
    redactor: Arc<dyn Redactor>,
    actor_id: String,
}

impl DirectToolSeam {
    pub fn new(
        agent: Arc<LocalDeviceAgent>,
        redactor: Arc<dyn Redactor>,
        actor_id: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            redactor,
            actor_id: actor_id.into(),
        }
    }

    /// A read-only, server-stamped envelope granting exactly `cap` (mirrors the
    /// collector's trusted-field stamp; no control end supplies any of these).
    fn build_envelope(&self, cap: Capability, input: OperationInput) -> AgentEnvelope {
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId(uuid::Uuid::new_v4().to_string()),
            parent_task_id: None,
            target: TargetRef::default(),
            actor: ActorRef {
                actor_type: ActorType::System,
                actor_id: self.actor_id.clone(),
                tenant_id: None,
            },
            caller: CallerRef {
                caller_type: CallerType::Human,
                model_provider: None,
                model_name: None,
                adapter: None,
            },
            scope: AgentScope {
                granted: vec![cap],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input,
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("ai agent loop read".into()),
            },
        }
    }
}

#[async_trait::async_trait(?Send)]
impl ToolSeam for DirectToolSeam {
    async fn run_read(&self, call: &ToolCall) -> Result<ToolRunOutput, AgentError> {
        let (cap, input) = build_read_operation(call)?;
        let envelope = self.build_envelope(cap, input);
        let output = self.agent.invoke(envelope).await?;

        // Redact via a one-entry snapshot so the exact send-time redaction +
        // screenshot refit run; a redaction failure is fail-closed (the loop
        // turns the Err into an error tool-result, never leaking raw output).
        let mut snapshot = EvidenceSnapshot::record(
            "live",
            String::new(),
            now_rfc3339(),
            vec![(cap, AgentOutcome::Ok(output))],
        );
        redact_snapshot(self.redactor.as_ref(), &mut snapshot).map_err(|e| AgentError {
            kind: AgentErrorKind::RedactionFailed,
            message: format!("evidence redaction failed: {}", e.reason),
            retryable: false,
            safe_for_model: true,
        })?;
        super::model::screenshot::refit_snapshot_screenshots(&mut snapshot);

        let entry = snapshot
            .contexts
            .first()
            .expect("the one entry we recorded is present");
        let content = serde_json::to_string(&entry.outcome).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolRunOutput {
            content,
            image_data_url: entry.image_data_url.clone(),
        })
    }
}

// ============================ Model seam ============================

/// Calls the configured provider via the streaming adapter, mapping the neutral
/// [`ModelRequest`] to a [`ChatRequest`]. Direct provider config only (the
/// manager-proxy gateway is handled on the manager runtime).
pub struct AdapterModelSeam {
    selector: Arc<dyn AdapterSelector>,
    settings: Arc<SharedSettings>,
}

impl AdapterModelSeam {
    pub fn new(selector: Arc<dyn AdapterSelector>, settings: Arc<SharedSettings>) -> Self {
        Self { selector, settings }
    }
}

/// The graceful turn returned when no model gateway is configured.
fn not_configured_turn() -> ModelTurn {
    ModelTurn {
        text: "AI model is not configured; set the provider, model, base URL, and \
               API key in AI model settings."
            .to_string(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

#[async_trait::async_trait(?Send)]
impl ModelSeam for AdapterModelSeam {
    async fn call(
        &self,
        request: ModelRequest,
        _sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        let config = { self.settings.read().await.ai_model.clone() };
        let (Some(model), Some(base_url), Some(api_key)) = (
            config.model.clone(),
            config.base_url.clone(),
            config.api_key.clone(),
        ) else {
            return Ok(not_configured_turn());
        };
        if model.is_empty() || base_url.is_empty() || api_key.is_empty() {
            return Ok(not_configured_turn());
        }
        let adapter = self.selector.select(config.provider.as_deref());
        let chat = ChatRequest {
            base_url,
            api_key,
            model,
            messages: request.messages,
            response_format: request.response_format,
            tools: request.tools,
            tool_choice: request.tool_choice,
        };
        // No token streaming on this path yet (added with the UI PR); the adapter
        // still streams internally, so feed it a no-op delta sink.
        let noop = |_: String| {};
        adapter.stream_chat(chat, &noop).await
    }
}

// ============================ Session seam (in-memory) ============================

/// Keeps agent sessions in process memory, keyed by conversation id. One daemon
/// process owns its sessions, so a single async mutex makes the claim atomic.
#[derive(Default)]
pub struct InMemorySessionSeam {
    sessions: tokio::sync::Mutex<HashMap<String, PersistedAgentSession>>,
}

impl InMemorySessionSeam {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait(?Send)]
impl SessionSeam for InMemorySessionSeam {
    async fn claim_turn(
        &self,
        params: ClaimTurnParams,
    ) -> Result<PersistedAgentSession, ClaimError> {
        let mut map = self.sessions.lock().await;
        let mut session = match map.get(&params.conversation_id) {
            Some(existing) => {
                existing
                    .check_subject(
                        params.tenant_id.as_deref(),
                        &params.actor_id,
                        &params.device_id,
                    )
                    .map_err(ClaimError::Subject)?;
                existing.clone()
            }
            None => PersistedAgentSession::new(
                params.conversation_id.clone(),
                params.tenant_id.clone(),
                params.actor_id.clone(),
                params.device_id.clone(),
                params.policy_revision,
                params.current_pdp_scope.clone(),
                params.now.clone(),
            ),
        };
        session
            .begin_turn(
                params.turn_id,
                params.request_id,
                params.connection_id,
                params.policy_revision,
                params.current_pdp_scope,
                params.now,
            )
            .map_err(|_| ClaimError::Busy)?;
        map.insert(session.conversation_id.clone(), session.clone());
        Ok(session)
    }

    async fn save(&self, session: &PersistedAgentSession) -> Result<(), AgentError> {
        self.sessions
            .lock()
            .await
            .insert(session.conversation_id.clone(), session.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::agent_loop::{LoopDeps, LoopOutcome, run_agent_turn};
    use desk_diagnose_core::chat::{ChatMessage, ChatRole, ModelTurn, StopReason, ToolCall};
    use desk_diagnose_core::prompt::ResponseFormatSpec;
    use desk_diagnose_core::seam::TurnSink;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::super::redaction::RegexRedactor;

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

    /// Every registered tool is read-only and its name maps back to a capability.
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

    /// The direct tool seam runs `read_system_info` against the real in-process
    /// agent and returns a JSON result (succeeds on every CI host).
    #[tokio::test]
    async fn direct_tool_seam_reads_system_info() {
        let seam = DirectToolSeam::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            "agent-loop",
        );
        let out = seam
            .run_read(&ToolCall {
                id: "c1".into(),
                name: "read_system_info".into(),
                arguments_json: String::new(),
            })
            .await
            .expect("read ok");
        assert!(out.content.contains("SystemInfo") || out.content.contains("hostname"));
        assert!(out.image_data_url.is_none());
    }

    struct Collector(RefCell<String>);
    impl TurnSink for Collector {
        fn on_text_delta(&mut self, delta: &str) {
            self.0.borrow_mut().push_str(delta);
        }
    }

    /// A scripted model seam to drive the loop without a network.
    struct ScriptModel(RefCell<VecDeque<ModelTurn>>);
    #[async_trait::async_trait(?Send)]
    impl ModelSeam for ScriptModel {
        async fn call(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            Ok(self.0.borrow_mut().pop_front().expect("a scripted turn"))
        }
    }

    /// End to end on the Direct seams: the model asks for `read_system_info`, the
    /// real tool runs, and the second turn answers — using the in-memory session.
    #[tokio::test]
    async fn loop_runs_read_tool_via_direct_seams() {
        let model = ScriptModel(RefCell::new(
            [
                ModelTurn {
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read_system_info".into(),
                        // Providers always emit a JSON object for arguments.
                        arguments_json: "{}".into(),
                    }],
                    ..Default::default()
                },
                ModelTurn {
                    text: "the host looks healthy".into(),
                    stop_reason: StopReason::EndTurn,
                    ..Default::default()
                },
            ]
            .into(),
        ));
        let tools = DirectToolSeam::new(
            Arc::new(LocalDeviceAgent::new()),
            Arc::new(RegexRedactor::new()),
            "agent-loop",
        );
        let session = InMemorySessionSeam::new();
        let registry = read_tool_registry();
        let clock = || now_rfc3339();
        let deps = LoopDeps {
            session_seam: &session,
            model: &model,
            tools: &tools,
            registry: &registry,
            response_format: ResponseFormatSpec::None,
            clock: &clock,
        };
        let claim = ClaimTurnParams {
            conversation_id: "conv-1".into(),
            tenant_id: None,
            actor_id: "actor".into(),
            device_id: "device".into(),
            policy_revision: 1,
            current_pdp_scope: AgentScope {
                granted: vec![Capability::SystemInfo],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            turn_id: "turn-1".into(),
            request_id: Some("req".into()),
            connection_id: Some("conn".into()),
            now: now_rfc3339(),
        };
        let mut sink = Collector(RefCell::new(String::new()));
        let user = ChatMessage::text("u", ChatRole::User, "is the host healthy?");
        let outcome = run_agent_turn(&deps, claim, user, &mut sink).await.unwrap();
        assert_eq!(
            outcome,
            LoopOutcome::Answered("the host looks healthy".into())
        );
    }
}
