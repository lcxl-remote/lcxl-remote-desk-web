//! Model integration for the diagnose orchestrator.
//!
//! [`ModelBackedDiagnoseModel`] is the real [`super::DiagnoseModel`]: it reads
//! the configured model gateway ([`crate::model::settings::AiModelSettings`]),
//! assembles a structured prompt ([`prompt`]) from the redacted evidence, calls
//! a [`ModelAdapter`] (the streaming transport), streams summary tokens back, and
//! parses the response into a [`Diagnosis`] ([`parser`]). It is the single point
//! that knows the provider / model / token usage, so it emits the
//! `ai.model.requested` / `ai.model.responded` audit events itself.
//!
//! [`ModelAdapter`] isolates the wire protocol. The OpenAI-compatible
//! implementation lives in [`openai`]; tests substitute a mock so the
//! orchestration (prompt → stream → parse → audit) is verified without a
//! network.

pub mod openai;
pub mod parser;
pub mod prompt;
pub mod screenshot;

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use desk_agent_protocol::audit::{AuditEvent, AuditSink};
use desk_agent_protocol::diagnose::{Confidence, Diagnosis};
use desk_agent_protocol::{AgentError, CallerRef, CallerType};

use super::{DiagnoseModel, new_event_id, now_rfc3339};
use crate::model::settings::SharedSettings;
use crate::worker::agent::eval::EvidenceSnapshot;

/// Default model context budget when `max_context_bytes` is unset (128 KB,
/// security model §7).
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 131_072;

/// Role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    System,
    User,
}

impl ChatRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatRole::System => "system",
            ChatRole::User => "user",
        }
    }
}

/// One chat message. `image_data_url`, when set, is attached as a vision image
/// alongside the text (OpenAI multi-part content).
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
    pub image_data_url: Option<String>,
}

/// A chat-completion request to the model gateway.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

/// Token accounting reported by the gateway.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

/// The accumulated model response.
#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub content: String,
    pub usage: TokenUsage,
}

/// Streaming transport to a chat-completions gateway. `on_delta` is called with
/// each incremental content fragment as it arrives. Object-safe so the model
/// holds `Arc<dyn ModelAdapter>` and tests substitute a mock.
///
/// `?Send`: the OpenAI implementation uses `awc` (`!Send`), and the diagnose
/// path runs on actix's single-threaded runtime — see [`super::DiagnoseModel`].
#[async_trait(?Send)]
pub trait ModelAdapter: Send + Sync {
    async fn stream_chat(
        &self,
        request: ChatRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ChatResponse, AgentError>;

    /// Stable adapter identifier recorded in the audit trail.
    fn name(&self) -> &'static str;
}

/// The real diagnose model: prompt assembly + adapter call + parse + audit.
pub struct ModelBackedDiagnoseModel {
    adapter: Arc<dyn ModelAdapter>,
    settings: Arc<SharedSettings>,
    audit: Arc<dyn AuditSink>,
}

impl ModelBackedDiagnoseModel {
    pub fn new(
        adapter: Arc<dyn ModelAdapter>,
        settings: Arc<SharedSettings>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            adapter,
            settings,
            audit,
        }
    }
}

/// The low-confidence diagnosis returned when the model gateway is not
/// configured, so the end-to-end path still produces a useful UI message.
fn not_configured() -> Diagnosis {
    Diagnosis {
        summary: "AI model is not configured; diagnosis is unavailable.".to_string(),
        confidence: Confidence::Low,
        missing_info: vec![
            "Set the provider, model, base URL, and API key in AI model settings.".to_string(),
        ],
        ..Default::default()
    }
}

#[async_trait(?Send)]
impl DiagnoseModel for ModelBackedDiagnoseModel {
    async fn diagnose(
        &self,
        request_id: &str,
        question: &str,
        evidence: &EvidenceSnapshot,
        on_partial: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Diagnosis, AgentError> {
        let config = { self.settings.read().await.ai_model.clone() };
        let (Some(model), Some(base_url), Some(api_key)) =
            (config.model, config.base_url, config.api_key)
        else {
            return Ok(not_configured());
        };
        if api_key.is_empty() || base_url.is_empty() || model.is_empty() {
            return Ok(not_configured());
        }
        let max_context_bytes = config
            .max_context_bytes
            .map(|b| b as usize)
            .unwrap_or(DEFAULT_MAX_CONTEXT_BYTES);

        let messages = prompt::build_messages(question, evidence, max_context_bytes);
        let caller = CallerRef {
            caller_type: CallerType::AiModel,
            model_provider: config.provider.clone(),
            model_name: Some(model.clone()),
            adapter: Some(self.adapter.name().to_string()),
        };

        // `ai.model.requested` — input token count is not known until the
        // gateway reports usage, so it is left unset here.
        self.audit
            .record(AuditEvent::model_requested(
                new_event_id(),
                now_rfc3339(),
                request_id,
                &caller,
                format!("evidence: {} contexts", evidence.contexts.len()),
                None,
            ))
            .await;

        let started = Instant::now();
        let request = ChatRequest {
            base_url,
            api_key,
            model,
            messages,
        };
        let response = self.adapter.stream_chat(request, on_partial).await?;
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

        let diagnosis = parser::parse_diagnosis(&response.content);
        self.audit
            .record(AuditEvent::model_responded(
                new_event_id(),
                now_rfc3339(),
                request_id,
                &caller,
                format!(
                    "diagnosis: {} findings, {} commands",
                    diagnosis.findings.len(),
                    diagnosis.commands.len()
                ),
                response.usage.input_tokens,
                response.usage.output_tokens,
                duration_ms,
            ))
            .await;

        Ok(diagnosis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::Capability;
    use desk_agent_protocol::audit::AuditEventType;
    use std::sync::Mutex;

    use crate::model::settings::Settings;

    /// A mock adapter that streams fixed fragments and returns a canned response.
    struct MockAdapter {
        fragments: Vec<String>,
        content: String,
        usage: TokenUsage,
        seen: Mutex<Option<ChatRequest>>,
    }

    #[async_trait(?Send)]
    impl ModelAdapter for MockAdapter {
        async fn stream_chat(
            &self,
            request: ChatRequest,
            on_delta: &(dyn Fn(String) + Send + Sync),
        ) -> Result<ChatResponse, AgentError> {
            *self.seen.lock().unwrap() = Some(request);
            for f in &self.fragments {
                on_delta(f.clone());
            }
            Ok(ChatResponse {
                content: self.content.clone(),
                usage: self.usage,
            })
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    /// An adapter that always errors (transport failure).
    struct FailingAdapter;
    #[async_trait(?Send)]
    impl ModelAdapter for FailingAdapter {
        async fn stream_chat(
            &self,
            _request: ChatRequest,
            _on_delta: &(dyn Fn(String) + Send + Sync),
        ) -> Result<ChatResponse, AgentError> {
            Err(AgentError {
                kind: desk_agent_protocol::AgentErrorKind::TransportError,
                message: "connection refused".into(),
                retryable: true,
                safe_for_model: true,
            })
        }
        fn name(&self) -> &'static str {
            "failing"
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

    fn configured_settings() -> Arc<SharedSettings> {
        let mut s = Settings::default();
        s.ai_model.provider = Some("openai-compatible".into());
        s.ai_model.model = Some("example-model".into());
        s.ai_model.base_url = Some("https://api.example/v1".into());
        s.ai_model.api_key = Some("sk-test".into());
        Arc::new(SharedSettings::from(s))
    }

    fn snapshot() -> EvidenceSnapshot {
        EvidenceSnapshot::record(
            "live",
            "q",
            "2026-06-13T00:00:00Z",
            vec![(
                Capability::SystemInfo,
                desk_agent_protocol::AgentOutcome::Err(AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::Internal,
                    message: "x".into(),
                    retryable: false,
                    safe_for_model: true,
                }),
            )],
        )
    }

    const WELL_FORMED: &str = r#"{"summary":"busy cpu","confidence":"high","findings":[{"title":"f","evidence_refs":[],"explanation":"e"}],"commands":[{"shell":"sh","command":"ps","purpose":"p","risk":"low","requires_confirmation":false}],"next_steps":[],"missing_info":[]}"#;

    /// The happy path: prompt sent, fragments streamed, response parsed, and both
    /// model audit events recorded with token usage.
    #[tokio::test]
    async fn streams_parses_and_audits() {
        let adapter = Arc::new(MockAdapter {
            fragments: vec!["busy".into(), " cpu".into()],
            content: WELL_FORMED.into(),
            usage: TokenUsage {
                input_tokens: Some(1200),
                output_tokens: Some(80),
            },
            seen: Mutex::new(None),
        });
        let audit = RecordingAuditSink::default();
        let model = ModelBackedDiagnoseModel::new(
            adapter.clone(),
            configured_settings(),
            Arc::new(audit.clone()),
        );

        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let p = partials.clone();
        let on_partial = move |s: String| p.lock().unwrap().push(s);
        let diag = model
            .diagnose("req_1", "why cpu high?", &snapshot(), &on_partial)
            .await
            .expect("diagnose ok");

        // Parsed structured result.
        assert_eq!(diag.confidence, Confidence::High);
        assert_eq!(diag.commands.len(), 1);
        // Streamed fragments reached on_partial.
        assert_eq!(*partials.lock().unwrap(), vec!["busy", " cpu"]);
        // The request carried the model + a system and user message.
        let req = adapter.seen.lock().unwrap().clone().unwrap();
        assert_eq!(req.model, "example-model");
        assert_eq!(req.messages.len(), 2);
        // Both model audit events, with token usage on the response.
        let events = audit.events.lock().unwrap();
        let types: Vec<_> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&AuditEventType::ModelRequested.as_str()));
        assert!(types.contains(&AuditEventType::ModelResponded.as_str()));
        let responded = events
            .iter()
            .find(|e| e.event_type == AuditEventType::ModelResponded.as_str())
            .unwrap();
        assert_eq!(responded.input_tokens, Some(1200));
        assert_eq!(responded.output_tokens, Some(80));
    }

    /// Without configuration the model returns the not-configured diagnosis and
    /// never calls the adapter (no audit).
    #[tokio::test]
    async fn unconfigured_returns_not_configured() {
        let adapter = Arc::new(MockAdapter {
            fragments: vec![],
            content: WELL_FORMED.into(),
            usage: TokenUsage::default(),
            seen: Mutex::new(None),
        });
        let audit = RecordingAuditSink::default();
        let model = ModelBackedDiagnoseModel::new(
            adapter.clone(),
            Arc::new(SharedSettings::from(Settings::default())),
            Arc::new(audit.clone()),
        );
        let noop = |_: String| {};
        let diag = model
            .diagnose("req_2", "q", &snapshot(), &noop)
            .await
            .expect("ok");
        assert_eq!(diag.confidence, Confidence::Low);
        assert!(adapter.seen.lock().unwrap().is_none());
        assert!(audit.events.lock().unwrap().is_empty());
    }

    /// An adapter transport error propagates as an `AgentError`.
    #[tokio::test]
    async fn adapter_error_propagates() {
        let audit = RecordingAuditSink::default();
        let model = ModelBackedDiagnoseModel::new(
            Arc::new(FailingAdapter),
            configured_settings(),
            Arc::new(audit.clone()),
        );
        let noop = |_: String| {};
        let err = model
            .diagnose("req_3", "q", &snapshot(), &noop)
            .await
            .expect_err("must error");
        assert_eq!(
            err.kind,
            desk_agent_protocol::AgentErrorKind::TransportError
        );
        // The request was still audited before the failure.
        assert_eq!(audit.events.lock().unwrap().len(), 1);
    }

    /// A malformed model response degrades to a low-confidence diagnosis (parser
    /// integration).
    #[tokio::test]
    async fn malformed_response_degrades() {
        let adapter = Arc::new(MockAdapter {
            fragments: vec!["text".into()],
            content: "not json at all".into(),
            usage: TokenUsage::default(),
            seen: Mutex::new(None),
        });
        let model = ModelBackedDiagnoseModel::new(
            adapter,
            configured_settings(),
            Arc::new(RecordingAuditSink::default()),
        );
        let noop = |_: String| {};
        let diag = model
            .diagnose("req_4", "q", &snapshot(), &noop)
            .await
            .unwrap();
        assert_eq!(diag.confidence, Confidence::Low);
        assert!(!diag.missing_info.is_empty());
    }
}
