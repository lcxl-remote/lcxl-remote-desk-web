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

pub mod anthropic;
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
use crate::model::settings::{GatewayMode, ResponseFormatMode, SharedSettings};
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

/// What `response_format` the gateway is asked for. The diagnosis-specific
/// schema (for [`ResponseFormatSpec::JsonSchema`]) is built by the model layer
/// and carried here, so the adapter stays generic and just serializes it.
#[derive(Debug, Clone)]
pub enum ResponseFormatSpec {
    /// Omit `response_format` entirely.
    None,
    /// `{"type":"json_object"}`.
    JsonObject,
    /// `{"type":"json_schema","json_schema":{name,strict:true,schema}}`.
    JsonSchema {
        name: String,
        schema: serde_json::Value,
    },
}

/// A chat-completion request to the model gateway.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub response_format: ResponseFormatSpec,
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

/// Resolve the [`ModelAdapter`] for a configured provider. The match is by
/// provider identifier; unknown / empty / absent providers fall back to the
/// OpenAI-compatible wire (the broadest compatibility).
///
/// The identifier is normalized (trimmed + lowercased) before matching so
/// `"Anthropic"`, `" anthropic "` and `"ANTHROPIC"` all resolve the same. An
/// unknown non-empty provider falls back but logs a warning, so a typo paired
/// with a mismatched base URL / auth header is visible rather than silently
/// dialing the wrong protocol.
pub fn build_adapter(provider: Option<&str>) -> Arc<dyn ModelAdapter> {
    let norm = provider.map(|p| p.trim().to_ascii_lowercase());
    match norm.as_deref() {
        Some("anthropic") => Arc::new(anthropic::AnthropicAdapter::new()),
        Some("") | Some("openai-compatible") | None => Arc::new(openai::OpenAiCompatAdapter::new()),
        Some(other) => {
            log::warn!("unknown AI provider {other:?}; using openai-compatible adapter");
            Arc::new(openai::OpenAiCompatAdapter::new())
        }
    }
}

/// Resolves the adapter for a provider at call time, so a runtime provider
/// change takes effect on the next diagnosis. Production wraps [`build_adapter`];
/// tests inject a selector that returns a fixed mock.
pub trait AdapterSelector: Send + Sync {
    fn select(&self, provider: Option<&str>) -> Arc<dyn ModelAdapter>;
}

/// Production selector: resolves via [`build_adapter`] on every call.
pub struct ProviderAdapterSelector;

impl AdapterSelector for ProviderAdapterSelector {
    fn select(&self, provider: Option<&str>) -> Arc<dyn ModelAdapter> {
        build_adapter(provider)
    }
}

/// A selector that always returns the same adapter regardless of provider.
/// Used by tests (and any caller that has already chosen an adapter) to keep
/// the per-call resolution path while pinning the wire implementation.
pub struct FixedAdapterSelector(pub Arc<dyn ModelAdapter>);

impl AdapterSelector for FixedAdapterSelector {
    fn select(&self, _provider: Option<&str>) -> Arc<dyn ModelAdapter> {
        self.0.clone()
    }
}

/// The real diagnose model: prompt assembly + adapter call + parse + audit.
pub struct ModelBackedDiagnoseModel {
    selector: Arc<dyn AdapterSelector>,
    settings: Arc<SharedSettings>,
    audit: Arc<dyn AuditSink>,
}

impl ModelBackedDiagnoseModel {
    /// Construct with an [`AdapterSelector`] (production = [`ProviderAdapterSelector`]),
    /// so the adapter is resolved per diagnosis from the current provider setting.
    pub fn new(
        selector: Arc<dyn AdapterSelector>,
        settings: Arc<SharedSettings>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            selector,
            settings,
            audit,
        }
    }

    /// Construct with a fixed adapter, pinning the wire implementation while
    /// still going through the per-call resolution path. Convenience for callers
    /// and tests that already hold a concrete adapter.
    pub fn with_adapter(
        adapter: Arc<dyn ModelAdapter>,
        settings: Arc<SharedSettings>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self::new(Arc::new(FixedAdapterSelector(adapter)), settings, audit)
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
        locale: Option<&str>,
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

        // Manager-proxied gateway is a reserved placeholder: the field and API
        // exist, but the proxy is not implemented. Refuse before resolving the
        // adapter (so no wire call is attempted) — the orchestrator turns this
        // into a `DiagnoseEvent::error`.
        if config.gateway_mode == GatewayMode::ManagerProxy {
            return Err(AgentError {
                kind: desk_agent_protocol::AgentErrorKind::UnsupportedCapability,
                message: "manager-proxied model gateway is not available yet".to_string(),
                retryable: false,
                safe_for_model: true,
            });
        }

        let max_context_bytes = config
            .max_context_bytes
            .map(|b| b as usize)
            .unwrap_or(DEFAULT_MAX_CONTEXT_BYTES);

        // Advertise the executable command catalog matching the configured
        // execution mode so the model prefers runnable forms. suggest_only →
        // nothing executable; read_only → read-only forms; confirm_each_action →
        // read-only + state-changing forms. (session_approved / automated are not
        // selectable, so they never reach here.)
        use desk_agent_protocol::ExecutionMode;
        let executable_commands: Vec<String> = match config.execution_mode {
            ExecutionMode::SuggestOnly => Vec::new(),
            mode => crate::exec::command_forms(matches!(mode, ExecutionMode::ConfirmEachAction))
                .into_iter()
                .map(|c| {
                    format!(
                        "`{}` — {}{}",
                        c.form,
                        c.impact,
                        if c.mutating { " (changes state)" } else { "" }
                    )
                })
                .collect(),
        };

        let messages = prompt::build_messages(
            question,
            evidence,
            max_context_bytes,
            locale,
            &executable_commands,
        );
        let response_format = match config.response_format {
            ResponseFormatMode::None => ResponseFormatSpec::None,
            ResponseFormatMode::JsonObject => ResponseFormatSpec::JsonObject,
            ResponseFormatMode::JsonSchema => ResponseFormatSpec::JsonSchema {
                name: "diagnosis".to_string(),
                schema: prompt::diagnosis_json_schema(),
            },
        };
        // Resolve the adapter from the current provider on every diagnosis, so a
        // runtime provider change takes effect immediately and the audit records
        // the concrete adapter actually used.
        let adapter = self.selector.select(config.provider.as_deref());
        let caller = CallerRef {
            caller_type: CallerType::AiModel,
            model_provider: config.provider.clone(),
            model_name: Some(model.clone()),
            adapter: Some(adapter.name().to_string()),
        };

        // `ai.model.requested` — input token count is not known until the
        // gateway reports usage, so it is left unset here. The prompt and
        // evidence-schema versions are stamped into the summary so a recorded
        // diagnosis is attributable to the contract that produced it.
        self.audit
            .record(AuditEvent::model_requested(
                new_event_id(),
                now_rfc3339(),
                request_id,
                &caller,
                format!(
                    "evidence: {} contexts prompt={} evidence_schema={}",
                    evidence.contexts.len(),
                    prompt::PROMPT_VERSION,
                    evidence.schema_version,
                ),
                None,
            ))
            .await;

        let started = Instant::now();
        let request = ChatRequest {
            base_url,
            api_key,
            model,
            messages,
            response_format,
        };
        let response = adapter.stream_chat(request, on_partial).await?;
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

        let (diagnosis, parse_outcome) = parser::parse_diagnosis(&response.content);
        self.audit
            .record(AuditEvent::model_responded(
                new_event_id(),
                now_rfc3339(),
                request_id,
                &caller,
                format!(
                    "diagnosis: {} findings, {} commands parse={}",
                    diagnosis.findings.len(),
                    diagnosis.commands.len(),
                    parse_outcome.as_str()
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
        let model = ModelBackedDiagnoseModel::with_adapter(
            adapter.clone(),
            configured_settings(),
            Arc::new(audit.clone()),
        );

        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let p = partials.clone();
        let on_partial = move |s: String| p.lock().unwrap().push(s);
        let diag = model
            .diagnose("req_1", "why cpu high?", &snapshot(), None, &on_partial)
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
        // The responded summary records the structured/degraded parse outcome.
        let responded_summary = responded.output_summary.as_deref().unwrap_or_default();
        assert!(
            responded_summary.contains("parse=structured"),
            "responded summary should record the parse outcome: {responded_summary:?}"
        );
        // The requested summary stamps the prompt + evidence-schema versions.
        let requested = events
            .iter()
            .find(|e| e.event_type == AuditEventType::ModelRequested.as_str())
            .unwrap();
        let requested_summary = requested.input_summary.as_deref().unwrap_or_default();
        assert!(
            requested_summary.contains(&format!("prompt={}", prompt::PROMPT_VERSION)),
            "requested summary should stamp the prompt version: {requested_summary:?}"
        );
        assert!(
            requested_summary.contains("evidence_schema="),
            "requested summary should stamp the evidence schema version: {requested_summary:?}"
        );
    }

    /// The configured `response_format` mode flows into the chat request: the
    /// default is `json_object`, and `json_schema` carries the diagnosis schema.
    #[tokio::test]
    async fn response_format_mode_flows_into_request() {
        // Default settings → json_object.
        let adapter = Arc::new(MockAdapter {
            fragments: vec![],
            content: WELL_FORMED.into(),
            usage: TokenUsage::default(),
            seen: Mutex::new(None),
        });
        let model = ModelBackedDiagnoseModel::with_adapter(
            adapter.clone(),
            configured_settings(),
            Arc::new(RecordingAuditSink::default()),
        );
        let noop = |_: String| {};
        model
            .diagnose("r", "q", &snapshot(), None, &noop)
            .await
            .unwrap();
        assert!(matches!(
            adapter
                .seen
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .response_format,
            ResponseFormatSpec::JsonObject
        ));

        // json_schema mode → the request carries the named diagnosis schema.
        let mut s = Settings::default();
        s.ai_model.provider = Some("openai-compatible".into());
        s.ai_model.model = Some("m".into());
        s.ai_model.base_url = Some("https://api.example/v1".into());
        s.ai_model.api_key = Some("sk".into());
        s.ai_model.response_format = ResponseFormatMode::JsonSchema;
        let adapter2 = Arc::new(MockAdapter {
            fragments: vec![],
            content: WELL_FORMED.into(),
            usage: TokenUsage::default(),
            seen: Mutex::new(None),
        });
        let model2 = ModelBackedDiagnoseModel::with_adapter(
            adapter2.clone(),
            Arc::new(SharedSettings::from(s)),
            Arc::new(RecordingAuditSink::default()),
        );
        model2
            .diagnose("r", "q", &snapshot(), None, &noop)
            .await
            .unwrap();
        match &adapter2
            .seen
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .response_format
        {
            ResponseFormatSpec::JsonSchema { name, schema } => {
                assert_eq!(name, "diagnosis");
                assert_eq!(schema["type"], "object");
            }
            other => panic!("expected json_schema, got {other:?}"),
        }
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
        let model = ModelBackedDiagnoseModel::with_adapter(
            adapter.clone(),
            Arc::new(SharedSettings::from(Settings::default())),
            Arc::new(audit.clone()),
        );
        let noop = |_: String| {};
        let diag = model
            .diagnose("req_2", "q", &snapshot(), None, &noop)
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
        let model = ModelBackedDiagnoseModel::with_adapter(
            Arc::new(FailingAdapter),
            configured_settings(),
            Arc::new(audit.clone()),
        );
        let noop = |_: String| {};
        let err = model
            .diagnose("req_3", "q", &snapshot(), None, &noop)
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
        let model = ModelBackedDiagnoseModel::with_adapter(
            adapter,
            configured_settings(),
            Arc::new(RecordingAuditSink::default()),
        );
        let noop = |_: String| {};
        let diag = model
            .diagnose("req_4", "q", &snapshot(), None, &noop)
            .await
            .unwrap();
        assert_eq!(diag.confidence, Confidence::Low);
        assert!(!diag.missing_info.is_empty());
    }

    /// `build_adapter` resolves each provider to the matching wire. Anthropic and
    /// OpenAI-compatible map to their adapters; normalization (case / surrounding
    /// whitespace) is applied before matching; an unknown non-empty provider and
    /// the empty / absent cases fall back to OpenAI-compatible.
    #[test]
    fn build_adapter_resolves_providers() {
        let openai_cases = [
            None,
            Some(""),
            Some("openai-compatible"),
            Some("  OpenAI-Compatible  "),
            Some("totally-unknown"), // unknown → fallback
        ];
        for provider in openai_cases {
            assert_eq!(
                build_adapter(provider).name(),
                "lcxl-openai-compat",
                "provider {provider:?} should resolve to the OpenAI-compatible adapter"
            );
        }

        let anthropic_cases = [Some("anthropic"), Some("Anthropic"), Some("  ANTHROPIC  ")];
        for provider in anthropic_cases {
            assert_eq!(
                build_adapter(provider).name(),
                "lcxl-anthropic",
                "provider {provider:?} should resolve to the Anthropic adapter"
            );
        }
    }

    /// A selector that records how many times it was asked to resolve an adapter,
    /// so tests can assert the model resolves per call (or not at all).
    struct CountingSelector {
        inner: Arc<dyn ModelAdapter>,
        calls: Arc<Mutex<usize>>,
    }
    impl AdapterSelector for CountingSelector {
        fn select(&self, _provider: Option<&str>) -> Arc<dyn ModelAdapter> {
            *self.calls.lock().unwrap() += 1;
            self.inner.clone()
        }
    }

    /// The adapter is resolved through the selector on each diagnosis (per-call
    /// resolution), so a runtime provider change takes effect immediately.
    #[tokio::test]
    async fn selector_resolves_adapter_per_diagnose() {
        let adapter = Arc::new(MockAdapter {
            fragments: vec![],
            content: WELL_FORMED.into(),
            usage: TokenUsage::default(),
            seen: Mutex::new(None),
        });
        let calls = Arc::new(Mutex::new(0usize));
        let selector = Arc::new(CountingSelector {
            inner: adapter,
            calls: calls.clone(),
        });
        let model = ModelBackedDiagnoseModel::new(
            selector,
            configured_settings(),
            Arc::new(RecordingAuditSink::default()),
        );
        let noop = |_: String| {};
        model
            .diagnose("r1", "q", &snapshot(), None, &noop)
            .await
            .unwrap();
        model
            .diagnose("r2", "q", &snapshot(), None, &noop)
            .await
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            2,
            "adapter must be resolved per diagnose"
        );
    }

    /// `gateway_mode = manager_proxy` refuses with `UnsupportedCapability` before
    /// touching the selector / adapter, and records no audit (no wire call).
    #[tokio::test]
    async fn manager_proxy_gateway_is_refused_without_dialing() {
        let mut s = Settings::default();
        s.ai_model.provider = Some("openai-compatible".into());
        s.ai_model.model = Some("m".into());
        s.ai_model.base_url = Some("https://api.example/v1".into());
        s.ai_model.api_key = Some("sk".into());
        s.ai_model.gateway_mode = crate::model::settings::GatewayMode::ManagerProxy;

        let calls = Arc::new(Mutex::new(0usize));
        let selector = Arc::new(CountingSelector {
            inner: Arc::new(MockAdapter {
                fragments: vec![],
                content: WELL_FORMED.into(),
                usage: TokenUsage::default(),
                seen: Mutex::new(None),
            }),
            calls: calls.clone(),
        });
        let audit = RecordingAuditSink::default();
        let model = ModelBackedDiagnoseModel::new(
            selector,
            Arc::new(SharedSettings::from(s)),
            Arc::new(audit.clone()),
        );
        let noop = |_: String| {};
        let err = model
            .diagnose("req_mp", "q", &snapshot(), None, &noop)
            .await
            .expect_err("manager_proxy must be refused");
        assert_eq!(
            err.kind,
            desk_agent_protocol::AgentErrorKind::UnsupportedCapability
        );
        // Neither the selector nor the audit trail was touched: no dial happened.
        assert_eq!(*calls.lock().unwrap(), 0, "selector must not be resolved");
        assert!(
            audit.events.lock().unwrap().is_empty(),
            "no audit on refusal"
        );
    }
}
