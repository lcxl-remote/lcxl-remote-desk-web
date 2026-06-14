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
    DiagnoseAvailability, DiagnoseProvider, McpPolicy, McpServer, ReadContextProvider, serve_stdio,
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

/// Max diagnoses running at once. Each runs on its own OS thread (a full,
/// possibly slow model call), so cap concurrency to bound thread / resource use
/// under a client that pipelines tool calls.
const MAX_CONCURRENT_DIAGNOSES: usize = 4;

/// Diagnose provider: runs one non-streaming diagnosis. `include_screen` is
/// forced `false` — the MCP path never captures the screen. Each run executes on
/// a dedicated thread with its own actix `System` so the `!Send` model adapters
/// stay off the MCP runtime. A semaphore bounds concurrent runs.
struct ServerDiagnoseProvider {
    orchestrator: Arc<DiagnoseOrchestrator>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl ServerDiagnoseProvider {
    fn new(orchestrator: Arc<DiagnoseOrchestrator>) -> Self {
        Self {
            orchestrator,
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DIAGNOSES)),
        }
    }
}

#[async_trait]
impl DiagnoseProvider for ServerDiagnoseProvider {
    async fn diagnose(
        &self,
        question: String,
        locale: Option<String>,
    ) -> Result<Diagnosis, AgentError> {
        // Bound concurrency. If the caller's future is dropped while waiting for a
        // permit, no thread is ever spawned — cancelling a not-yet-started run.
        // The permit is moved into the worker thread and released when it (and the
        // model call) finishes, so at most `MAX_CONCURRENT_DIAGNOSES` threads run.
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AgentError {
                kind: AgentErrorKind::Internal,
                message: "diagnose concurrency gate closed".to_string(),
                retryable: false,
                safe_for_model: true,
            })?;
        let orchestrator = self.orchestrator.clone();
        // A fresh id per call so each MCP diagnosis is its own correlation chain
        // in the evidence envelope and audit trail (concurrent runs do not share
        // one id).
        let request_id = mcp_request_id();
        let (mut tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _permit = permit;
            let system = actix_web::rt::System::new();
            system.block_on(async move {
                let sink = CapturingSink::default();
                let request = DiagnoseRequestData {
                    question,
                    // Hard off: an MCP client must never pull a screenshot.
                    include_screen: false,
                    context_kinds: Vec::new(),
                    locale,
                };
                let run = orchestrator.run(&request_id, request, &sink);
                tokio::pin!(run);
                // Cancel the in-flight diagnosis if the caller goes away: when the
                // MCP client disconnects / cancels, the outer future (and the
                // receiver) is dropped, so `tx.closed()` resolves. Dropping the
                // `run` future then aborts the model call at its next await point,
                // releasing the permit promptly instead of finishing wasted work.
                //
                // Boundary: the collect phase runs blocking probes via
                // `spawn_blocking` (read collectors are syscalls). Once such a
                // probe has started it cannot be interrupted by dropping `run`, so
                // a cancellation during collection lets the current local probes
                // finish (they are bounded and fast); only the async model dial is
                // truly cut short. See the M3 plan's deferred-hardening notes.
                let outcome = tokio::select! {
                    _ = &mut run => Some(sink.take_result()),
                    _ = tx.closed() => None,
                };
                if let Some(result) = outcome {
                    let _ = tx.send(result);
                }
            });
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

/// Live MCP policy that keeps the whole MCP diagnose stack on one fresh settings
/// source. On each gated call it re-reads the persisted config file and writes it
/// back into the shared `SharedSettings`, so the gate decision *and* the evidence
/// collector (`allow_logs` / `allow_screen`) *and* the model dial (provider /
/// base URL / key / gateway_mode) — all of which read this same `Arc` — see the
/// operator's latest config without a restart. The in-process settings are a
/// startup snapshot otherwise, and the file is the cross-process source of truth
/// the desk server writes to. Fail-closed: an unreadable config denies logs and
/// treats diagnosis as not configured (and leaves the existing settings intact).
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
        self.refresh().await.is_some_and(|s| s.ai_model.allow_logs)
    }

    async fn diagnose_availability(&self) -> DiagnoseAvailability {
        match self.refresh().await {
            Some(s) => {
                if s.ai_model.gateway_mode == GatewayMode::ManagerProxy {
                    DiagnoseAvailability::ManagerProxyUnavailable
                } else if s.ai_model.is_configured() {
                    DiagnoseAvailability::Available
                } else {
                    DiagnoseAvailability::NotConfigured
                }
            }
            None => DiagnoseAvailability::NotConfigured,
        }
    }
}

/// Captures the terminal frame (final diagnosis or error) of an orchestrator run.
#[derive(Default)]
struct CapturingSink {
    result: Mutex<Option<Result<Diagnosis, AgentError>>>,
}

impl CapturingSink {
    /// Take the captured terminal result. Borrows `&self` (not consuming) so it
    /// can be called while the orchestrator run future — which borrows the sink —
    /// is still pinned in the `select!` scope.
    fn take_result(&self) -> Result<Diagnosis, AgentError> {
        self.result
            .lock()
            .expect("capturing sink lock")
            .take()
            .unwrap_or_else(|| {
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

/// A unique correlation id for one MCP diagnosis, prefixed so audit / logs show
/// the request originated from the MCP path.
fn mcp_request_id() -> String {
    format!("mcp-{}", uuid::Uuid::new_v4())
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
/// `settings` is the startup snapshot used to construct the agent / model; the
/// policy gate ([`ConfigPolicy`]) instead re-reads the persisted config per call
/// so a permission change takes effect without restarting the MCP process.
async fn build_mcp_server(args: Args, settings: Arc<SharedSettings>) -> McpServer {
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
        Arc::new(ServerDiagnoseProvider::new(orchestrator)),
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
    use crate::diagnose::redaction::RegexRedactor;
    use crate::diagnose::{ContextCollector, DiagnoseModel, NoopContextCollector};
    use crate::worker::agent::eval::EvidenceSnapshot;
    use desk_agent_protocol::audit::NoopAuditSink;
    use desk_agent_protocol::diagnose::Diagnosis;

    /// `mcp_request_id` is unique per call and carries the `mcp-` prefix.
    #[test]
    fn mcp_request_id_is_unique_and_prefixed() {
        let a = mcp_request_id();
        let b = mcp_request_id();
        assert_ne!(a, b);
        assert!(a.starts_with("mcp-") && b.starts_with("mcp-"));
    }

    /// Each MCP diagnosis carries a distinct correlation id into the orchestrator
    /// (so audit / evidence chains never collide across concurrent runs).
    #[tokio::test]
    async fn each_mcp_diagnosis_gets_a_unique_request_id() {
        struct RecordingModel {
            ids: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait(?Send)]
        impl DiagnoseModel for RecordingModel {
            async fn diagnose(
                &self,
                request_id: &str,
                _question: &str,
                _evidence: &EvidenceSnapshot,
                _locale: Option<&str>,
                _on_partial: &(dyn Fn(String) + Send + Sync),
            ) -> Result<Diagnosis, AgentError> {
                self.ids.lock().unwrap().push(request_id.to_string());
                Ok(Diagnosis::default())
            }
        }

        let ids = Arc::new(Mutex::new(Vec::new()));
        let orchestrator = Arc::new(DiagnoseOrchestrator::new(
            Arc::new(NoopContextCollector),
            Arc::new(RegexRedactor::new()),
            Arc::new(RecordingModel { ids: ids.clone() }),
            Arc::new(NoopAuditSink),
        ));
        let provider = ServerDiagnoseProvider::new(orchestrator);
        provider.diagnose("q1".into(), None).await.unwrap();
        provider.diagnose("q2".into(), None).await.unwrap();

        let ids = ids.lock().unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "each diagnosis must use a distinct request_id"
        );
        assert!(ids.iter().all(|id| id.starts_with("mcp-")));
    }

    /// When the caller drops the diagnose future (the MCP client disconnected /
    /// cancelled), the in-flight model call is aborted rather than running to
    /// completion — the worker observes the dropped receiver and drops the run
    /// future, which cancels the model at its next await point.
    #[tokio::test]
    async fn mcp_diagnosis_is_cancelled_when_caller_drops() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropGuard(Arc<AtomicBool>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct BlockingModel {
            started: Arc<AtomicBool>,
            cancelled: Arc<AtomicBool>,
        }
        #[async_trait(?Send)]
        impl DiagnoseModel for BlockingModel {
            async fn diagnose(
                &self,
                _request_id: &str,
                _question: &str,
                _evidence: &EvidenceSnapshot,
                _locale: Option<&str>,
                _on_partial: &(dyn Fn(String) + Send + Sync),
            ) -> Result<Diagnosis, AgentError> {
                self.started.store(true, Ordering::SeqCst);
                // Set when this future is dropped (i.e. cancelled).
                let _guard = DropGuard(self.cancelled.clone());
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let started = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let orchestrator = Arc::new(DiagnoseOrchestrator::new(
            Arc::new(NoopContextCollector),
            Arc::new(RegexRedactor::new()),
            Arc::new(BlockingModel {
                started: started.clone(),
                cancelled: cancelled.clone(),
            }),
            Arc::new(NoopAuditSink),
        ));
        let provider = Arc::new(ServerDiagnoseProvider::new(orchestrator));

        let p = provider.clone();
        let task = tokio::spawn(async move {
            let _ = p.diagnose("q".into(), None).await;
        });

        // Wait until the model call has started on its worker thread.
        for _ in 0..2000 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            started.load(Ordering::SeqCst),
            "model call should have started"
        );

        // Caller goes away → diagnose future dropped → receiver dropped → cancel.
        task.abort();
        for _ in 0..2000 {
            if cancelled.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            cancelled.load(Ordering::SeqCst),
            "dropping the caller must cancel the in-flight diagnosis"
        );
    }

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

        std::fs::write(&toml_path, "[ai_model]\nallow_logs = false\n").unwrap();
        assert!(!policy.allow_logs().await, "false in config → denied");

        std::fs::write(&toml_path, "[ai_model]\nallow_logs = true\n").unwrap();
        assert!(
            policy.allow_logs().await,
            "live re-read picks up the change"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `ConfigPolicy::diagnose_availability` mirrors the gate precedence from the
    /// persisted config: manager_proxy wins over configuration completeness.
    #[tokio::test]
    async fn config_policy_diagnose_availability_precedence() {
        let dir = std::env::temp_dir().join(format!("mcp-policy-av-{}", uuid::Uuid::new_v4()));
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

        // No credentials → not configured.
        std::fs::write(&toml_path, "[ai_model]\n").unwrap();
        assert_eq!(
            policy.diagnose_availability().await,
            DiagnoseAvailability::NotConfigured
        );

        // Manager proxy selected (still no credentials) → proxy wins.
        std::fs::write(&toml_path, "[ai_model]\ngateway_mode = \"manager_proxy\"\n").unwrap();
        assert_eq!(
            policy.diagnose_availability().await,
            DiagnoseAvailability::ManagerProxyUnavailable
        );

        // Direct + fully configured → available.
        std::fs::write(
            &toml_path,
            "[ai_model]\nmodel = \"m\"\nbase_url = \"http://x/v1\"\napi_key = \"k\"\n",
        )
        .unwrap();
        assert_eq!(
            policy.diagnose_availability().await,
            DiagnoseAvailability::Available
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// End-to-end: with a startup snapshot of `allow_logs = true`, an operator
    /// revoking it in the config file means the next MCP diagnosis (which runs the
    /// gate first) collects no `log.recent` evidence — the gate's refresh
    /// republishes the live policy into the shared settings the collector reads,
    /// closing the indirect-log-leak path.
    #[tokio::test]
    async fn mcp_diagnose_collection_respects_live_allow_logs() {
        // Startup snapshot: logs allowed.
        let mut startup = Settings::default();
        startup.ai_model.allow_logs = true;
        let shared = Arc::new(SharedSettings::from(startup));

        // Operator revokes logs in the persisted config.
        let dir = std::env::temp_dir().join(format!("mcp-live-logs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("config");
        std::fs::write(dir.join("config.toml"), "[ai_model]\nallow_logs = false\n").unwrap();
        let args = Args {
            config_file_path: base.to_string_lossy().to_string(),
            ..Default::default()
        };
        let policy = ConfigPolicy {
            args,
            settings: shared.clone(),
        };

        // The diagnose gate runs first and refreshes the shared settings.
        let _ = policy.diagnose_availability().await;
        assert!(
            !shared.read().await.ai_model.allow_logs,
            "gate must publish the live allow_logs=false into shared settings"
        );

        // The collector reads the same shared settings → excludes log.recent even
        // though it was in the default set and the startup snapshot allowed it.
        let collector =
            AgentContextCollector::new(Arc::new(LocalDeviceAgent::new()), shared.clone());
        let request = DiagnoseRequestData {
            question: "why is the host slow?".into(),
            include_screen: false,
            context_kinds: Vec::new(),
            locale: None,
        };
        let snapshot = collector.collect("mcp-live", &request).await;
        assert!(
            !snapshot
                .contexts
                .iter()
                .any(|c| c.capability == "log.recent"),
            "live allow_logs=false must keep log.recent out of MCP evidence"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The gate's refresh republishes the *whole* model config into the shared
    /// settings the model dials from, so a changed provider / base URL / key takes
    /// effect on the next diagnosis (no gate-vs-dial split-brain).
    #[tokio::test]
    async fn config_policy_refresh_republishes_model_config() {
        let mut startup = Settings::default();
        startup.ai_model.model = Some("old-model".into());
        startup.ai_model.base_url = Some("http://old/v1".into());
        let shared = Arc::new(SharedSettings::from(startup));

        let dir = std::env::temp_dir().join(format!("mcp-live-model-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("config");
        std::fs::write(
            dir.join("config.toml"),
            "[ai_model]\nprovider = \"anthropic\"\nmodel = \"new-model\"\nbase_url = \"https://api.anthropic.com\"\napi_key = \"k\"\n",
        )
        .unwrap();
        let args = Args {
            config_file_path: base.to_string_lossy().to_string(),
            ..Default::default()
        };
        let policy = ConfigPolicy {
            args,
            settings: shared.clone(),
        };

        assert_eq!(
            policy.diagnose_availability().await,
            DiagnoseAvailability::Available
        );
        let ai = shared.read().await.ai_model.clone();
        assert_eq!(ai.model.as_deref(), Some("new-model"));
        assert_eq!(ai.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(ai.provider.as_deref(), Some("anthropic"));

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
        let result = sink.take_result().expect("final diagnosis captured");
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
        let err = sink.take_result().expect_err("error frame yields Err");
        assert_eq!(err.kind, AgentErrorKind::TransportError);
    }
}
