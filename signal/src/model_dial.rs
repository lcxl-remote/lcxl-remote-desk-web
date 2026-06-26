//! The signal central brain's model seam: dials the configured provider.
//!
//! This is signal's own implementation of [`desk_diagnose_core::seam::ModelSeam`]
//! — the agentic core never speaks a provider wire dialect, it hands a neutral
//! [`ModelRequest`] across this seam and gets back a normalized [`ModelTurn`].
//! The manager has its own seam (its model dialect); signal's is separate and
//! deliberately simpler: a **non-streaming** request/response over `awc`, which
//! is enough for the single-turn diagnose (the structured `Final` result is the
//! essential output; per-token `Partial` streaming is a later enhancement).
//!
//! Two dialects are supported, resolved from the provider identifier exactly as
//! the edge did: `anthropic` → the Anthropic Messages API, everything else → an
//! OpenAI-compatible `/chat/completions` endpoint. The body builders and response
//! parsers are pure functions so they are unit-tested without a network; the HTTP
//! send is a thin `awc` wrapper.
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

    fn parse_response(&self, body: &Value) -> Result<ModelTurn, AgentError> {
        match self.dialect {
            Dialect::OpenAiCompatible => parse_openai_response(body),
            Dialect::Anthropic => parse_anthropic_response(body),
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
        _sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
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

        let json: Value = response
            .json()
            .limit(8 * 1024 * 1024)
            .await
            .map_err(|e| transport_error(format!("failed to read model response: {e}")))?;
        self.parse_response(&json)
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

/// Build the non-streaming `/chat/completions` body. The diagnose path is
/// tool-free, so no tools are advertised.
fn build_openai_body(model: &str, request: &ModelRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(openai_message_to_json)
        .collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
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

/// Parse a non-streaming OpenAI chat-completions response into a [`ModelTurn`].
fn parse_openai_response(body: &Value) -> Result<ModelTurn, AgentError> {
    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| transport_error("model response had no choices"))?;
    let text = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    let stop_reason = openai_stop_reason(choice.get("finish_reason").and_then(|f| f.as_str()));
    let usage = body.get("usage");
    let prompt = usage.and_then(|u| u["prompt_tokens"].as_i64());
    let cached = usage.and_then(|u| u["prompt_tokens_details"]["cached_tokens"].as_i64());
    // OpenAI `prompt_tokens` includes the cached portion; subtract it out so the
    // cache read is not double-counted against `input_tokens`.
    let input_tokens = match (prompt, cached) {
        (Some(p), Some(c)) => Some((p - c).max(0)),
        (p, _) => p,
    };
    let usage = TokenUsage {
        input_tokens,
        output_tokens: usage.and_then(|u| u["completion_tokens"].as_i64()),
        cache_read_tokens: cached,
        cache_write_tokens: None,
    };
    Ok(ModelTurn {
        text,
        tool_calls: Vec::new(),
        stop_reason,
        usage,
    })
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

/// Build the non-streaming `/v1/messages` body. System text is hoisted to the
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
        "stream": false,
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

/// Parse a non-streaming Anthropic Messages response into a [`ModelTurn`].
fn parse_anthropic_response(body: &Value) -> Result<ModelTurn, AgentError> {
    let blocks = body
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| transport_error("model response had no content"))?;
    // Concatenate every text block; non-text blocks are ignored on this tool-free
    // path.
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");
    let stop_reason = anthropic_stop_reason(body.get("stop_reason").and_then(|r| r.as_str()));
    let usage = body.get("usage");
    let usage = TokenUsage {
        // Anthropic's `input_tokens` already excludes cache, so it maps as-is.
        input_tokens: usage.and_then(|u| u["input_tokens"].as_i64()),
        output_tokens: usage.and_then(|u| u["output_tokens"].as_i64()),
        cache_read_tokens: usage.and_then(|u| u["cache_read_input_tokens"].as_i64()),
        cache_write_tokens: usage.and_then(|u| u["cache_creation_input_tokens"].as_i64()),
    };
    Ok(ModelTurn {
        text,
        tool_calls: Vec::new(),
        stop_reason,
        usage,
    })
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
        assert_eq!(body["stream"], false);
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

    #[test]
    fn parse_openai_response_extracts_text_stop_and_usage() {
        let resp = json!({
            "choices": [{
                "message": {"content": "{\"summary\":\"ok\"}"},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 30},
            },
        });
        let turn = parse_openai_response(&resp).expect("parse");
        assert_eq!(turn.text, "{\"summary\":\"ok\"}");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        // input_tokens excludes the cached portion (100 - 30).
        assert_eq!(turn.usage.input_tokens, Some(70));
        assert_eq!(turn.usage.cache_read_tokens, Some(30));
        assert_eq!(turn.usage.output_tokens, Some(20));
    }

    #[test]
    fn parse_openai_response_without_choices_is_a_transport_error() {
        let resp = json!({"usage": {}});
        assert!(parse_openai_response(&resp).is_err());
    }

    #[test]
    fn parse_anthropic_response_concatenates_text_blocks_and_maps_usage() {
        let resp = json!({
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "tool_use", "name": "ignored"},
                {"type": "text", "text": "world"},
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 10,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 7,
            },
        });
        let turn = parse_anthropic_response(&resp).expect("parse");
        assert_eq!(turn.text, "hello world");
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(turn.usage.input_tokens, Some(50));
        assert_eq!(turn.usage.output_tokens, Some(10));
        assert_eq!(turn.usage.cache_read_tokens, Some(5));
        assert_eq!(turn.usage.cache_write_tokens, Some(7));
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
