//! The signal central brain's model seam: dials the configured provider.
//!
//! This is signal's own implementation of [`desk_diagnose_core::seam::ModelSeam`]
//! — the agentic core never speaks a provider wire dialect, it hands a neutral
//! [`ModelRequest`] across this seam and gets back a normalized [`ModelTurn`].
//! The manager has its own seam (its model dialect); signal's is separate.
//!
//! The dial **streams** the provider response over Server-Sent Events: each text
//! delta is forwarded to the [`TurnSink`] as it arrives (so the terminal copilot
//! can render the explanation live), while the call still assembles and returns the
//! complete, normalized [`ModelTurn`] (the structured answer parse runs on the full
//! text). A caller that does not want streaming passes a no-op sink.
//!
//! Two dialects are supported, resolved from the provider identifier exactly as
//! the edge did: `anthropic` → the Anthropic Messages API, everything else → an
//! OpenAI-compatible `/chat/completions` endpoint. The body builders and the SSE
//! event parsers are pure so they are unit-tested without a network; the HTTP send
//! and the byte-stream pump are a thin `awc` wrapper.
//!
//! `?Send`: `awc` is `!Send` and the orchestration runs on actix's
//! single-threaded runtime, so the seam future is non-`Send`, matching the core's
//! `ModelSeam` contract.

use async_trait::async_trait;
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_diagnose_core::chat::{ChatMessage, ChatRole, ModelTurn, StopReason, TokenUsage};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, TurnSink};
use serde_json::{Value, json};

use crate::model_provider::ModelProviderConfig;

/// Upper bound on generated tokens for the Anthropic dialect, which requires
/// `max_tokens`. Generous for a structured diagnosis; the prompt and the parser
/// degrade gracefully if the model runs long.
const ANTHROPIC_MAX_TOKENS: u32 = 4096;
/// The Anthropic API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Connect timeout for the provider HTTP client.
const CONNECT_TIMEOUT_SECS: u64 = 30;
/// Overall request timeout: hosted models can take many seconds to first byte.
const REQUEST_TIMEOUT_SECS: u64 = 180;

/// Which provider wire dialect to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// OpenAI-compatible `/chat/completions` (OpenAI, Azure-compatible gateways,
    /// most local inference servers).
    OpenAiCompatible,
    /// Anthropic Messages `/v1/messages`.
    Anthropic,
}

impl Dialect {
    /// Resolve the dialect from the configured provider identifier, normalized
    /// case-insensitively. `anthropic` selects the Anthropic dialect; everything
    /// else (including empty / unset) falls back to OpenAI-compatible.
    pub fn from_provider(provider: Option<&str>) -> Self {
        match provider.map(|p| p.trim().to_ascii_lowercase()).as_deref() {
            Some("anthropic") => Dialect::Anthropic,
            _ => Dialect::OpenAiCompatible,
        }
    }
}

fn config_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
    }
}

fn transport_error(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::TransportError,
        message: message.into(),
        retryable: true,
        safe_for_model: true,
    }
}

/// Signal's model seam over a single resolved provider.
pub struct SignalModelSeam {
    dialect: Dialect,
    base_url: String,
    api_key: String,
    model: String,
}

impl SignalModelSeam {
    /// Build the seam from the configured provider, failing closed if a required
    /// field (model / base url / api key) is unset.
    pub fn from_config(config: &ModelProviderConfig) -> Result<Self, AgentError> {
        let base_url = non_empty(config.base_url.as_deref())
            .ok_or_else(|| config_error("model provider base_url is not configured"))?;
        let api_key = non_empty(config.api_key.as_deref())
            .ok_or_else(|| config_error("model provider api_key is not configured"))?;
        let model = non_empty(config.model.as_deref())
            .ok_or_else(|| config_error("model provider model is not configured"))?;
        Ok(Self {
            dialect: Dialect::from_provider(config.provider.as_deref()),
            base_url,
            api_key,
            model,
        })
    }

    fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.dialect {
            Dialect::OpenAiCompatible => format!("{base}/chat/completions"),
            Dialect::Anthropic => format!("{base}/v1/messages"),
        }
    }

    fn build_body(&self, request: &ModelRequest) -> Value {
        match self.dialect {
            Dialect::OpenAiCompatible => build_openai_body(&self.model, request),
            Dialect::Anthropic => build_anthropic_body(&self.model, request),
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[async_trait(?Send)]
impl ModelSeam for SignalModelSeam {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        use futures_util::StreamExt;

        // A TLS-capable client: `awc::Client::default()` has no TLS connector and
        // fails instantly on `https://` gateways. Pin the `ring` provider (the
        // rustls default `aws_lc_rs` fast-fails the process on Windows).
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = root_store.add(cert);
        }
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default TLS protocol versions")
        .with_root_certificates(std::sync::Arc::new(root_store))
        .with_no_client_auth();
        let client = awc::Client::builder()
            .connector(
                awc::Connector::new()
                    .timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
                    .rustls_0_23(std::sync::Arc::new(tls)),
            )
            .finish();

        let body = self.build_body(&request);
        let mut http = client
            .post(self.endpoint())
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS));
        http = match self.dialect {
            Dialect::OpenAiCompatible => {
                http.insert_header(("Authorization", format!("Bearer {}", self.api_key)))
            }
            Dialect::Anthropic => http
                .insert_header(("x-api-key", self.api_key.clone()))
                .insert_header(("anthropic-version", ANTHROPIC_VERSION)),
        };

        let mut response = http
            .insert_header(("Content-Type", "application/json"))
            .insert_header(("Accept", "text/event-stream"))
            .send_json(&body)
            .await
            .map_err(|e| transport_error(format!("model request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            // The gateway's own error text never contains our api_key, so the
            // bounded body is safe to surface (a bad model name / auth failure).
            let err_body = response.body().limit(16 * 1024).await.unwrap_or_default();
            let detail = String::from_utf8_lossy(&err_body);
            let detail = detail.trim();
            log::warn!("[model-dial] gateway returned {status}: {detail}");
            return Err(transport_error(if detail.is_empty() {
                format!("model gateway returned status {status}")
            } else {
                format!("model gateway returned status {status}: {detail}")
            }));
        }

        // Pump the SSE byte stream: frame complete events, apply each to the
        // dialect accumulator, and forward any text delta to the sink as it
        // arrives. The fully assembled, normalized turn is returned at the end.
        let mut decoder = SseDecoder::new();
        let mut state = StreamState::new(self.dialect);
        while let Some(item) = response.next().await {
            let chunk =
                item.map_err(|e| transport_error(format!("model stream interrupted: {e}")))?;
            for data in decoder.push(&chunk) {
                if let Some(delta) = state.apply(&data) {
                    sink.on_text_delta(&delta);
                }
            }
            if let Some(err) = state.take_error() {
                return Err(transport_error(err));
            }
        }
        Ok(state.into_turn())
    }
}

// ============================ SSE stream framing ============================

/// Incremental Server-Sent Events framer. SSE separates events with a blank line;
/// an event's payload is the concatenation of its `data:` field lines (joined by
/// `\n`). Buffers raw bytes across chunk boundaries so a multi-byte UTF-8 sequence
/// split mid-character is never decoded until its event completes.
struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append a raw chunk and return the `data` payload of every event that is now
    /// complete. `\r` bytes (CRLF line endings) are dropped so event boundaries are
    /// uniformly `\n\n`; a `\r` never appears literally inside SSE JSON data (it is
    /// escaped as `\r`).
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf
            .extend(bytes.iter().copied().filter(|&b| b != b'\r'));
        let mut events = Vec::new();
        while let Some(end) = find_subslice(&self.buf, b"\n\n") {
            let event: Vec<u8> = self.buf.drain(..end + 2).collect();
            let text = String::from_utf8_lossy(&event[..event.len() - 2]);
            if let Some(data) = event_data(&text) {
                events.push(data);
            }
        }
        events
    }
}

/// Concatenate an event's `data:` field value(s); `None` for an event carrying no
/// data field (comments / other fields).
fn event_data(event: &str) -> Option<String> {
    let mut data: Option<String> = None;
    for line in event.split('\n') {
        if let Some(rest) = line.strip_prefix("data:") {
            // A single optional leading space after the colon is part of the SSE
            // framing, not the value.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            match &mut data {
                Some(acc) => {
                    acc.push('\n');
                    acc.push_str(rest);
                }
                None => data = Some(rest.to_string()),
            }
        }
    }
    data
}

/// First index of `needle` within `hay`, or `None`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Short message from a provider error object (`{ "message": ... }`), falling back
/// to a compact JSON rendering.
fn error_message(err: &Value) -> String {
    err.get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| err.to_string())
}

/// The dialect-specific stream accumulator: applies each SSE `data` payload and
/// assembles the normalized [`ModelTurn`] at the end.
enum StreamState {
    OpenAi(OpenAiStreamState),
    Anthropic(AnthropicStreamState),
}

impl StreamState {
    fn new(dialect: Dialect) -> Self {
        match dialect {
            Dialect::OpenAiCompatible => StreamState::OpenAi(OpenAiStreamState::default()),
            Dialect::Anthropic => StreamState::Anthropic(AnthropicStreamState::default()),
        }
    }

    /// Apply one event payload; return the text delta to forward (if any).
    fn apply(&mut self, data: &str) -> Option<String> {
        match self {
            StreamState::OpenAi(s) => s.apply(data),
            StreamState::Anthropic(s) => s.apply(data),
        }
    }

    /// Take a mid-stream provider error, if one was reported.
    fn take_error(&mut self) -> Option<String> {
        match self {
            StreamState::OpenAi(s) => s.error.take(),
            StreamState::Anthropic(s) => s.error.take(),
        }
    }

    fn into_turn(self) -> ModelTurn {
        match self {
            StreamState::OpenAi(s) => s.into_turn(),
            StreamState::Anthropic(s) => s.into_turn(),
        }
    }
}

// ============================ OpenAI dialect ============================

/// Map one [`ChatMessage`] to OpenAI message JSON (text-only diagnose shape; a
/// vision image rides as a multimodal content array).
fn openai_message_to_json(m: &ChatMessage) -> Value {
    let content = match &m.image_data_url {
        Some(url) => json!([
            {"type": "text", "text": m.text},
            {"type": "image_url", "image_url": {"url": url}},
        ]),
        None => json!(m.text),
    };
    json!({ "role": m.role.as_str(), "content": content })
}

/// Build the streaming `/chat/completions` body. The diagnose path is tool-free,
/// so no tools are advertised. `stream_options.include_usage` asks the gateway to
/// emit a final usage chunk (omitted by default when streaming).
fn build_openai_body(model: &str, request: &ModelRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(openai_message_to_json)
        .collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    match &request.response_format {
        ResponseFormatSpec::None => {}
        ResponseFormatSpec::JsonObject => {
            body["response_format"] = json!({ "type": "json_object" });
        }
        ResponseFormatSpec::JsonSchema { name, schema } => {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": name, "strict": true, "schema": schema },
            });
        }
    }
    body
}

/// Map an OpenAI `finish_reason` onto the neutral [`StopReason`].
fn openai_stop_reason(finish: Option<&str>) -> StopReason {
    match finish {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

/// Map an OpenAI streaming `usage` object onto the neutral [`TokenUsage`]. OpenAI
/// `prompt_tokens` includes the cached portion, so it is subtracted out to avoid
/// double-counting the cache read against `input_tokens`.
fn openai_usage(usage: Option<&Value>) -> TokenUsage {
    let prompt = usage.and_then(|u| u["prompt_tokens"].as_i64());
    let cached = usage.and_then(|u| u["prompt_tokens_details"]["cached_tokens"].as_i64());
    let input_tokens = match (prompt, cached) {
        (Some(p), Some(c)) => Some((p - c).max(0)),
        (p, _) => p,
    };
    TokenUsage {
        input_tokens,
        output_tokens: usage.and_then(|u| u["completion_tokens"].as_i64()),
        cache_read_tokens: cached,
        cache_write_tokens: None,
    }
}

/// Accumulates an OpenAI `/chat/completions` SSE stream into a [`ModelTurn`].
#[derive(Default)]
struct OpenAiStreamState {
    text: String,
    finish_reason: Option<String>,
    usage: Option<Value>,
    error: Option<String>,
}

impl OpenAiStreamState {
    /// Apply one SSE `data` payload, returning the text delta (if any). The
    /// `[DONE]` sentinel and keep-alive / unparseable payloads are ignored.
    fn apply(&mut self, data: &str) -> Option<String> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return None;
        }
        let v: Value = serde_json::from_str(data).ok()?;
        if let Some(err) = v.get("error") {
            self.error = Some(error_message(err));
            return None;
        }
        if let Some(u) = v.get("usage")
            && !u.is_null()
        {
            self.usage = Some(u.clone());
        }
        let choice = v.get("choices")?.as_array()?.first()?;
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.finish_reason = Some(fr.to_string());
        }
        let delta = choice.get("delta")?.get("content")?.as_str()?;
        if delta.is_empty() {
            return None;
        }
        self.text.push_str(delta);
        Some(delta.to_string())
    }

    fn into_turn(self) -> ModelTurn {
        ModelTurn {
            stop_reason: openai_stop_reason(self.finish_reason.as_deref()),
            usage: openai_usage(self.usage.as_ref()),
            text: self.text,
            tool_calls: Vec::new(),
        }
    }
}

// ============================ Anthropic dialect ============================

/// Map one non-system [`ChatMessage`] to an Anthropic `messages[]` entry.
fn anthropic_message_to_json(m: &ChatMessage) -> Value {
    let content = match &m.image_data_url {
        Some(url) => {
            // A data URL is `data:<media_type>;base64,<data>`; Anthropic wants the
            // media type and raw base64 split out. Fall back to a text-only block
            // if the shape is unexpected.
            match split_data_url(url) {
                Some((media_type, data)) => json!([
                    {"type": "text", "text": m.text},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": media_type, "data": data,
                    }},
                ]),
                None => json!(m.text),
            }
        }
        None => json!(m.text),
    };
    json!({ "role": m.role.as_str(), "content": content })
}

/// Split a `data:<media_type>;base64,<data>` URL into `(media_type, base64)`.
fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type.to_string(), data.to_string()))
}

/// Build the streaming `/v1/messages` body. System text is hoisted to the
/// top-level `system` field; the rest become `messages`. The diagnose path is
/// tool-free.
fn build_anthropic_body(model: &str, request: &ModelRequest) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in &request.messages {
        if m.role == ChatRole::System {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&m.text);
            continue;
        }
        messages.push(anthropic_message_to_json(m));
    }
    let mut body = json!({
        "model": model,
        "max_tokens": ANTHROPIC_MAX_TOKENS,
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    body
}

/// Map an Anthropic `stop_reason` onto the neutral [`StopReason`].
fn anthropic_stop_reason(reason: Option<&str>) -> StopReason {
    match reason {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

/// Accumulates an Anthropic Messages SSE stream into a [`ModelTurn`]. The event
/// types consumed: `message_start` (input usage), `content_block_delta` with a
/// `text_delta` (the streamed text), `message_delta` (stop reason + output usage),
/// and `error`. Other events (`ping`, block start/stop, `message_stop`) are inert.
#[derive(Default)]
struct AnthropicStreamState {
    text: String,
    stop_reason: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    error: Option<String>,
}

impl AnthropicStreamState {
    /// Apply one SSE `data` payload, returning the text delta (if any).
    fn apply(&mut self, data: &str) -> Option<String> {
        let data = data.trim();
        if data.is_empty() {
            return None;
        }
        let v: Value = serde_json::from_str(data).ok()?;
        match v.get("type").and_then(|t| t.as_str())? {
            "error" => {
                self.error = Some(
                    v.get("error")
                        .map(error_message)
                        .unwrap_or_else(|| "anthropic stream error".to_string()),
                );
                None
            }
            "message_start" => {
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    self.input_tokens = u["input_tokens"].as_i64();
                    self.cache_read = u["cache_read_input_tokens"].as_i64();
                    self.cache_write = u["cache_creation_input_tokens"].as_i64();
                }
                None
            }
            "content_block_delta" => {
                let d = v.get("delta")?;
                if d.get("type").and_then(|t| t.as_str()) != Some("text_delta") {
                    return None;
                }
                let text = d.get("text")?.as_str()?;
                if text.is_empty() {
                    return None;
                }
                self.text.push_str(text);
                Some(text.to_string())
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(ot) = v.get("usage").and_then(|u| u["output_tokens"].as_i64()) {
                    self.output_tokens = Some(ot);
                }
                None
            }
            _ => None,
        }
    }

    fn into_turn(self) -> ModelTurn {
        ModelTurn {
            stop_reason: anthropic_stop_reason(self.stop_reason.as_deref()),
            usage: TokenUsage {
                // Anthropic's `input_tokens` already excludes cache, so it maps as-is.
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_tokens: self.cache_read,
                cache_write_tokens: self.cache_write,
            },
            text: self.text,
            tool_calls: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider::ResponseFormatMode;

    fn text_request(format: ResponseFormatSpec) -> ModelRequest {
        ModelRequest::text_only(
            vec![
                ChatMessage::text("s", ChatRole::System, "you are a diagnostician"),
                ChatMessage::text("u", ChatRole::User, "why is it slow?"),
            ],
            format,
        )
    }

    #[test]
    fn dialect_resolves_case_insensitively_with_openai_fallback() {
        assert_eq!(
            Dialect::from_provider(Some("anthropic")),
            Dialect::Anthropic
        );
        assert_eq!(
            Dialect::from_provider(Some(" Anthropic ")),
            Dialect::Anthropic
        );
        assert_eq!(
            Dialect::from_provider(Some("openai-compatible")),
            Dialect::OpenAiCompatible
        );
        assert_eq!(
            Dialect::from_provider(Some("something-else")),
            Dialect::OpenAiCompatible
        );
        assert_eq!(Dialect::from_provider(None), Dialect::OpenAiCompatible);
    }

    #[test]
    fn from_config_fails_closed_without_required_fields() {
        // A default config has no provider creds → seam build fails closed.
        let mut cfg = ModelProviderConfig::default();
        assert!(SignalModelSeam::from_config(&cfg).is_err());
        cfg.base_url = Some("https://api.example.com".to_string());
        cfg.model = Some("gpt-test".to_string());
        // Still missing api_key.
        assert!(SignalModelSeam::from_config(&cfg).is_err());
        cfg.api_key = Some("sk-secret".to_string());
        let seam = SignalModelSeam::from_config(&cfg).expect("seam builds");
        assert_eq!(seam.dialect, Dialect::OpenAiCompatible);
        assert_eq!(seam.endpoint(), "https://api.example.com/chat/completions");
    }

    #[test]
    fn anthropic_endpoint_and_dialect_from_config() {
        let cfg = ModelProviderConfig {
            provider: Some("anthropic".to_string()),
            model: Some("claude-x".to_string()),
            base_url: Some("https://api.anthropic.com/".to_string()),
            api_key: Some("sk-ant".to_string()),
            max_context_bytes: None,
            response_format: ResponseFormatMode::JsonObject,
            execution_mode: Default::default(),
        };
        let seam = SignalModelSeam::from_config(&cfg).expect("seam builds");
        assert_eq!(seam.dialect, Dialect::Anthropic);
        // The trailing slash is tolerated.
        assert_eq!(seam.endpoint(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn openai_body_carries_messages_and_response_format() {
        let body = build_openai_body(
            "gpt-test",
            &text_request(ResponseFormatSpec::JsonSchema {
                name: "diagnosis".to_string(),
                schema: json!({"type": "object"}),
            }),
        );
        assert_eq!(body["model"], "gpt-test");
        // The dial streams and asks for a trailing usage chunk.
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "why is it slow?");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "diagnosis");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        // A tool-free diagnose request advertises no tools.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn anthropic_body_hoists_system_and_requires_max_tokens() {
        let body = build_anthropic_body("claude-x", &text_request(ResponseFormatSpec::JsonObject));
        assert_eq!(body["model"], "claude-x");
        assert_eq!(body["max_tokens"], ANTHROPIC_MAX_TOKENS);
        // System text is hoisted to the top-level field, not left in messages.
        assert_eq!(body["system"], "you are a diagnostician");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    /// Drive an OpenAI stream: deltas concatenate in order, a `finish_reason`
    /// chunk maps the stop reason, and the trailing usage chunk maps token counts
    /// (cache subtracted out of `input_tokens`). Keep-alive chunks without a
    /// content delta are inert.
    #[test]
    fn openai_stream_assembles_text_stop_and_usage() {
        let mut s = OpenAiStreamState::default();
        let mut deltas = String::new();
        for payload in [
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":30}}}"#,
            "[DONE]",
        ] {
            if let Some(d) = s.apply(payload) {
                deltas.push_str(&d);
            }
            assert!(s.error.is_none());
        }
        let turn = s.into_turn();
        assert_eq!(deltas, "Hello");
        assert_eq!(turn.text, "Hello");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        // input_tokens excludes the cached portion (100 - 30).
        assert_eq!(turn.usage.input_tokens, Some(70));
        assert_eq!(turn.usage.cache_read_tokens, Some(30));
        assert_eq!(turn.usage.output_tokens, Some(20));
    }

    /// A mid-stream OpenAI error payload is surfaced (so the dial fails) rather
    /// than silently producing a partial turn.
    #[test]
    fn openai_stream_surfaces_mid_stream_error() {
        let mut s = OpenAiStreamState::default();
        assert!(s.apply(r#"{"error":{"message":"rate limit"}}"#).is_none());
        assert_eq!(s.error.as_deref(), Some("rate limit"));
    }

    /// Drive an Anthropic stream: `content_block_delta` text deltas concatenate,
    /// `message_start` carries input/cache usage, `message_delta` carries the stop
    /// reason and output usage; `ping` and non-text deltas are inert.
    #[test]
    fn anthropic_stream_assembles_text_stop_and_usage() {
        let mut s = AnthropicStreamState::default();
        let mut deltas = String::new();
        for payload in [
            r#"{"type":"message_start","message":{"usage":{"input_tokens":50,"cache_read_input_tokens":5,"cache_creation_input_tokens":7}}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello "}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            if let Some(d) = s.apply(payload) {
                deltas.push_str(&d);
            }
            assert!(s.error.is_none());
        }
        let turn = s.into_turn();
        assert_eq!(deltas, "hello world");
        assert_eq!(turn.text, "hello world");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(turn.usage.input_tokens, Some(50));
        assert_eq!(turn.usage.output_tokens, Some(10));
        assert_eq!(turn.usage.cache_read_tokens, Some(5));
        assert_eq!(turn.usage.cache_write_tokens, Some(7));
    }

    /// A mid-stream Anthropic `error` event is surfaced.
    #[test]
    fn anthropic_stream_surfaces_error_event() {
        let mut s = AnthropicStreamState::default();
        assert!(
            s.apply(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#
            )
            .is_none()
        );
        assert_eq!(s.error.as_deref(), Some("overloaded"));
    }

    /// The SSE framer joins multi-line `data:` fields, ignores comments, and only
    /// yields an event once its terminating blank line has arrived — even when the
    /// event is split across chunks mid-UTF-8.
    #[test]
    fn sse_decoder_frames_events_across_chunk_boundaries() {
        let mut d = SseDecoder::new();
        // No terminating blank line yet → nothing emitted.
        assert!(d.push(b": keep-alive comment\n").is_empty());
        assert!(d.push(b"data: {\"a\":1}\n").is_empty());
        let evs = d.push(b"\ndata: line1\ndata: line2\n\n");
        assert_eq!(
            evs,
            vec!["{\"a\":1}".to_string(), "line1\nline2".to_string()]
        );

        // A multi-byte char (… = E2 80 A6) split across two chunks decodes intact
        // once the event completes.
        let mut d2 = SseDecoder::new();
        assert!(d2.push(b"data: x\xe2\x80").is_empty());
        let evs2 = d2.push(b"\xa6y\n\n");
        assert_eq!(evs2, vec!["x…y".to_string()]);
    }

    /// CRLF line endings are tolerated: `\r` is stripped so events still frame on a
    /// blank line.
    #[test]
    fn sse_decoder_tolerates_crlf() {
        let mut d = SseDecoder::new();
        let evs = d.push(b"data: {\"ok\":true}\r\n\r\n");
        assert_eq!(evs, vec!["{\"ok\":true}".to_string()]);
    }

    #[test]
    fn split_data_url_parses_media_type_and_payload() {
        assert_eq!(
            split_data_url("data:image/png;base64,QUJD"),
            Some(("image/png".to_string(), "QUJD".to_string()))
        );
        assert_eq!(split_data_url("not-a-data-url"), None);
    }
}
