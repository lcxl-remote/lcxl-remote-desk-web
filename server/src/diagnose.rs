//! Server-side evidence collection for the central diagnose brain.
//!
//! AI diagnosis is orchestrated by the central signaling server; this host only
//! provides the capability side. The [`DiagnoseOrchestrator`] runs the
//! collect + fail-closed redact phases for an inbound `CollectRequest`
//! ([`collect_for_remote`](DiagnoseOrchestrator::collect_for_remote)) and never
//! dials a model. It is **mode-independent**: it depends only on the
//! [`ContextCollector`] and [`Redactor`] traits. The service-daemon path does not
//! run an orchestrator (the router replies `FEATURE_UNAVAILABLE`); Default /
//! DeskServer wire the in-process agent collector.

pub mod agent;
pub mod collector;
pub mod selection;

pub mod redaction {
    pub use crate::agent_adapter::redaction::*;
}

pub mod model {
    pub mod screenshot {
        pub use crate::agent_adapter::screenshot::*;
    }
}

use std::sync::Arc;

use async_trait::async_trait;
use desk_agent_protocol::diagnose::DiagnoseRequestData;
use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::worker::agent::eval::EvidenceSnapshot;
use redaction::{Redactor, redact_snapshot};

/// Collects read-only evidence for a diagnosis into an [`EvidenceSnapshot`]
/// (reusing the eval snapshot format). The policy gate over which capabilities
/// to read lives in the implementation ([`collector::AgentContextCollector`]);
/// the orchestrator runs the redaction pass over the returned snapshot. This
/// trait is the seam the in-process agent collector and a future service-daemon
/// IPC collector both implement.
#[async_trait]
pub trait ContextCollector: Send + Sync {
    async fn collect(&self, request_id: &str, request: &DiagnoseRequestData) -> EvidenceSnapshot;
}

/// Runs the collect + fail-closed redact phases for the central diagnose brain.
/// The model call, audit, and rendering happen centrally on the signaling
/// server; this host never dials a model. Redaction is fail-closed: a redactor
/// failure surfaces as an error so the central brain aborts before the model.
pub struct DiagnoseOrchestrator {
    collector: Arc<dyn ContextCollector>,
    redactor: Arc<dyn Redactor>,
}

impl DiagnoseOrchestrator {
    pub fn new(collector: Arc<dyn ContextCollector>, redactor: Arc<dyn Redactor>) -> Self {
        Self {
            collector,
            redactor,
        }
    }

    /// Collect and redact evidence for the central brain. Runs only the collect +
    /// fail-closed redact phases; the model call, audit, and rendering happen
    /// centrally on the signaling server. Raw screenshot bytes are stripped after
    /// the refit so only the small model-ready data URL travels off the host. A
    /// redaction failure returns an error (the central brain then aborts without
    /// calling the model — fail-closed end to end); no audit is recorded here,
    /// since the central brain owns the audit trail for the remote path.
    pub async fn collect_for_remote(
        &self,
        request_id: &str,
        request: &DiagnoseRequestData,
    ) -> Result<EvidenceSnapshot, AgentError> {
        let mut snapshot = self.collector.collect(request_id, request).await;
        if let Err(error) = redact_snapshot(self.redactor.as_ref(), &mut snapshot) {
            return Err(AgentError {
                kind: AgentErrorKind::RedactionFailed,
                message: format!("evidence redaction failed: {}", error.reason),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        }
        crate::diagnose::model::screenshot::strip_raw_screenshots(&mut snapshot);
        Ok(snapshot)
    }
}

/// Current time as an RFC3339 string (the evidence snapshot timestamp format).
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Skeleton collector that gathers nothing. Used where no in-process agent is
/// available (and in tests); a diagnosis then reports an empty `collected` list.
#[derive(Default)]
pub struct NoopContextCollector;

#[async_trait]
impl ContextCollector for NoopContextCollector {
    async fn collect(&self, _request_id: &str, _request: &DiagnoseRequestData) -> EvidenceSnapshot {
        EvidenceSnapshot::record("empty", "no evidence collected", now_rfc3339(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentOutcome, Capability};
    use redaction::{Redacted, RedactionError, RegexRedactor};

    /// A collector returning one Ok log outcome with redactable text, so the
    /// redaction pass actually runs over it.
    struct OkLogCollector;

    #[async_trait]
    impl ContextCollector for OkLogCollector {
        async fn collect(
            &self,
            _request_id: &str,
            _request: &DiagnoseRequestData,
        ) -> EvidenceSnapshot {
            use desk_agent_protocol::{
                LogEvent, LogRecentOutput, LogSeverity, OperationOutput, ReadContextOutput,
            };
            let out = OperationOutput::ReadContext(ReadContextOutput::LogRecent(LogRecentOutput {
                events: vec![LogEvent {
                    timestamp: "t".into(),
                    source: "s".into(),
                    severity: LogSeverity::Error,
                    message: "token=abc on failure".into(),
                    redactions: Vec::new(),
                }],
                truncated: false,
            }));
            EvidenceSnapshot::record(
                "test",
                "log",
                "2026-06-13T00:00:00Z",
                vec![(Capability::LogRecent, AgentOutcome::Ok(out))],
            )
        }
    }

    /// A redactor that always fails, exercising the fail-closed path.
    struct FailingRedactor;
    impl Redactor for FailingRedactor {
        fn redact(&self, _input: &str) -> Result<Redacted, RedactionError> {
            Err(RedactionError {
                reason: "redactor panic".into(),
            })
        }
    }

    fn request() -> DiagnoseRequestData {
        DiagnoseRequestData {
            question: "why?".to_string(),
            include_screen: false,
            context_kinds: vec![],
            locale: None,
            conversation_id: None,
            model_id: None,
            org_id: None,
        }
    }

    /// `collect_for_remote` returns the redacted snapshot for the central brain:
    /// the secret in the log evidence is scrubbed (the model call and audit happen
    /// centrally, not here).
    #[tokio::test]
    async fn collect_for_remote_returns_redacted_snapshot() {
        let orch =
            DiagnoseOrchestrator::new(Arc::new(OkLogCollector), Arc::new(RegexRedactor::new()));
        let snapshot = orch
            .collect_for_remote("req_rc", &request())
            .await
            .expect("collection succeeds");
        // The redaction pass ran: the token in the log message is scrubbed.
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(
            !json.contains("token=abc"),
            "secret must be redacted: {json}"
        );
        assert!(snapshot.contexts.iter().any(|c| !c.redactions.is_empty()));
    }

    /// `collect_for_remote` is fail-closed: a redactor failure returns an error
    /// (so the central brain never calls the model).
    #[tokio::test]
    async fn collect_for_remote_fails_closed_on_redactor_failure() {
        let orch = DiagnoseOrchestrator::new(Arc::new(OkLogCollector), Arc::new(FailingRedactor));
        let err = orch
            .collect_for_remote("req_rc_fail", &request())
            .await
            .expect_err("redaction failure must surface as an error");
        assert_eq!(err.kind, AgentErrorKind::RedactionFailed);
    }
}
