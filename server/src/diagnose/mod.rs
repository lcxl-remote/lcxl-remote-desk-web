//! Server-side AI Diagnose orchestrator.
//!
//! The orchestrator is the AI-orchestration layer for single-machine diagnosis:
//! it takes a user question + collection options, collects read-only evidence,
//! (redacts, in a later PR), calls the model, and streams a structured
//! [`Diagnosis`] back as [`DiagnoseEvent`] frames.
//!
//! It is **mode-independent**: it depends only on the [`ContextCollector`] and
//! [`DiagnoseModel`] traits and a [`DiagnoseEventSink`]. The daemon router wires
//! the real implementations (Default / DeskServer); tests substitute mocks. The
//! service-daemon path does not run an orchestrator (the router replies
//! `FEATURE_UNAVAILABLE`), so cross-process collection is a later additive step
//! behind [`ContextCollector`].
//!
//! The model call is **stubbed** here; the real adapter (with redaction, prompt
//! assembly, streaming, and token accounting) lands in a later PR.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use desk_agent_protocol::AgentError;
#[cfg(test)]
use desk_agent_protocol::AgentErrorKind;
use desk_agent_protocol::diagnose::{Confidence, DiagnoseEvent, DiagnoseRequestData, Diagnosis};

/// Receives streamed diagnose frames. The router implements this over the
/// connection's outbound channel (emitting notification-style `DiagnoseEvent`
/// signaling frames); tests record into a buffer.
pub trait DiagnoseEventSink: Send + Sync {
    fn emit(&self, event: DiagnoseEvent);
}

/// Collects read-only evidence for a diagnosis. Returns the dotted capability
/// names actually collected (for [`Diagnosis::collected`]). Evidence assembly,
/// the `allow_logs` / `allow_screen` policy gate, and redaction land in later
/// PRs; this trait is the seam the in-process [`crate::worker::agent`] collector
/// and a future service-daemon IPC collector both implement.
#[async_trait]
pub trait ContextCollector: Send + Sync {
    async fn collect(&self, request: &DiagnoseRequestData) -> Vec<String>;
}

/// Produces a diagnosis from the question + collected evidence. Streaming is
/// surfaced through `on_partial` (each call becomes a `Partial` frame). The real
/// model adapter replaces [`StubDiagnoseModel`] in a later PR.
#[async_trait]
pub trait DiagnoseModel: Send + Sync {
    async fn diagnose(
        &self,
        question: &str,
        collected: &[String],
        on_partial: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Diagnosis, AgentError>;
}

/// Drives the diagnose state machine: collect → model → render, emitting
/// `DiagnoseEvent` frames with a monotonic `seq`.
pub struct DiagnoseOrchestrator {
    collector: Arc<dyn ContextCollector>,
    model: Arc<dyn DiagnoseModel>,
}

impl DiagnoseOrchestrator {
    pub fn new(collector: Arc<dyn ContextCollector>, model: Arc<dyn DiagnoseModel>) -> Self {
        Self { collector, model }
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

        sink.emit(DiagnoseEvent::status(request_id, next(), "collecting"));
        let collected = self.collector.collect(&request).await;

        sink.emit(DiagnoseEvent::status(request_id, next(), "modeling"));
        let on_partial = |fragment: String| {
            sink.emit(DiagnoseEvent::partial(request_id, next(), fragment));
        };

        match self
            .model
            .diagnose(&request.question, &collected, &on_partial)
            .await
        {
            Ok(mut diagnosis) => {
                // The orchestrator owns the authoritative collected list.
                diagnosis.collected = collected;
                sink.emit(DiagnoseEvent::final_result(request_id, next(), diagnosis));
            }
            Err(error) => {
                sink.emit(DiagnoseEvent::error(request_id, next(), error));
            }
        }
    }
}

/// Skeleton collector that gathers nothing yet. The real collector (wrapping the
/// in-process device agent, with the policy gate + redaction) lands in a later
/// PR; until then a diagnosis reports an empty `collected` list.
#[derive(Default)]
pub struct NoopContextCollector;

#[async_trait]
impl ContextCollector for NoopContextCollector {
    async fn collect(&self, _request: &DiagnoseRequestData) -> Vec<String> {
        Vec::new()
    }
}

/// Placeholder model used until the real adapter lands. It streams a short
/// notice and returns a low-confidence diagnosis stating the model is not yet
/// configured, so the end-to-end path (signaling → orchestrator → streamed
/// frames → UI) is exercisable before the adapter exists.
#[derive(Default)]
pub struct StubDiagnoseModel;

#[async_trait]
impl DiagnoseModel for StubDiagnoseModel {
    async fn diagnose(
        &self,
        _question: &str,
        _collected: &[String],
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
#[async_trait]
impl DiagnoseModel for FailingDiagnoseModel {
    async fn diagnose(
        &self,
        _question: &str,
        _collected: &[String],
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
    use desk_agent_protocol::diagnose::DiagnoseEventKind;
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

    /// A collector that reports a fixed set of collected capabilities.
    struct FixedCollector(Vec<String>);

    #[async_trait]
    impl ContextCollector for FixedCollector {
        async fn collect(&self, _request: &DiagnoseRequestData) -> Vec<String> {
            self.0.clone()
        }
    }

    /// A model that streams N partial fragments then returns a final diagnosis.
    struct StreamingModel {
        fragments: Vec<String>,
    }

    #[async_trait]
    impl DiagnoseModel for StreamingModel {
        async fn diagnose(
            &self,
            _question: &str,
            _collected: &[String],
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

    fn request() -> DiagnoseRequestData {
        DiagnoseRequestData {
            question: "why?".to_string(),
            include_screen: false,
            context_kinds: vec![],
        }
    }

    /// A successful run streams: status(collecting) → status(modeling) →
    /// every partial → final. Multiple partials all arrive, in order — this is
    /// the orchestrator-side half of the "frames are not collapsed into one
    /// response" guarantee (the signaling-side half is the router's
    /// `response_state = None`).
    #[tokio::test]
    async fn run_streams_status_partials_then_final() {
        let orch = DiagnoseOrchestrator::new(
            Arc::new(FixedCollector(vec![
                "system.info".into(),
                "process.list".into(),
            ])),
            Arc::new(StreamingModel {
                fragments: vec!["a".into(), "b".into(), "c".into()],
            }),
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
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Partial,
                DiagnoseEventKind::Final,
            ]
        );
        // seq is monotonic and gapless.
        let seqs: Vec<_> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5]);
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
            Arc::new(FailingDiagnoseModel),
        );
        let sink = RecordingSink::default();
        orch.run("req_2", request(), &sink).await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.last().unwrap().kind, DiagnoseEventKind::Error);
        assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
        let err = events.last().unwrap().error.as_ref().unwrap();
        assert_eq!(err.kind, AgentErrorKind::Internal);
    }

    /// The wired stub returns a low-confidence "not configured" diagnosis and
    /// still streams partials, so the path is exercisable before the adapter.
    #[tokio::test]
    async fn stub_model_yields_not_configured_diagnosis() {
        let orch =
            DiagnoseOrchestrator::new(Arc::new(NoopContextCollector), Arc::new(StubDiagnoseModel));
        let sink = RecordingSink::default();
        orch.run("req_3", request(), &sink).await;

        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|e| e.kind == DiagnoseEventKind::Partial));
        let final_diag = events.last().unwrap().final_result.as_ref().unwrap();
        assert_eq!(final_diag.confidence, Confidence::Low);
        assert!(!final_diag.missing_info.is_empty());
    }
}
