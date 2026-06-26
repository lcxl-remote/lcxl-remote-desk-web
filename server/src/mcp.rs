//! Read-only MCP server wiring (the `mcp-stdio` startup mode).
//!
//! Bridges the single-machine read agent to [`desk_mcp_server`]: the MCP crate
//! carries the protocol + tool whitelist and depends only on
//! `desk-agent-protocol`; this module supplies the concrete read agent behind the
//! [`ReadContextProvider`] trait, so the server → mcp-server → agent-protocol
//! dependency direction has no cycle and the trust-field injection / auditing
//! stay server-side.
//!
//! The MCP surface is a pure **read-only context provider**: AI diagnosis is
//! orchestrated by the central signaling brain, not exposed as an MCP tool. Read
//! tools (`LocalDeviceAgent::invoke`) are `Send` and run directly on the MCP
//! runtime.

use std::sync::Arc;

use async_trait::async_trait;
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentScope,
    AuditMeta, CallerRef, CallerType, Capability, ContextKind, DeviceAgent, ExecutionMode,
    OperationInput, OperationOutput, ProtocolVersion, ReadContextInput, ReadContextOutput,
    RequestId, TargetRef,
};
use desk_mcp_server::{McpPolicy, McpServer, ReadContextProvider, serve_stdio};

use crate::error::DeskError;
use crate::model::settings::{Args, Settings, SharedSettings, StartupMode};
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

/// Live MCP policy that keeps the read tools on one fresh settings source. On each
/// gated call it re-reads the persisted config file and writes it back into the
/// shared `SharedSettings`, so the gate decision *and* the evidence collector
/// (`allow_logs`) — both of which read this same `Arc` — see the operator's latest
/// config without a restart. The in-process settings are a startup snapshot
/// otherwise, and the file is the cross-process source of truth the desk server
/// writes to. Fail-closed: an unreadable config denies logs (and leaves the
/// existing settings intact).
struct ConfigPolicy {
    args: Args,
    settings: Arc<SharedSettings>,
}

impl ConfigPolicy {
    /// Re-read the config file and publish it into the shared settings. Returns
    /// the freshly loaded settings, or `None` if the config could not be read
    /// (in which case the shared settings are left unchanged).
    async fn refresh(&self) -> Option<Settings> {
        let args = self.args.clone();
        let loaded = tokio::task::spawn_blocking(move || Settings::load_readonly(&args).ok())
            .await
            .ok()
            .flatten()?;
        *self.settings.write().await = loaded.clone();
        Some(loaded)
    }
}

#[async_trait]
impl McpPolicy for ConfigPolicy {
    async fn allow_logs(&self) -> bool {
        // Fail closed: deny logs if the config cannot be read.
        self.refresh()
            .await
            .is_some_and(|s| s.collection_policy.allow_logs)
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
            reason: Some("mcp read tool".into()),
        },
    }
}

/// Build the read-only [`McpServer`] from the in-process read agent. `settings` is
/// the startup snapshot used to construct the agent; the policy gate
/// ([`ConfigPolicy`]) instead re-reads the persisted config per call so a
/// permission change takes effect without restarting the MCP process.
async fn build_mcp_server(args: Args, settings: Arc<SharedSettings>) -> McpServer {
    let audit: Arc<dyn AuditSink> = Arc::new(LogAuditSink);
    let agent =
        Arc::new(LocalDeviceAgent::with_settings(settings.clone()).with_audit(audit.clone()));

    McpServer::new(
        Arc::new(ServerReadProvider { agent }),
        Arc::new(ConfigPolicy { args, settings }),
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
        let server = build_mcp_server(shared.read().await.args.clone(), shared.clone()).await;
        serve_stdio(server).await.map_err(|e| {
            DeskError::from(std::io::Error::other(format!("mcp stdio server: {e}")))
        })?;
        Ok::<(), DeskError>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ConfigPolicy` reflects the persisted config on each query, so an operator
    /// flipping `allow_logs` in the config file takes effect without restarting
    /// the MCP process. Also fail-closed when the config is unreadable.
    #[tokio::test]
    async fn config_policy_reflects_persisted_allow_logs_live() {
        let dir = std::env::temp_dir().join(format!("mcp-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("config");
        let toml_path = dir.join("config.toml");
        let args = Args {
            config_file_path: base.to_string_lossy().to_string(),
            ..Default::default()
        };
        let policy = ConfigPolicy {
            args,
            settings: Arc::new(SharedSettings::from(Settings::default())),
        };

        std::fs::write(&toml_path, "[collection_policy]\nallow_logs = false\n").unwrap();
        assert!(!policy.allow_logs().await, "false in config → denied");

        std::fs::write(&toml_path, "[collection_policy]\nallow_logs = true\n").unwrap();
        assert!(
            policy.allow_logs().await,
            "live re-read picks up the change"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

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
}
