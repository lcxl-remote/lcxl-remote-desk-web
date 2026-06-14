//! Read-only MCP server wiring (the `mcp-stdio` startup mode).
//!
//! Bridges the single-machine diagnose stack to [`desk_mcp_server`]: the MCP
//! crate carries the protocol + tool whitelist and depends only on
//! `desk-agent-protocol`; this module supplies the concrete read agent and
//! diagnose orchestrator behind the [`ReadContextProvider`] / [`DiagnoseProvider`]
//! traits, so the server → mcp-server → agent-protocol dependency direction has
//! no cycle and the trust-field injection / auditing stay server-side.
//!
//! Runtime note: the diagnose path uses `awc` (`!Send`) and the OpenAI/Anthropic
//! adapters run on actix's single-threaded runtime, while the MCP server runs on
//! a multi-threaded tokio runtime that requires `Send` handler futures. The
//! diagnose provider therefore isolates each diagnosis onto a dedicated thread
//! with its own actix `System` and returns the result over a `oneshot` channel,
//! keeping its own future `Send`. Read tools (`LocalDeviceAgent::invoke`) are
//! `Send` and run directly on the MCP runtime.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use desk_agent_protocol::diagnose::{DiagnoseEvent, DiagnoseRequestData, Diagnosis};
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentScope,
    AuditMeta, CallerRef, CallerType, Capability, ContextKind, DeviceAgent, ExecutionMode,
    OperationInput, OperationOutput, ProtocolVersion, ReadContextInput, ReadContextOutput,
    RequestId, TargetRef,
};
use desk_mcp_server::{
    DiagnoseAvailability, DiagnoseProvider, McpServer, ReadContextProvider, serve_stdio,
};

use crate::diagnose::collector::AgentContextCollector;
use crate::diagnose::model::{ModelBackedDiagnoseModel, ProviderAdapterSelector};
use crate::diagnose::redaction::RegexRedactor;
use crate::diagnose::{DiagnoseEventSink, DiagnoseOrchestrator};
use crate::error::DeskError;
use crate::model::settings::{Args, GatewayMode, Settings, SharedSettings, StartupMode};
use crate::telemetry;
use crate::worker::agent::LocalDeviceAgent;
use crate::worker::agent::audit_sink::LogAuditSink;
use desk_agent_protocol::audit::AuditSink;

/// Read provider: runs a single read-context capability through the in-process
/// device agent with a server-stamped, read-only envelope.
struct ServerReadProvider {
    agent: Arc<LocalDeviceAgent>,
}

#[async_trait]
impl ReadContextProvider for ServerReadProvider {
    async fn read(&self, kind: ContextKind) -> Result<ReadContextOutput, AgentError> {
        let input = OperationInput::ReadContext(ReadContextInput { kind });
        // A read context always carries a capability; the envelope grants exactly
        // it (read-only), mirroring the diagnose collector's trust-field stamp.
        let cap = input.capability().ok_or_else(|| AgentError {
            kind: AgentErrorKind::Internal,
            message: "read context has no capability".to_string(),
            retryable: false,
            safe_for_model: true,
        })?;
        let envelope = build_read_envelope(cap, input);
        match self.agent.invoke(envelope).await? {
            OperationOutput::ReadContext(output) => Ok(output),
            OperationOutput::Exec(_) => Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "read produced an exec output".to_string(),
                retryable: false,
                safe_for_model: true,
            }),
        }
    }
}

/// Diagnose provider: runs one non-streaming diagnosis. `include_screen` is
/// forced `false` — the MCP path never captures the screen. Each run executes on
/// a dedicated thread with its own actix `System` so the `!Send` model adapters
/// stay off the MCP runtime.
struct ServerDiagnoseProvider {
    orchestrator: Arc<DiagnoseOrchestrator>,
}

#[async_trait]
impl DiagnoseProvider for ServerDiagnoseProvider {
    async fn diagnose(
        &self,
        question: String,
        locale: Option<String>,
    ) -> Result<Diagnosis, AgentError> {
        let orchestrator = self.orchestrator.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let system = actix_web::rt::System::new();
            let result = system.block_on(async move {
                let sink = CapturingSink::default();
                let request = DiagnoseRequestData {
                    question,
                    // Hard off: an MCP client must never pull a screenshot.
                    include_screen: false,
                    context_kinds: Vec::new(),
                    locale,
                };
                orchestrator.run("mcp-diagnose", request, &sink).await;
                sink.into_result()
            });
            let _ = tx.send(result);
        });
        rx.await.unwrap_or_else(|_| {
            Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "diagnosis task ended without a result".to_string(),
                retryable: true,
                safe_for_model: true,
            })
        })
    }
}

/// Captures the terminal frame (final diagnosis or error) of an orchestrator run.
#[derive(Default)]
struct CapturingSink {
    result: Mutex<Option<Result<Diagnosis, AgentError>>>,
}

impl CapturingSink {
    fn into_result(self) -> Result<Diagnosis, AgentError> {
        self.result.into_inner().unwrap().unwrap_or_else(|| {
            Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "diagnosis produced no final result".to_string(),
                retryable: false,
                safe_for_model: true,
            })
        })
    }
}

impl DiagnoseEventSink for CapturingSink {
    fn emit(&self, event: DiagnoseEvent) {
        if let Some(diagnosis) = event.final_result {
            *self.result.lock().unwrap() = Some(Ok(diagnosis));
        } else if let Some(error) = event.error {
            *self.result.lock().unwrap() = Some(Err(error));
        }
    }
}

/// Assemble a read-only, server-stamped envelope for one MCP read tool call.
fn build_read_envelope(cap: Capability, input: OperationInput) -> AgentEnvelope {
    AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        request_id: RequestId(uuid::Uuid::new_v4().to_string()),
        parent_task_id: None,
        target: TargetRef::default(),
        actor: ActorRef {
            actor_type: ActorType::System,
            actor_id: "mcp-server".into(),
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
            policy_id: None,
        },
        operation: AgentOperation {
            risk_hint: None,
            input,
        },
        audit: AuditMeta {
            approval_id: None,
            reason: Some("mcp read tool".into()),
        },
    }
}

/// Build the [`McpServer`] from the configured single-machine diagnose stack.
/// `allow_logs` / `diagnose_configured` snapshot the current policy (this is a
/// freshly spawned per-session process, so a startup snapshot is sufficient).
async fn build_mcp_server(settings: Arc<SharedSettings>) -> McpServer {
    let (allow_logs, availability) = {
        let s = settings.read().await;
        // Mirror the diagnose gate precedence: manager-proxy wins over the
        // not-configured check so the MCP path reports the same reason as the
        // model / router layers even without direct credentials.
        let availability = if s.ai_model.gateway_mode == GatewayMode::ManagerProxy {
            DiagnoseAvailability::ManagerProxyUnavailable
        } else if s.ai_model.is_configured() {
            DiagnoseAvailability::Available
        } else {
            DiagnoseAvailability::NotConfigured
        };
        (s.ai_model.allow_logs, availability)
    };

    let audit: Arc<dyn AuditSink> = Arc::new(LogAuditSink);
    let agent =
        Arc::new(LocalDeviceAgent::with_settings(settings.clone()).with_audit(audit.clone()));
    let collector = Arc::new(AgentContextCollector::new(agent.clone(), settings.clone()));
    let model = Arc::new(ModelBackedDiagnoseModel::new(
        Arc::new(ProviderAdapterSelector),
        settings.clone(),
        audit.clone(),
    ));
    let orchestrator = Arc::new(DiagnoseOrchestrator::new(
        collector,
        Arc::new(RegexRedactor::new()),
        model,
        audit,
    ));

    McpServer::new(
        Arc::new(ServerReadProvider { agent }),
        Arc::new(ServerDiagnoseProvider { orchestrator }),
        allow_logs,
        availability,
    )
}

/// Entry point for `--startup-mode mcp-stdio`: load settings, serve the read-only
/// MCP server over stdio until the client disconnects. Logging is file-only
/// (stdout is the protocol channel).
pub fn run_mcp_stdio(args: Args) -> Result<(), DeskError> {
    let settings = Settings::new(&args)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let shared = Arc::new(SharedSettings::from(settings));
        let _guard = telemetry::init_telemetry(shared.clone(), &StartupMode::McpStdio).await?;
        let server = build_mcp_server(shared).await;
        serve_stdio(server).await.map_err(|e| {
            DeskError::from(std::io::Error::other(format!("mcp stdio server: {e}")))
        })?;
        Ok::<(), DeskError>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read through the provider hits the real in-process agent: `system.info`
    /// succeeds on every CI host and returns a system-info read output.
    #[tokio::test]
    async fn read_provider_returns_system_info() {
        let provider = ServerReadProvider {
            agent: Arc::new(LocalDeviceAgent::new()),
        };
        let output = provider
            .read(ContextKind::SystemInfo(Default::default()))
            .await
            .expect("system.info read should succeed");
        assert!(matches!(output, ReadContextOutput::SystemInfo(_)));
    }

    /// The read envelope is read-only and grants exactly the requested capability
    /// (the trust-field stamp the agent authorizes against).
    #[test]
    fn read_envelope_is_readonly_and_scoped() {
        let input = OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::ProcessList(Default::default()),
        });
        let envelope = build_read_envelope(Capability::ProcessList, input);
        assert_eq!(envelope.scope.mode, ExecutionMode::ReadOnly);
        assert_eq!(envelope.scope.granted, vec![Capability::ProcessList]);
        assert_eq!(envelope.actor.actor_type, ActorType::System);
    }

    /// The capturing sink keeps the final diagnosis from the terminal frame.
    #[test]
    fn capturing_sink_keeps_final_diagnosis() {
        let sink = CapturingSink::default();
        sink.emit(DiagnoseEvent::status("r", 0, "collecting"));
        sink.emit(DiagnoseEvent::final_result(
            "r",
            1,
            Diagnosis {
                summary: "done".into(),
                ..Default::default()
            },
        ));
        let result = sink.into_result().expect("final diagnosis captured");
        assert_eq!(result.summary, "done");
    }

    /// An error frame is surfaced as an `Err` result.
    #[test]
    fn capturing_sink_surfaces_error() {
        let sink = CapturingSink::default();
        sink.emit(DiagnoseEvent::error(
            "r",
            0,
            AgentError {
                kind: AgentErrorKind::TransportError,
                message: "boom".into(),
                retryable: true,
                safe_for_model: true,
            },
        ));
        let err = sink.into_result().expect_err("error frame yields Err");
        assert_eq!(err.kind, AgentErrorKind::TransportError);
    }
}
