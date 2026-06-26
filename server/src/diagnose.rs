//! Server-side AI Diagnose orchestrator.
//!
//! The orchestrator is the AI-orchestration layer for single-machine diagnosis:
//! it takes a user question + collection options, collects read-only evidence,
//! redacts it (fail-closed), calls the model, and streams a structured
//! [`Diagnosis`] back as [`DiagnoseEvent`] frames.
//!
//! It is **mode-independent**: it depends only on the [`ContextCollector`],
//! [`Redactor`], and [`DiagnoseModel`] traits and a [`DiagnoseEventSink`]. The
//! daemon router wires the real implementations (Default / DeskServer); tests
//! substitute mocks. The service-daemon path does not run an orchestrator (the
//! router replies `FEATURE_UNAVAILABLE`), so cross-process collection is a later
//! additive step behind [`ContextCollector`].
//!
//! The real model integration ([`model::ModelBackedDiagnoseModel`]) assembles
//! the prompt, streams tokens, parses the response, and accounts tokens; a
//! [`StubDiagnoseModel`] remains for the unconfigured / test paths. Evidence is
//! always redacted before it reaches the model trait.

pub mod agent;
pub mod collector;
pub mod model;
pub mod redaction;
pub mod remote_read;
pub mod selection;
pub mod terminal_complete;
pub mod terminal_copilot;

#[cfg(test)]
mod eval;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use desk_agent_protocol::audit::{AuditEvent, AuditSink};
use desk_agent_protocol::diagnose::{Confidence, DiagnoseEvent, DiagnoseRequestData, Diagnosis};
use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::worker::agent::eval::EvidenceSnapshot;
use redaction::{Redactor, redact_snapshot};

/// Receives streamed diagnose frames. The router implements this over the
/// connection's outbound channel (emitting notification-style `DiagnoseEvent`
/// signaling frames); tests record into a buffer.
pub trait DiagnoseEventSink: Send + Sync {
    fn emit(&self, event: DiagnoseEvent);
}

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

/// Produces a diagnosis from the question + already-redacted evidence. Streaming
/// is surfaced through `on_partial` (each call becomes a `Partial` frame).
/// `locale` is the BCP-47 tag of the control-end UI so the answer matches the
/// user's language (`None` = model default).
///
/// `?Send`: the OpenAI adapter uses `awc`, whose client/futures are `!Send`
/// (actix runs single-threaded per worker). The diagnose path is driven inline
/// on the signaling proxy's actix runtime, so a non-`Send` future is fine; the
/// trait object stays `Send + Sync` (only the returned future is `!Send`).
#[async_trait(?Send)]
pub trait DiagnoseModel: Send + Sync {
    async fn diagnose(
        &self,
        request_id: &str,
        question: &str,
        evidence: &EvidenceSnapshot,
        locale: Option<&str>,
        on_partial: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Diagnosis, AgentError>;
}

/// Drives the diagnose state machine: collect → redact → model → render,
/// emitting `DiagnoseEvent` frames with a monotonic `seq`. Redaction is
/// fail-closed: a redactor failure aborts before the model is called.
pub struct DiagnoseOrchestrator {
    collector: Arc<dyn ContextCollector>,
    redactor: Arc<dyn Redactor>,
    model: Arc<dyn DiagnoseModel>,
    audit: Arc<dyn AuditSink>,
}

impl DiagnoseOrchestrator {
    pub fn new(
        collector: Arc<dyn ContextCollector>,
        redactor: Arc<dyn Redactor>,
        model: Arc<dyn DiagnoseModel>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            collector,
            redactor,
            model,
            audit,
        }
    }

    /// Run one diagnosis, streaming frames to `sink`. Always terminates with a
    /// single `Final` or `Error` frame.
    pub async fn run(
        &self,
        request_id: &str,
        request: DiagnoseRequestData,
        sink: &dyn DiagnoseEventSink,
    ) {
        let seq = AtomicU32::new(0);
        let next = || seq.fetch_add(1, Ordering::Relaxed);

        // Phase 1: collect.
        sink.emit(DiagnoseEvent::status(request_id, next(), "collecting"));
        let mut snapshot = self.collector.collect(request_id, &request).await;

        // Phase 2: redact (fail-closed). On failure, never send to the model.
        sink.emit(DiagnoseEvent::status(request_id, next(), "redacting"));
        if let Err(error) = redact_snapshot(self.redactor.as_ref(), &mut snapshot) {
            self.audit
                .record(AuditEvent::redaction_failed(
                    new_event_id(),
                    now_rfc3339(),
                    request_id,
                    &error.reason,
                ))
                .await;
            sink.emit(DiagnoseEvent::error(
                request_id,
                next(),
                AgentError {
                    kind: AgentErrorKind::RedactionFailed,
                    message: "evidence redaction failed; diagnosis aborted".to_string(),
                    retryable: false,
                    safe_for_model: true,
                },
            ));
            return;
        }

        // Phase 3: model.
        sink.emit(DiagnoseEvent::status(request_id, next(), "modeling"));
        let on_partial = |fragment: String| {
            sink.emit(DiagnoseEvent::partial(request_id, next(), fragment));
        };

        match self
            .model
            .diagnose(
                request_id,
                &request.question,
                &snapshot,
                request.locale.as_deref(),
                &on_partial,
            )
            .await
        {
            Ok(mut diagnosis) => {
                // The orchestrator owns the authoritative collected list.
                diagnosis.collected = snapshot
                    .contexts
                    .iter()
                    .map(|c| c.capability.clone())
                    .collect();
                sink.emit(DiagnoseEvent::final_result(request_id, next(), diagnosis));
            }
            Err(error) => {
                sink.emit(DiagnoseEvent::error(request_id, next(), error));
            }
        }
    }

    /// Collect and redact evidence for a **remote** orchestrator (the central
    /// brain). Runs only the collect + fail-closed redact phases of [`run`]; the
    /// model call, audit, and rendering happen centrally on the manager. Raw
    /// screenshot bytes are stripped after the refit so only the small model-ready
    /// data URL travels off the host. A redaction failure returns an error (the
    /// manager then aborts without calling the model — fail-closed end to end);
    /// no audit is recorded here, since the central brain owns the audit trail for
    /// the remote path.
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
            });
        }
        crate::diagnose::model::screenshot::strip_raw_screenshots(&mut snapshot);
        Ok(snapshot)
    }

    /// Record that the operator handed a diagnosis off to a human ("转人工").
    /// Handoff is a UI-side action with no orchestrator state-machine branch;
    /// this only emits the `ai.task.cancelled` audit so the handoff is
    /// auditable. `request_id` correlates the cancelled diagnosis.
    pub async fn audit_cancellation(&self, request_id: &str) {
        self.audit
            .record(AuditEvent::task_cancelled(
                new_event_id(),
                now_rfc3339(),
                request_id,
            ))
            .await;
    }
}

/// Fresh audit event identifier.
fn new_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Current time as an RFC3339 string (the audit event timestamp format).
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

/// Placeholder model used until the real adapter lands. It streams a short
/// notice and returns a low-confidence diagnosis stating the model is not yet
/// configured, so the end-to-end path (signaling → orchestrator → streamed
/// frames → UI) is exercisable before the adapter exists.
#[derive(Default)]
pub struct StubDiagnoseModel;

#[async_trait(?Send)]
impl DiagnoseModel for StubDiagnoseModel {
    async fn diagnose(
        &self,
        _request_id: &str,
        _question: &str,
        _evidence: &EvidenceSnapshot,
        _locale: Option<&str>,
        on_partial: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Diagnosis, AgentError> {
        on_partial("AI diagnosis is not configured yet. ".to_string());
        on_partial("Connect a model to enable real analysis.".to_string());
        Ok(Diagnosis {
            summary: "AI model is not configured; diagnosis is unavailable.".to_string(),
            confidence: Confidence::Low,
            missing_info: vec!["A model adapter is not yet wired (skeleton).".to_string()],
            ..Default::default()
        })
    }
}

/// A model that always fails — used to exercise the orchestrator's error path.
#[cfg(test)]
#[derive(Default)]
pub struct FailingDiagnoseModel;

#[cfg(test)]
#[async_trait(?Send)]
impl DiagnoseModel for FailingDiagnoseModel {
    async fn diagnose(
        &self,
        _request_id: &str,
        _question: &str,
        _evidence: &EvidenceSnapshot,
        _locale: Option<&str>,
        _on_partial: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Diagnosis, AgentError> {
        Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: "model exploded".to_string(),
            retryable: true,
            safe_for_model: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::audit::NoopAuditSink;
    use desk_agent_protocol::diagnose::DiagnoseEventKind;
    use desk_agent_protocol::{AgentOutcome, Capability};
    use redaction::{Redacted, RedactionError, RegexRedactor};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<DiagnoseEvent>>,
    }

    impl DiagnoseEventSink for RecordingSink {
        fn emit(&self, event: DiagnoseEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[derive(Clone, Default)]
    struct RecordingAuditSink {
        events: Arc<Mutex<Vec<AuditEvent>>>,
    }

    #[async_trait]
    impl AuditSink for RecordingAuditSink {
        async fn record(&self, event: AuditEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    /// A collector that reports a fixed set of collected capabilities (with Err
    /// outcomes — the capability names are what the test asserts on).
    struct FixedCollector(Vec<Capability>);

    #[async_trait]
    impl ContextCollector for FixedCollector {
        async fn collect(
            &self,
            _request_id: &str,
            _request: &DiagnoseRequestData,
        ) -> EvidenceSnapshot {
            let entries = self
                .0
                .iter()
                .map(|cap| {
                    (
                        *cap,
                        AgentOutcome::Err(AgentError {
                            kind: AgentErrorKind::UnsupportedCapability,
                            message: "stub".into(),
                            retryable: false,
                            safe_for_model: true,
                        }),
                    )
                })
                .collect();
            EvidenceSnapshot::record("test", "fixed", "2026-06-13T00:00:00Z", entries)
        }
    }

    /// A model that streams N partial fragments then returns a final diagnosis.
    struct StreamingModel {
        fragments: Vec<String>,
    }

    #[async_trait(?Send)]
    impl DiagnoseModel for StreamingModel {
        async fn diagnose(
            &self,
            _request_id: &str,
            _question: &str,
            _evidence: &EvidenceSnapshot,
            _locale: Option<&str>,
            on_partial: &(dyn Fn(String) + Send + Sync),
        ) -> Result<Diagnosis, AgentError> {
            for f in &self.fragments {
                on_partial(f.clone());
            }
            Ok(Diagnosis {
                summary: "done".to_string(),
                confidence: Confidence::High,
                ..Default::default()
            })
        }
    }

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
        }
    }

    /// A successful run streams: status(collecting) → status(redacting) →
    /// status(modeling) → every partial → final. Multiple partials all arrive,
    /// in order — this is the orchestrator-side half of the "frames are not
    /// collapsed into one response" guarantee (the signaling-side half is the
    /// router's `response_state = None`).
    #[tokio::test]
    async fn run_streams_status_partials_then_final() {
        let orch = DiagnoseOrchestrator::new(
            Arc::new(FixedCollector(vec![
                Capability::SystemInfo,
                Capability::ProcessList,
            ])),
            Arc::new(RegexRedactor::new()),
            Arc::new(StreamingModel {
                fragments: vec!["a".into(), "b".into(), "c".into()],
            }),
            Arc::new(NoopAuditSink),
        );
        let sink = RecordingSink::default();
        orch.run("req_1", request(), &sink).await;

        let events = sink.events.lock().unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiagnoseEventKind::Status,
                DiagnoseEventKind::Status,
                DiagnoseEventKind::Status,
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Final,
            ]
        );
        // seq is monotonic and gapless.
        let seqs: Vec<_> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5, 6]);
        // The three status phases are named in order.
        let phases: Vec<_> = events.iter().filter_map(|e| e.status.clone()).collect();
        assert_eq!(phases, vec!["collecting", "redacting", "modeling"]);
        // Exactly one terminal frame, last.
        assert!(events.last().unwrap().is_terminal());
        assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
        // The orchestrator stamps the authoritative collected list onto the
        // final diagnosis.
        let final_diag = events.last().unwrap().final_result.as_ref().unwrap();
        assert_eq!(final_diag.collected, vec!["system.info", "process.list"]);
    }

    /// A model failure terminates the stream with a single `Error` frame.
    #[tokio::test]
    async fn run_emits_error_frame_on_model_failure() {
        let orch = DiagnoseOrchestrator::new(
            Arc::new(NoopContextCollector),
            Arc::new(RegexRedactor::new()),
            Arc::new(FailingDiagnoseModel),
            Arc::new(NoopAuditSink),
        );
        let sink = RecordingSink::default();
        orch.run("req_2", request(), &sink).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.last().unwrap().kind, DiagnoseEventKind::Error);
        assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
        let err = events.last().unwrap().error.as_ref().unwrap();
        assert_eq!(err.kind, AgentErrorKind::Internal);
    }

    /// A redactor failure aborts before the model: the stream ends with a
    /// `RedactionFailed` error frame (never reaching `modeling`) and an
    /// `ai.redaction.failed` audit event is recorded (fail-closed).
    #[tokio::test]
    async fn run_fails_closed_when_redactor_fails() {
        let audit = RecordingAuditSink::default();
        let orch = DiagnoseOrchestrator::new(
            Arc::new(OkLogCollector),
            Arc::new(FailingRedactor),
            Arc::new(StubDiagnoseModel),
            Arc::new(audit.clone()),
        );
        let sink = RecordingSink::default();
        orch.run("req_x", request(), &sink).await;

        let events = sink.events.lock().unwrap();
        // Never reached the modeling phase.
        let phases: Vec<_> = events.iter().filter_map(|e| e.status.clone()).collect();
        assert_eq!(phases, vec!["collecting", "redacting"]);
        // Terminal error is RedactionFailed.
        let last = events.last().unwrap();
        assert_eq!(last.kind, DiagnoseEventKind::Error);
        assert_eq!(
            last.error.as_ref().unwrap().kind,
            AgentErrorKind::RedactionFailed
        );
        // The fail-closed audit event was emitted.
        let audited = audit.events.lock().unwrap();
        assert_eq!(audited.len(), 1);
        assert_eq!(audited[0].event_type, "ai.redaction.failed");
        assert_eq!(audited[0].request_id, "req_x");
    }

    /// `collect_for_remote` returns the redacted snapshot for the central brain:
    /// the secret in the log evidence is scrubbed, and no model / audit runs here
    /// (the manager owns those for the remote path).
    #[tokio::test]
    async fn collect_for_remote_returns_redacted_snapshot() {
        let audit = RecordingAuditSink::default();
        let orch = DiagnoseOrchestrator::new(
            Arc::new(OkLogCollector),
            Arc::new(RegexRedactor::new()),
            Arc::new(StubDiagnoseModel),
            Arc::new(audit.clone()),
        );
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
        // No audit is recorded on the edge for the remote path.
        assert!(audit.events.lock().unwrap().is_empty());
    }

    /// `collect_for_remote` is fail-closed: a redactor failure returns an error
    /// (so the manager never calls the model) and records no edge audit.
    #[tokio::test]
    async fn collect_for_remote_fails_closed_on_redactor_failure() {
        let orch = DiagnoseOrchestrator::new(
            Arc::new(OkLogCollector),
            Arc::new(FailingRedactor),
            Arc::new(StubDiagnoseModel),
            Arc::new(NoopAuditSink),
        );
        let err = orch
            .collect_for_remote("req_rc_fail", &request())
            .await
            .expect_err("redaction failure must surface as an error");
        assert_eq!(err.kind, AgentErrorKind::RedactionFailed);
    }

    /// The wired stub returns a low-confidence "not configured" diagnosis and
    /// still streams partials, so the path is exercisable before the adapter.
    #[tokio::test]
    async fn stub_model_yields_not_configured_diagnosis() {
        let orch = DiagnoseOrchestrator::new(
            Arc::new(NoopContextCollector),
            Arc::new(RegexRedactor::new()),
            Arc::new(StubDiagnoseModel),
            Arc::new(NoopAuditSink),
        );
        let sink = RecordingSink::default();
        orch.run("req_3", request(), &sink).await;

        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|e| e.kind == DiagnoseEventKind::Partial));
        let final_diag = events.last().unwrap().final_result.as_ref().unwrap();
        assert_eq!(final_diag.confidence, Confidence::Low);
        assert!(!final_diag.missing_info.is_empty());
    }

    /// Handoff to a human records a single `ai.task.cancelled` audit correlated
    /// to the cancelled diagnosis, and streams no diagnose frames.
    #[tokio::test]
    async fn audit_cancellation_records_task_cancelled() {
        let audit = RecordingAuditSink::default();
        let orch = DiagnoseOrchestrator::new(
            Arc::new(NoopContextCollector),
            Arc::new(RegexRedactor::new()),
            Arc::new(StubDiagnoseModel),
            Arc::new(audit.clone()),
        );
        orch.audit_cancellation("req_handoff").await;

        let audited = audit.events.lock().unwrap();
        assert_eq!(audited.len(), 1);
        assert_eq!(audited[0].event_type, "ai.task.cancelled");
        assert_eq!(audited[0].request_id, "req_handoff");
    }
}
